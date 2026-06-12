//! Matrix adapter: rooms are projects, Matrix threads are sessions.
//!
//! Implements the [`ChatConnector`](crate::application::chat::ChatConnector)
//! port with matrix-rust-sdk and runs a sync loop that routes room messages
//! into the same session runtime the Discord connector uses. Matrix has no
//! slash commands, so commands are plain text starting with `!` (e.g.
//! `!add-project /code/app`, `!queue fix the tests`).
//!
//! Id mapping: a channel id is a room id (`!room:server`); a thread id is
//! `room|root-event` since a Matrix thread is identified by its root event.
//! Agent output is sent as `m.notice` (the bot message type), with markdown
//! rendered to HTML.

use crate::application::chat::ChatConnector;
use crate::application::session_runtime::{self as runner, AppState};
use crate::application::{commands, task_runner};
use crate::domain::delivery::{self, Delivery};
use crate::domain::session::{EnqueueResult, QueueEditOutcome, QueuedMessage};
use anyhow::{anyhow, Context as _, Result};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::ruma::events::relation::Thread;
use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
};
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, RoomId};
use matrix_sdk::{Client, Room, RoomState};
use std::sync::Arc;

/// Separator between room id and thread-root event id in thread ids.
const THREAD_SEP: char = '|';

fn make_thread_id(room: &RoomId, root: &OwnedEventId) -> String {
    format!("{room}{THREAD_SEP}{root}")
}

fn split_thread_id(thread_id: &str) -> Result<(OwnedRoomId, Option<OwnedEventId>)> {
    match thread_id.split_once(THREAD_SEP) {
        Some((room, root)) => Ok((
            RoomId::parse(room).context("bad matrix room id")?,
            Some(root.try_into().context("bad matrix event id")?),
        )),
        None => Ok((RoomId::parse(thread_id).context("bad matrix room id")?, None)),
    }
}

pub struct MatrixChat {
    pub client: Client,
}

impl MatrixChat {
    fn room(&self, room_id: &RoomId) -> Result<Room> {
        self.client
            .get_room(room_id)
            .ok_or_else(|| anyhow!("not joined to matrix room {room_id}"))
    }

    async fn send_in(&self, target: &str, content: RoomMessageEventContent) -> Result<String> {
        let (room_id, root) = split_thread_id(target)?;
        let room = self.room(&room_id)?;
        let mut content = content;
        if let Some(root) = root {
            content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root)));
        }
        let resp = room.send(content).await.context("matrix send failed")?;
        Ok(resp.response.event_id.to_string())
    }
}

#[serenity::async_trait]
impl ChatConnector for MatrixChat {
    async fn send_message(&self, thread_id: &str, content: &str) -> Result<()> {
        // m.notice is the bot message type: clients render it without pinging.
        self.send_in(thread_id, RoomMessageEventContent::notice_markdown(content)).await?;
        Ok(())
    }

    async fn post_message(&self, channel_id: &str, content: &str) -> Result<String> {
        self.send_in(channel_id, RoomMessageEventContent::text_markdown(content)).await
    }

    async fn create_thread(
        &self,
        channel_id: &str,
        from_message: Option<&str>,
        name: &str,
    ) -> Result<String> {
        let (room_id, _) = split_thread_id(channel_id)?;
        let root = match from_message {
            // Matrix threads hang off an existing event; nothing to create.
            Some(event_id) => event_id.to_string(),
            // Otherwise post a root message carrying the thread "name".
            None => {
                self.send_in(room_id.as_ref(), RoomMessageEventContent::text_markdown(name))
                    .await?
            }
        };
        let root: OwnedEventId = root.as_str().try_into().context("bad matrix event id")?;
        Ok(make_thread_id(&room_id, &root))
    }

    async fn add_thread_member(&self, _thread_id: &str, _user_id: &str) -> Result<()> {
        // Threads are visible to every room member; nothing to do.
        Ok(())
    }

    async fn start_typing(&self, thread_id: &str) {
        if let Ok((room_id, _)) = split_thread_id(thread_id)
            && let Ok(room) = self.room(&room_id) {
                let _ = room.typing_notice(true).await;
            }
    }

    async fn thread_parent(&self, thread_id: &str) -> Result<Option<String>> {
        let (room_id, root) = split_thread_id(thread_id)?;
        Ok(root.map(|_| room_id.to_string()))
    }
}

