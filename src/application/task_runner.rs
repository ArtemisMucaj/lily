//! Scheduled-task runner: a polling loop that claims due tasks, posts them
//! into Discord, starts agent sessions, and reschedules cron tasks.

use crate::application::session_runtime::{self, AppState};
use crate::domain::rendering;
use crate::domain::session::QueuedMessage;
use crate::domain::task::{next_cron_run, ScheduledTask, TaskPayload};
use anyhow::{anyhow, Context as _, Result};
use chrono::Utc;
use serenity::all::{AutoArchiveDuration, ChannelId, CreateMessage, CreateThread, Http, UserId};
use std::sync::Arc;
use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const STALE_RUNNING: chrono::Duration = chrono::Duration::minutes(2);
pub const DUE_BATCH_SIZE: usize = 20;

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
                    CreateMessage::new()
                        .content(format!("{}**lily-cli:**\n{}", rendering::QUEUE_PREFIX, prompt)),
                )
                .await?;
            let rt = session_runtime::get_or_create_runtime(state, thread, directory).await;
            session_runtime::enqueue_incoming(
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
            let thread_name = name.unwrap_or_else(|| rendering::prompt_preview(&prompt, 100));
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
                let rt = session_runtime::get_or_create_runtime(state, thread.id, directory).await;
                session_runtime::enqueue_incoming(
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
    if let Some(wt) = state.db.get_thread_worktree(&thread_id.to_string())? {
        // Never fall back to the project directory while a worktree is
        // assigned: running there would break the isolation the worktree
        // exists to provide.
        match wt.status.as_str() {
            "ready" => {
                if let Some(dir) = wt.worktree_directory {
                    return Ok(dir);
                }
                return Err(anyhow!("worktree is marked ready but has no directory"));
            }
            "pending" => {
                return Err(anyhow!(
                    "the worktree for this thread is still being created; try again in a moment"
                ));
            }
            other => {
                let detail = wt.error_message.map(|e| format!(": {e}")).unwrap_or_default();
                return Err(anyhow!("worktree is in state '{other}'{detail}"));
            }
        }
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
