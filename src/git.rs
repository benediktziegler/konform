use crate::types::ChangedFiles;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_lines(args: &[&str]) -> Vec<String> {
    Command::new("git")
        .args(args)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Return the set of files considered "changed" for violation routing.
/// Mirrors the Python `get_changed_files` function exactly.
pub fn get_changed_files() -> ChangedFiles {
    let mut staged = git_lines(&["diff", "--cached", "--name-only"]);
    let not_staged = git_lines(&["diff", "--name-only"]);
    staged.extend(not_staged);

    if !staged.is_empty() {
        return ChangedFiles {
            files: staged.into_iter().collect::<HashSet<_>>(),
        };
    }

    let committed = git_lines(&["diff", "--name-only", "HEAD~1", "HEAD"]);
    ChangedFiles {
        files: committed.into_iter().collect::<HashSet<_>>(),
    }
}

/// Walk upward from `anchor` looking for `.git` or `pyproject.toml`.
pub fn find_repo_root(anchor: &Path) -> Option<PathBuf> {
    let start = if anchor.is_file() {
        anchor.parent()?.to_path_buf()
    } else {
        anchor.to_path_buf()
    };
    for dir in start.ancestors() {
        if dir.join(".git").exists() || dir.join("pyproject.toml").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}
