//! Git adapter: executes the worktree lifecycle (create, rebase-merge, list)
//! by shelling out to `git`. Naming rules and outcome types live in
//! `domain::worktree`.

use crate::domain::worktree::{branch_name, MergeOutcome};
use anyhow::{anyhow, Context as _, Result};
use std::path::{Path, PathBuf};
use std::process::Output;
use tokio::process::Command;

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
    //! Lifecycle tests against a real git repository in a temp directory.

    use super::*;
    use crate::domain::worktree::worktree_directory;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        // Some CI environments enforce commit signing; tests don't need it.
        let mut full = vec!["-c", "commit.gpgsign=false"];
        full.extend_from_slice(args);
        let out = Command::new("git")
            .args(&full)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().expect("spawn git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn setup_repo(root: &Path) -> PathBuf {
        let repo = root.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("file.txt"), "hello\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "initial"]);
        repo
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lily-wt-test-{}", std::process::id())).join(
            format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ),
        );
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn create_commit_and_merge_worktree() {
        let tmp = tempdir();
        let repo = setup_repo(&tmp);
        let data_dir = tmp.join("data");

        let slug = "test-feature";
        let wt_dir = worktree_directory(&data_dir, repo.to_str().unwrap(), slug);
        create_worktree(repo.to_str().unwrap(), &wt_dir, slug, None)
            .await
            .expect("create worktree");
        assert!(wt_dir.join("file.txt").exists());
        assert_eq!(git_out(&wt_dir, &["symbolic-ref", "--short", "HEAD"]), "lily/test-feature");

        // Commit in the worktree, then merge back.
        std::fs::write(wt_dir.join("new.txt"), "feature work\n").unwrap();
        run_git(&wt_dir, &["add", "."]);
        run_git(&wt_dir, &["commit", "-m", "add feature"]);

        let outcome = merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge");
        match outcome {
            MergeOutcome::Success { commit_count, .. } => assert_eq!(commit_count, 1),
            other => panic!("expected success, got {other:?}"),
        }
        // The commit landed on main in the original checkout and the worktree is gone.
        assert!(repo.join("new.txt").exists());
        assert!(!wt_dir.exists());
        assert_eq!(git_out(&repo, &["log", "--oneline", "-1", "--format=%s"]), "add feature");
    }

    #[tokio::test]
    async fn merge_reports_conflicts_and_leaves_rebase_in_progress() {
        let tmp = tempdir();
        let repo = setup_repo(&tmp);
        let data_dir = tmp.join("data");

        let slug = "conflicting";
        let wt_dir = worktree_directory(&data_dir, repo.to_str().unwrap(), slug);
        create_worktree(repo.to_str().unwrap(), &wt_dir, slug, None)
            .await
            .expect("create worktree");

        // Divergent edits to the same line on both sides.
        std::fs::write(wt_dir.join("file.txt"), "worktree version\n").unwrap();
        run_git(&wt_dir, &["commit", "-am", "worktree edit"]);
        std::fs::write(repo.join("file.txt"), "main version\n").unwrap();
        run_git(&repo, &["commit", "-am", "main edit"]);

        let outcome = merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge call");
        match outcome {
            MergeOutcome::RebaseConflict { target_branch } => assert_eq!(target_branch, "main"),
            other => panic!("expected rebase conflict, got {other:?}"),
        }
        // A second call while the rebase is unresolved reports the same state.
        let again = merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge call");
        assert!(matches!(again, MergeOutcome::RebaseConflict { .. }));
    }

    #[tokio::test]
    async fn merge_refuses_dirty_worktree() {
        let tmp = tempdir();
        let repo = setup_repo(&tmp);
        let data_dir = tmp.join("data");

        let slug = "dirty";
        let wt_dir = worktree_directory(&data_dir, repo.to_str().unwrap(), slug);
        create_worktree(repo.to_str().unwrap(), &wt_dir, slug, None)
            .await
            .expect("create worktree");
        std::fs::write(wt_dir.join("file.txt"), "uncommitted\n").unwrap();

        let outcome = merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge call");
        assert!(matches!(outcome, MergeOutcome::DirtyWorktree));
    }
}
