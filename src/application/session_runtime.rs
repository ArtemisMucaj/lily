//! Per-thread session orchestration.
//!
//! Every chat thread maps to one OpenCode session. A `ThreadRuntime` holds
//! the session id, a busy flag, and the local message queue. Messages sent
//! while a run is active either wait in the queue (`. queue`) or interrupt the
//! run after a grace period (normal messages).
//!
//! This layer talks to the chat platform only through the
//! [`ChatConnector`](crate::application::chat::ChatConnector) port; thread and
//! message ids are opaque strings.

use crate::application::chat::ChatConnector;
use crate::application::config::Config;
use crate::connector::opencode::OpencodeClient;
use crate::connector::sqlite::Db;
use crate::domain::delivery::{self, Delivery};
use crate::domain::rendering;
use crate::domain::session::{EnqueueResult, QueueEditOutcome, QueuedMessage};
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct AppState {
    pub db: Arc<Db>,
    pub oc: OpencodeClient,
    pub config: Config,
    runtimes: Mutex<HashMap<String, Arc<ThreadRuntime>>>,
}

impl AppState {
    pub fn new(db: Arc<Db>, oc: OpencodeClient, config: Config) -> Self {
        Self { db, oc, config, runtimes: Mutex::new(HashMap::new()) }
    }
}

pub struct ThreadRuntime {
    pub thread_id: String,
    state: Mutex<RuntimeState>,
}

#[derive(Default)]
struct RuntimeState {
    /// Working directory of the session (project dir or its worktree).
    directory: String,
    session_id: Option<String>,
    busy: bool,
    queue: VecDeque<QueuedMessage>,
}

/// Look up or create the runtime for a thread, restoring the persisted
/// session id when one exists.
///
/// A runtime is never replaced for a live thread: when the resolved directory
/// changes (worktree became ready, or was merged away), the existing runtime
/// is retargeted in place so its dispatch loop and queue carry over.
pub async fn get_or_create_runtime(
    state: &Arc<AppState>,
    thread_id: &str,
    directory: String,
) -> Result<Arc<ThreadRuntime>> {
    let mut map = state.runtimes.lock().await;
    if let Some(rt) = map.get(thread_id) {
        let rt = rt.clone();
        drop(map);
        let mut s = rt.state.lock().await;
        if s.directory != directory {
            tracing::info!("thread {thread_id} working directory changed to {directory}");
            s.directory = directory;
        }
        drop(s);
        return Ok(rt);
    }
    // A storage failure must not look like "no session": that would silently
    // start a fresh session and overwrite the thread's context binding.
    let session_id = state.db.get_thread_session(thread_id)?;
    let rt = Arc::new(ThreadRuntime {
        thread_id: thread_id.to_string(),
        state: Mutex::new(RuntimeState { directory, session_id, ..Default::default() }),
    });
    map.insert(thread_id.to_string(), rt.clone());
    Ok(rt)
}

/// Drop the session binding so the next message starts a fresh session (used
/// after a merge removes the worktree directory the session lived in).
pub async fn reset_session(state: &AppState, rt: &ThreadRuntime) -> Result<()> {
    // Persist first; memory only changes once storage agrees.
    state.db.delete_thread_session(&rt.thread_id)?;
    rt.state.lock().await.session_id = None;
    Ok(())
}

async fn send(chat: &dyn ChatConnector, thread_id: &str, content: &str) {
    if let Err(err) = chat.send_message(thread_id, content).await {
        tracing::warn!("failed to send message to {thread_id}: {err:#}");
    }
}

/// Entry point for a message that should reach the agent in this thread.
///
/// Idle session → dispatch immediately. Busy session → either wait in the
/// queue (`queue_delivery`) or join the queue front and interrupt the current
/// step after the configured grace period.
pub async fn enqueue_incoming(
    state: Arc<AppState>,
    chat: Arc<dyn ChatConnector>,
    rt: Arc<ThreadRuntime>,
    msg: QueuedMessage,
    queue_delivery: bool,
) -> EnqueueResult {
    let mut s = rt.state.lock().await;
    if !s.busy {
        s.busy = true;
        drop(s);
        tokio::spawn(dispatch_loop(state, chat, rt, msg));
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
    let marker = msg.source_message_id.clone();
    s.queue.push_front(msg);
    drop(s);

    let timeout = Duration::from_millis(state.config.interrupt_timeout_ms);
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let s = rt.state.lock().await;
        let still_waiting =
            s.busy && s.queue.front().map(|m| m.source_message_id == marker).unwrap_or(false);
        let session = s.session_id.clone();
        let directory = s.directory.clone();
        drop(s);
        if still_waiting
            && let Some(session_id) = session {
                tracing::info!("interrupting session {session_id} to deliver new message");
                if let Err(err) = state.oc.abort(&directory, &session_id).await {
                    tracing::warn!("abort failed: {err:#}");
                }
                // The aborted prompt call returns and the drain loop picks the
                // message up; wait_idle is a safety net for lost responses.
                state.oc.wait_idle(&directory, &session_id, Duration::from_secs(3)).await;
            }
    });
    EnqueueResult::Dispatched
}

