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
///
/// The anchor is made absolute first so relative anchors such as `.` or
/// `a.py` still walk up past the current directory.
pub fn find_repo_root(anchor: &Path) -> Option<PathBuf> {
    let anchor = std::path::absolute(anchor).ok()?;
    let start = if anchor.is_file() {
        anchor.parent()?.to_path_buf()
    } else {
        anchor
    };
    for dir in start.ancestors() {
        if dir.join(".git").exists() || dir.join("pyproject.toml").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_root_from_nested_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("x.py"), "").unwrap();
        assert_eq!(find_repo_root(&nested), Some(tmp.path().to_path_buf()));
        assert_eq!(
            find_repo_root(&nested.join("x.py")),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn finds_git_dir_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        assert_eq!(find_repo_root(tmp.path()), Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn relative_anchor_is_made_absolute() {
        // The cwd is the crate root (has Cargo.toml but also .git or
        // pyproject.toml somewhere above or in it); a relative anchor must
        // resolve to the same root as its absolute form.
        let rel = Path::new("src");
        let abs = std::env::current_dir().unwrap().join("src");
        assert_eq!(find_repo_root(rel), find_repo_root(&abs));
        assert_eq!(find_repo_root(Path::new(".")), find_repo_root(&abs));
    }
}
