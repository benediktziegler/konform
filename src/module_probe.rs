//! Filesystem-based Python module existence probe.
//!
//! Asks the Python interpreter for `sys.path` once at startup, then answers
//! every `(module_name, attr_name)` query purely via filesystem look-ups:
//! no code execution, no pyo3, no `__import__`.
//!
//! Results are cached in a [`dashmap::DashMap`] for the lifetime of the
//! process.

use dashmap::DashMap;
use ruff_python_ast::Stmt;
use ruff_python_parser::parse_module;
use seahash::SeaHasher;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the interpreter may take to answer one probe query before it is
/// killed and the probe is treated as unusable.
const INTERPRETER_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of checking whether an imported name is a module.
///
/// See [`ModuleProbe::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleCheck {
    /// `attr_name` resolved to a real module.
    Module,
    /// `attr_name` resolved and is definitively not a module.
    NotModule,
    /// The root package (first dotted component of `module_name`) could not
    /// be found anywhere in `sys.path`, e.g. it isn't installed in this
    /// Python environment. There is no way to tell whether `attr_name` would
    /// be a module or not, so callers should treat this as "unknown" rather
    /// than a definitive violation.
    Unknown,
}

/// Run `python -c code` and return its stdout, or `None` if it could not be
/// started, exited unsuccessfully, printed non-UTF-8, or did not finish within
/// [`INTERPRETER_TIMEOUT`] (in which case it is killed).
fn run_python(python: &Path, code: &str) -> Option<String> {
    run_python_with_timeout(python, code, INTERPRETER_TIMEOUT)
}

fn run_python_with_timeout(python: &Path, code: &str, timeout: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = Command::new(python)
        .args(["-c", code])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Drain stdout on a thread so a chatty child cannot block on a full pipe
    // while we poll for its exit.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).ok().map(|_| buf)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Timed out (or `try_wait` failed): do not join the reader, a
            // grandchild may still hold the pipe open.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let buf = reader.join().ok()??;
    if !status.success() {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// Everything [`ModuleProbe`] needs from the interpreter, fetched in one call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct InterpreterInfo {
    /// Raw `sys.path`; the `""` (working directory) entry is resolved later.
    sys_path: Vec<String>,
    /// `sys.stdlib_module_names` (Python 3.10+), else `sys.builtin_module_names`:
    /// modules with no discoverable file on disk (e.g. `itertools`, `sys`).
    builtin_modules: Vec<String>,
    /// `site.getusersitepackages()`. Python adds it to `sys.path` only once
    /// it exists, so its creation must invalidate a saved answer too.
    user_site: Option<String>,
}

/// One interpreter call answering all of [`InterpreterInfo`].
const INTERPRETER_QUERY: &str = "import json,sys\n\
names = getattr(sys, 'stdlib_module_names', None) or sys.builtin_module_names\n\
try:\n    import site; user_site = site.getusersitepackages()\n\
except Exception:\n    user_site = None\n\
print(json.dumps({'sys_path': sys.path, 'builtin_modules': list(names), 'user_site': user_site}))";

/// Ask `python` for its [`InterpreterInfo`]; `None` if it could not be run.
fn query_interpreter(python: &Path) -> Option<InterpreterInfo> {
    let stdout = run_python(python, INTERPRETER_QUERY)?;
    serde_json::from_str(stdout.trim()).ok()
}

/// Environment variables that change the interpreter's `sys.path`.
const PATH_ENV_VARS: &[&str] = &[
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONSAFEPATH",
    "PYTHONNOUSERSITE",
    "PYTHONUSERBASE",
    "PYTHONPLATLIBDIR",
    "HOME",
    "APPDATA",
];

/// The file a bare interpreter name (`python3`) runs: the first match on
/// `PATH`. Paths with a directory part are taken as they are.
fn resolve_executable(python: &Path) -> Option<PathBuf> {
    if python.components().count() > 1 {
        return std::path::absolute(python).ok();
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(python))
        .find(|p| p.is_file())
}

/// Key of the saved [`InterpreterInfo`] for `python`, or `None` when the
/// interpreter cannot be located (it is then queried, never cached).
fn interpreter_key(python: &Path) -> Option<u64> {
    let exe = resolve_executable(python)?;
    // Follow symlinks (a venv's `python` links to the base interpreter) so
    // upgrading the base interpreter changes the key.
    let real = std::fs::canonicalize(&exe).ok()?;
    let meta = std::fs::metadata(&real).ok()?;
    let mut h = SeaHasher::new();
    exe.hash(&mut h);
    real.hash(&mut h);
    meta.len().hash(&mut h);
    dir_stamp(&real).hash(&mut h);
    // A venv's `pyvenv.cfg` sits next to `bin/` (or `Scripts/`).
    exe.parent()
        .and_then(Path::parent)
        .map(|env| dir_stamp(&env.join("pyvenv.cfg")))
        .hash(&mut h);
    for var in PATH_ENV_VARS {
        std::env::var_os(var).hash(&mut h);
    }
    // Relative `PYTHONPATH` entries resolve against the working directory.
    std::env::current_dir().ok().hash(&mut h);
    Some(h.finish())
}

/// Modification time of `path` (seconds, nanoseconds), or `None` if it
/// cannot be read (for example because it does not exist).
fn dir_stamp(path: &Path) -> Option<(i64, u32)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    Some((mtime.seconds(), mtime.nanoseconds()))
}