/// Log in (or restore a persisted session) and return a ready client.
pub async fn build_client(state: &Arc<AppState>) -> Result<Client> {
    let config = &state.config;
    let homeserver = config
        .matrix_homeserver
        .as_ref()
        .ok_or_else(|| anyhow!("MATRIX_HOMESERVER is not set"))?;
    let user = config.matrix_user.as_ref().ok_or_else(|| anyhow!("MATRIX_USER is not set"))?;
    let password =
        config.matrix_password.as_ref().ok_or_else(|| anyhow!("MATRIX_PASSWORD is not set"))?;

    let store = config.data_dir.join("matrix-store");
    let session_file = config.data_dir.join("matrix-session.json");
    std::fs::create_dir_all(&store)?;

    let client = Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(&store, None)
        .build()
        .await
        .context("failed to build matrix client")?;

    if session_file.exists() {
        let session: MatrixSession = serde_json::from_str(&std::fs::read_to_string(&session_file)?)
            .context("corrupt matrix session file; delete it to re-login")?;
        client.restore_session(session).await.context("failed to restore matrix session")?;
        tracing::info!("restored matrix session from {}", session_file.display());
    } else {
        client
            .matrix_auth()
            .login_username(user, password)
            .initial_device_display_name("lily")
            .send()
            .await
            .context("matrix login failed")?;
        if let Some(session) = client.matrix_auth().session() {
            std::fs::write(&session_file, serde_json::to_string(&session)?)?;
        }
        tracing::info!("logged in to matrix as {user}");
    }
    Ok(client)
}

/// Attach handlers and run the sync loop forever.
pub async fn run(state: Arc<AppState>, client: Client) -> Result<()> {
    // First sync drains the backlog before handlers attach, so old messages
    // don't replay into sessions on every restart.
    let response = client.sync_once(SyncSettings::default()).await.context("initial matrix sync failed")?;
    client.add_event_handler_context(state);

    // Auto-join rooms the bot is invited to.
    client.add_event_handler(
        |ev: StrippedRoomMemberEvent, room: Room, client: Client| async move {
            if client.user_id().map(|u| u == ev.state_key) != Some(true) {
                return;
            }
            tracing::info!("joining matrix room {} on invite", room.room_id());
            if let Err(err) = room.join().await {
                tracing::warn!("failed to join {}: {err:#}", room.room_id());
            }
        },
    );

    client.add_event_handler(
        |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client, ctx: Ctx<Arc<AppState>>| async move {
            if room.state() != RoomState::Joined {
                tracing::debug!("ignoring message in non-joined room {}", room.room_id());
                return;
            }
            if client.user_id().map(|u| u == ev.sender) == Some(true) {
                return;
            }
            let state = ctx.0;
            if !state.config.is_user_allowed(ev.sender.as_str()) {
                tracing::warn!("ignoring message from disallowed user {}", ev.sender);
                return;
            }
            tracing::debug!("received message from {} in {}", ev.sender, room.room_id());
            let chat: Arc<dyn ChatConnector> = Arc::new(MatrixChat { client });
            if let Err(err) = handle_message(&state, &chat, &room, &ev).await {
                tracing::warn!("matrix message handling failed: {err:#}");
                let content = RoomMessageEventContent::notice_markdown(format!("⚠️ {err:#}"));
                let _ = room.send(content).await;
            }
        },
    );

    tracing::info!("matrix connector running");
    client
        .sync(SyncSettings::default().token(response.next_batch))
        .await
        .context("matrix sync loop ended")?;
    Ok(())
}

