//! Per-thread session orchestration.
//!
//! Every Discord thread maps to one OpenCode session. A `ThreadRuntime` holds
//! the session id, a busy flag, and the local message queue. Messages sent
//! while a run is active either wait in the queue (`. queue`) or interrupt the
//! run after a grace period (normal messages).

use crate::config::Config;
use crate::db::Db;
use crate::format;
use crate::opencode::OpencodeClient;
use anyhow::{anyhow, Result};
use serenity::all::{ChannelId, CreateMessage, Http, MessageFlags, MessageId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct AppState {
    pub db: Arc<Db>,
    pub oc: OpencodeClient,
    pub config: Config,
    runtimes: Mutex<HashMap<ChannelId, Arc<ThreadRuntime>>>,
}

impl AppState {
    pub fn new(db: Arc<Db>, oc: OpencodeClient, config: Config) -> Self {
        Self { db, oc, config, runtimes: Mutex::new(HashMap::new()) }
    }
}

#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub prompt: String,
    pub username: String,
    /// Discord message that produced this entry; used so editing the message
    /// updates (or removes) the queued prompt.
    pub source_message_id: Option<MessageId>,
    /// Show the `»` dispatched-from-queue marker when this entry had to wait.
    pub show_marker: bool,
}

#[derive(Debug)]
pub enum EnqueueResult {
    /// Dispatch started immediately.
    Dispatched,
    /// Waiting behind the current run; 1-based position.
    Queued(usize),
}

pub struct ThreadRuntime {
    pub thread_id: ChannelId,
    /// Working directory of the session (project dir or its worktree).
    pub directory: String,
    state: Mutex<RuntimeState>,
}

#[derive(Default)]
struct RuntimeState {
    session_id: Option<String>,
    busy: bool,
    queue: VecDeque<QueuedMessage>,
}

/// Look up or create the runtime for a thread, restoring the persisted
/// session id when one exists.
pub async fn get_or_create_runtime(
    state: &Arc<AppState>,
    thread_id: ChannelId,
    directory: String,
) -> Arc<ThreadRuntime> {
    let mut map = state.runtimes.lock().await;
    if let Some(rt) = map.get(&thread_id) {
        return rt.clone();
    }
    let session_id = state.db.get_thread_session(&thread_id.to_string()).ok().flatten();
    let rt = Arc::new(ThreadRuntime {
        thread_id,
        directory,
        state: Mutex::new(RuntimeState { session_id, ..Default::default() }),
    });
    map.insert(thread_id, rt.clone());
    rt
}

pub async fn remove_runtime(state: &Arc<AppState>, thread_id: ChannelId) {
    state.runtimes.lock().await.remove(&thread_id);
}

async fn send_silent(http: &Http, channel: ChannelId, content: &str) {
    for chunk in format::split_markdown(content, format::DISCORD_MESSAGE_LIMIT) {
        let msg = CreateMessage::new()
            .content(chunk)
            .flags(MessageFlags::SUPPRESS_NOTIFICATIONS);
        if let Err(err) = channel.send_message(http, msg).await {
            tracing::warn!("failed to send message to {channel}: {err}");
        }
    }
}

/// Entry point for a message that should reach the agent in this thread.
///
/// Idle session → dispatch immediately. Busy session → either wait in the
/// queue (`queue_delivery`) or join the queue front and interrupt the current
/// step after the configured grace period.
pub async fn enqueue_incoming(
    state: Arc<AppState>,
    http: Arc<Http>,
    rt: Arc<ThreadRuntime>,
    msg: QueuedMessage,
    queue_delivery: bool,
) -> EnqueueResult {
    let mut s = rt.state.lock().await;
    if !s.busy {
        s.busy = true;
        drop(s);
        tokio::spawn(dispatch_loop(state, http, rt, msg));
        return EnqueueResult::Dispatched;
    }

    if queue_delivery {
        let mut msg = msg;
        msg.show_marker = true;
        s.queue.push_back(msg);
        return EnqueueResult::Queued(s.queue.len());
    }

    // Normal message during a run: it goes to the front of the queue and, if
    // the current step is still going after the grace period, we abort the
    // step so the message takes over (a message acts as an interrupt).
    let marker = msg.source_message_id;
    s.queue.push_front(msg);
    drop(s);

    let timeout = Duration::from_millis(state.config.interrupt_timeout_ms);
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let s = rt.state.lock().await;
        let still_waiting =
            s.busy && s.queue.front().map(|m| m.source_message_id == marker).unwrap_or(false);
        let session = s.session_id.clone();
        drop(s);
        if still_waiting
            && let Some(session_id) = session {
                tracing::info!("interrupting session {session_id} to deliver new message");
                if let Err(err) = state.oc.abort(&rt.directory, &session_id).await {
                    tracing::warn!("abort failed: {err:#}");
                }
                // The aborted prompt call returns and the drain loop picks the
                // message up; wait_idle is a safety net for lost responses.
                state.oc.wait_idle(&rt.directory, &session_id, Duration::from_secs(3)).await;
            }
    });
    EnqueueResult::Dispatched
}

