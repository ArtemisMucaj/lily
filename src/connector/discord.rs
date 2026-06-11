//! Discord gateway handler: channels are projects, threads are sessions.
//!
//! - A message in a linked channel creates a thread and starts a session.
//! - Messages inside a thread continue that thread's session.
//! - Slash commands implement project linking, the queue, btw forks, and
//!   worktree management.

use crate::application::chat::ChatConnector;
use crate::application::session_runtime::{self as runner, AppState};
use crate::application::task_runner;
use crate::connector::git;
use crate::domain::delivery::{self, Delivery};
use crate::domain::rendering;
use crate::domain::session::{EnqueueResult, QueueEditOutcome, QueuedMessage};
use crate::domain::task::describe_task;
use crate::domain::worktree;
use anyhow::{anyhow, Context as _, Result};
use serenity::all::{
    AutoArchiveDuration, ChannelId, ChannelType, Command, CommandInteraction, CommandOptionType,
    Context, CreateCommand, CreateCommandOption, CreateInteractionResponse, CreateMessage,
    MessageFlags,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateThread, EditChannel,
    EditThread, EventHandler, GuildChannel, Http, Interaction, Message, MessageId,
    MessageUpdateEvent, Permissions, Ready, ResolvedValue,
};
use serenity::async_trait;
use std::sync::Arc;

/// Serenity-backed implementation of the chat port. Holds the HTTP client;
/// gateway events are handled separately by [`Handler`].
pub struct DiscordChat {
    pub http: Arc<Http>,
}

fn parse_id(id: &str) -> Result<u64> {
    id.parse().with_context(|| format!("not a Discord id: {id}"))
}

#[serenity::async_trait]
impl ChatConnector for DiscordChat {
    async fn send_message(&self, thread_id: &str, content: &str) -> Result<()> {
        let channel = ChannelId::new(parse_id(thread_id)?);
        for chunk in rendering::split_markdown(content, rendering::DISCORD_MESSAGE_LIMIT) {
            let msg = CreateMessage::new()
                .content(chunk)
                .flags(MessageFlags::SUPPRESS_NOTIFICATIONS);
            channel.send_message(&self.http, msg).await?;
        }
        Ok(())
    }

    async fn post_message(&self, channel_id: &str, content: &str) -> Result<String> {
        let channel = ChannelId::new(parse_id(channel_id)?);
        // Posts also respect Discord's message limit.
        let mut last_id = None;
        for chunk in rendering::split_markdown(content, rendering::DISCORD_MESSAGE_LIMIT) {
            let msg = channel.send_message(&self.http, CreateMessage::new().content(chunk)).await?;
            last_id = Some(msg.id.to_string());
        }
        last_id.ok_or_else(|| anyhow!("empty message"))
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message: Option<&str>,
        name: &str,
    ) -> Result<String> {
        let channel = ChannelId::new(parse_id(channel_id)?);
        let builder = CreateThread::new(truncate_name(name))
            .auto_archive_duration(AutoArchiveDuration::OneDay);
        let thread = match from_message {
            Some(mid) => {
                let message = serenity::all::MessageId::new(parse_id(mid)?);
                channel.create_thread_from_message(&self.http, message, builder).await?
            }
            None => {
                channel
                    .create_thread(&self.http, builder.kind(ChannelType::PublicThread))
                    .await?
            }
        };
        Ok(thread.id.to_string())
    }

    async fn add_thread_member(&self, thread_id: &str, user_id: &str) -> Result<()> {
        let thread = ChannelId::new(parse_id(thread_id)?);
        let user = serenity::all::UserId::new(parse_id(user_id)?);
        thread.add_thread_member(&self.http, user).await?;
        Ok(())
    }

    async fn start_typing(&self, thread_id: &str) {
        if let Ok(id) = parse_id(thread_id) {
            let _ = ChannelId::new(id).broadcast_typing(&self.http).await;
        }
    }

