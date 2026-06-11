//! Git worktree management: isolate a session's work in a separate checkout,
//! then rebase the commits back onto the default branch.

use anyhow::{anyhow, Context as _, Result};
use std::path::{Path, PathBuf};
use std::process::Output;
use tokio::process::Command;

/// Branch prefix for worktrees created by lily.
pub const BRANCH_PREFIX: &str = "lily/";
/// Thread title prefix marking an unmerged worktree thread.
pub const THREAD_PREFIX: &str = "⬦ worktree: ";

#[derive(Debug)]
pub enum MergeOutcome {
    Success {
        target_branch: String,
        branch_name: String,
        commit_count: u64,
        short_sha: String,
    },
    /// Rebase stopped on conflicts; git is left mid-rebase so the agent can
    /// resolve them, after which /merge-worktree is run again.
    RebaseConflict { target_branch: String },
    DirtyWorktree,
    TargetDirty { target_branch: String },
    NothingToMerge,
}

async fn git(dir: &Path, args: &[&str]) -> Result<Output> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    Ok(out)
}

async fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args).await?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fnv1a64(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in data.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Lowercase, collapse whitespace to dashes, drop anything but [a-z0-9-].
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Compress long auto-derived slugs by stripping vowels from each word,
/// keeping the first letter: `configurable-sidebar-width` → `cnfgrbl-sdbr-wdth`.
pub fn compress_slug(slug: &str) -> String {
    if slug.len() <= 20 {
        return slug.to_string();
    }
    slug.split('-')
        .map(|word| {
            let mut out = String::new();
            for (i, ch) in word.chars().enumerate() {
                if i == 0 || !matches!(ch, 'a' | 'e' | 'i' | 'o' | 'u') {
                    out.push(ch);
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("-")
}

pub fn branch_name(slug: &str) -> String {
    format!("{BRANCH_PREFIX}{slug}")
}

/// Directory for a worktree: `<data_dir>/worktrees/<project-hash>/<slug>`.
pub fn worktree_directory(data_dir: &Path, project_directory: &str, slug: &str) -> PathBuf {
    let hash = format!("{:08x}", fnv1a64(project_directory) & 0xffff_ffff);
    data_dir.join("worktrees").join(hash).join(slug)
}

/// Create a worktree on a fresh branch. `base_ref` defaults to HEAD so the
/// branch can start from unpushed commits.
pub async fn create_worktree(
    project_directory: &str,
    worktree_dir: &Path,
    slug: &str,
    base_ref: Option<&str>,
) -> Result<()> {
    let project = Path::new(project_directory);
    let base = base_ref.unwrap_or("HEAD");
    if let Some(explicit) = base_ref {
        git_ok(project, &["check-ref-format", "--branch", explicit])
            .await
            .map_err(|_| anyhow!("invalid base branch: {explicit}"))?;
    }
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let branch = branch_name(slug);
    let dir_str = worktree_dir.to_string_lossy().to_string();
    git_ok(project, &["worktree", "add", &dir_str, "-B", &branch, base]).await?;
    Ok(())
}

pub async fn default_branch(project_directory: &str) -> Result<String> {
    let project = Path::new(project_directory);
    // Prefer the remote HEAD if configured, fall back to common names.
    if let Ok(r) = git_ok(project, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await
        && let Some(name) = r.strip_prefix("origin/") {
            return Ok(name.to_string());
        }
    for cand in ["main", "master"] {
        if git(project, &["show-ref", "--verify", &format!("refs/heads/{cand}")])
            .await?
            .status
            .success()
        {
            return Ok(cand.to_string());
        }
    }
    git_ok(project, &["symbolic-ref", "--short", "HEAD"]).await
}

async fn rebase_in_progress(worktree_dir: &Path) -> Result<bool> {
    for kind in ["rebase-merge", "rebase-apply"] {
        let p = git_ok(worktree_dir, &["rev-parse", "--git-path", kind]).await?;
        let path = if Path::new(&p).is_absolute() {
            PathBuf::from(&p)
        } else {
            worktree_dir.join(&p)
        };
        if path.exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Rebase the worktree's commits onto `target_branch` and fast-forward the
/// target in the main checkout, then clean up branch and worktree.
pub async fn merge_worktree(
    worktree_dir: &Path,
    main_repo_dir: &str,
    slug: &str,
    target_branch: &str,
) -> Result<MergeOutcome> {
    let main_repo = Path::new(main_repo_dir);
    let branch = branch_name(slug);

    // A rebase already in progress means the agent is (or should be) resolving
    // conflicts; report that instead of stacking another rebase.
    if rebase_in_progress(worktree_dir).await? {
        return Ok(MergeOutcome::RebaseConflict { target_branch: target_branch.to_string() });
    }

    if !git_ok(worktree_dir, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(MergeOutcome::DirtyWorktree);
    }

    let merge_base = git_ok(worktree_dir, &["merge-base", "HEAD", target_branch]).await?;
    let count: u64 = git_ok(worktree_dir, &["rev-list", "--count", &format!("{merge_base}..HEAD")])
        .await?
        .parse()
        .unwrap_or(0);
    if count == 0 {
        return Ok(MergeOutcome::NothingToMerge);
    }

    // Rebase unless the worktree is already based on the target's tip.
    let target_tip = git_ok(worktree_dir, &["rev-parse", target_branch]).await?;
    if merge_base != target_tip {
        let out = git(worktree_dir, &["rebase", target_branch]).await?;
        if !out.status.success() {
            if rebase_in_progress(worktree_dir).await? {
                return Ok(MergeOutcome::RebaseConflict {
                    target_branch: target_branch.to_string(),
                });
            }
            return Err(anyhow!(
                "rebase failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    // The target must now be an ancestor of HEAD (pure fast-forward).
    let ff = git(worktree_dir, &["merge-base", "--is-ancestor", target_branch, "HEAD"]).await?;
    if !ff.status.success() {
        return Err(anyhow!("merge would not be a fast-forward after rebase"));
    }

    // If the target branch is checked out in the main repo it must be clean,
    // because the push below updates its working tree in place.
    let main_branch = git_ok(main_repo, &["symbolic-ref", "--short", "HEAD"]).await.unwrap_or_default();
    if main_branch == target_branch
        && !git_ok(main_repo, &["status", "--porcelain"]).await?.is_empty()
    {
        return Ok(MergeOutcome::TargetDirty { target_branch: target_branch.to_string() });
    }

    // Fast-forward the target branch via a local push; `updateInstead` lets us
    // update a checked-out branch without switching to it.
    let common_dir_raw = git_ok(worktree_dir, &["rev-parse", "--git-common-dir"]).await?;
    let common_dir = if Path::new(&common_dir_raw).is_absolute() {
        PathBuf::from(&common_dir_raw)
    } else {
        worktree_dir.join(&common_dir_raw)
    };
    git_ok(
        worktree_dir,
        &[
            "push",
            "--receive-pack=git -c receive.denyCurrentBranch=updateInstead receive-pack",
            &common_dir.to_string_lossy(),
            &format!("HEAD:{target_branch}"),
        ],
    )
    .await?;

    let short_sha = git_ok(worktree_dir, &["rev-parse", "--short", "HEAD"]).await?;

    // Clean up: detach so the branch can be deleted, drop branch and worktree.
    let _ = git(worktree_dir, &["checkout", "--detach", target_branch]).await;
    let _ = git(main_repo, &["branch", "-D", &branch]).await;
    let dir_str = worktree_dir.to_string_lossy().to_string();
    let removed = git(main_repo, &["worktree", "remove", &dir_str]).await?;
    if !removed.status.success() {
        let _ = git(main_repo, &["worktree", "remove", "--force", &dir_str]).await;
    }
    let _ = git(main_repo, &["worktree", "prune"]).await;

    Ok(MergeOutcome::Success {
        target_branch: target_branch.to_string(),
        branch_name: branch,
        commit_count: count,
        short_sha,
    })
}

/// Prompt sent to the agent when a merge rebase hits conflicts.
pub fn conflict_resolution_prompt(target_branch: &str) -> String {
    format!(
        "The rebase onto `{target_branch}` stopped on conflicts. Resolve them: \
         run `git status` to find conflicted files, understand both sides using \
         the merge base and commit messages, edit the files to resolve every \
         conflict marker, `git add` them, then `git rebase --continue`. Repeat \
         until the rebase completes. Do not abort the rebase. When it is done, \
         tell the user to run /merge-worktree again to complete the merge."
    )
}

/// Worktrees of a project as reported by git itself.
pub async fn list_worktrees(project_directory: &str) -> Result<Vec<(String, String)>> {
    let raw = git_ok(Path::new(project_directory), &["worktree", "list", "--porcelain"]).await?;
    let mut result = Vec::new();
    let mut path: Option<String> = None;
    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let Some(p) = path.take() {
                result.push((p, b.trim_start_matches("refs/heads/").to_string()));
            }
        } else if line == "detached"
            && let Some(p) = path.take() {
                result.push((p, "(detached)".to_string()));
            }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Redesign  the Sidebar!"), "redesign-the-sidebar");
    }

    #[test]
    fn compress_strips_vowels_keeping_first_letter() {
        assert_eq!(compress_slug("configurable-sidebar-width"), "cnfgrbl-sdbr-wdth");
    }

    #[test]
    fn compress_keeps_short_slugs() {
        assert_eq!(compress_slug("fix-login"), "fix-login");
    }
}
