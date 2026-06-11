//! Routes chat-port calls to the right platform connector when several run
//! in one process. Matrix ids are self-describing (`!room:server`,
//! `room|$event`); anything else is treated as a Discord snowflake.

use crate::application::chat::ChatConnector;
use anyhow::{anyhow, Result};
use std::sync::Arc;

pub struct RoutedChat {
    pub discord: Option<Arc<dyn ChatConnector>>,
    pub matrix: Option<Arc<dyn ChatConnector>>,
}

impl RoutedChat {
    fn pick(&self, id: &str) -> Result<&Arc<dyn ChatConnector>> {
        let target = if id.starts_with('!') || id.contains('|') {
            self.matrix.as_ref()
        } else {
            self.discord.as_ref()
        };
        target.ok_or_else(|| anyhow!("no connector running for id {id}"))
    }
}

#[serenity::async_trait]
impl ChatConnector for RoutedChat {
    async fn send_message(&self, thread_id: &str, content: &str) -> Result<()> {
        self.pick(thread_id)?.send_message(thread_id, content).await
    }

    async fn post_message(&self, channel_id: &str, content: &str) -> Result<String> {
        self.pick(channel_id)?.post_message(channel_id, content).await
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message: Option<&str>,
        name: &str,
    ) -> Result<String> {
        self.pick(channel_id)?.create_thread(channel_id, from_message, name).await
    }

    async fn add_thread_member(&self, thread_id: &str, user_id: &str) -> Result<()> {
        self.pick(thread_id)?.add_thread_member(thread_id, user_id).await
    }

    async fn rename_thread(&self, thread_id: &str, name: &str) -> Result<()> {
        self.pick(thread_id)?.rename_thread(thread_id, name).await
    }

    async fn start_typing(&self, thread_id: &str) {
        if let Ok(chat) = self.pick(thread_id) {
            chat.start_typing(thread_id).await;
        }
    }

    async fn thread_parent(&self, thread_id: &str) -> Result<Option<String>> {
        self.pick(thread_id)?.thread_parent(thread_id).await
    }
}
