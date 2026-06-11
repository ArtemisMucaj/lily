//! Scheduled tasks: run a prompt once at a future UTC time or repeatedly on a
//! cron expression. Tasks are created by `lily send --send-at` (or without
//! `--send-at` for an immediate one-shot) and executed by a polling loop in
//! the running bot.

use crate::db::{NewScheduledTask, ScheduledTask};
use crate::format;
use crate::runner::{self, AppState, QueuedMessage};
use anyhow::{anyhow, Context as _, Result};
use chrono::{DateTime, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};
use serenity::all::{
    AutoArchiveDuration, ChannelId, CreateMessage, CreateThread, Http, UserId,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const STALE_RUNNING: chrono::Duration = chrono::Duration::minutes(2);
pub const DUE_BATCH_SIZE: usize = 20;
pub const PROMPT_MAX_LEN: usize = 1900;

/// What to do when a task fires, stored as JSON in `scheduled_tasks.payload_json`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskPayload {
    /// Post into an existing thread and continue its session.
    Thread { thread_id: String, prompt: String, user_id: Option<String> },
    /// Post a starter message in a project channel, create a thread, and
    /// (unless `notify_only`) start a session with the prompt.
    Channel {
        channel_id: String,
        prompt: String,
        /// Thread name; defaults to a preview of the prompt.
        name: Option<String>,
        notify_only: bool,
        user_id: Option<String>,
    },
}

#[derive(Debug)]
pub struct ParsedSendAt {
    pub schedule_kind: &'static str,
    pub run_at: Option<DateTime<Utc>>,
    pub cron_expr: Option<String>,
    pub timezone: Option<String>,
    pub next_run_at: DateTime<Utc>,
}

/// Parse a `--send-at` value: a UTC ISO timestamp ending in `Z` (one-time) or
/// a cron expression (recurring, evaluated in UTC).
pub fn parse_send_at(value: &str, now: DateTime<Utc>) -> Result<ParsedSendAt> {
    let value = value.trim();
    let looks_like_cron = value.starts_with('@') || value.split_whitespace().count() >= 5;
    if looks_like_cron {
        let next = next_cron_run(value, now)?;
        return Ok(ParsedSendAt {
            schedule_kind: "cron",
            run_at: None,
            cron_expr: Some(value.to_string()),
            timezone: Some("UTC".to_string()),
            next_run_at: next,
        });
    }
    if !value.ends_with('Z') {
        return Err(anyhow!(
            "--send-at must be a UTC ISO timestamp ending in Z (e.g. 2026-03-01T09:00:00Z) or a cron expression"
        ));
    }
    let at = DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid timestamp: {value}"))?
        .with_timezone(&Utc);
    if at <= now {
        return Err(anyhow!("--send-at must be in the future ({value} is not)"));
    }
    Ok(ParsedSendAt {
        schedule_kind: "at",
        run_at: Some(at),
        cron_expr: None,
        timezone: None,
        next_run_at: at,
    })
}

pub fn next_cron_run(expr: &str, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let cron = Cron::from_str(expr).map_err(|e| anyhow!("invalid cron expression {expr:?}: {e}"))?;
    let next = cron
        .find_next_occurrence(&from, false)
        .map_err(|e| anyhow!("no next occurrence for {expr:?}: {e}"))?;
    Ok(next)
}

pub fn build_task(payload: &TaskPayload, send_at: &ParsedSendAt) -> Result<NewScheduledTask> {
    let (prompt, channel_id, thread_id) = match payload {
        TaskPayload::Thread { thread_id, prompt, .. } => (prompt, None, Some(thread_id.clone())),
        TaskPayload::Channel { channel_id, prompt, .. } => (prompt, Some(channel_id.clone()), None),
    };
    if prompt.len() > PROMPT_MAX_LEN {
        return Err(anyhow!("prompt exceeds {PROMPT_MAX_LEN} characters"));
    }
    Ok(NewScheduledTask {
        schedule_kind: send_at.schedule_kind.to_string(),
        run_at: send_at.run_at,
        cron_expr: send_at.cron_expr.clone(),
        timezone: send_at.timezone.clone(),
        next_run_at: send_at.next_run_at,
        payload_json: serde_json::to_string(payload)?,
        prompt_preview: format::prompt_preview(prompt, 120),
        channel_id,
        thread_id,
    })
}

