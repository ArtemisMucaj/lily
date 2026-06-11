//! Session domain: the messages that flow through a thread's queue and the
//! outcomes of queue operations. The runtime that drives them lives in
//! `application::session_runtime`.

use serde_json::Value;

/// One part of an assistant turn (text, tool call, file, ...), produced by
/// the agent backend and rendered into Discord by `domain::rendering`.
#[derive(Debug, Clone)]
pub struct Part {
    pub id: String,
    pub kind: String,
    pub payload: Value,
}

/// A message bound for the agent, possibly waiting in the thread's queue.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub prompt: String,
    pub username: String,
    /// Chat message that produced this entry (platform-specific id); used so
    /// editing the message updates (or removes) the queued prompt.
    pub source_message_id: Option<String>,
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

/// Result of applying a Discord message edit to the queue.
#[derive(Debug, PartialEq, Eq)]
pub enum QueueEditOutcome {
    Updated,
    Removed,
    NotFound,
}
