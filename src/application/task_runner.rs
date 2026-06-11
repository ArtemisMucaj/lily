//! Scheduled-task runner: a polling loop that claims due tasks, posts them
//! into the chat platform, starts agent sessions, and reschedules cron tasks.

use crate::application::chat::ChatConnector;
use crate::application::session_runtime::{self, AppState};
use crate::domain::rendering;
use crate::domain::session::QueuedMessage;
use crate::domain::task::{next_cron_run, ScheduledTask, TaskPayload};
use anyhow::{anyhow, Context as _, Result};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const STALE_RUNNING: chrono::Duration = chrono::Duration::minutes(2);
pub const DUE_BATCH_SIZE: usize = 20;

/// The polling loop. Spawned once by `lily run`.
pub async fn run_task_loop(state: Arc<AppState>, chat: Arc<dyn ChatConnector>) {
    loop {
        if let Err(err) = tick(&state, &chat).await {
            tracing::warn!("task scheduler tick failed: {err:#}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn tick(state: &Arc<AppState>, chat: &Arc<dyn ChatConnector>) -> Result<()> {
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
        // Cancellation of a claimed task is cooperative: re-check right before
        // the side effects so a `task delete` landing after the claim usually
        // wins. (A cancel arriving mid-execution stays best-effort.)
        if state.db.get_task_status(task.id)?.as_deref() != Some("running") {
            tracing::info!("task {} was cancelled after claim; skipping", task.id);
            continue;
        }
        let outcome = execute_task(state, chat, &task).await;
        finalize_task(state, &task, outcome)?;
    }
    Ok(())
}

async fn execute_task(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    task: &ScheduledTask,
) -> Result<()> {
    let payload: TaskPayload =
        serde_json::from_str(&task.payload_json).context("bad task payload json")?;
    match payload {
        TaskPayload::Thread { thread_id, prompt, .. } => {
            let directory = resolve_thread_directory(state, chat.as_ref(), &thread_id).await?;
            chat.post_message(
                &thread_id,
                &format!("{}**lily-cli:**\n{}", rendering::QUEUE_PREFIX, prompt),
            )
            .await?;
            let rt = session_runtime::get_or_create_runtime(state, &thread_id, directory).await?;
            // Scheduled sends into a live thread wait politely (queue
            // semantics) rather than interrupting whatever is running.
            session_runtime::enqueue_incoming(
                state.clone(),
                chat.clone(),
                rt,
                QueuedMessage {
                    prompt,
                    username: "lily-cli".to_string(),
                    source_message_id: None,
                    show_marker: false,
                },
                true,
            )
            .await;
        }
        TaskPayload::Channel { channel_id, prompt, name, notify_only, user_id } => {
            let directory = state
                .db
                .get_channel_directory(&channel_id)?
                .ok_or_else(|| anyhow!("channel {channel_id} is not linked to a project"))?;
            let starter = chat
                .post_message(&channel_id, &prompt)
                .await
                .context("failed to post task starter message")?;
            let thread_name = name.unwrap_or_else(|| rendering::prompt_preview(&prompt, 100));
            let thread_id = chat
                .create_thread(&channel_id, Some(&starter), &thread_name)
                .await
                .context("failed to create task thread")?;
            if let Some(uid) = user_id {
                let _ = chat.add_thread_member(&thread_id, &uid).await;
            }
            if !notify_only {
                let rt =
                    session_runtime::get_or_create_runtime(state, &thread_id, directory).await?;
                session_runtime::enqueue_incoming(
                    state.clone(),
                    chat.clone(),
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
    chat: &dyn ChatConnector,
    thread_id: &str,
) -> Result<String> {
    if let Some(wt) = state.db.get_thread_worktree(thread_id)? {
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
    let parent = chat
        .thread_parent(thread_id)
        .await
        .context("failed to resolve thread parent")?
        .ok_or_else(|| anyhow!("thread {thread_id} has no parent channel"))?;
    state
        .db
        .get_channel_directory(&parent)?
        .ok_or_else(|| anyhow!("channel {parent} is not linked to a project (use /add-project)"))
}
