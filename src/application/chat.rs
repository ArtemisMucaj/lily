//! The chat port: the surface the application layer needs from a chat
//! platform. `connector::discord` implements it with serenity; a Matrix (or
//! other) connector implements the same trait without touching this layer.
//!
//! Ids are opaque strings — Discord snowflakes and Matrix room/event ids both
//! fit, and the database already stores them as TEXT.

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ChatConnector: Send + Sync {
    /// Deliver agent output into a thread. The connector handles platform
    /// limits (chunking) and notification suppression (silent flags, notice
    /// message types).
    async fn send_message(&self, thread_id: &str, content: &str) -> Result<()>;

    /// Post a regular message to a channel, returning its message id.
    async fn post_message(&self, channel_id: &str, content: &str) -> Result<String>;

    /// Create a thread in a channel, anchored to `from_message` when the
    /// platform supports it. Returns the new thread id.
    async fn create_thread(
        &self,
        channel_id: &str,
        from_message: Option<&str>,
        name: &str,
    ) -> Result<String>;

    /// Invite/add a user to a thread. Best-effort; failures are non-fatal.
    async fn add_thread_member(&self, thread_id: &str, user_id: &str) -> Result<()>;

    /// Show a short-lived typing indicator in a thread.
    async fn start_typing(&self, thread_id: &str);

    /// The parent channel of a thread, when the platform models that.
    async fn thread_parent(&self, thread_id: &str) -> Result<Option<String>>;
}