    async fn thread_parent(&self, thread_id: &str) -> Result<Option<String>> {
        let channel = ChannelId::new(parse_id(thread_id)?)
            .to_channel(&self.http)
            .await
            .context("failed to fetch thread channel")?;
        Ok(channel.guild().and_then(|gc| gc.parent_id).map(|p| p.to_string()))
    }
}

pub struct Handler {
    pub state: Arc<AppState>,
}

impl Handler {
    fn chat(&self, ctx: &Context) -> Arc<dyn ChatConnector> {
        Arc::new(DiscordChat { http: ctx.http.clone() })
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!("connected to Discord as {}", ready.user.name);
        if let Err(err) = register_commands(&ctx.http).await {
            tracing::error!("failed to register slash commands: {err:#}");
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot || msg.content.trim().is_empty() {
            return;
        }
        // Messages in project channels start agent runs on the host machine;
        // ignore users outside the allowlist (when one is configured).
        if !self.state.config.is_user_allowed(msg.author.id.get()) {
            return;
        }
        if let Err(err) = self.handle_message(&ctx, &msg).await {
            tracing::warn!("message handling failed: {err:#}");
            let _ = msg.channel_id.say(&ctx.http, format!("⚠️ {err:#}")).await;
        }
    }

    async fn message_update(
        &self,
        ctx: Context,
        _old: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        let Some(content) = event.content.clone() else { return };
        let Some(author) = event.author.clone() else { return };
        if author.bot || !self.state.config.is_user_allowed(author.id.get()) {
            return;
        }
        if let Err(err) = self
            .handle_message_edit(&ctx, event.channel_id, event.id, &author.name, &content)
            .await
        {
            tracing::warn!("message edit handling failed: {err:#}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(cmd) = interaction else { return };
        if !self.state.config.is_user_allowed(cmd.user.id.get()) {
            let _ = cmd
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("You are not authorized to use this bot.")
                            .ephemeral(true),
                    ),
                )
                .await;
            return;
        }
        if let Err(err) = self.handle_command(&ctx, &cmd).await {
            tracing::warn!("command /{} failed: {err:#}", cmd.data.name);
            let message = format!("⚠️ {err:#}");
            // Works whether or not the interaction was already acknowledged.
            if cmd
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content(message.clone()),
                    ),
                )
                .await
                .is_err()
            {
                let _ = cmd
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new().content(message),
                    )
                    .await;
            }
        }
    }
}

async fn register_commands(http: &Http) -> Result<()> {
    // Commands that point the bot at host directories or mutate git/task state
    // default to Manage Guild; server admins can adjust this per command in
    // Server Settings → Integrations.
    let admin = Permissions::MANAGE_GUILD;
    let commands = vec![
        CreateCommand::new("add-project")
            .description("Link this channel to a project directory on the bot's machine")
            .default_member_permissions(admin)
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "directory",
                    "Absolute path of the project directory",
                )
                .required(true),
            ),
        CreateCommand::new("queue")
            .description("Queue a message to send when the current run finishes")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "message", "The message to queue")
                    .required(true),
            ),
        CreateCommand::new("clear-queue")
            .description("Clear all queued messages, or one position")
            .add_option(CreateCommandOption::new(
                CommandOptionType::Integer,
                "position",
                "1-based position to remove (omit to clear all)",
            )),
        CreateCommand::new("btw")
            .description("Fork this session's context into a new thread to ask a side question")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "prompt", "The side question")
                    .required(true),
            ),
        CreateCommand::new("new-worktree")
            .description("Move this session into an isolated git worktree")
            .default_member_permissions(admin)
            .add_option(CreateCommandOption::new(
                CommandOptionType::String,
                "name",
                "Worktree name (derived from the thread name when omitted)",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::String,
                "base-branch",
                "Base ref for the worktree branch (defaults to HEAD)",
            )),
        CreateCommand::new("merge-worktree")
            .description("Rebase this thread's worktree commits back onto the default branch")
            .default_member_permissions(admin)
            .add_option(CreateCommandOption::new(
                CommandOptionType::String,
                "target-branch",
                "Target branch (defaults to the project's default branch)",
            )),
        CreateCommand::new("worktrees").description("List worktrees for this channel's project"),
        CreateCommand::new("tasks").description("List scheduled tasks"),
        CreateCommand::new("cancel-task")
            .description("Cancel a scheduled task by id")
            .default_member_permissions(admin)
            .add_option(
                CreateCommandOption::new(CommandOptionType::Integer, "id", "Task id from /tasks")
                    .required(true),
            ),
    ];
    Command::set_global_commands(http, commands).await?;
    Ok(())
}

