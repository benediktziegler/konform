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
use std::path::{Path, PathBuf};
use std::process::Command;

/// Caches `(module_name, attr_name) -> is_module` probe results.
pub struct ModuleProbe {
    /// Ordered list of directories to search, from `python3 -c "import sys,json; print(json.dumps(sys.path))"`.
    sys_path: Vec<PathBuf>,
    /// In-process result cache.
    cache: DashMap<(String, String), bool>,
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
        }
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
        Some(
            paths
                .into_iter()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect(),
        )
    }

    /// Returns `true` iff `attr_name` inside `module_name` is itself a module.
    ///
    /// Example: `is_module("os", "path")` → `true` (os/path.py exists)
    ///          `is_module("os.path", "join")` → `false` (join is a function)
    pub fn is_module(&self, module_name: &str, attr_name: &str) -> bool {
        let key = (module_name.to_string(), attr_name.to_string());
        if let Some(v) = self.cache.get(&key) {
            return *v;
        }
        // Insert a `false` sentinel before recursing so that circular
        // re-export chains (A re-exports from B, B re-exports from A) terminate
        // rather than stack-overflow.
        self.cache.insert(key.clone(), false);
        let result = self.check_filesystem(module_name, attr_name);
        self.cache.insert(key, result);
        result
    }

    fn check_filesystem(&self, module_name: &str, attr_name: &str) -> bool {
        // "myorg.utils.public" → "myorg/utils/public"
        let module_dir = module_name.replace('.', "/");

        for base in &self.sys_path {
            let parent_dir = base.join(&module_dir);

            // 1. Package directory: parent_dir/attr_name/__init__.py
            if parent_dir.join(attr_name).join("__init__.py").exists() {
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
            if self.check_init_reexport(&parent_dir, attr_name) {
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
    fn check_init_reexport(&self, package_dir: &Path, attr_name: &str) -> bool {
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
                let local = alias.asname.as_deref().unwrap_or_else(|| alias.name.as_str());
                if local == attr_name {
                    // Recursively check whether the original symbol in the
                    // source module is itself a module.  The sentinel in
                    // `is_module` prevents infinite loops on circular imports.
                    if self.is_module(&module, alias.name.as_str()) {
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
        // SomeClass is not a module — must remain false.
        assert!(!probe.is_module("pub", "SomeClass"));
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
}
