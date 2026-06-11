//! Client for the OpenCode server (`opencode serve`, default port 4096).
//!
//! Endpoints used:
//! - `POST /session`                create a session
//! - `POST /session/:id/message`    send a prompt; blocks until the run ends
//! - `POST /session/:id/abort`      abort the running step
//! - `POST /session/:id/fork`       fork the full session context
//! - `GET  /session/status`         busy/idle per session
//! - `GET  /event`                  SSE stream of bus events (shared globally)
//!
//! Requests are scoped to a project directory with the `x-opencode-directory`
//! header, mirroring how the kimaki CLI drives the same server.

use crate::domain::session::Part;
use anyhow::{anyhow, Context as _, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct OpencodeClient {
    http: reqwest::Client,
    base: String,
    events: broadcast::Sender<Event>,
}

/// A loosely-typed server event. OpenCode's bus carries many event kinds;
/// we keep the raw payload and pull out the fields we route on.
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: String,
    pub session_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct Session {
    pub id: String,
}

#[derive(Debug)]
pub struct PromptResult {
    pub parts: Vec<Part>,
}

/// Build a `Part` from a part object found in an event or message response.
pub fn part_from_event(v: &Value) -> Option<Part> {
    Some(Part {
        id: v.get("id")?.as_str()?.to_string(),
        kind: v.get("type")?.as_str()?.to_string(),
        payload: v.clone(),
    })
}

impl OpencodeClient {
    pub fn new(base_url: &str) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            http: reqwest::Client::new(),
            base: base_url.trim_end_matches('/').to_string(),
            events: tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn req(&self, method: reqwest::Method, path: &str, directory: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .header("x-opencode-directory", directory)
            .query(&[("directory", directory)])
    }

    pub async fn create_session(&self, directory: &str, title: &str) -> Result<Session> {
        let resp = self
            .req(reqwest::Method::POST, "/session", directory)
            .json(&json!({ "title": title }))
            .send()
            .await
            .context("opencode: create session request failed")?
            .error_for_status()
            .context("opencode: create session returned error status")?;
        Ok(resp.json().await?)
    }

    pub async fn fork_session(&self, directory: &str, session_id: &str) -> Result<Session> {
        let resp = self
            .req(reqwest::Method::POST, &format!("/session/{session_id}/fork"), directory)
            .json(&json!({}))
            .send()
            .await
            .context("opencode: fork session request failed")?
            .error_for_status()
            .context("opencode: fork session returned error status")?;
        Ok(resp.json().await?)
    }

    /// Send a prompt and wait for the run to finish. Incremental progress is
    /// observed separately through the event stream.
    pub async fn prompt(&self, directory: &str, session_id: &str, text: &str) -> Result<PromptResult> {
        let body = json!({ "parts": [{ "type": "text", "text": text }] });
        let resp = self
            .req(reqwest::Method::POST, &format!("/session/{session_id}/message"), directory)
            .json(&body)
            // Agent runs can be very long; cap at 30 minutes.
            .timeout(Duration::from_secs(30 * 60))
            .send()
            .await
            .context("opencode: prompt request failed")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(anyhow!("opencode: prompt returned {status}: {body}"));
        }
        let parts = body
            .get("parts")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(part_from_event).collect())
            .unwrap_or_default();
        Ok(PromptResult { parts })
    }

    pub async fn abort(&self, directory: &str, session_id: &str) -> Result<()> {
        self.req(reqwest::Method::POST, &format!("/session/{session_id}/abort"), directory)
            .json(&json!({}))
            .send()
            .await
            .context("opencode: abort request failed")?
            .error_for_status()
            .context("opencode: abort returned error status")?;
        Ok(())
    }

    /// Busy/idle status per session id.
    pub async fn session_status(&self, directory: &str) -> Result<Value> {
        let resp = self
            .req(reqwest::Method::GET, "/session/status", directory)
            .send()
            .await
            .context("opencode: status request failed")?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    pub async fn is_session_busy(&self, directory: &str, session_id: &str) -> Result<bool> {
        let status = self.session_status(directory).await?;
        let entry = status.get(session_id);
        Ok(match entry {
            None => false,
            Some(v) => v.get("type").and_then(Value::as_str).map(|t| t != "idle").unwrap_or(false),
        })
    }

    /// Poll until the session reports idle, or the timeout elapses.
    pub async fn wait_idle(&self, directory: &str, session_id: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(false) = self.is_session_busy(directory, session_id).await { return true }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Run the global SSE listener, broadcasting events to subscribers.
    /// Reconnects with backoff; intended to be spawned once at startup.
    pub async fn run_event_listener(&self, directory: &str) {
        let mut backoff = Duration::from_millis(500);
        loop {
            match self.stream_events_once(directory).await {
                Ok(()) => backoff = Duration::from_millis(500),
                Err(err) => tracing::warn!("opencode event stream error: {err:#}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
    }

    async fn stream_events_once(&self, directory: &str) -> Result<()> {
        let resp = self
            .req(reqwest::Method::GET, "/event", directory)
            .header("accept", "text/event-stream")
            .send()
            .await?
            .error_for_status()?;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE messages are separated by blank lines.
            while let Some(pos) = buf.find("\n\n") {
                let raw = buf[..pos].to_string();
                buf.drain(..pos + 2);
                for line in raw.lines() {
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let Ok(value) = serde_json::from_str::<Value>(data.trim()) else { continue };
                    self.broadcast_event(value);
                }
            }
        }
        Ok(())
    }

    fn broadcast_event(&self, value: Value) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let props = value.get("properties").unwrap_or(&Value::Null);
        let session_id = props
            .get("sessionID")
            .or_else(|| props.get("part").and_then(|p| p.get("sessionID")))
            .or_else(|| props.get("info").and_then(|i| i.get("sessionID")))
            .and_then(Value::as_str)
            .map(str::to_string);
        let _ = self.events.send(Event { kind, session_id, payload: value });
    }
}