fn str_option(cmd: &CommandInteraction, name: &str) -> Option<String> {
    for opt in cmd.data.options() {
        if opt.name == name
            && let ResolvedValue::String(s) = opt.value {
                return Some(s.to_string());
            }
    }
    None
}

fn int_option(cmd: &CommandInteraction, name: &str) -> Option<i64> {
    for opt in cmd.data.options() {
        if opt.name == name
            && let ResolvedValue::Integer(i) = opt.value {
                return Some(i);
            }
    }
    None
}

async fn respond(ctx: &Context, cmd: &CommandInteraction, text: impl Into<String>) -> Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(text.into())),
    )
    .await?;
    Ok(())
}

async fn defer(ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
    )
    .await?;
    Ok(())
}

async fn followup(ctx: &Context, cmd: &CommandInteraction, text: impl Into<String>) -> Result<()> {
    cmd.create_followup(&ctx.http, CreateInteractionResponseFollowup::new().content(text.into()))
        .await?;
    Ok(())
}

impl Handler {
    async fn guild_channel(&self, ctx: &Context, id: ChannelId) -> Result<GuildChannel> {
        let channel = id.to_channel(&ctx.http).await.context("failed to fetch channel")?;
        channel.guild().ok_or_else(|| anyhow!("not a guild channel"))
    }

    fn is_thread(kind: ChannelType) -> bool {
        matches!(kind, ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread)
    }

    // ---- plain messages ----

    async fn handle_message(&self, ctx: &Context, msg: &Message) -> Result<()> {
        let channel = self.guild_channel(ctx, msg.channel_id).await?;
        if Self::is_thread(channel.kind) {
            self.handle_thread_message(ctx, msg, &channel).await
        } else {
            self.handle_channel_message(ctx, msg).await
        }
    }

