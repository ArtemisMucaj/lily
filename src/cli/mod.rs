//! CLI layer: argument parsing and the entry points for each command.

use crate::application::config::Config;
use crate::application::{session_runtime, task_runner};
use crate::connector::sqlite::Db;
use crate::application::chat::ChatConnector;
use crate::connector::{discord, matrix, opencode, router};
use crate::domain::rendering;
use crate::domain::task::{
    build_task, describe_task, parse_send_at, ParsedSendAt, TaskPayload, PROMPT_MAX_LEN,
};
use anyhow::{anyhow, Context as _, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "lily", version, about = "Drive coding agents from Discord: channels are projects, threads are sessions")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the Discord bot (requires DISCORD_TOKEN and a running `opencode serve`).
    Run,
    /// Manage project ↔ channel links.
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Send a prompt to a project channel or thread, now or on a schedule.
    Send {
        /// Project channel id to start a new thread in.
        #[arg(long, conflicts_with = "thread")]
        channel: Option<String>,
        /// Existing thread id to continue.
        #[arg(long)]
        thread: Option<String>,
        /// The prompt text.
        #[arg(long)]
        prompt: String,
        /// UTC ISO timestamp ending in Z (one-time) or a cron expression
        /// (recurring, UTC). Omit to send as soon as the bot picks it up.
        #[arg(long = "send-at")]
        send_at: Option<String>,
        /// Post the message without starting an AI session.
        #[arg(long = "notify-only", default_value_t = false)]
        notify_only: bool,
        /// Thread name (channel sends only).
        #[arg(long)]
        name: Option<String>,
        /// Discord user id to add to the created thread.
        #[arg(long)]
        user: Option<String>,
    },
    /// Manage scheduled tasks.
    Task {
        #[command(subcommand)]
        command: TaskCommands,
    },
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// Link a Discord channel to a project directory.
    Add {
        /// Project directory (defaults to the current directory).
        directory: Option<String>,
        /// Discord channel id to link.
        #[arg(long)]
        channel: String,
    },
    /// List linked projects.
    List,
}

#[derive(Subcommand)]
enum TaskCommands {
    /// List scheduled tasks.
    List {
        /// Include completed, cancelled and failed tasks.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Cancel a task.
    Delete { id: i64 },
    /// Edit a still-planned task.
    Edit {
        id: i64,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long = "send-at")]
        send_at: Option<String>,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let db = Arc::new(Db::open(&config.db_path())?);

    match cli.command {
        Commands::Run => run_bot(config, db).await,
        Commands::Project { command } => run_project(command, db),
        Commands::Send { channel, thread, prompt, send_at, notify_only, name, user } => {
            run_send(db, channel, thread, prompt, send_at, notify_only, name, user)
        }
        Commands::Task { command } => run_task(command, db),
    }
}

async fn run_bot(config: Config, db: Arc<Db>) -> Result<()> {
    let discord_enabled = config.discord_token.is_some();
    let matrix_enabled = config.matrix_homeserver.is_some();
    if !discord_enabled && !matrix_enabled {
        return Err(anyhow!(
            "no connector configured: set DISCORD_TOKEN and/or MATRIX_HOMESERVER (+ MATRIX_USER, MATRIX_PASSWORD)"
        ));
    }
    let oc = opencode::OpencodeClient::new(&config.opencode_url);
    tracing::info!("using OpenCode server at {}", config.opencode_url);
    let state = Arc::new(session_runtime::AppState::new(db, oc.clone(), config));

    // Single global SSE listener feeding all per-thread renderers.
    {
        let oc = oc.clone();
        tokio::spawn(async move { oc.run_event_listener("/").await });
    }

    let mut discord_client = if discord_enabled {
        let token = state.config.discord_token.clone().unwrap();
        let intents = serenity::all::GatewayIntents::GUILDS
            | serenity::all::GatewayIntents::GUILD_MESSAGES
            | serenity::all::GatewayIntents::MESSAGE_CONTENT;
        Some(
            serenity::Client::builder(&token, intents)
                .event_handler(discord::Handler { state: state.clone() })
                .await
                .context("failed to build Discord client")?,
        )
    } else {
        None
    };
    let matrix_client = if matrix_enabled {
        Some(matrix::build_client(&state).await?)
    } else {
        None
    };

    // Scheduled tasks address either platform; route by id shape.
    let chat: Arc<dyn ChatConnector> = Arc::new(router::RoutedChat {
        discord: discord_client
            .as_ref()
            .map(|c| Arc::new(discord::DiscordChat { http: c.http.clone() }) as Arc<dyn ChatConnector>),
        matrix: matrix_client
            .clone()
            .map(|c| Arc::new(matrix::MatrixChat { client: c }) as Arc<dyn ChatConnector>),
    });
    tokio::spawn(task_runner::run_task_loop(state.clone(), chat));

    match (discord_client.as_mut(), matrix_client) {
        (Some(discord), Some(matrix)) => {
            tokio::select! {
                r = discord.start() => r.context("Discord client error")?,
                r = matrix::run(state, matrix) => r?,
            }
        }
        (Some(discord), None) => discord.start().await.context("Discord client error")?,
        (None, Some(matrix)) => matrix::run(state, matrix).await?,
        (None, None) => unreachable!(),
    }
    Ok(())
}