/// The directories whose mtimes guard a saved [`InterpreterInfo`]: every
/// non-empty `sys.path` entry plus the user site directory.
fn info_stamps(info: &InterpreterInfo) -> Vec<(String, Option<(i64, u32)>)> {
    info.sys_path
        .iter()
        .filter(|p| !p.is_empty())
        .chain(info.user_site.iter())
        .map(|p| (p.clone(), dir_stamp(Path::new(p))))
        .collect()
}

/// On-disk form of a saved [`InterpreterInfo`].
#[derive(Serialize, Deserialize)]
struct SavedInfo {
    info: InterpreterInfo,
    stamps: Vec<(String, Option<(i64, u32)>)>,
}

/// The [`InterpreterInfo`] saved in `file`, if present, readable, and still
/// current (no guarded directory changed since it was saved).
fn load_info(file: &Path) -> Option<InterpreterInfo> {
    let bytes = std::fs::read(file).ok()?;
    let saved: SavedInfo = serde_json::from_slice(&bytes).ok()?;
    (info_stamps(&saved.info) == saved.stamps).then_some(saved.info)
}

/// Best-effort save of `info` to `file` (write to a temp file, then rename,
/// so a concurrent run never reads a partial file).
fn store_info(file: &Path, key: u64, info: &InterpreterInfo) {
    let saved = SavedInfo {
        info: info.clone(),
        stamps: info_stamps(info),
    };
    let Ok(bytes) = serde_json::to_vec(&saved) else {
        return;
    };
    // `<hash>.<pid>.<n>.tmp`, the name the cache's stale-temp sweep knows.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = file.with_file_name(format!("{key:016x}.{}.{n}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Caches `(module_name, attr_name) -> is_module` probe results.
pub struct ModuleProbe {
    /// Ordered list of directories to search, from `python3 -c "import sys,json; print(json.dumps(sys.path))"`.
    sys_path: Vec<PathBuf>,
    /// Names of modules that are compiled into the interpreter or otherwise
    /// part of the standard library, and therefore have no corresponding
    /// file/directory anywhere in `sys_path` (e.g. `itertools`, `sys`,
    /// `_thread`). Sourced from `sys.stdlib_module_names` (falling back to
    /// `sys.builtin_module_names` on Python < 3.10).
    builtin_modules: HashSet<String>,
    /// In-process result cache.
    cache: DashMap<(String, String), bool>,
    /// Caches whether a root package name exists anywhere in `sys_path`.
    root_cache: DashMap<String, bool>,
    /// `true` if the interpreter at `python` could not be invoked at all
    /// (missing binary, not executable, crashed, etc.), as opposed to
    /// running fine but simply not having a given package installed. When
    /// this is `true`, every KIS001 import in the project will be reported
    /// as "unknown"/unfixable, which looks identical to "package genuinely
    /// not installed" unless callers surface this flag explicitly.
    probe_unusable: bool,
}

impl ModuleProbe {
    /// Create a new probe using `python` as the interpreter for `sys.path` discovery.
    ///
    /// Pass the result of [`crate::config::resolve_python`] here so the probe
    /// searches the same environment that owns the files being checked.
    ///
    /// `project_root` stands in for "current working directory" when
    /// resolving the empty-string entry in `sys.path` (see
    /// [`ModuleProbe::resolve_sys_path`]). It should be the root of the
    /// project being linted (e.g. `config.config_dir`, or the target path's
    /// repo root) -- **not** necessarily the konform process's own OS working
    /// directory, since `konform check some/other/project` may be invoked
    /// from anywhere.
    ///
    /// `src_roots` is `config.src` -- directories (relative to `project_root`)
    /// to additionally search, mirroring Ruff's `src` setting (defaults to
    /// `[".", "src"]`).
    pub fn new(python: &Path, project_root: &Path, src_roots: &[String]) -> Self {
        Self::from_info(query_interpreter(python), project_root, src_roots)
    }

    /// Like [`ModuleProbe::new`], but reuses the interpreter's answer saved
    /// in `cache_dir` by an earlier run, so a run whose environment did not
    /// change starts no Python process at all.
    ///
    /// The saved answer is keyed by the interpreter (its resolved path, size
    /// and mtime), the environment variables that shape `sys.path`, and the
    /// working directory. It is used only while every `sys.path` directory
    /// (and the user site directory) still has the mtime it had when saved,
    /// so installing a package or adding a `.pth` file re-queries.
    pub fn new_cached(
        python: &Path,
        project_root: &Path,
        src_roots: &[String],
        cache_dir: &Path,
    ) -> Self {
        let info = match interpreter_key(python) {
            Some(key) => {
                let file = cache_dir.join(format!("{key:016x}.python"));
                load_info(&file).or_else(|| {
                    let info = query_interpreter(python)?;
                    store_info(&file, key, &info);
                    Some(info)
                })
            }
            None => query_interpreter(python),
        };
        Self::from_info(info, project_root, src_roots)
    }

    /// Build a probe from the interpreter's answer; `None` (the interpreter
    /// could not be run) yields an unusable probe rather than silently
    /// proceeding with empty data, which would make every import in the
    /// project look like an unresolvable package instead of surfacing the
    /// real problem: a broken/misconfigured `python`.
    fn from_info(info: Option<InterpreterInfo>, project_root: &Path, src_roots: &[String]) -> Self {
        let probe_unusable = info.is_none();
        let (sys_path, builtin_modules) = info
            .map(|i| {
                (
                    Self::resolve_sys_path(i.sys_path, Some(project_root), src_roots),
                    i.builtin_modules.into_iter().collect(),
                )
            })
            .unwrap_or_default();
        Self {
            sys_path,
            builtin_modules,
            cache: DashMap::new(),
            root_cache: DashMap::new(),
            probe_unusable,
        }
    }

    /// `true` if the Python interpreter passed to [`ModuleProbe::new`]
    /// couldn't be invoked at all, meaning every KIS001 "unknown module"
    /// result in this run reflects a broken environment rather than a
    /// genuinely uninstalled package. Callers should surface this
    /// distinctly (e.g. a top-level warning) since the per-import message
    /// alone can't tell the two apart.
    pub fn probe_unusable(&self) -> bool {
        self.probe_unusable
    }

    /// Fingerprint of the Python environment this probe searches.
    ///
    /// Hashes each `sys.path` directory's own mtime (not a recursive scan).
    /// Installing, upgrading, or removing a package touches the mtime of the
    /// directory that contains it (site-packages, a namespace-package root,
    /// etc.), so this changes whenever the resolvable module set changes,
    /// even though no *source file being linted* was touched.
    ///
    /// Callers should fold this into any on-disk cache key that depends on
    /// [`ModuleProbe::is_module`] results (e.g. KIS001 violations), otherwise
    /// a cache keyed purely on source-file mtime will keep serving stale
    /// results after a `pip install` / `uv sync` until the file itself is
    /// edited.
    pub fn env_fingerprint(&self) -> u64 {
        let mut h = SeaHasher::new();
        for dir in &self.sys_path {
            format!("{dir:?}").hash(&mut h);
            match std::fs::metadata(dir) {
                Ok(meta) => {
                    let mtime = filetime::FileTime::from_last_modification_time(&meta);
                    mtime.seconds().hash(&mut h);
                    mtime.nanoseconds().hash(&mut h);
                }
                // Missing/unreadable sys.path entries still contribute a
                // stable (but distinct) value so the fingerprint changes if
                // the entry starts or stops existing.
                Err(_) => "missing".hash(&mut h),
            }
        }
        h.finish()
    }

    /// Resolve the raw `sys.path` strings returned by the interpreter into
    /// filesystem paths.
    ///
    /// Python represents "current working directory" as an empty string in
    /// `sys.path` (e.g. `sys.path[0]` for `python -c "..."` / interactive
    /// use). `python` is spawned without overriding its working directory,
    /// so that empty string would otherwise mean *this process's* OS `cwd` --
    /// which is wrong here, since konform may be checking a project that
    /// isn't the directory it was launched from (e.g.
    /// `konform check ../other-project`). Substitute `project_root` (the
    /// root of the project actually being linted) instead of silently
    /// dropping the entry or using the wrong directory, otherwise
    /// modules/packages only reachable via that root (e.g. namespace
    /// packages rooted at the repo root, such as a `tests/mocks` directory
    /// with no `__init__.py`) are never found and get incorrectly flagged as
    /// "not a module".
    ///
    /// Also resolve each entry of `src_roots` (`config.src`, mirroring
    /// Ruff's `src` setting) relative to `project_root` and append any that
    /// exist as a directory and aren't already present. Many projects use a
    /// "src layout" (packages live under `src/` but are not installed into
    /// site-packages during local development), or a non-standard layout
    /// entirely; without this, valid local imports in such layouts can be
    /// misclassified as non-modules.
    fn resolve_sys_path(
        paths: Vec<String>,
        cwd: Option<&Path>,
        src_roots: &[String],
    ) -> Vec<PathBuf> {
        let mut resolved: Vec<PathBuf> = paths
            .into_iter()
            .filter_map(|p| {
                if p.is_empty() {
                    cwd.map(Path::to_path_buf)
                } else {
                    Some(PathBuf::from(p))
                }
            })
            .collect();

        if let Some(cwd) = cwd {
            for root in src_roots {
                let dir = if root == "." {
                    cwd.to_path_buf()
                } else {
                    cwd.join(root)
                };
                if dir.is_dir() && !resolved.iter().any(|p| p == &dir) {
                    resolved.push(dir);
                }
            }
        }

        resolved
    }

    /// Returns `true` iff `attr_name` inside `module_name` is itself a module.
    ///
    /// Example: `is_module("os", "path")` → `true` (os/path.py exists)
    ///          `is_module("os.path", "join")` → `false` (join is a function)
    ///
    /// # Thread-safety
    /// `ModuleProbe` is shared across rayon worker threads (one per checked
    /// file). This method must never write a placeholder/sentinel value to
    /// the shared `cache` before the real result is known: a concurrent
    /// call for the *same* `(module_name, attr_name)` key (e.g. two files
    /// both importing the same symbol) could observe that placeholder and
    /// return a wrong, transient answer. Only fully-resolved results are
    /// ever cached; the re-export cycle guard lives in a call-local
    /// `visiting` set instead.
    pub fn is_module(&self, module_name: &str, attr_name: &str) -> bool {
        let key = (module_name.to_string(), attr_name.to_string());
        if let Some(v) = self.cache.get(&key) {
            return *v;
        }
        let mut visiting = HashSet::new();
        self.is_module_recursive(module_name, attr_name, &mut visiting)
    }

    /// Tri-state version of [`ModuleProbe::is_module`].
    ///
    /// Distinguishes "definitely not a module" from "cannot tell, because the
    /// root package isn't installed in this environment" so callers (e.g.
    /// KIS001) can downgrade the latter to a warning instead of an error.
    pub fn check(&self, module_name: &str, attr_name: &str) -> ModuleCheck {
        if self.is_module(module_name, attr_name) {
            return ModuleCheck::Module;
        }
        if self.root_package_exists(module_name) {
            ModuleCheck::NotModule
        } else {
            ModuleCheck::Unknown
        }
    }

    /// Returns `true` iff the root package (first dotted component of
    /// `module_name`) can be found anywhere in `sys_path`, as a regular
    /// package, namespace package, plain module file, or C extension.
    ///
    /// Used to tell "the package isn't installed, so we can't validate this
    /// import" apart from "the package is installed but this name isn't a
    /// submodule of it".
    fn root_package_exists(&self, module_name: &str) -> bool {
        let root = module_name.split('.').next().unwrap_or(module_name);
        if let Some(v) = self.root_cache.get(root) {
            return *v;
        }
        let found = self.builtin_modules.contains(root)
            || self.sys_path.iter().any(|base| {
                base.join(root).is_dir()
                    || base.join(format!("{root}.py")).exists()
                    || std::fs::read_dir(base).is_ok_and(|entries| {
                        entries.flatten().any(|entry| {
                            let fname = entry.file_name();
                            let fname_str = fname.to_string_lossy();
                            fname_str.starts_with(&format!("{root}.")) && {
                                let ext = entry
                                    .path()
                                    .extension()
                                    .map(|e| e.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                matches!(ext.as_str(), "so" | "pyd" | "dylib")
                            }
                        })
                    })
            });
        self.root_cache.insert(root.to_owned(), found);
        found
    }

    /// Recursive core of [`ModuleProbe::is_module`].
    ///
    /// `visiting` guards against circular `__init__.py` re-export chains
    /// (A re-exports from B, B re-exports from A). It is local to a single
    /// top-level `is_module` call: never shared across threads or across
    /// unrelated queries: so it cannot corrupt the shared `cache`.
    fn is_module_recursive(
        &self,
        module_name: &str,
        attr_name: &str,
        visiting: &mut HashSet<(String, String)>,
    ) -> bool {
        let key = (module_name.to_owned(), attr_name.to_owned());
        if let Some(v) = self.cache.get(&key) {
            return *v;
        }
        if !visiting.insert(key.clone()) {
            // Already being resolved higher up this same call chain: it's a
            // cycle in the on-disk re-export graph, not a real module.
            return false;
        }
        let result = self.check_filesystem(module_name, attr_name, visiting);
        // Only ever cache the fully-resolved result -- concurrent readers
        // either miss (and recompute, harmlessly redundant) or see the
        // correct final answer, never a mid-computation placeholder.
        self.cache.insert(key, result);
        result
    }

    fn check_filesystem(
        &self,
        module_name: &str,
        attr_name: &str,
        visiting: &mut HashSet<(String, String)>,
    ) -> bool {
        // "myorg.utils.public" → "myorg/utils/public"
        let module_dir = module_name.replace('.', "/");

        for base in &self.sys_path {
            let parent_dir = base.join(&module_dir);

            // 1. Package directory: parent_dir/attr_name
            //    Supports both regular packages (`__init__.py`) and implicit
            //    namespace packages (directory exists without `__init__.py`).
            if parent_dir.join(attr_name).is_dir() {
                return true;
            }

            // 2. Plain module file: parent_dir/attr_name.py
            if parent_dir.join(format!("{attr_name}.py")).exists() {
                return true;
            }

            // 3. C extension: parent_dir/attr_name.*.so / .pyd / .dylib
            //    Handles: attr.so, attr.pyd, attr.cpython-310-x86_64-linux-gnu.so
            if let Ok(entries) = std::fs::read_dir(&parent_dir) {
                for entry in entries.flatten() {
                    let fname = entry.file_name();
                    let fname_str = fname.to_string_lossy();
                    // Must start with exactly `attr_name` followed by a dot.
                    if fname_str.starts_with(&format!("{attr_name}.")) {
                        let ext = entry
                            .path()
                            .extension()
                            .map(|e| e.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if matches!(ext.as_str(), "so" | "pyd" | "dylib") {
                            return true;
                        }
                    }
                }
            }

            // 4. __init__.py re-export: the package may forward-import attr_name
            //    from another location (e.g. `from myorg.utils import networking`
            //    in `myorg/utils/public/__init__.py`).  Follow that one hop.
            if self.check_init_reexport(&parent_dir, attr_name, visiting) {
                return true;
            }
        }
        false
    }

    /// Returns `true` iff `package_dir/__init__.py` contains an absolute
    /// `from X import attr_name` (or `from X import Y as attr_name`) statement
    /// where `X.Y` is itself a module.
    ///
    /// This handles packages that re-export a sub-module from another location:
    ///
    /// ```python
    /// # mypkg/__init__.py
    /// from mypkg import networking   # networking is a real module
    /// __all__ = ["networking"]
    /// ```
    ///
    /// so that `from mypkg import networking` is not incorrectly flagged by
    /// KIS001.
    fn check_init_reexport(
        &self,
        package_dir: &Path,
        attr_name: &str,
        visiting: &mut HashSet<(String, String)>,
    ) -> bool {
        let source = match std::fs::read_to_string(package_dir.join("__init__.py")) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let stmts = match parse_module(&source) {
            Ok(parsed) => parsed.into_suite(),
            Err(_) => return false,
        };
        for stmt in &stmts {
            let Stmt::ImportFrom(node) = stmt else {
                continue;
            };
            let module = match &node.module {
                Some(m) => m.as_str().to_owned(),
                None => continue, // bare `from . import X`
            };
            // Skip relative imports (level != 0).
            if node.level != 0 {
                continue;
            }
            for alias in &node.names {
                // The local name after the import (asname if present, else name).
                let local = alias
                    .asname
                    .as_deref()
                    .unwrap_or_else(|| alias.name.as_str());
                if local == attr_name {
                    // Recursively check whether the original symbol in the
                    // source module is itself a module.  `visiting` prevents
                    // infinite loops on circular imports.
                    if self.is_module_recursive(&module, alias.name.as_str(), visiting) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl Default for ModuleProbe {
    /// Convenience constructor using the system `python3`.
    /// Prefer [`ModuleProbe::new`] with [`crate::config::resolve_python`]
    /// and an explicit project root when a project config is available.
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let default_src = vec![".".to_owned(), "src".to_owned()];
        Self::new(
            Path::new(if cfg!(windows) {
                "python.exe"
            } else {
                "python3"
            }),
            &cwd,
            &default_src,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-python");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_hanging_interpreter_is_killed_and_the_probe_is_unusable() {
        let dir = TempDir::new().unwrap();
        let hang = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        assert_eq!(
            run_python_with_timeout(&hang, "print(1)", Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_output_is_returned_and_failures_are_none() {
        let dir = TempDir::new().unwrap();
        let ok = script(dir.path(), "echo '[\"a\"]'");
        assert_eq!(
            run_python(&ok, "ignored").as_deref().map(str::trim),
            Some("[\"a\"]")
        );
        let fail = script(dir.path(), "echo oops; exit 3");
        assert_eq!(run_python(&fail, "ignored"), None);
        assert_eq!(run_python(&dir.path().join("missing"), "x"), None);
    }

    /// A fake interpreter answering [`INTERPRETER_QUERY`] with `site_dir` as
    /// its only `sys.path` entry, counting its runs in `<dir>/calls`.
    #[cfg(unix)]
    fn counting_python(dir: &Path, site_dir: &Path) -> PathBuf {
        let calls = dir.join("calls");
        script(
            dir,
            &format!(
                "echo x >> '{}'\necho '{{\"sys_path\": [\"{}\"], \"builtin_modules\": [\"sys\"], \"user_site\": null}}'",
                calls.display(),
                site_dir.display()
            ),
        )
    }

    #[cfg(unix)]
    fn calls(dir: &Path) -> usize {
        fs::read_to_string(dir.join("calls")).map_or(0, |s| s.lines().count())
    }

    #[cfg(unix)]
    #[test]
    fn real_interpreter_answers_the_query() {
        let info = query_interpreter(Path::new("python3")).expect("python3 on PATH");
        assert!(info.sys_path.iter().any(|p| !p.is_empty()));
        assert!(info.builtin_modules.iter().any(|m| m == "sys"));
    }

    #[cfg(unix)]
    #[test]
    fn new_cached_reuses_the_saved_answer() {
        let dir = TempDir::new().unwrap();
        let site = dir.path().join("site");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&site).unwrap();
        fs::create_dir_all(&cache).unwrap();
        let py = counting_python(dir.path(), &site);

        let first = ModuleProbe::new_cached(&py, dir.path(), &[], &cache);
        let second = ModuleProbe::new_cached(&py, dir.path(), &[], &cache);
        assert_eq!(calls(dir.path()), 1, "second run must not start Python");
        assert_eq!(first.sys_path, second.sys_path);
        assert_eq!(first.builtin_modules, second.builtin_modules);
        assert!(!second.probe_unusable());
    }

    #[cfg(unix)]
    #[test]
    fn new_cached_requeries_when_a_sys_path_dir_changes() {
        let dir = TempDir::new().unwrap();
        let site = dir.path().join("site");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&site).unwrap();
        fs::create_dir_all(&cache).unwrap();
        let py = counting_python(dir.path(), &site);

        ModuleProbe::new_cached(&py, dir.path(), &[], &cache);
        // Installing a package changes the site directory's mtime.
        filetime::set_file_mtime(&site, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        ModuleProbe::new_cached(&py, dir.path(), &[], &cache);
        assert_eq!(calls(dir.path()), 2);
    }

    #[cfg(unix)]
    #[test]
    fn new_cached_requeries_when_the_saved_file_is_damaged() {
        let dir = TempDir::new().unwrap();
        let site = dir.path().join("site");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&site).unwrap();
        fs::create_dir_all(&cache).unwrap();
        let py = counting_python(dir.path(), &site);

        ModuleProbe::new_cached(&py, dir.path(), &[], &cache);
        for entry in fs::read_dir(&cache).unwrap() {
            fs::write(entry.unwrap().path(), "not json").unwrap();
        }
        let probe = ModuleProbe::new_cached(&py, dir.path(), &[], &cache);
        assert_eq!(calls(dir.path()), 2);
        assert!(probe.builtin_modules.contains("sys"));
    }

    #[cfg(unix)]
    #[test]
    fn new_cached_does_not_save_a_failed_query() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let py = script(dir.path(), "exit 1");

        assert!(ModuleProbe::new_cached(&py, dir.path(), &[], &cache).probe_unusable());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
    }

    #[test]
    fn module_probe_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ModuleProbe>();
    }

    /// Build a `ModuleProbe` whose `sys_path` is exactly one directory.
    fn probe_for(root: &std::path::Path) -> ModuleProbe {
        ModuleProbe {
            sys_path: vec![root.to_path_buf()],
            builtin_modules: HashSet::new(),
            cache: DashMap::new(),
            root_cache: DashMap::new(),
            probe_unusable: false,
        }
    }

    /// Build a `ModuleProbe` whose `sys_path` is exactly one directory and
    /// with a fixed set of builtin/stdlib module names, without spawning a
    /// real Python interpreter.
    fn probe_with_builtins(root: &std::path::Path, builtin_modules: &[&str]) -> ModuleProbe {
        ModuleProbe {
            sys_path: vec![root.to_path_buf()],
            builtin_modules: builtin_modules.iter().map(|s| s.to_string()).collect(),
            cache: DashMap::new(),
            root_cache: DashMap::new(),
            probe_unusable: false,
        }
    }

    #[test]
    fn direct_submodule_found() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("__init__.py"), "").unwrap();
        fs::write(pkg.join("utils.py"), "").unwrap();

        let probe = probe_for(tmp.path());
        assert!(probe.is_module("mypkg", "utils"));
        assert!(!probe.is_module("mypkg", "nonexistent"));
    }

    #[test]
    fn init_reexport_module_recognised() {
        // Simulate:
        //   mypkg/__init__.py  →  from mypkg import networking
        //   mypkg/networking.py  (the real module)
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("__init__.py"),
            "from mypkg import networking\n__all__ = ['networking']\n",
        )
        .unwrap();
        fs::write(pkg.join("networking.py"), "").unwrap();

        let probe = probe_for(tmp.path());
        // Direct check: networking is a direct child: always true.
        assert!(probe.is_module("mypkg", "networking"));
    }

    #[test]
    fn namespace_subpackage_directory_without_init_is_module() {
        // `from acme.framework.pytest.plugins.xcp import plugin` is valid when
        // `.../xcp/plugin/` exists as an implicit namespace package (no
        // `plugin/__init__.py`).
        let tmp = TempDir::new().unwrap();
        let xcp_pkg = tmp
            .path()
            .join("acme")
            .join("framework")
            .join("pytest")
            .join("plugins")
            .join("xcp");
        fs::create_dir_all(xcp_pkg.join("plugin")).unwrap();

        let probe = probe_for(tmp.path());
        assert!(probe.is_module("acme.framework.pytest.plugins.xcp", "plugin"));
        assert_eq!(
            probe.check("acme.framework.pytest.plugins.xcp", "plugin"),
            ModuleCheck::Module
        );
    }

    #[test]
    fn init_reexport_from_sibling_package() {
        // Simulate:
        //   pub/__init__.py  →  from impl_pkg import networking
        //   impl_pkg/networking.py  (module lives under a different package)
        let tmp = TempDir::new().unwrap();
        let pub_pkg = tmp.path().join("pub");
        let impl_pkg = tmp.path().join("impl_pkg");
        fs::create_dir_all(&pub_pkg).unwrap();
        fs::create_dir_all(&impl_pkg).unwrap();
        fs::write(
            pub_pkg.join("__init__.py"),
            "from impl_pkg import networking\n__all__ = ['networking']\n",
        )
        .unwrap();
        fs::write(impl_pkg.join("__init__.py"), "").unwrap();
        fs::write(impl_pkg.join("networking.py"), "").unwrap();

        let probe = probe_for(tmp.path());
        // networking lives under impl_pkg, not pub: check_init_reexport should
        // follow the import chain and return true.
        assert!(probe.is_module("pub", "networking"));
    }

    #[test]
    fn init_reexport_non_module_still_flagged() {
        // pub/__init__.py re-exports SomeClass (a class, not a module).
        let tmp = TempDir::new().unwrap();
        let pub_pkg = tmp.path().join("pub");
        let impl_pkg = tmp.path().join("impl_pkg");
        fs::create_dir_all(&pub_pkg).unwrap();
        fs::create_dir_all(&impl_pkg).unwrap();
        fs::write(
            pub_pkg.join("__init__.py"),
            "from impl_pkg.core import SomeClass\n__all__ = ['SomeClass']\n",
        )
        .unwrap();
        fs::write(impl_pkg.join("__init__.py"), "").unwrap();
        // No core.py: SomeClass is not a module anywhere
        // (impl_pkg/core.py does not exist)

        let probe = probe_for(tmp.path());
        // SomeClass is not a module -- must remain false.
        assert!(!probe.is_module("pub", "SomeClass"));
    }

    #[test]
    fn check_returns_not_module_when_package_installed() {
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("__init__.py"), "").unwrap();
        fs::write(pkg.join("utils.py"), "").unwrap();

        let probe = probe_for(tmp.path());
        assert_eq!(probe.check("mypkg", "utils"), ModuleCheck::Module);
        assert_eq!(probe.check("mypkg", "nonexistent"), ModuleCheck::NotModule);
    }

    #[test]
    fn builtin_stdlib_module_recognised_as_root_package() {
        // itertools (and other compiled-in stdlib modules) have no file or
        // directory anywhere on sys.path -- they only exist in
        // sys.stdlib_module_names / sys.builtin_module_names. Without
        // consulting that list, root_package_exists would wrongly report
        // "not installed" and check() would return Unknown instead of
        // correctly flagging `chain` as not-a-module.
        let tmp = TempDir::new().unwrap();
        let probe = probe_with_builtins(tmp.path(), &["itertools"]);
        assert_eq!(probe.check("itertools", "chain"), ModuleCheck::NotModule);
    }

    #[test]
    fn non_builtin_missing_package_still_unknown() {
        let tmp = TempDir::new().unwrap();
        let probe = probe_with_builtins(tmp.path(), &["itertools"]);
        assert_eq!(
            probe.check("not_installed_pkg", "thing"),
            ModuleCheck::Unknown
        );
    }

    #[test]
    fn check_returns_unknown_when_package_not_installed() {
        let tmp = TempDir::new().unwrap();
        let probe = probe_for(tmp.path());
        assert_eq!(probe.check("requests", "models"), ModuleCheck::Unknown);
    }

    #[test]
    fn circular_reexport_terminates() {
        // A/__init__.py re-exports x from B, B/__init__.py re-exports x from A.
        // Should not stack-overflow; returns false for both.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("__init__.py"), "from b import x\n").unwrap();
        fs::write(b.join("__init__.py"), "from a import x\n").unwrap();

        let probe = probe_for(tmp.path());
        // Neither package has x as a real file; cycle guard prevents infinite loop.
        assert!(!probe.is_module("a", "x"));
        assert!(!probe.is_module("b", "x"));
    }

    // ── env_fingerprint ────────────────────────────────────────────

    #[test]
    fn env_fingerprint_changes_when_a_sys_path_dir_is_touched() {
        let tmp = TempDir::new().unwrap();
        let probe = probe_for(tmp.path());
        let before = probe.env_fingerprint();

        // Simulate a package install/removal: bump the sys.path root's mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let new_mtime = filetime::FileTime::from_unix_time(9_999_999, 0);
        filetime::set_file_mtime(tmp.path(), new_mtime).unwrap();

        let after = probe.env_fingerprint();
        assert_ne!(
            before, after,
            "fingerprint must change when a sys.path directory's mtime changes"
        );
    }

    #[test]
    fn env_fingerprint_stable_when_nothing_changes() {
        let tmp = TempDir::new().unwrap();
        let probe = probe_for(tmp.path());
        assert_eq!(probe.env_fingerprint(), probe.env_fingerprint());
    }

    // ── resolve_sys_path (empty-string / cwd handling) ──────────────

    #[test]
    fn resolve_sys_path_substitutes_empty_string_with_cwd() {
        // Python represents cwd as "" in sys.path; this must resolve to the
        // actual cwd rather than being dropped, otherwise namespace packages
        // rooted at the repo root (e.g. `tests/mocks` with no `__init__.py`)
        // are invisible to the probe.
        let cwd = PathBuf::from("/some/repo/root");
        let raw = vec![
            "".to_owned(),
            "/usr/lib/python3.10".to_owned(),
            "/some/repo/root/src".to_owned(),
        ];
        let resolved = ModuleProbe::resolve_sys_path(raw, Some(&cwd), &[]);
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/some/repo/root"),
                PathBuf::from("/usr/lib/python3.10"),
                PathBuf::from("/some/repo/root/src"),
            ]
        );
    }

    #[test]
    fn resolve_sys_path_drops_empty_string_when_cwd_unavailable() {
        let raw = vec!["".to_owned(), "/usr/lib/python3.10".to_owned()];
        let resolved = ModuleProbe::resolve_sys_path(raw, None, &[]);
        assert_eq!(resolved, vec![PathBuf::from("/usr/lib/python3.10")]);
    }

    #[test]
    fn resolve_sys_path_adds_cwd_src_for_src_layout_projects() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join("src")).unwrap();

        let raw = vec!["".to_owned(), "/usr/lib/python3.10".to_owned()];
        let default_src = vec![".".to_owned(), "src".to_owned()];
        let resolved = ModuleProbe::resolve_sys_path(raw, Some(cwd), &default_src);

        assert_eq!(
            resolved,
            vec![
                cwd.to_path_buf(),
                PathBuf::from("/usr/lib/python3.10"),
                cwd.join("src"),
            ]
        );
    }

    #[test]
    fn cwd_namespace_package_found_via_resolved_empty_path_entry() {
        // Reproduces the real bug: `tests/mocks/mock_adb_server.py` is only
        // reachable through the cwd ("") entry of sys.path because
        // `tests/mocks` has no `__init__.py` (implicit namespace package).
        let tmp = TempDir::new().unwrap();
        let tests_dir = tmp.path().join("tests");
        let mocks_dir = tests_dir.join("mocks");
        fs::create_dir_all(&mocks_dir).unwrap();
        fs::write(tests_dir.join("__init__.py"), "").unwrap();
        // Deliberately no mocks/__init__.py.
        fs::write(mocks_dir.join("mock_adb_server.py"), "").unwrap();

        let sys_path = ModuleProbe::resolve_sys_path(vec![String::new()], Some(tmp.path()), &[]);
        let probe = ModuleProbe {
            sys_path,
            builtin_modules: HashSet::new(),
            cache: DashMap::new(),
            root_cache: DashMap::new(),
            probe_unusable: false,
        };
        assert!(probe.is_module("tests.mocks", "mock_adb_server"));
    }

    #[test]
    fn new_resolves_cwd_entry_against_project_root_not_process_cwd() {
        // End-to-end regression test for the real bug: `ModuleProbe::new`
        // must resolve sys.path's cwd ("") entry against the project being
        // linted (`project_root`), not wherever the konform process itself
        // happens to be running from (`std::env::current_dir()`).
        //
        // The test process's actual cwd is the crate root, deliberately
        // different from `tmp` here -- if `new`/`get_sys_path` regress to
        // using `std::env::current_dir()` internally, this namespace
        // package would no longer be found and the test fails.
        let tmp = TempDir::new().unwrap();
        let tests_dir = tmp.path().join("tests");
        let mocks_dir = tests_dir.join("mocks");
        fs::create_dir_all(&mocks_dir).unwrap();
        fs::write(tests_dir.join("__init__.py"), "").unwrap();
        // Deliberately no mocks/__init__.py: implicit namespace package,
        // only discoverable via the resolved cwd entry.
        fs::write(mocks_dir.join("mock_adb_server.py"), "").unwrap();

        assert_ne!(
            std::env::current_dir().unwrap(),
            tmp.path(),
            "test is only meaningful if the process cwd differs from project_root"
        );

        let python = if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        };
        let default_src = vec![".".to_owned(), "src".to_owned()];
        let probe = ModuleProbe::new(Path::new(python), tmp.path(), &default_src);
        assert!(probe.is_module("tests.mocks", "mock_adb_server"));
    }

    #[test]
    fn src_layout_project_module_is_found() {
        // Reproduces a src-layout false positive:
        //   from acme.framework.pytest.plugins.xcp import plugin
        // where code lives under src/acme/... and is not installed.
        let tmp = TempDir::new().unwrap();
        let xcp_dir = tmp
            .path()
            .join("src")
            .join("acme")
            .join("framework")
            .join("pytest")
            .join("plugins")
            .join("xcp");
        fs::create_dir_all(&xcp_dir).unwrap();
        fs::write(xcp_dir.join("__init__.py"), "").unwrap();
        fs::write(xcp_dir.join("plugin.py"), "").unwrap();

        let sys_path = ModuleProbe::resolve_sys_path(
            vec![String::new()],
            Some(tmp.path()),
            &[".".to_owned(), "src".to_owned()],
        );
        let probe = ModuleProbe {
            sys_path,
            builtin_modules: HashSet::new(),
            cache: DashMap::new(),
            root_cache: DashMap::new(),
            probe_unusable: false,
        };

        assert!(probe.is_module("acme.framework.pytest.plugins.xcp", "plugin"));
        assert_eq!(
            probe.check("acme.framework.pytest.plugins.xcp", "plugin"),
            ModuleCheck::Module
        );
    }

    // ── concurrency ─────────────────────────────────────────────────

    #[test]
    fn arbitrary_configured_src_root_is_honoured() {
        let tmp = TempDir::new().unwrap();
        let pkg_dir = tmp.path().join("lib").join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("__init__.py"), "").unwrap();
        fs::write(pkg_dir.join("utils.py"), "").unwrap();

        let sys_path = ModuleProbe::resolve_sys_path(
            vec![String::new()],
            Some(tmp.path()),
            &["lib".to_owned()],
        );
        let probe = ModuleProbe {
            sys_path,
            builtin_modules: HashSet::new(),
            cache: DashMap::new(),
            root_cache: DashMap::new(),
            probe_unusable: false,
        };

        assert!(probe.is_module("mypkg", "utils"));
    }

    #[test]
    fn concurrent_queries_for_the_same_key_never_see_a_placeholder() {
        // Regression test for a data race in the previous implementation:
        // `is_module` inserted a `false` *sentinel* into the shared cache
        // before computing the real result, so a concurrent call for the
        // exact same (module, attr) key (e.g. two files both importing
        // `acme.sample_pkg.controller`, checked on different rayon worker
        // threads) could observe that sentinel and wrongly report "not a
        // module". This spins up many threads hammering the same key and
        // asserts every single one gets the correct answer.
        let tmp = TempDir::new().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("__init__.py"), "").unwrap();
        fs::write(pkg.join("controller.py"), "").unwrap();

        let probe = std::sync::Arc::new(probe_for(tmp.path()));
        let mut handles = Vec::new();
        for _ in 0..64 {
            let probe = std::sync::Arc::clone(&probe);
            handles.push(std::thread::spawn(move || {
                probe.is_module("mypkg", "controller")
            }));
        }
        for h in handles {
            assert!(
                h.join().unwrap(),
                "every concurrent caller must see the correct (true) result, \
                 never a transient sentinel"
            );
        }
    }
}
