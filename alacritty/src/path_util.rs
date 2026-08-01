//! Path display utilities shared across tab titles and the palette.

use std::path::{Path, PathBuf};

/// Compact a path for display: replace the home prefix with `~`, and if still long keep only the
/// last few segments.
pub(crate) fn shorten_path(path: &Path) -> String {
    let s = match home::home_dir() {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).unwrap_or(Path::new("")).display())
        },
        _ => path.display().to_string(),
    };

    const MAX_LEN: usize = 40;
    if s.len() <= MAX_LEN {
        return s;
    }

    let segments: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if segments.len() <= 3 {
        return s;
    }
    let tail = segments[segments.len() - 3..].join("/");
    format!("…/{}", tail)
}

/// Resolve the current git branch for a path by walking up to find `.git/HEAD`.
/// Returns `None` if not in a git repo.
pub(crate) fn git_branch(path: &Path) -> Option<String> {
    for ancestor in path.ancestors() {
        let head = ancestor.join(".git").join("HEAD");
        if let Ok(content) = std::fs::read_to_string(&head) {
            let content = content.trim();
            if let Some(branch) = content.strip_prefix("ref: refs/heads/") {
                return Some(branch.to_string());
            }
            if content.len() >= 7 {
                return Some(content[..7].to_string());
            }
            return Some(content.to_string());
        }
    }
    None
}

/// Walk up from `path` to find the nearest directory containing a `.git` entry (a directory for
/// normal clones, a file for worktrees). Returns that directory, or `None` if `path` is not
/// inside a git repo.
pub(crate) fn git_repo_root(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.join(".git").exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_repo_root_finds_enclosing_repo() {
        // Build <tmp>/flare-<pid>-<count>/{.git/, subdir/nested}
        let base = std::env::temp_dir().join(format!("flare-reporoot-{}", std::process::id()));
        let repo = base.join("myproj");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();

        // A nested path resolves up to the repo root.
        assert_eq!(git_repo_root(&nested).as_deref(), Some(repo.as_path()));

        // The repo root resolves to itself.
        assert_eq!(git_repo_root(&repo).as_deref(), Some(repo.as_path()));

        // Outside the repo: nothing.
        assert_eq!(git_repo_root(&base), None);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn git_repo_root_handles_worktree_git_file() {
        let base = std::env::temp_dir().join(format!("flare-wt-{}", std::process::id()));
        let wt = base.join("worktree");
        let nested = wt.join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        // A worktree's `.git` is a file, not a directory.
        std::fs::write(wt.join(".git"), "gitdir: /somewhere/else\n").unwrap();

        assert_eq!(git_repo_root(&nested).as_deref(), Some(wt.as_path()));

        std::fs::remove_dir_all(&base).ok();
    }
}