fn run_project(command: ProjectCommands, db: Arc<Db>) -> Result<()> {
    match command {
        ProjectCommands::Add { directory, channel } => {
            let dir = match directory {
                Some(d) => std::fs::canonicalize(d)?,
                None => std::env::current_dir()?,
            };
            if !dir.is_dir() {
                return Err(anyhow!("{} is not a directory", dir.display()));
            }
            db.set_channel_directory(&channel, &dir.to_string_lossy())?;
            println!("Linked channel {channel} to {}", dir.display());
            Ok(())
        }
        ProjectCommands::List => {
            for (channel, dir) in db.list_channel_directories()? {
                println!("{channel}\t{dir}");
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_send(
    db: Arc<Db>,
    channel: Option<String>,
    thread: Option<String>,
    prompt: String,
    send_at: Option<String>,
    notify_only: bool,
    name: Option<String>,
    user: Option<String>,
) -> Result<()> {
    let payload = match (&channel, &thread) {
        (Some(c), None) => {
            // Fail fast: the task runner would hard-fail on this later.
            if db.get_channel_directory(c)?.is_none() {
                return Err(anyhow!("channel {c} is not linked to a project"));
            }
            TaskPayload::Channel {
                channel_id: c.clone(),
                prompt: prompt.clone(),
                name,
                notify_only,
                user_id: user,
            }
        }
        (None, Some(t)) => {
            if notify_only {
                return Err(anyhow!("--notify-only only applies to channel sends"));
            }
            // Thread sends post into an existing thread: there is no thread to
            // name and no member to add, so reject flags that would be ignored.
            if name.is_some() {
                return Err(anyhow!("--name only applies to channel sends"));
            }
            if user.is_some() {
                return Err(anyhow!("--user only applies to channel sends"));
            }
            TaskPayload::Thread { thread_id: t.clone(), prompt: prompt.clone(), user_id: None }
        }
        _ => return Err(anyhow!("pass exactly one of --channel or --thread")),
    };
    let now = Utc::now();
    let parsed = match send_at {
        Some(v) => parse_send_at(&v, now)?,
        // No --send-at: schedule for now; the running bot picks it up within
        // one scheduler poll interval.
        None => ParsedSendAt::immediately(now),
    };
    let task = build_task(&payload, &parsed)?;
    let id = db.create_scheduled_task(&task)?;
    match parsed.schedule_kind {
        "cron" => println!(
            "Scheduled recurring task #{id} ({}), next run {}",
            task.cron_expr.as_deref().unwrap_or("?"),
            task.next_run_at.format("%Y-%m-%d %H:%M:%S UTC")
        ),
        _ => println!(
            "Scheduled task #{id} for {}",
            task.next_run_at.format("%Y-%m-%d %H:%M:%S UTC")
        ),
    }
    Ok(())
}

fn run_task(command: TaskCommands, db: Arc<Db>) -> Result<()> {
    match command {
        TaskCommands::List { all } => {
            let tasks = db.list_tasks(all)?;
            if tasks.is_empty() {
                println!("No scheduled tasks.");
            }
            for t in tasks {
                println!("{}", describe_task(&t));
            }
            Ok(())
        }
        TaskCommands::Delete { id } => {
            if db.cancel_task(id)? {
                println!("Cancelled task #{id}");
            } else {
                return Err(anyhow!("task #{id} is not planned or running"));
            }
            Ok(())
        }
        TaskCommands::Edit { id, prompt, send_at } => {
            if prompt.is_none() && send_at.is_none() {
                return Err(anyhow!("pass --prompt and/or --send-at"));
            }
            let task = db
                .list_tasks(true)?
                .into_iter()
                .find(|t| t.id == id)
                .ok_or_else(|| anyhow!("task #{id} not found"))?;

            // Validate and build both mutations fully before touching the
            // database, then apply them in one atomic update so a failed
            // --send-at can't leave a half-edited task behind.
            let mut payload: TaskPayload = serde_json::from_str(&task.payload_json)?;
            let mut preview = task.prompt_preview.clone();
            if let Some(p) = &prompt {
                if p.len() > PROMPT_MAX_LEN {
                    return Err(anyhow!("prompt exceeds {PROMPT_MAX_LEN} characters"));
                }
                match &mut payload {
                    TaskPayload::Thread { prompt: old, .. } => *old = p.clone(),
                    TaskPayload::Channel { prompt: old, .. } => *old = p.clone(),
                }
                preview = rendering::prompt_preview(p, 120);
            }
            let payload_json = serde_json::to_string(&payload)?;

            let parsed = send_at.as_deref().map(|v| parse_send_at(v, Utc::now())).transpose()?;
            let schedule = match &parsed {
                Some(p) => (
                    p.schedule_kind,
                    p.run_at,
                    p.cron_expr.as_deref(),
                    p.timezone.as_deref(),
                    p.next_run_at,
                ),
                None => (
                    task.schedule_kind.as_str(),
                    task.run_at,
                    task.cron_expr.as_deref(),
                    task.timezone.as_deref(),
                    task.next_run_at,
                ),
            };
            if !db.update_task(id, &payload_json, &preview, schedule)? {
                return Err(anyhow!("task #{id} is no longer planned"));
            }
            println!("Updated task #{id}");
            Ok(())
        }
    }
}
