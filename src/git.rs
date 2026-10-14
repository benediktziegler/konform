use crate::types::ChangedFiles;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
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

/// Resolve `.` and `..` components of an absolute path.
///
/// `std::path::absolute` keeps `..`, so `/proj/../other` would otherwise have
/// `/proj` among its ancestors. A `..` that follows a symlink is resolved
/// against the link target (as the OS would), everything else is purely
/// lexical, so paths without `..` are returned unchanged (no `/var` ->
/// `/private/var` rewriting, no `\\?\` verbatim prefixes).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.is_symlink() {
                    if let Ok(real) = out.canonicalize() {
                        out = strip_verbatim(real);
                    }
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Drop the Windows `\\?\` verbatim prefix that `canonicalize` adds.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            if !rest.starts_with("UNC\\") {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// Walk upward from `anchor` looking for `.git` or `pyproject.toml`.
///
/// The anchor is made absolute first so relative anchors such as `.` or
/// `a.py` still walk up past the current directory.
pub fn find_repo_root(anchor: &Path) -> Option<PathBuf> {
    find_repo_root_in(anchor, &std::env::current_dir().ok()?)
}

/// [`find_repo_root`] with relative anchors resolved against `base`.
fn find_repo_root_in(anchor: &Path, base: &Path) -> Option<PathBuf> {
    let anchor = normalize(&base.join(anchor));
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
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.py"), "").unwrap();
        let root = Some(tmp.path().to_path_buf());
        assert_eq!(find_repo_root_in(Path::new("src"), tmp.path()), root);
        assert_eq!(find_repo_root_in(Path::new("."), tmp.path()), root);
        assert_eq!(find_repo_root_in(Path::new("src/a.py"), tmp.path()), root);
        // The public entry point resolves against the process cwd.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(find_repo_root(Path::new(".")), find_repo_root(&cwd));
    }

    #[test]
    fn dotdot_components_are_resolved_before_walking_up() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::create_dir_all(other.join("deep")).unwrap();
        std::fs::write(proj.join("pyproject.toml"), "").unwrap();
        // `other` has no marker of its own; `proj/../other` must not pick up
        // `proj` as an ancestor.
        assert_eq!(find_repo_root(&proj.join("../other")), None);
        assert_eq!(find_repo_root(&proj.join("../other/deep")), None);
        // `.` and `..` inside the project still resolve to the project root.
        assert_eq!(find_repo_root(&proj.join("sub/./..")), Some(proj.clone()));
        assert_eq!(
            find_repo_root(&other.join("../proj/sub/../sub")),
            Some(proj)
        );
        // Non-existent trailing components are resolved lexically.
        assert_eq!(
            normalize(&tmp.path().join("a/b/../c")),
            tmp.path().join("a/c")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_after_symlink_follows_the_link_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real/inner");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(tmp.path().join("real/pyproject.toml"), "").unwrap();
        std::fs::create_dir(tmp.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("elsewhere/link")).unwrap();
        // `elsewhere/link/..` is `real`, not `elsewhere`.
        let root = find_repo_root(&tmp.path().join("elsewhere/link/..")).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().join("real").canonicalize().unwrap()
        );
    }
}
