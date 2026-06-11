//! Git adapter: creates and lists isolated worktrees by shelling out to
//! `git`. Naming rules live in `domain::worktree`.

use crate::domain::worktree::branch_name;
use anyhow::{anyhow, Context as _, Result};
use std::path::Path;
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
    use std::path::PathBuf;
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
    async fn create_worktree_checks_out_isolated_branch() {
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

        // Commits in the worktree stay isolated from the main checkout.
        std::fs::write(wt_dir.join("new.txt"), "feature work\n").unwrap();
        run_git(&wt_dir, &["add", "."]);
        run_git(&wt_dir, &["commit", "-m", "add feature"]);
        assert!(!repo.join("new.txt").exists());

        let listed = list_worktrees(repo.to_str().unwrap()).await.expect("list");
        assert!(listed.iter().any(|(_, b)| b == "lily/test-feature"));
    }

    #[tokio::test]
    async fn create_worktree_rejects_bad_base_ref() {
        let tmp = tempdir();
        let repo = setup_repo(&tmp);
        let data_dir = tmp.join("data");
        let wt_dir = worktree_directory(&data_dir, repo.to_str().unwrap(), "bad-base");

        let err = create_worktree(repo.to_str().unwrap(), &wt_dir, "bad-base", Some("--exec=true"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid base branch"));
    }
}
