//! Server-side watching of config and pattern files.
//!
//! The language server normally asks the editor to watch these files
//! (`client/registerCapability` for `workspace/didChangeWatchedFiles`). Some
//! clients cannot: they lack dynamic registration, or they reject the
//! request. Without a fallback, edits to `pyproject.toml` or a patterns file
//! then go unnoticed until the server restarts. Like ty, konform falls back
//! to watching the workspace itself.

use crossbeam_channel::Receiver;
use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};

/// A running watcher: [`Self::changes`] yields `()` whenever a watched file
/// is created, changed or removed. Dropping it stops watching.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    changes: Receiver<()>,
}

impl ConfigWatcher {
    /// Watch `root` recursively, plus the parent directory of every absolute
    /// glob (files pulled in through `extend` may live outside the
    /// workspace), reporting changes to paths that match `globs`.
    pub fn start(root: &Path, globs: &[String]) -> notify::Result<Self> {
        let matcher = compile(globs);
        let root_for_events = root.to_path_buf();
        let (tx, changes) = crossbeam_channel::unbounded();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                if event.kind.is_access() {
                    return;
                }
                if event
                    .paths
                    .iter()
                    .any(|p| is_watched(&matcher, &root_for_events, p))
                {
                    let _ = tx.send(());
                }
            })?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        for dir in outside_dirs(root, globs) {
            // A missing directory is not worth failing over: there is
            // nothing to edit there yet.
            let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
        }
        Ok(Self {
            _watcher: watcher,
            changes,
        })
    }

    pub fn changes(&self) -> &Receiver<()> {
        &self.changes
    }
}

fn compile(globs: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        if let Ok(g) = Glob::new(glob) {
            builder.add(g);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

/// Whether `path` (absolute, from the OS) matches the watch globs. The
/// `**/name` globs are written relative to the workspace root.
fn is_watched(matcher: &GlobSet, root: &Path, path: &Path) -> bool {
    matcher.is_match(path)
        || path
            .strip_prefix(root)
            .is_ok_and(|rel| matcher.is_match(rel))
}

/// Directories holding absolute globs that lie outside `root`.
fn outside_dirs(root: &Path, globs: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = globs
        .iter()
        .map(Path::new)
        .filter(|p| p.is_absolute() && !p.starts_with(root))
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn globs() -> Vec<String> {
        vec!["**/pyproject.toml".into(), "**/konform.toml".into()]
    }

    #[test]
    fn matches_relative_and_absolute_paths() {
        let root = Path::new("/ws");
        let m = compile(&["**/pyproject.toml".into(), "/elsewhere/base.toml".into()]);
        assert!(is_watched(&m, root, Path::new("/ws/pyproject.toml")));
        assert!(is_watched(&m, root, Path::new("/ws/sub/pyproject.toml")));
        assert!(is_watched(&m, root, Path::new("/elsewhere/base.toml")));
        assert!(!is_watched(&m, root, Path::new("/ws/app.py")));
    }

    #[test]
    fn invalid_globs_are_skipped() {
        let m = compile(&["a/[".into(), "**/konform.toml".into()]);
        assert!(is_watched(
            &m,
            Path::new("/ws"),
            Path::new("/ws/konform.toml")
        ));
    }

    #[test]
    fn only_absolute_globs_outside_the_root_add_directories() {
        // Absolute on every platform (`/ws` is not absolute on Windows).
        let base = std::env::temp_dir();
        let root = base.join("ws");
        let shared = base.join("shared");
        let glob = |p: PathBuf| p.to_string_lossy().into_owned();
        let dirs = outside_dirs(
            &root,
            &[
                "**/pyproject.toml".into(),
                glob(root.join("base.toml")),
                glob(shared.join("base.toml")),
                glob(shared.join("other.toml")),
            ],
        );
        assert_eq!(dirs, [shared]);
    }

    #[test]
    fn reports_a_config_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let watcher = ConfigWatcher::start(&root, &globs()).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[tool.konform]\n").unwrap();
        watcher
            .changes()
            .recv_timeout(Duration::from_secs(10))
            .expect("a change to pyproject.toml should be reported");
    }
}
