//! Filesystem-based Python module existence probe.
//!
//! Asks the Python interpreter for `sys.path` once at startup, then answers
//! every `(module_name, attr_name)` query purely via filesystem look-ups —
//! no code execution, no pyo3, no `__import__`.
//!
//! Results are cached in a [`dashmap::DashMap`] for the lifetime of the
//! process.

use dashmap::DashMap;
use ruff_python_ast::Stmt;
use ruff_python_parser::parse_module;
use seahash::SeaHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// be found anywhere in `sys.path` — e.g. it isn't installed in this
    /// Python environment. There is no way to tell whether `attr_name` would
    /// be a module or not, so callers should treat this as "unknown" rather
    /// than a definitive violation.
    Unknown,
}

/// Caches `(module_name, attr_name) -> is_module` probe results.
pub struct ModuleProbe {
    /// Ordered list of directories to search, from `python3 -c "import sys,json; print(json.dumps(sys.path))"`.
    sys_path: Vec<PathBuf>,
    /// In-process result cache.
    cache: DashMap<(String, String), bool>,
    /// Caches whether a root package name exists anywhere in `sys_path`.
    root_cache: DashMap<String, bool>,
}

impl ModuleProbe {
    /// Create a new probe using `python` as the interpreter for `sys.path` discovery.
    ///
    /// Pass the result of [`crate::config::resolve_python`] here so the probe
    /// searches the same environment that owns the files being checked.
    pub fn new(python: &Path) -> Self {
        let sys_path = Self::get_sys_path(python).unwrap_or_default();
        Self {
            sys_path,
            cache: DashMap::new(),
            root_cache: DashMap::new(),
        }
    }

    /// Fingerprint of the Python environment this probe searches.
    ///
    /// Hashes each `sys.path` directory's own mtime (not a recursive scan).
    /// Installing, upgrading, or removing a package touches the mtime of the
    /// directory that contains it (site-packages, a namespace-package root,
    /// etc.), so this changes whenever the resolvable module set changes —
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

    fn get_sys_path(python: &Path) -> Option<Vec<PathBuf>> {
        let output = Command::new(python)
            .args(["-c", "import json,sys; print(json.dumps(sys.path))"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8(output.stdout).ok()?;
        let paths: Vec<String> = serde_json::from_str(stdout.trim()).ok()?;
        let cwd = std::env::current_dir().ok();
        Some(Self::resolve_sys_path(paths, cwd.as_deref()))
    }

    /// Resolve the raw `sys.path` strings returned by the interpreter into
    /// filesystem paths.
    ///
    /// Python represents "current working directory" as an empty string in
    /// `sys.path` (e.g. `sys.path[0]` for `python -c "..."` / interactive
    /// use). Since `python` is spawned without overriding its working
    /// directory, that empty string means *this process's* `cwd` — resolve
    /// it rather than silently dropping it, otherwise modules/packages only
    /// reachable via cwd (e.g. namespace packages rooted at the repo root,
    /// such as a `tests/mocks` directory with no `__init__.py`) are never
    /// found and get incorrectly flagged as "not a module".
    ///
    /// Also include `<cwd>/src` when present. Many projects use a "src layout"
    /// (packages live under `src/` but are not installed into site-packages
    /// during local development). Without this, valid local imports can be
    /// misclassified as non-modules.
    fn resolve_sys_path(paths: Vec<String>, cwd: Option<&Path>) -> Vec<PathBuf> {
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
            let src = cwd.join("src");
            if src.is_dir() && !resolved.iter().any(|p| p == &src) {
                resolved.push(src);
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
        let found = self.sys_path.iter().any(|base| {
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
    /// top-level `is_module` call — never shared across threads or across
    /// unrelated queries — so it cannot corrupt the shared `cache`.
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
    /// when a project config is available.
    fn default() -> Self {
        Self::new(Path::new(if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        }))
    }
}

// SAFETY: DashMap<K,V> is Send + Sync when K and V are Send + Sync.
// (String, String) and bool are both Send + Sync.
// Vec<PathBuf> is Send + Sync.
// The unsafe impls are needed because the compiler cannot see through the
// DashMap newtype when inferring auto-traits for the struct.
unsafe impl Send for ModuleProbe {}
unsafe impl Sync for ModuleProbe {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a `ModuleProbe` whose `sys_path` is exactly one directory.
    fn probe_for(root: &std::path::Path) -> ModuleProbe {
        ModuleProbe {
            sys_path: vec![root.to_path_buf()],
            cache: DashMap::new(),
            root_cache: DashMap::new(),
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
        // Direct check: networking is a direct child — always true.
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
        // networking lives under impl_pkg, not pub — check_init_reexport should
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
        // No core.py — SomeClass is not a module anywhere
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
        let resolved = ModuleProbe::resolve_sys_path(raw, Some(&cwd));
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
        let resolved = ModuleProbe::resolve_sys_path(raw, None);
        assert_eq!(resolved, vec![PathBuf::from("/usr/lib/python3.10")]);
    }

    #[test]
    fn resolve_sys_path_adds_cwd_src_for_src_layout_projects() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join("src")).unwrap();

        let raw = vec!["".to_owned(), "/usr/lib/python3.10".to_owned()];
        let resolved = ModuleProbe::resolve_sys_path(raw, Some(cwd));

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

        let sys_path = ModuleProbe::resolve_sys_path(vec![String::new()], Some(tmp.path()));
        let probe = ModuleProbe {
            sys_path,
            cache: DashMap::new(),
            root_cache: DashMap::new(),
        };
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

        let sys_path = ModuleProbe::resolve_sys_path(vec![String::new()], Some(tmp.path()));
        let probe = ModuleProbe {
            sys_path,
            cache: DashMap::new(),
            root_cache: DashMap::new(),
        };

        assert!(probe.is_module("acme.framework.pytest.plugins.xcp", "plugin"));
        assert_eq!(
            probe.check("acme.framework.pytest.plugins.xcp", "plugin"),
            ModuleCheck::Module
        );
    }

    // ── concurrency ─────────────────────────────────────────────────

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
