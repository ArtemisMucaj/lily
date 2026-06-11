//! Worktree domain: naming rules and on-disk layout. The git plumbing that
//! acts on these lives in `connector::git`.

use std::path::{Path, PathBuf};

/// Branch prefix for worktrees created by lily.
pub const BRANCH_PREFIX: &str = "lily/";
/// Thread title prefix marking an unmerged worktree thread.
pub const THREAD_PREFIX: &str = "⬦ worktree: ";

/// A thread's worktree assignment, persisted between restarts.
#[derive(Debug, Clone)]
pub struct ThreadWorktree {
    pub thread_id: String,
    pub worktree_name: String,
    pub worktree_directory: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
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