/// The polling loop. Spawned once by `lily run`.
pub async fn run_task_loop(state: Arc<AppState>, http: Arc<Http>) {
    loop {
        if let Err(err) = tick(&state, &http).await {
            tracing::warn!("task scheduler tick failed: {err:#}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn tick(state: &Arc<AppState>, http: &Arc<Http>) -> Result<()> {
    let now = Utc::now();
    let recovered = state.db.recover_stale_running_tasks(now - STALE_RUNNING)?;
    if recovered > 0 {
        tracing::info!("recovered {recovered} stale running task(s)");
    }
    let due = state.db.get_due_planned_tasks(now, DUE_BATCH_SIZE)?;
    for task in due {
        if !state.db.claim_task_running(task.id, Utc::now())? {
            continue;
        }
        let outcome = execute_task(state, http, &task).await;
        finalize_task(state, &task, outcome)?;
    }
    Ok(())
}

async fn execute_task(state: &Arc<AppState>, http: &Arc<Http>, task: &ScheduledTask) -> Result<()> {
    let payload: TaskPayload =
        serde_json::from_str(&task.payload_json).context("bad task payload json")?;
    match payload {
        TaskPayload::Thread { thread_id, prompt, .. } => {
            let thread: ChannelId = thread_id.parse().context("bad thread id in task")?;
            let directory = resolve_thread_directory(state, http, thread).await?;
            thread
                .send_message(
                    http,
                    CreateMessage::new().content(format!("{}**lily-cli:**\n{}", format::QUEUE_PREFIX, prompt)),
                )
                .await?;
            let rt = runner::get_or_create_runtime(state, thread, directory).await;
            runner::enqueue_incoming(
                state.clone(),
                http.clone(),
                rt,
                QueuedMessage {
                    prompt,
                    username: "lily-cli".to_string(),
                    source_message_id: None,
                    show_marker: false,
                },
                false,
            )
            .await;
        }
        TaskPayload::Channel { channel_id, prompt, name, notify_only, user_id } => {
            let channel: ChannelId = channel_id.parse().context("bad channel id in task")?;
            let directory = state
                .db
                .get_channel_directory(&channel_id)?
                .ok_or_else(|| anyhow!("channel {channel_id} is not linked to a project"))?;
            let starter = channel
                .send_message(http, CreateMessage::new().content(prompt.clone()))
                .await
                .context("failed to post task starter message")?;
            let thread_name = name.unwrap_or_else(|| format::prompt_preview(&prompt, 100));
            let thread = channel
                .create_thread_from_message(
                    http,
                    starter.id,
                    CreateThread::new(truncate(&thread_name, 100))
                        .auto_archive_duration(AutoArchiveDuration::OneDay),
                )
                .await
                .context("failed to create task thread")?;
            if let Some(uid) = user_id.and_then(|u| u.parse::<u64>().ok()) {
                let _ = thread.id.add_thread_member(http, UserId::new(uid)).await;
            }
            if !notify_only {
                let rt = runner::get_or_create_runtime(state, thread.id, directory).await;
                runner::enqueue_incoming(
                    state.clone(),
                    http.clone(),
                    rt,
                    QueuedMessage {
                        prompt,
                        username: "lily-cli".to_string(),
                        source_message_id: None,
                        show_marker: false,
                    },
                    false,
                )
                .await;
            }
        }
    }
    Ok(())
}

fn finalize_task(state: &Arc<AppState>, task: &ScheduledTask, outcome: Result<()>) -> Result<()> {
    let now = Utc::now();
    match (&outcome, task.schedule_kind.as_str()) {
        (Ok(()), "cron") => {
            let expr = task.cron_expr.as_deref().unwrap_or_default();
            match next_cron_run(expr, now) {
                Ok(next) => state.db.mark_task_cron_rescheduled(task.id, now, next, None)?,
                Err(err) => state.db.mark_task_failed(task.id, now, &format!("{err:#}"))?,
            }
        }
        (Ok(()), _) => state.db.mark_task_completed(task.id, now)?,
        (Err(err), "cron") => {
            tracing::warn!("scheduled task {} failed: {err:#}", task.id);
            let expr = task.cron_expr.as_deref().unwrap_or_default();
            match next_cron_run(expr, now) {
                Ok(next) => {
                    state.db.mark_task_cron_rescheduled(task.id, now, next, Some(&format!("{err:#}")))?
                }
                Err(_) => state.db.mark_task_failed(task.id, now, &format!("{err:#}"))?,
            }
        }
        (Err(err), _) => {
            tracing::warn!("scheduled task {} failed: {err:#}", task.id);
            state.db.mark_task_failed(task.id, now, &format!("{err:#}"))?;
        }
    }
    Ok(())
}

/// Resolve the working directory for a thread: its worktree when one is
/// ready, otherwise the parent channel's project directory.
pub async fn resolve_thread_directory(
    state: &Arc<AppState>,
    http: &Arc<Http>,
    thread_id: ChannelId,
) -> Result<String> {
    if let Some(wt) = state.db.get_thread_worktree(&thread_id.to_string())?
        && wt.status == "ready"
            && let Some(dir) = wt.worktree_directory {
                return Ok(dir);
            }
    // The thread's session directory comes from the parent channel mapping.
    let channel = thread_id.to_channel(http).await.context("failed to fetch thread channel")?;
    let guild_channel = channel.guild().ok_or_else(|| anyhow!("not a guild thread"))?;
    let parent = guild_channel
        .parent_id
        .ok_or_else(|| anyhow!("thread {thread_id} has no parent channel"))?;
    state
        .db
        .get_channel_directory(&parent.to_string())?
        .ok_or_else(|| anyhow!("channel {parent} is not linked to a project (use /add-project)"))
}

fn truncate(s: &str, max: usize) -> String {
    let mut cut = s.len().min(max);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// Human-readable task list used by both `/tasks` and `lily task list`.
pub fn describe_task(t: &ScheduledTask) -> String {
    let when = match t.schedule_kind.as_str() {
        "cron" => format!(
            "cron `{}` {} (next {})",
            t.cron_expr.as_deref().unwrap_or("?"),
            t.timezone.as_deref().unwrap_or("UTC"),
            t.next_run_at.format("%Y-%m-%d %H:%M UTC")
        ),
        _ => format!("at {}", t.next_run_at.format("%Y-%m-%d %H:%M UTC")),
    };
    let target = match (&t.thread_id, &t.channel_id) {
        (Some(th), _) => format!(" → thread {th}"),
        (None, Some(ch)) => format!(" → channel {ch}"),
        _ => String::new(),
    };
    let error = match (&t.last_error, t.attempts) {
        (Some(e), n) if n > 0 => format!(" (attempts: {n}, last error: {e})"),
        _ => String::new(),
    };
    let last_run = t
        .last_run_at
        .map(|d| format!(" (last run {})", d.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default();
    format!("#{} [{}] {}{}{} — {}{}", t.id, t.status, when, last_run, target, t.prompt_preview, error)
}
