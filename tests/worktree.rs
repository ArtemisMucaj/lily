//! End-to-end tests for the worktree lifecycle against a real git repo.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
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

fn setup_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("project");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    repo
}

#[tokio::test]
async fn create_commit_and_merge_worktree() {
    let tmp = tempdir();
    let repo = setup_repo(&tmp);
    let data_dir = tmp.join("data");

    let slug = "test-feature";
    let wt_dir = lily_worktree::worktree_directory(&data_dir, repo.to_str().unwrap(), slug);
    lily_worktree::create_worktree(repo.to_str().unwrap(), &wt_dir, slug, None)
        .await
        .expect("create worktree");
    assert!(wt_dir.join("file.txt").exists());
    assert_eq!(git_out(&wt_dir, &["symbolic-ref", "--short", "HEAD"]), "lily/test-feature");

    // Commit in the worktree, then merge back.
    std::fs::write(wt_dir.join("new.txt"), "feature work\n").unwrap();
    git(&wt_dir, &["add", "."]);
    git(&wt_dir, &["commit", "-m", "add feature"]);

    let outcome =
        lily_worktree::merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge");
    match outcome {
        lily_worktree::MergeOutcome::Success { commit_count, .. } => assert_eq!(commit_count, 1),
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
    let wt_dir = lily_worktree::worktree_directory(&data_dir, repo.to_str().unwrap(), slug);
    lily_worktree::create_worktree(repo.to_str().unwrap(), &wt_dir, slug, None)
        .await
        .expect("create worktree");

    // Divergent edits to the same line on both sides.
    std::fs::write(wt_dir.join("file.txt"), "worktree version\n").unwrap();
    git(&wt_dir, &["commit", "-am", "worktree edit"]);
    std::fs::write(repo.join("file.txt"), "main version\n").unwrap();
    git(&repo, &["commit", "-am", "main edit"]);

    let outcome =
        lily_worktree::merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge call");
    match outcome {
        lily_worktree::MergeOutcome::RebaseConflict { target_branch } => {
            assert_eq!(target_branch, "main");
        }
        other => panic!("expected rebase conflict, got {other:?}"),
    }
    // A second call while the rebase is unresolved reports the same state.
    let again = lily_worktree::merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
        .await
        .expect("merge call");
    assert!(matches!(again, lily_worktree::MergeOutcome::RebaseConflict { .. }));
}

#[tokio::test]
async fn merge_refuses_dirty_worktree() {
    let tmp = tempdir();
    let repo = setup_repo(&tmp);
    let data_dir = tmp.join("data");

    let slug = "dirty";
    let wt_dir = lily_worktree::worktree_directory(&data_dir, repo.to_str().unwrap(), slug);
    lily_worktree::create_worktree(repo.to_str().unwrap(), &wt_dir, slug, None)
        .await
        .expect("create worktree");
    std::fs::write(wt_dir.join("file.txt"), "uncommitted\n").unwrap();

    let outcome =
        lily_worktree::merge_worktree(&wt_dir, repo.to_str().unwrap(), slug, "main")
            .await
            .expect("merge call");
    assert!(matches!(outcome, lily_worktree::MergeOutcome::DirtyWorktree));
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lily-wt-test-{}", std::process::id()))
        .join(format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// The binary crate's modules aren't importable from integration tests, so the
// worktree module is included directly; it only depends on anyhow and tokio.
#[path = "../src/worktree.rs"]
mod lily_worktree;
