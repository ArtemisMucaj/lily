//! Platform-neutral command flows shared by every chat connector: project
//! linking, btw forks, the worktree lifecycle, and task management. Each
//! connector only parses its native command format (slash commands, `!text`
//! commands) and delegates here.

use crate::application::chat::ChatConnector;
use crate::application::session_runtime::{self as runner, AppState};
use crate::application::task_runner;
use crate::connector::git;
use crate::domain::rendering;
use crate::domain::session::QueuedMessage;
use crate::domain::task::describe_task;
use crate::domain::worktree;
use anyhow::{anyhow, Context as _, Result};
use std::sync::Arc;

/// Link a channel to a project directory on the host.
pub fn add_project(state: &AppState, channel_id: &str, directory: &str) -> Result<String> {
    let path = std::path::Path::new(directory);
    if !path.is_absolute() || !path.is_dir() {
        return Err(anyhow!(
            "`{directory}` is not an absolute path to an existing directory on the bot's machine"
        ));
    }
    state.db.set_channel_directory(channel_id, directory)?;
    Ok(format!("Linked this channel to `{directory}`. Send a message to start a session."))
}

/// Fork the session's full context into a new `btw:` thread and dispatch the
/// side question there. The original thread keeps working untouched.
/// Returns the new thread id.
#[allow(clippy::too_many_arguments)]
pub async fn fork_btw(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    source_thread_id: &str,
    parent_channel_id: &str,
    directory: &str,
    prompt: &str,
    user_id: &str,
    username: &str,
) -> Result<String> {
    let session_id = state
        .db
        .get_thread_session(source_thread_id)?
        .ok_or_else(|| anyhow!("no active session in this thread"))?;
    let forked = state
        .oc
        .fork_session(directory, &session_id)
        .await
        .context("failed to fork session")?;

    let name = format!("btw: {}", rendering::prompt_preview(prompt, 90));
    let thread_id = chat
        .create_thread(parent_channel_id, None, &name)
        .await
        .context("failed to create btw thread")?;
    state.db.set_thread_session(&thread_id, &forked.id)?;
    let _ = chat.add_thread_member(&thread_id, user_id).await;
    // The fork is already created and persisted; a failed preamble send must
    // not make the whole operation look failed.
    if let Err(err) = chat
        .send_message(
            &thread_id,
            &format!("Reusing context from the original thread to answer prompt...\n{prompt}"),
        )
        .await
    {
        tracing::warn!("btw preamble send failed: {err:#}");
    }

    let wrapped = format!(
        "The user asked a side question while you were working on another task.\n\
         This is a forked session whose ONLY goal is to answer this question.\n\
         Do NOT continue, resume, or reference the previous task. Only answer the question below.\n\n{prompt}"
    );
    let rt = runner::get_or_create_runtime(state, &thread_id, directory.to_string()).await?;
    runner::enqueue_incoming(
        state.clone(),
        chat.clone(),
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
    Ok(thread_id)
}

/// Where /new-worktree was invoked from.
pub enum WorktreeScope {
    /// Inside an existing session thread; the name falls back to a compressed
    /// slug of `name_hint` (typically the thread title).
    Thread { thread_id: String, name_hint: String },
    /// From a project channel: a fresh thread is created so the user can
    /// start typing while the worktree builds.
    Channel { channel_id: String, user_id: String },
}

/// Create an isolated git worktree for a session. Replies immediately and
/// builds the worktree in the background, retargeting the thread's runtime
/// (and forking its session) once ready.
pub async fn new_worktree(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    scope: WorktreeScope,
    name: Option<String>,
    base_branch: Option<String>,
) -> Result<String> {
    let (project_directory, thread_id, slug, rename_after) = match scope {
        WorktreeScope::Thread { thread_id, name_hint } => {
            let parent = chat
                .thread_parent(&thread_id)
                .await?
                .ok_or_else(|| anyhow!("thread has no parent channel"))?;
            let project = state
                .db
                .get_channel_directory(&parent)?
                .ok_or_else(|| anyhow!("parent channel is not linked to a project"))?;
            let slug = match &name {
                Some(n) => worktree::slugify(n),
                // Auto-derived names get the vowel-stripping compression.
                None => worktree::compress_slug(&worktree::slugify(&name_hint)),
            };
            (project, thread_id, slug, Some(name_hint))
        }
        WorktreeScope::Channel { channel_id, user_id } => {
            let project = state
                .db
                .get_channel_directory(&channel_id)?
                .ok_or_else(|| anyhow!("this channel is not linked to a project"))?;
            let name =
                name.ok_or_else(|| anyhow!("pass a name when creating a worktree from a channel"))?;
            let slug = worktree::slugify(&name);
            let thread_id = chat
                .create_thread(&channel_id, None, &format!("{}{}", worktree::THREAD_PREFIX, slug))
                .await?;
            let _ = chat.add_thread_member(&thread_id, &user_id).await;
            (project, thread_id, slug, None)
        }
    };
    if slug.is_empty() {
        return Err(anyhow!("could not derive a worktree name; pass one explicitly"));
    }

    let wt_dir = worktree::worktree_directory(&state.config.data_dir, &project_directory, &slug);
    state.db.create_pending_worktree(
        &thread_id,
        &worktree::branch_name(&slug),
        &project_directory,
    )?;
    let reply = format!(
        "Creating worktree `{}` on branch `{}`...",
        wt_dir.display(),
        worktree::branch_name(&slug)
    );

    // Build the worktree in the background, then switch the thread to it.
    let state = state.clone();
    let chat = chat.clone();
    tokio::spawn(async move {
        let result =
            git::create_worktree(&project_directory, &wt_dir, &slug, base_branch.as_deref()).await;
        match result {
            Ok(()) => {
                let dir_str = wt_dir.to_string_lossy().to_string();
                if let Err(err) = state.db.set_worktree_ready(&thread_id, &dir_str) {
                    tracing::warn!("failed to persist worktree state: {err:#}");
                }
                // Retarget the thread's runtime in place (keeping its queue
                // and dispatch loop) so the next message runs inside the
                // worktree. If a session already exists, fork it into the
                // worktree so the context carries over.
                let old_session = state.db.get_thread_session(&thread_id).ok().flatten();
                match runner::get_or_create_runtime(&state, &thread_id, dir_str.clone()).await {
                    Ok(rt) => {
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
                    }
                    Err(err) => tracing::warn!("could not retarget runtime: {err:#}"),
                }
                if let Some(hint) = rename_after
                    && !hint.starts_with(worktree::THREAD_PREFIX) {
                        let _ = chat
                            .rename_thread(&thread_id, &format!("{}{}", worktree::THREAD_PREFIX, hint))
                            .await;
                    }
                let _ = chat
                    .send_message(
                        &thread_id,
                        &format!("🌳 Worktree ready at `{dir_str}`. New messages run in the worktree."),
                    )
                    .await;
            }
            Err(err) => {
                let _ = state.db.set_worktree_error(&thread_id, &format!("{err:#}"));
                let _ = chat
                    .send_message(&thread_id, &format!("⚠️ Worktree creation failed: {err:#}"))
                    .await;
            }
        }
    });
    Ok(reply)
}

/// Rebase this thread's worktree back onto the target branch. Returns the
/// reply text; on conflicts the agent is asked (in the thread) to resolve
/// them.
pub async fn merge_worktree(
    state: &Arc<AppState>,
    chat: &Arc<dyn ChatConnector>,
    thread_id: &str,
    thread_name: &str,
    target_branch: Option<String>,
) -> Result<String> {
    let wt = state
        .db
        .get_thread_worktree(thread_id)?
        .ok_or_else(|| anyhow!("this thread has no worktree (use new-worktree first)"))?;
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
    let target = match target_branch {
        Some(t) => t,
        None => git::default_branch(&wt.project_directory).await?,
    };

    let outcome = git::merge_worktree(
        std::path::Path::new(&wt_dir),
        &wt.project_directory,
        &slug,
        &target,
    )
    .await?;

    match outcome {
        worktree::MergeOutcome::Success {
            target_branch,
            branch_name,
            commit_count,
            short_sha,
            cleanup_warning,
        } => {
            state.db.delete_thread_worktree(thread_id)?;
            // The worktree directory is gone: retarget the runtime back at
            // the project (keeping its queue) and drop the session that was
            // bound to the removed directory.
            let rt =
                runner::get_or_create_runtime(state, thread_id, wt.project_directory.clone()).await?;
            runner::reset_session(state, &rt).await?;
            if let Some(stripped) = thread_name.strip_prefix(worktree::THREAD_PREFIX.trim_end()) {
                let _ = chat
                    .rename_thread(thread_id, stripped.trim_start_matches([' ', ':']))
                    .await;
            }
            let warning = cleanup_warning
                .map(|w| format!("\n⚠️ Cleanup needs attention: {w}"))
                .unwrap_or_default();
            Ok(format!(
                "Merged {commit_count} commit(s) from `{branch_name}` into `{target_branch}` (now at `{short_sha}`). Worktree removed.{warning}"
            ))
        }
        worktree::MergeOutcome::RebaseConflict { target_branch } => {
            let directory = task_runner::resolve_thread_directory(state, chat.as_ref(), thread_id).await?;
            let rt = runner::get_or_create_runtime(state, thread_id, directory).await?;
            runner::enqueue_incoming(
                state.clone(),
                chat.clone(),
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
            Ok(format!(
                "Rebase onto `{target_branch}` hit conflicts. Asking the agent to resolve them; run merge-worktree again once it finishes."
            ))
        }
        worktree::MergeOutcome::DirtyWorktree => {
            Ok("The worktree has uncommitted changes. Commit or stash them first.".to_string())
        }
        worktree::MergeOutcome::TargetDirty { target_branch } => Ok(format!(
            "`{target_branch}` is checked out with uncommitted changes in the main repo. Clean it up first."
        )),
        worktree::MergeOutcome::NothingToMerge => Ok("No commits to merge yet.".to_string()),
    }
}

/// List the project's worktrees (lily-created or not) for a channel.
pub async fn worktrees_text(state: &AppState, channel_id: &str) -> Result<String> {
    let project = state
        .db
        .get_channel_directory(channel_id)?
        .ok_or_else(|| anyhow!("this channel is not linked to a project"))?;
    let git_list = git::list_worktrees(&project).await?;
    let db_list = state.db.list_worktrees_for_project(&project)?;
    let mut lines = vec![format!("Worktrees for `{project}`:")];
    for (path, branch) in &git_list {
        let meta = db_list
            .iter()
            .find(|w| w.worktree_directory.as_deref() == Some(path.as_str()))
            .map(|w| format!(" — lily thread {} ({})", w.thread_id, w.status))
            .unwrap_or_default();
        lines.push(format!("- `{path}` on `{branch}`{meta}"));
    }
    if git_list.is_empty() {
        lines.push("(none)".to_string());
    }
    Ok(lines.join("\n"))
}

/// List planned/running scheduled tasks.
pub fn tasks_text(state: &AppState) -> Result<String> {
    let tasks = state.db.list_tasks(false)?;
    if tasks.is_empty() {
        return Ok("No scheduled tasks. Create one with `lily send --send-at ...`.".to_string());
    }
    let lines: Vec<String> = tasks.iter().map(describe_task).collect();
    Ok(format!("Scheduled tasks:\n{}", lines.join("\n")))
}

/// Cancel a scheduled task by id.
pub fn cancel_task_text(state: &AppState, id: i64) -> Result<String> {
    if state.db.cancel_task(id)? {
        Ok(format!("Cancelled task #{id}"))
    } else {
        Ok(format!("Task #{id} is not planned or running"))
    }
}