async fn handle_message(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    room: &Room,
    ev: &OriginalSyncRoomMessageEvent,
) -> Result<()> {
    let room_id = room.room_id().to_string();
    let sender = ev.sender.to_string();
    let username = ev.sender.localpart().to_string();

    match &ev.content.relates_to {
        // Edited message: update (or drop) its queued entry, wherever it is.
        Some(Relation::Replacement(repl)) => {
            let MessageType::Text(text) = &repl.new_content.msgtype else { return Ok(()) };
            let original = repl.event_id.to_string();
            if let Some((thread_id, outcome)) =
                runner::update_queue_item_in_any(state, &original, &text.body).await
            {
                let verb = match outcome {
                    QueueEditOutcome::Updated => "edited queued message",
                    QueueEditOutcome::Removed => "removed message from queue",
                    QueueEditOutcome::NotFound => return Ok(()),
                };
                chat.send_message(&thread_id, &format!("⬦ **{username}** {verb}")).await?;
            }
            Ok(())
        }
        // Message inside a thread: continue that session.
        Some(Relation::Thread(thread)) => {
            let MessageType::Text(text) = &ev.content.msgtype else {
                tracing::debug!("ignoring non-text thread message from {sender}");
                return Ok(());
            };
            let thread_id = make_thread_id(room.room_id(), &thread.event_id);
            tracing::info!("thread message from {sender} in {thread_id}");
            handle_thread_message(state, chat, &room_id, &thread_id, &text.body, &sender, &username, ev.event_id.as_ref())
                .await
        }
        // Room-level message: a command, or the start of a new session.
        _ => {
            let MessageType::Text(text) = &ev.content.msgtype else {
                tracing::debug!("ignoring non-text message from {sender} in {room_id}");
                return Ok(());
            };
            if let Some(rest) = text.body.strip_prefix('!') {
                tracing::info!("command from {sender} in {room_id}: !{rest}");
                let reply =
                    handle_command(state, chat, &room_id, None, rest, &sender, &username).await?;
                chat.send_message(&room_id, &reply).await?;
                return Ok(());
            }
            // Only linked rooms start sessions; stay quiet elsewhere.
            if state.db.get_channel_directory(&room_id)?.is_none() {
                tracing::debug!("ignoring message in unlinked room {room_id} — use !add-project to link it");
                return Ok(());
            }
            let directory = state.db.get_channel_directory(&room_id)?.unwrap();
            let parsed = delivery::parse_message(&text.body);
            tracing::info!("new session from {sender} in {room_id} → {directory}");
            // The user's message becomes the thread root, like Discord's
            // create-thread-from-message.
            let thread_id = make_thread_id(room.room_id(), &ev.event_id.to_owned());
            let rt = runner::get_or_create_runtime(state, &thread_id, directory).await?;
            runner::enqueue_incoming(
                state.clone(),
                chat.clone(),
                rt,
                QueuedMessage {
                    prompt: parsed.prompt,
                    username,
                    source_message_id: Some(ev.event_id.to_string()),
                    show_marker: false,
                },
                false,
            )
            .await;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_thread_message(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    room_id: &str,
    thread_id: &str,
    body: &str,
    sender: &str,
    username: &str,
    event_id: &str,
) -> Result<()> {
    if let Some(rest) = body.strip_prefix('!') {
        let reply =
            handle_command(state, chat, room_id, Some(thread_id), rest, sender, username).await?;
        chat.send_message(thread_id, &reply).await?;
        return Ok(());
    }
    let directory = task_runner::resolve_thread_directory(state, chat.as_ref(), thread_id).await?;
    let parsed = delivery::parse_message(body);
    let rt = runner::get_or_create_runtime(state, thread_id, directory.clone()).await?;

    match parsed.delivery {
        Delivery::Btw => {
            commands::fork_btw(
                state,
                chat,
                thread_id,
                room_id,
                &directory,
                &parsed.prompt,
                sender,
                username,
            )
            .await?;
        }
        Delivery::Queue => {
            let result = runner::enqueue_incoming(
                state.clone(),
                chat.clone(),
                rt,
                QueuedMessage {
                    prompt: parsed.prompt,
                    username: username.to_string(),
                    source_message_id: Some(event_id.to_string()),
                    show_marker: false,
                },
                true,
            )
            .await;
            if let EnqueueResult::Queued(pos) = result {
                chat.send_message(
                    thread_id,
                    &format!("Queued at position {pos}. Edit your message to update it in the queue."),
                )
                .await?;
            }
        }
        Delivery::Normal => {
            runner::enqueue_incoming(
                state.clone(),
                chat.clone(),
                rt,
                QueuedMessage {
                    prompt: parsed.prompt,
                    username: username.to_string(),
                    source_message_id: Some(event_id.to_string()),
                    show_marker: false,
                },
                false,
            )
            .await;
        }
    }
    Ok(())
}

/// Text commands, the Matrix counterpart of Discord slash commands.
/// `thread_id` is set when the command was sent inside a thread.
async fn handle_command(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    room_id: &str,
    thread_id: Option<&str>,
    input: &str,
    sender: &str,
    username: &str,
) -> Result<String> {
    let (cmd, args) = match input.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (input.trim(), ""),
    };
    let need_thread = || thread_id.ok_or_else(|| anyhow!("!{cmd} only works inside a session thread"));

    match cmd {
        "add-project" => {
            if args.is_empty() {
                return Err(anyhow!("usage: !add-project /absolute/path"));
            }
            let reply = commands::add_project(state, room_id, args)?;
            tracing::info!("add-project by {sender}: {args} → {reply}");
            Ok(reply)
        }
        "queue" => {
            if args.is_empty() {
                return Err(anyhow!("usage: !queue <message>"));
            }
            let thread_id = need_thread()?;
            let directory =
                task_runner::resolve_thread_directory(state, chat.as_ref(), thread_id).await?;
            let rt = runner::get_or_create_runtime(state, thread_id, directory).await?;
            let result = runner::enqueue_incoming(
                state.clone(),
                chat.clone(),
                rt,
                QueuedMessage {
                    prompt: args.to_string(),
                    username: username.to_string(),
                    source_message_id: None,
                    show_marker: false,
                },
                true,
            )
            .await;
            Ok(match result {
                EnqueueResult::Queued(pos) => format!("Queued message (position {pos})"),
                EnqueueResult::Dispatched => "Session was idle; message sent immediately.".to_string(),
            })
        }
        "clear" => {
            let thread_id = need_thread()?;
            let position = if args.is_empty() { None } else { Some(args.parse::<usize>()?) };
            let directory =
                task_runner::resolve_thread_directory(state, chat.as_ref(), thread_id).await?;
            let rt = runner::get_or_create_runtime(state, thread_id, directory).await?;
            let removed = runner::clear_queue(&rt, position).await?;
            Ok(match position {
                Some(p) => format!("Cleared queued message at position {p}"),
                None => format!("Cleared {removed} queued message(s)"),
            })
        }
        "btw" => {
            if args.is_empty() {
                return Err(anyhow!("usage: !btw <prompt>"));
            }
            let thread_id = need_thread()?;
            let directory =
                task_runner::resolve_thread_directory(state, chat.as_ref(), thread_id).await?;
            commands::fork_btw(state, chat, thread_id, room_id, &directory, args, sender, username)
                .await?;
            Ok("Session forked! Continue in the new btw thread.".to_string())
        }
        "worktree" => {
            let mut parts = args.split_whitespace();
            let name = parts.next().map(str::to_string);
            let base = parts.next().map(str::to_string);
            let scope = match thread_id {
                Some(t) => commands::WorktreeScope::Thread {
                    thread_id: t.to_string(),
                    name_hint: name.clone().unwrap_or_else(|| "worktree".to_string()),
                },
                None => commands::WorktreeScope::Channel {
                    channel_id: room_id.to_string(),
                    user_id: sender.to_string(),
                },
            };
            commands::new_worktree(state, chat, scope, name, base).await
        }
        "list-worktrees" => commands::worktrees_text(state, room_id).await,
        "tasks" => commands::tasks_text(state),
        "delete-task" => {
            let id: i64 = args.parse().context("usage: !delete-task <id>")?;
            commands::delete_task_text(state, id)
        }
        "help" => Ok("Commands: !add-project <dir>, !queue <msg>, !clear [n], !btw <prompt>, \
                      !worktree [name] [base], !list-worktrees, !tasks, !delete-task <id>. \
                      Suffixes: end a message with `. queue` or `. btw`."
            .to_string()),
        other => Err(anyhow!("unknown command !{other} (try !help)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_round_trip() {
        let room = RoomId::parse("!abc:example.org").unwrap();
        let event: OwnedEventId = "$deadbeef".try_into().unwrap();
        let id = make_thread_id(&room, &event);
        assert_eq!(id, "!abc:example.org|$deadbeef");
        let (r, e) = split_thread_id(&id).unwrap();
        assert_eq!(r, room);
        assert_eq!(e, Some(event));
    }

    #[test]
    fn bare_room_id_has_no_root() {
        let (r, e) = split_thread_id("!abc:example.org").unwrap();
        assert_eq!(r.as_str(), "!abc:example.org");
        assert_eq!(e, None);
    }
}
