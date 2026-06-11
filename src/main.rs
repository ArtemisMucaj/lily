//! lily — a collaborative agent orchestrator inside Discord.
//!
//! Channels are projects, threads are coding sessions. lily bridges Discord
//! to a local OpenCode server: send a message in a project channel and an AI
//! agent edits code on this machine, replying in a thread.

mod config;
mod db;
mod discord;
mod format;
mod opencode;
mod runner;
mod scheduler;
mod suffix;
mod worktree;

use anyhow::{anyhow, Context as _, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "lily", version, about = "Drive coding agents from Discord: channels are projects, threads are sessions")]
struct Cli {
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,serenity=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = config::Config::from_env()?;
    let db = Arc::new(db::Db::open(&config.db_path())?);

    match cli.command {
        Commands::Run => run_bot(config, db).await,
        Commands::Project { command } => run_project(command, db),
        Commands::Send { channel, thread, prompt, send_at, notify_only, name, user } => {
            run_send(db, channel, thread, prompt, send_at, notify_only, name, user)
        }
        Commands::Task { command } => run_task(command, db),
    }
}

async fn run_bot(config: config::Config, db: Arc<db::Db>) -> Result<()> {
    let token = config
        .discord_token
        .clone()
        .ok_or_else(|| anyhow!("DISCORD_TOKEN is not set"))?;
    let oc = opencode::OpencodeClient::new(&config.opencode_url);
    tracing::info!("using OpenCode server at {}", config.opencode_url);
    let state = Arc::new(runner::AppState::new(db, oc.clone(), config));

    // Single global SSE listener feeding all per-thread renderers.
    {
        let oc = oc.clone();
        tokio::spawn(async move { oc.run_event_listener("/").await });
    }

    let intents = serenity::all::GatewayIntents::GUILDS
        | serenity::all::GatewayIntents::GUILD_MESSAGES
        | serenity::all::GatewayIntents::MESSAGE_CONTENT;
    let mut client = serenity::Client::builder(&token, intents)
        .event_handler(discord::Handler { state: state.clone() })
        .await
        .context("failed to build Discord client")?;

    // Scheduled-task loop shares the same HTTP client as the gateway.
    tokio::spawn(scheduler::run_task_loop(state, client.http.clone()));

    client.start().await.context("Discord client error")?;
    Ok(())
}

fn run_project(command: ProjectCommands, db: Arc<db::Db>) -> Result<()> {
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
    db: Arc<db::Db>,
    channel: Option<String>,
    thread: Option<String>,
    prompt: String,
    send_at: Option<String>,
    notify_only: bool,
    name: Option<String>,
    user: Option<String>,
) -> Result<()> {
    let payload = match (&channel, &thread) {
        (Some(c), None) => scheduler::TaskPayload::Channel {
            channel_id: c.clone(),
            prompt: prompt.clone(),
            name,
            notify_only,
            user_id: user,
        },
        (None, Some(t)) => {
            if notify_only {
                return Err(anyhow!("--notify-only only applies to channel sends"));
            }
            scheduler::TaskPayload::Thread { thread_id: t.clone(), prompt: prompt.clone(), user_id: user }
        }
        _ => return Err(anyhow!("pass exactly one of --channel or --thread")),
    };
    let now = Utc::now();
    let parsed = match send_at {
        Some(v) => scheduler::parse_send_at(&v, now)?,
        // No --send-at: schedule for now; the running bot picks it up within
        // one scheduler poll interval.
        None => scheduler::ParsedSendAt {
            schedule_kind: "at",
            run_at: Some(now),
            cron_expr: None,
            timezone: None,
            next_run_at: now,
        },
    };
    let task = scheduler::build_task(&payload, &parsed)?;
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

fn run_task(command: TaskCommands, db: Arc<db::Db>) -> Result<()> {
    match command {
        TaskCommands::List { all } => {
            let tasks = db.list_tasks(all)?;
            if tasks.is_empty() {
                println!("No scheduled tasks.");
            }
            for t in tasks {
                println!("{}", scheduler::describe_task(&t));
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
            if let Some(p) = &prompt {
                if p.len() > scheduler::PROMPT_MAX_LEN {
                    return Err(anyhow!("prompt exceeds {} characters", scheduler::PROMPT_MAX_LEN));
                }
                // Rewrite the payload with the new prompt, keeping its shape.
                let task = db
                    .list_tasks(true)?
                    .into_iter()
                    .find(|t| t.id == id)
                    .ok_or_else(|| anyhow!("task #{id} not found"))?;
                let mut payload: scheduler::TaskPayload = serde_json::from_str(&task.payload_json)?;
                match &mut payload {
                    scheduler::TaskPayload::Thread { prompt: old, .. } => *old = p.clone(),
                    scheduler::TaskPayload::Channel { prompt: old, .. } => *old = p.clone(),
                }
                let payload_json = serde_json::to_string(&payload)?;
                let preview = format::prompt_preview(p, 120);
                if !db.update_task(id, Some(&payload_json), Some(&preview), None)? {
                    return Err(anyhow!("task #{id} is no longer planned"));
                }
            }
            if let Some(v) = &send_at {
                let parsed = scheduler::parse_send_at(v, Utc::now())?;
                let updated = db.update_task(
                    id,
                    None,
                    None,
                    Some((
                        parsed.schedule_kind,
                        parsed.run_at,
                        parsed.cron_expr.as_deref(),
                        parsed.timezone.as_deref(),
                        parsed.next_run_at,
                    )),
                )?;
                if !updated {
                    return Err(anyhow!("task #{id} is no longer planned"));
                }
            }
            println!("Updated task #{id}");
            Ok(())
        }
    }
}