/// Run one prompt, then keep draining the queue until it is empty.
async fn dispatch_loop(
    state: Arc<AppState>,
    http: Arc<Http>,
    rt: Arc<ThreadRuntime>,
    first: QueuedMessage,
) {
    let mut current = first;
    loop {
        if current.show_marker {
            let preview = format::prompt_preview(&current.prompt, 150);
            send_silent(
                &http,
                rt.thread_id,
                &format!("{}**{}:** {}", format::QUEUE_PREFIX, current.username, preview),
            )
            .await;
        }
        if let Err(err) = run_prompt(&state, &http, &rt, &current.prompt).await {
            send_silent(&http, rt.thread_id, &format!("⚠️ {err:#}")).await;
        }
        let mut s = rt.state.lock().await;
        match s.queue.pop_front() {
            Some(next) => current = next,
            None => {
                s.busy = false;
                return;
            }
        }
    }
}

async fn ensure_session(state: &AppState, rt: &ThreadRuntime) -> Result<String> {
    let mut s = rt.state.lock().await;
    if let Some(id) = &s.session_id {
        return Ok(id.clone());
    }
    let session = state
        .oc
        .create_session(&rt.directory, &format!("discord thread {}", rt.thread_id))
        .await?;
    s.session_id = Some(session.id.clone());
    state.db.set_thread_session(&rt.thread_id.to_string(), &session.id)?;
    Ok(session.id)
}

/// Send one prompt to the session and render the run into the thread:
/// tool-activity lines stream live from the event bus, the final text parts
/// are rendered when the run completes, followed by a duration footer.
async fn run_prompt(
    state: &Arc<AppState>,
    http: &Arc<Http>,
    rt: &Arc<ThreadRuntime>,
    prompt: &str,
) -> Result<()> {
    let session_id = ensure_session(state, rt).await?;
    let started = std::time::Instant::now();

    let rendered_parts: Arc<std::sync::Mutex<HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));

    // Live renderer: stream tool/progress lines while the run is going.
    let live = {
        let mut events = state.oc.subscribe();
        let http = http.clone();
        let thread_id = rt.thread_id;
        let session_id = session_id.clone();
        let rendered = rendered_parts.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if event.session_id.as_deref() != Some(&session_id) {
                    continue;
                }
                if event.kind != "message.part.updated" {
                    continue;
                }
                let Some(part_value) = event.payload.get("properties").and_then(|p| p.get("part"))
                else {
                    continue;
                };
                let Some(part) = crate::opencode::part_from_event(part_value) else { continue };
                // Text parts are rendered at the end of the run.
                if part.kind == "text" {
                    continue;
                }
                let status = part
                    .payload
                    .get("state")
                    .and_then(|s| s.get("status"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let key = format!("{}:{}", part.id, status);
                if !rendered.lock().expect("rendered set poisoned").insert(key) {
                    continue;
                }
                // Render each tool once, when it completes (or errors).
                if status != "completed" && status != "error" {
                    continue;
                }
                if let Some(line) = format::format_part(&part) {
                    send_silent(&http, thread_id, &line).await;
                }
            }
        })
    };

    // Typing indicator while the run is active.
    let typing = {
        let http = http.clone();
        let thread_id = rt.thread_id;
        tokio::spawn(async move {
            loop {
                let _ = thread_id.broadcast_typing(&http).await;
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
        })
    };

    let result = state.oc.prompt(&rt.directory, &session_id, prompt).await;
    live.abort();
    typing.abort();

    let result = result?;
    let mut sent_anything = false;
    for part in &result.parts {
        if part.kind != "text" {
            continue;
        }
        if let Some(text) = format::format_part(part) {
            send_silent(http, rt.thread_id, &text).await;
            sent_anything = true;
        }
    }
    if !sent_anything {
        send_silent(http, rt.thread_id, "⬥ (no reply text)").await;
    }
    send_silent(http, rt.thread_id, &format::turn_footer(started.elapsed())).await;
    Ok(())
}

// ---- queue management (edits, clears) ----

#[derive(Debug, PartialEq, Eq)]
pub enum QueueEditOutcome {
    Updated,
    Removed,
    NotFound,
}

/// Apply an edit of a Discord message to its queued entry: new text with the
/// queue suffix keeps it (updated), losing the suffix drops it.
pub async fn update_queue_item_for_edit(
    rt: &ThreadRuntime,
    source_message_id: MessageId,
    new_content: &str,
) -> QueueEditOutcome {
    let parsed = crate::suffix::parse_message(new_content);
    let mut s = rt.state.lock().await;
    let Some(pos) = s
        .queue
        .iter()
        .position(|m| m.source_message_id == Some(source_message_id))
    else {
        return QueueEditOutcome::NotFound;
    };
    if matches!(parsed.delivery, crate::suffix::Delivery::Queue) {
        s.queue[pos].prompt = parsed.prompt;
        QueueEditOutcome::Updated
    } else {
        s.queue.remove(pos);
        QueueEditOutcome::Removed
    }
}

/// Clear the whole queue (None) or one 1-based position. Returns the number
/// of removed entries, or an error for a bad position.
pub async fn clear_queue(rt: &ThreadRuntime, position: Option<usize>) -> Result<usize> {
    let mut s = rt.state.lock().await;
    match position {
        None => {
            let n = s.queue.len();
            s.queue.clear();
            Ok(n)
        }
        Some(p) => {
            if p == 0 || p > s.queue.len() {
                return Err(anyhow!("no queued message at position {p}"));
            }
            s.queue.remove(p - 1);
            Ok(1)
        }
    }
}

/// Replace the runtime's session id (used after forking into a worktree).
pub async fn set_session_id(state: &AppState, rt: &ThreadRuntime, session_id: &str) -> Result<()> {
    let mut s = rt.state.lock().await;
    s.session_id = Some(session_id.to_string());
    state.db.set_thread_session(&rt.thread_id.to_string(), session_id)?;
    Ok(())
}