/// Run one prompt, then keep draining the queue until it is empty.
async fn dispatch_loop(
    state: Arc<AppState>,
    chat: Arc<dyn ChatConnector>,
    rt: Arc<ThreadRuntime>,
    first: QueuedMessage,
) {
    let mut current = first;
    loop {
        if current.show_marker {
            let preview = rendering::prompt_preview(&current.prompt, 150);
            send(
                chat.as_ref(),
                &rt.thread_id,
                &format!("{}**{}:** {}", rendering::QUEUE_PREFIX, current.username, preview),
            )
            .await;
        }
        if let Err(err) = run_prompt(&state, &chat, &rt, &current.prompt).await {
            send(chat.as_ref(), &rt.thread_id, &format!("⚠️ {err:#}")).await;
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

/// Returns the session id and the directory it is bound to, creating the
/// session on first use.
///
/// The runtime lock is not held across the (potentially slow) OpenCode call,
/// so queue edits and interrupt checks stay responsive, and the in-memory
/// binding is only committed after it has been persisted. Only the dispatch
/// loop calls this, one prompt at a time, so two creations cannot race.
async fn ensure_session(state: &AppState, rt: &ThreadRuntime) -> Result<(String, String)> {
    let (existing, directory) = {
        let s = rt.state.lock().await;
        (s.session_id.clone(), s.directory.clone())
    };
    if let Some(id) = existing {
        return Ok((id, directory));
    }
    let session = state
        .oc
        .create_session(&directory, &format!("chat thread {}", rt.thread_id))
        .await?;
    state.db.set_thread_session(&rt.thread_id, &session.id)?;
    rt.state.lock().await.session_id = Some(session.id.clone());
    Ok((session.id, directory))
}

/// Send one prompt to the session and render the run into the thread:
/// tool-activity lines stream live from the event bus, the final text parts
/// are rendered when the run completes, followed by a duration footer.
async fn run_prompt(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    rt: &Arc<ThreadRuntime>,
    prompt: &str,
) -> Result<()> {
    let (session_id, directory) = ensure_session(state, rt).await?;
    let started = std::time::Instant::now();

    let rendered_parts: Arc<std::sync::Mutex<HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));

    // Live renderer: stream tool/progress lines while the run is going.
    let live = {
        let mut events = state.oc.subscribe();
        let chat = chat.clone();
        let thread_id = rt.thread_id.clone();
        let session_id = session_id.clone();
        let rendered = rendered_parts.clone();
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    // Falling behind drops the oldest events; keep streaming
                    // rather than going silent for the rest of the run.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("live renderer lagged, skipped {n} event(s)");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
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
                let Some(part) = crate::connector::opencode::part_from_event(part_value) else { continue };
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
                if let Some(line) = rendering::format_part(&part) {
                    send(chat.as_ref(), &thread_id, &line).await;
                }
            }
        })
    };

    // Typing indicator while the run is active.
    let typing = {
        let chat = chat.clone();
        let thread_id = rt.thread_id.clone();
        tokio::spawn(async move {
            loop {
                chat.start_typing(&thread_id).await;
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
        })
    };

    let result = state.oc.prompt(&directory, &session_id, prompt).await;
    live.abort();
    typing.abort();

    let result = result?;
    let mut sent_anything = false;
    for part in &result.parts {
        if part.kind != "text" {
            continue;
        }
        if let Some(text) = rendering::format_part(part) {
            send(chat.as_ref(), &rt.thread_id, &text).await;
            sent_anything = true;
        }
    }
    if !sent_anything {
        send(chat.as_ref(), &rt.thread_id, "⬥ (no reply text)").await;
    }
    send(chat.as_ref(), &rt.thread_id, &rendering::turn_footer(started.elapsed())).await;
    Ok(())
}

// ---- queue management (edits, clears) ----