    /// First message in a project channel: create a thread and start a session.
    async fn handle_channel_message(&self, ctx: &Context, msg: &Message) -> Result<()> {
        let Some(directory) =
            self.state.db.get_channel_directory(&msg.channel_id.to_string())?
        else {
            // Not a project channel; stay quiet.
            return Ok(());
        };
        let parsed = delivery::parse_message(&msg.content);
        let thread_name = rendering::prompt_preview(&parsed.prompt, 80);
        let thread = msg
            .channel_id
            .create_thread_from_message(
                &ctx.http,
                msg.id,
                CreateThread::new(if thread_name.is_empty() { "session".into() } else { thread_name })
                    .auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await
            .context("failed to create thread")?;
        let _ = thread.id.add_thread_member(&ctx.http, msg.author.id).await;

        let rt = runner::get_or_create_runtime(&self.state, &thread.id.to_string(), directory).await;
        // A fresh session is idle: queue/btw suffixes behave like a normal send.
        runner::enqueue_incoming(
            self.state.clone(),
            self.chat(ctx),
            rt,
            QueuedMessage {
                prompt: parsed.prompt,
                username: msg.author.name.clone(),
                source_message_id: Some(msg.id.to_string()),
                show_marker: false,
            },
            false,
        )
        .await;
        Ok(())
    }

    /// Message in an existing thread: continue the session, queue, or btw.
    async fn handle_thread_message(
        &self,
        ctx: &Context,
        msg: &Message,
        thread: &GuildChannel,
    ) -> Result<()> {
        let directory =
            task_runner::resolve_thread_directory(&self.state, &*self.chat(ctx), &msg.channel_id.to_string()).await?;
        let parsed = delivery::parse_message(&msg.content);
        let rt = runner::get_or_create_runtime(&self.state, &msg.channel_id.to_string(), directory.clone()).await;

        match parsed.delivery {
            Delivery::Btw => {
                let parent = thread
                    .parent_id
                    .ok_or_else(|| anyhow!("thread has no parent channel"))?;
                self.fork_btw(
                    ctx,
                    msg.channel_id,
                    parent,
                    &directory,
                    &parsed.prompt,
                    msg.author.id.get(),
                    &msg.author.name,
                )
                .await?;
            }
            Delivery::Queue => {
                let result = runner::enqueue_incoming(
                    self.state.clone(),
                    self.chat(ctx),
                    rt,
                    QueuedMessage {
                        prompt: parsed.prompt,
                        username: msg.author.name.clone(),
                        source_message_id: Some(msg.id.to_string()),
                        show_marker: false,
                    },
                    true,
                )
                .await;
                if let EnqueueResult::Queued(pos) = result {
                    msg.channel_id
                        .say(
                            &ctx.http,
                            format!("Queued at position {pos}. Edit your message to update it in the queue."),
                        )
                        .await?;
                }
            }
            Delivery::Normal => {
                runner::enqueue_incoming(
                    self.state.clone(),
                    self.chat(ctx),
                    rt,
                    QueuedMessage {
                        prompt: parsed.prompt,
                        username: msg.author.name.clone(),
                        source_message_id: Some(msg.id.to_string()),
                        show_marker: false,
                    },
                    false,
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_message_edit(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
        message_id: MessageId,
        username: &str,
        new_content: &str,
    ) -> Result<()> {
        let channel = self.guild_channel(ctx, channel_id).await?;
        if !Self::is_thread(channel.kind) {
            return Ok(());
        }
        let Ok(directory) =
            task_runner::resolve_thread_directory(&self.state, &*self.chat(ctx), &channel_id.to_string()).await
        else {
            return Ok(());
        };
        let rt = runner::get_or_create_runtime(&self.state, &channel_id.to_string(), directory).await;
        match runner::update_queue_item_for_edit(&rt, &message_id.to_string(), new_content).await {
            QueueEditOutcome::Updated => {
                channel_id
                    .say(&ctx.http, format!("⬦ **{username}** edited queued message"))
                    .await?;
            }
            QueueEditOutcome::Removed => {
                channel_id
                    .say(&ctx.http, format!("⬦ **{username}** removed message from queue"))
                    .await?;
            }
            QueueEditOutcome::NotFound => {}
        }
        Ok(())
    }

    /// Fork the session's full context into a new `btw:` thread and dispatch
    /// the side question there. The original thread keeps working untouched.
    #[allow(clippy::too_many_arguments)]
    async fn fork_btw(
        &self,
        ctx: &Context,
        source_thread: ChannelId,
        parent_channel: ChannelId,
        directory: &str,
        prompt: &str,
        user_id: u64,
        username: &str,
    ) -> Result<ChannelId> {
        let session_id = self
            .state
            .db
            .get_thread_session(&source_thread.to_string())?
            .ok_or_else(|| anyhow!("no active session in this thread"))?;
        let forked = self
            .state
            .oc
            .fork_session(directory, &session_id)
            .await
            .context("failed to fork session")?;

        let name = format!("btw: {}", rendering::prompt_preview(prompt, 90));
        let thread = parent_channel
            .create_thread(
                &ctx.http,
                CreateThread::new(name)
                    .kind(ChannelType::PublicThread)
                    .auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await
            .context("failed to create btw thread")?;
        self.state.db.set_thread_session(&thread.id.to_string(), &forked.id)?;
        let _ = thread
            .id
            .add_thread_member(&ctx.http, serenity::all::UserId::new(user_id))
            .await;
        thread
            .id
            .say(
                &ctx.http,
                format!("Reusing context from <#{source_thread}> to answer prompt...\n{prompt}"),
            )
            .await?;

        let wrapped = format!(
            "The user asked a side question while you were working on another task.\n\
             This is a forked session whose ONLY goal is to answer this question.\n\
             Do NOT continue, resume, or reference the previous task. Only answer the question below.\n\n{prompt}"
        );
        let rt =
            runner::get_or_create_runtime(&self.state, &thread.id.to_string(), directory.to_string()).await;
        runner::enqueue_incoming(
            self.state.clone(),
            self.chat(ctx),
            rt,
            QueuedMessage {
                prompt: wrapped,
                username: username.to_string(),
                source_message_id: None,
                show_marker: false,
            },
            false,
        )
        .await;
        Ok(thread.id)
    }

    // ---- slash commands ----

    async fn handle_command(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        match cmd.data.name.as_str() {
            "add-project" => self.cmd_add_project(ctx, cmd).await,
            "queue" => self.cmd_queue(ctx, cmd).await,
            "clear-queue" => self.cmd_clear_queue(ctx, cmd).await,
            "btw" => self.cmd_btw(ctx, cmd).await,
            "new-worktree" => self.cmd_new_worktree(ctx, cmd).await,
            "merge-worktree" => self.cmd_merge_worktree(ctx, cmd).await,
            "worktrees" => self.cmd_worktrees(ctx, cmd).await,
            "tasks" => self.cmd_tasks(ctx, cmd).await,
            "cancel-task" => self.cmd_cancel_task(ctx, cmd).await,
            other => Err(anyhow!("unknown command: {other}")),
        }
    }

    async fn cmd_add_project(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let directory = str_option(cmd, "directory").ok_or_else(|| anyhow!("directory is required"))?;
        let path = std::path::Path::new(&directory);
        if !path.is_absolute() || !path.is_dir() {
            return Err(anyhow!("`{directory}` is not an absolute path to an existing directory on the bot's machine"));
        }
        let channel = self.guild_channel(ctx, cmd.channel_id).await?;
        if Self::is_thread(channel.kind) {
            return Err(anyhow!("run /add-project in a channel, not a thread"));
        }
        self.state.db.set_channel_directory(&cmd.channel_id.to_string(), &directory)?;
        // Mirror the mapping in the channel topic so it is visible in Discord.
        let topic = format!("<lily><directory>{directory}</directory></lily>");
        let _ = cmd.channel_id.edit(&ctx.http, EditChannel::new().topic(topic)).await;
        respond(
            ctx,
            cmd,
            format!("Linked this channel to `{directory}`. Send a message to start a session."),
        )
        .await
    }

    async fn thread_runtime(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
    ) -> Result<Arc<runner::ThreadRuntime>> {
        let channel = self.guild_channel(ctx, cmd.channel_id).await?;
        if !Self::is_thread(channel.kind) {
            return Err(anyhow!("this command only works inside a session thread"));
        }
        let directory =
            task_runner::resolve_thread_directory(&self.state, &*self.chat(ctx), &cmd.channel_id.to_string()).await?;
        Ok(runner::get_or_create_runtime(&self.state, &cmd.channel_id.to_string(), directory).await)
    }

    async fn cmd_queue(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let message = str_option(cmd, "message").ok_or_else(|| anyhow!("message is required"))?;
        let rt = self.thread_runtime(ctx, cmd).await?;
        let result = runner::enqueue_incoming(
            self.state.clone(),
            self.chat(ctx),
            rt,
            QueuedMessage {
                prompt: message,
                username: cmd.user.name.clone(),
                source_message_id: None,
                show_marker: false,
            },
            true,
        )
        .await;
        match result {
            EnqueueResult::Queued(pos) => respond(ctx, cmd, format!("Queued message (position {pos})")).await,
            EnqueueResult::Dispatched => respond(ctx, cmd, "Session was idle; message sent immediately.").await,
        }
    }

    async fn cmd_clear_queue(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let position = int_option(cmd, "position").map(|i| i.max(0) as usize);
        let rt = self.thread_runtime(ctx, cmd).await?;
        let removed = runner::clear_queue(&rt, position).await?;
        match position {
            Some(p) => respond(ctx, cmd, format!("Cleared queued message at position {p}")).await,
            None => respond(ctx, cmd, format!("Cleared {removed} queued message(s)")).await,
        }
    }

    async fn cmd_btw(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let prompt = str_option(cmd, "prompt").ok_or_else(|| anyhow!("prompt is required"))?;
        let channel = self.guild_channel(ctx, cmd.channel_id).await?;
        if !Self::is_thread(channel.kind) {
            return Err(anyhow!("/btw only works inside a session thread"));
        }
        let parent = channel.parent_id.ok_or_else(|| anyhow!("thread has no parent channel"))?;
        let directory =
            task_runner::resolve_thread_directory(&self.state, &*self.chat(ctx), &cmd.channel_id.to_string()).await?;
        defer(ctx, cmd).await?;
        let thread_id = self
            .fork_btw(ctx, cmd.channel_id, parent, &directory, &prompt, cmd.user.id.get(), &cmd.user.name)
            .await?;
        followup(ctx, cmd, format!("Session forked! Continue in <#{thread_id}>")).await
    }

    async fn cmd_new_worktree(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let channel = self.guild_channel(ctx, cmd.channel_id).await?;
        let name = str_option(cmd, "name");
        let base_branch = str_option(cmd, "base-branch");

        let (project_directory, thread_id, slug) = if Self::is_thread(channel.kind) {
            let parent = channel.parent_id.ok_or_else(|| anyhow!("thread has no parent channel"))?;
            let project = self
                .state
                .db
                .get_channel_directory(&parent.to_string())?
                .ok_or_else(|| anyhow!("parent channel is not linked to a project"))?;
            let slug = match &name {
                Some(n) => worktree::slugify(n),
                // Auto-derived names get the vowel-stripping compression.
                None => worktree::compress_slug(&worktree::slugify(&channel.name)),
            };
            (project, cmd.channel_id, slug)
        } else {
            let project = self
                .state
                .db
                .get_channel_directory(&cmd.channel_id.to_string())?
                .ok_or_else(|| anyhow!("this channel is not linked to a project"))?;
            let name = name.ok_or_else(|| anyhow!("pass a name when running /new-worktree from a channel"))?;
            let slug = worktree::slugify(&name);
            // Create the thread right away so the user can start typing.
            let thread = cmd
                .channel_id
                .create_thread(
                    &ctx.http,
                    CreateThread::new(format!("{}{}", worktree::THREAD_PREFIX, slug))
                        .kind(ChannelType::PublicThread)
                        .auto_archive_duration(AutoArchiveDuration::OneDay),
                )
                .await?;
            let _ = thread.id.add_thread_member(&ctx.http, cmd.user.id).await;
            (project, thread.id, slug)
        };
        if slug.is_empty() {
            return Err(anyhow!("could not derive a worktree name; pass one explicitly"));
        }

        let wt_dir = worktree::worktree_directory(&self.state.config.data_dir, &project_directory, &slug);
        self.state.db.create_pending_worktree(
            &thread_id.to_string(),
            &worktree::branch_name(&slug),
            &project_directory,
        )?;
        respond(
            ctx,
            cmd,
            format!(
                "Creating worktree `{}` on branch `{}`...",
                wt_dir.display(),
                worktree::branch_name(&slug)
            ),
        )
        .await?;

        // Build the worktree in the background, then switch the thread to it.
        let state = self.state.clone();
        let http = ctx.http.clone();
        let in_thread = Self::is_thread(channel.kind);
        let channel_name = channel.name.clone();
        tokio::spawn(async move {
            let result = git::create_worktree(
                &project_directory,
                &wt_dir,
                &slug,
                base_branch.as_deref(),
            )
            .await;
            match result {
                Ok(()) => {
                    let dir_str = wt_dir.to_string_lossy().to_string();
                    if let Err(err) = state.db.set_worktree_ready(&thread_id.to_string(), &dir_str) {
                        tracing::warn!("failed to persist worktree state: {err:#}");
                    }
                    // Retarget the thread's runtime in place (keeping its queue
                    // and dispatch loop) so the next message runs inside the
                    // worktree. If a session already exists, fork it into the
                    // worktree so the context carries over.
                    let old_session = state.db.get_thread_session(&thread_id.to_string()).ok().flatten();
                    let rt = runner::get_or_create_runtime(&state, &thread_id.to_string(), dir_str.clone()).await;
                    if let Some(old) = old_session {
                        match state.oc.fork_session(&dir_str, &old).await {
                            Ok(forked) => {
                                let _ = runner::set_session_id(&state, &rt, &forked.id).await;
                            }
                            Err(err) => {
                                tracing::warn!("could not fork session into worktree: {err:#}");
                            }
                        }
                    }
                    if in_thread && !channel_name.starts_with(worktree::THREAD_PREFIX) {
                        let new_name = format!("{}{}", worktree::THREAD_PREFIX, channel_name);
                        let _ = thread_id
                            .edit_thread(&http, EditThread::new().name(truncate_name(&new_name)))
                            .await;
                    }
                    let _ = thread_id
                        .say(&http, format!("🌳 Worktree ready at `{dir_str}`. New messages run in the worktree."))
                        .await;
                }
                Err(err) => {
                    let _ = state.db.set_worktree_error(&thread_id.to_string(), &format!("{err:#}"));
                    let _ = thread_id.say(&http, format!("⚠️ Worktree creation failed: {err:#}")).await;
                }
            }
        });
        Ok(())
    }

    async fn cmd_merge_worktree(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let channel = self.guild_channel(ctx, cmd.channel_id).await?;
        if !Self::is_thread(channel.kind) {
            return Err(anyhow!("/merge-worktree only works inside a worktree thread"));
        }
        let wt = self
            .state
            .db
            .get_thread_worktree(&cmd.channel_id.to_string())?
            .ok_or_else(|| anyhow!("this thread has no worktree (use /new-worktree first)"))?;
        if wt.status != "ready" {
            let detail = wt.error_message.clone().map(|e| format!(": {e}")).unwrap_or_default();
            return Err(anyhow!("worktree is not ready (status: {}{detail})", wt.status));
        }
        let wt_dir = wt
            .worktree_directory
            .clone()
            .ok_or_else(|| anyhow!("worktree directory missing"))?;
        let slug = wt
            .worktree_name
            .strip_prefix(worktree::BRANCH_PREFIX)
            .unwrap_or(&wt.worktree_name)
            .to_string();
        let target = match str_option(cmd, "target-branch") {
            Some(t) => t,
            None => git::default_branch(&wt.project_directory).await?,
        };
        defer(ctx, cmd).await?;

        let outcome = git::merge_worktree(
            std::path::Path::new(&wt_dir),
            &wt.project_directory,
            &slug,
            &target,
        )
        .await?;

        match outcome {
            worktree::MergeOutcome::Success { target_branch, branch_name, commit_count, short_sha } => {
                self.state.db.delete_thread_worktree(&cmd.channel_id.to_string())?;
                // The worktree directory is gone: retarget the runtime back at
                // the project (keeping its queue) and drop the session that was
                // bound to the removed directory.
                let rt = runner::get_or_create_runtime(
                    &self.state,
                    &cmd.channel_id.to_string(),
                    wt.project_directory.clone(),
                )
                .await;
                runner::reset_session(&self.state, &rt).await?;
                if let Some(stripped) = channel.name.strip_prefix(worktree::THREAD_PREFIX.trim_end()) {
                    let _ = cmd
                        .channel_id
                        .edit_thread(&ctx.http, EditThread::new().name(truncate_name(stripped.trim_start_matches([' ', ':']))))
                        .await;
                }
                followup(
                    ctx,
                    cmd,
                    format!("Merged {commit_count} commit(s) from `{branch_name}` into `{target_branch}` (now at `{short_sha}`). Worktree removed."),
                )
                .await
            }
            worktree::MergeOutcome::RebaseConflict { target_branch } => {
                followup(
                    ctx,
                    cmd,
                    format!("Rebase onto `{target_branch}` hit conflicts. Asking the agent to resolve them; run /merge-worktree again once it finishes."),
                )
                .await?;
                let directory =
                    task_runner::resolve_thread_directory(&self.state, &*self.chat(ctx), &cmd.channel_id.to_string()).await?;
                let rt = runner::get_or_create_runtime(&self.state, &cmd.channel_id.to_string(), directory).await;
                runner::enqueue_incoming(
                    self.state.clone(),
                    self.chat(ctx),
                    rt,
                    QueuedMessage {
                        prompt: worktree::conflict_resolution_prompt(&target_branch),
                        username: "lily".to_string(),
                        source_message_id: None,
                        show_marker: false,
                    },
                    false,
                )
                .await;
                Ok(())
            }
            worktree::MergeOutcome::DirtyWorktree => {
                followup(ctx, cmd, "The worktree has uncommitted changes. Commit or stash them first.").await
            }
            worktree::MergeOutcome::TargetDirty { target_branch } => {
                followup(
                    ctx,
                    cmd,
                    format!("`{target_branch}` is checked out with uncommitted changes in the main repo. Clean it up first."),
                )
                .await
            }
            worktree::MergeOutcome::NothingToMerge => {
                followup(ctx, cmd, "No commits to merge yet.").await
            }
        }
    }

    async fn cmd_worktrees(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let channel = self.guild_channel(ctx, cmd.channel_id).await?;
        let channel_id = if Self::is_thread(channel.kind) {
            channel.parent_id.ok_or_else(|| anyhow!("thread has no parent"))?
        } else {
            cmd.channel_id
        };
        let project = self
            .state
            .db
            .get_channel_directory(&channel_id.to_string())?
            .ok_or_else(|| anyhow!("this channel is not linked to a project"))?;
        let git_list = git::list_worktrees(&project).await?;
        let db_list = self.state.db.list_worktrees_for_project(&project)?;
        let mut lines = vec![format!("Worktrees for `{project}`:")];
        for (path, branch) in &git_list {
            let meta = db_list
                .iter()
                .find(|w| w.worktree_directory.as_deref() == Some(path.as_str()))
                .map(|w| format!(" — lily thread <#{}> ({})", w.thread_id, w.status))
                .unwrap_or_default();
            lines.push(format!("- `{path}` on `{branch}`{meta}"));
        }
        if git_list.is_empty() {
            lines.push("(none)".to_string());
        }
        respond(ctx, cmd, lines.join("\n")).await
    }

    async fn cmd_tasks(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let tasks = self.state.db.list_tasks(false)?;
        if tasks.is_empty() {
            return respond(ctx, cmd, "No scheduled tasks. Create one with `lily send --send-at ...`.").await;
        }
        let lines: Vec<String> = tasks.iter().map(describe_task).collect();
        respond(ctx, cmd, format!("Scheduled tasks:\n{}", lines.join("\n"))).await
    }

    async fn cmd_cancel_task(&self, ctx: &Context, cmd: &CommandInteraction) -> Result<()> {
        let id = int_option(cmd, "id").ok_or_else(|| anyhow!("id is required"))?;
        if self.state.db.cancel_task(id)? {
            respond(ctx, cmd, format!("Cancelled task #{id}")).await
        } else {
            respond(ctx, cmd, format!("Task #{id} is not planned or running")).await
        }
    }
}

fn truncate_name(s: &str) -> String {
    let mut cut = s.len().min(100);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}