/// Apply an edit of a chat message to its queued entry: new text with the
/// queue suffix keeps it (updated), losing the suffix drops it.
pub async fn update_queue_item_for_edit(
    rt: &ThreadRuntime,
    source_message_id: &str,
    new_content: &str,
) -> QueueEditOutcome {
    let parsed = delivery::parse_message(new_content);
    let mut s = rt.state.lock().await;
    let Some(pos) = s
        .queue
        .iter()
        .position(|m| m.source_message_id.as_deref() == Some(source_message_id))
    else {
        return QueueEditOutcome::NotFound;
    };
    if matches!(parsed.delivery, Delivery::Queue) {
        s.queue[pos].prompt = parsed.prompt;
        QueueEditOutcome::Updated
    } else {
        s.queue.remove(pos);
        QueueEditOutcome::Removed
    }
}

/// Apply a message edit when the platform doesn't say which thread it
/// belongs to (Matrix `m.replace` events): scan every live runtime for the
/// queued entry. Returns the owning thread id and the outcome.
pub async fn update_queue_item_in_any(
    state: &Arc<AppState>,
    source_message_id: &str,
    new_content: &str,
) -> Option<(String, QueueEditOutcome)> {
    let runtimes: Vec<Arc<ThreadRuntime>> =
        state.runtimes.lock().await.values().cloned().collect();
    for rt in runtimes {
        let outcome = update_queue_item_for_edit(&rt, source_message_id, new_content).await;
        if outcome != QueueEditOutcome::NotFound {
            return Some((rt.thread_id.clone(), outcome));
        }
    }
    None
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
    // Persist first; memory only changes once storage agrees.
    state.db.set_thread_session(&rt.thread_id, session_id)?;
    rt.state.lock().await.session_id = Some(session_id.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime in the "busy" state, as if a run were in flight, so queue
    /// behavior can be exercised without a chat platform or agent backend.
    fn busy_runtime() -> ThreadRuntime {
        ThreadRuntime {
            thread_id: "thread-1".to_string(),
            state: Mutex::new(RuntimeState {
                directory: "/tmp/project".to_string(),
                session_id: Some("ses_test".to_string()),
                busy: true,
                queue: VecDeque::new(),
            }),
        }
    }

    fn msg(prompt: &str, source: Option<&str>) -> QueuedMessage {
        QueuedMessage {
            prompt: prompt.to_string(),
            username: "tester".to_string(),
            source_message_id: source.map(str::to_string),
            show_marker: false,
        }
    }

    async fn push(rt: &ThreadRuntime, m: QueuedMessage) {
        rt.state.lock().await.queue.push_back(m);
    }

    #[tokio::test]
    async fn edit_with_queue_suffix_updates_prompt() {
        let rt = busy_runtime();
        push(&rt, msg("original", Some("m1"))).await;

        let outcome = update_queue_item_for_edit(&rt, "m1", "edited text. queue").await;
        assert_eq!(outcome, QueueEditOutcome::Updated);
        assert_eq!(rt.state.lock().await.queue[0].prompt, "edited text");
    }

    #[tokio::test]
    async fn edit_without_queue_suffix_removes_entry() {
        let rt = busy_runtime();
        push(&rt, msg("original", Some("m1"))).await;

        let outcome = update_queue_item_for_edit(&rt, "m1", "no longer queued").await;
        assert_eq!(outcome, QueueEditOutcome::Removed);
        assert!(rt.state.lock().await.queue.is_empty());
    }

    #[tokio::test]
    async fn edit_of_unknown_message_is_a_noop() {
        let rt = busy_runtime();
        push(&rt, msg("original", Some("m1"))).await;

        let outcome = update_queue_item_for_edit(&rt, "m2", "whatever. queue").await;
        assert_eq!(outcome, QueueEditOutcome::NotFound);
        assert_eq!(rt.state.lock().await.queue.len(), 1);
    }

    #[tokio::test]
    async fn clear_queue_by_position_and_fully() {
        let rt = busy_runtime();
        push(&rt, msg("a", Some("m1"))).await;
        push(&rt, msg("b", Some("m2"))).await;
        push(&rt, msg("c", Some("m3"))).await;

        // 1-based position removal.
        assert_eq!(clear_queue(&rt, Some(2)).await.unwrap(), 1);
        {
            let s = rt.state.lock().await;
            assert_eq!(s.queue.len(), 2);
            assert_eq!(s.queue[0].prompt, "a");
            assert_eq!(s.queue[1].prompt, "c");
        }
        // Out-of-range position errors.
        assert!(clear_queue(&rt, Some(5)).await.is_err());
        assert!(clear_queue(&rt, Some(0)).await.is_err());
        // Clearing everything reports the count.
        assert_eq!(clear_queue(&rt, None).await.unwrap(), 2);
        assert!(rt.state.lock().await.queue.is_empty());
    }
}
