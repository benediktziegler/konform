use crate::types::Level;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    // ── Rule selection (prefix-matched, empty = all rules enabled) ────────
    pub select: Vec<String>,
    pub ignore: Vec<String>,

    // ── Global defaults ───────────────────────────────────────────────────
    pub level: Level,
    pub cache_dir: String,
    pub workers: usize,

    // ── Per-category raw TOML blobs ───────────────────────────────────────
    /// `rules["KIS"]` contains the parsed `[tool.konform.KIS]` section.
    /// Passed verbatim to `Rule::check` / `Rule::fix` as `cfg`.
    #[serde(skip)]
    pub rules: HashMap<String, toml::Value>,

    // ── Python interpreter for module probing ─────────────────────────────
    /// Explicit path to the Python interpreter.
    /// When `None`, `resolve_python` auto-discovers the project venv.
    pub python: Option<String>,

    // ── Config file location ──────────────────────────────────────────────
    /// Directory that contains `pyproject.toml` / `konform.toml`.
    /// Used by `resolve_python` and by rule engines that load extra files
    /// relative to the project root (e.g. `konform_patterns.toml`).
    #[serde(skip)]
    pub config_dir: Option<PathBuf>,

    // ── Runtime overrides (set from CLI flags, never from config file) ────
    /// When `true`, all `# noqa` suppression comments are ignored.
    /// Set by `konform check --ignore-noqa`.
    #[serde(skip)]
    pub ignore_noqa: bool,

    /// Per-file rule overrides: each key is a glob pattern, the value is a
    /// list of rule codes / category prefixes to suppress for matching files.
    ///
    /// Populated from `[tool.konform] per_file_ignores = {"tests/**" = ["KIS001"]}`
    /// or from `--per-file-ignores` CLI flags.
    #[serde(skip)]
    pub per_file_ignores: HashMap<String, Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            select: vec![],
            ignore: vec![],
            level: Level::Error,
            cache_dir: ".konform_cache".into(),
            workers: 0,
            rules: HashMap::new(),
            python: None,
            config_dir: None,
            ignore_noqa: false,
            per_file_ignores: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Rule selection helpers
// ---------------------------------------------------------------------------

impl Config {
    /// Return `true` when the rule identified by `code` should run.
    ///
    /// Both `select` and `ignore` use **prefix matching** so entire
    /// categories can be toggled with a short token:
    ///
    /// ```toml
    /// [tool.konform]
    /// select = ["KIS"]   # run all KIS* rules
    /// ignore = ["KIS001"] # except KIS001 specifically
    /// ```
    pub fn is_enabled(&self, code: &str) -> bool {
        let selected =
            self.select.is_empty() || self.select.iter().any(|s| code.starts_with(s.as_str()));
        let ignored = self.ignore.iter().any(|i| code.starts_with(i.as_str()));
        selected && !ignored
    }

    /// Return the raw TOML configuration blob for a rule category.
    ///
    /// For example, calling `rule_config("KIS")` returns the parsed
    /// `[tool.konform.KIS]` section so the rule can read its own settings.
    /// Returns an empty table when the section is absent.
    pub fn rule_config(&self, category: &str) -> &toml::Value {
        static EMPTY: OnceLock<toml::Value> = OnceLock::new();
        self.rules
            .get(category)
            .unwrap_or_else(|| EMPTY.get_or_init(|| toml::Value::Table(Default::default())))
    }
}

// ---------------------------------------------------------------------------
// Config discovery and loading
// ---------------------------------------------------------------------------

/// Walk upward from `start` looking for `konform.toml` first, then `pyproject.toml`.
pub fn find_config_file(start: &Path) -> Option<PathBuf> {
    let start = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    for dir in start.ancestors() {
        for name in ["konform.toml", "pyproject.toml"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

pub fn load_config(start: Option<&Path>, explicit_path: Option<&Path>) -> Config {
    let path = explicit_path
        .map(|p| p.to_path_buf())
        .or_else(|| start.and_then(find_config_file));

    let path = match path {
        Some(p) => p,
        None => return Config::default(),
    };

    let config_dir = path.parent().map(Path::to_path_buf);

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return Config {
                config_dir,
                ..Config::default()
            }
        }
    };

    let raw: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            return Config {
                config_dir,
                ..Config::default()
            }
        }
    };

    // Extract [tool.konform] or top-level [konform] section.
    let section = if path.file_name().is_some_and(|n| n == "pyproject.toml") {
        raw.get("tool").and_then(|t| t.get("konform")).cloned()
    } else {
        raw.get("konform").cloned().or(Some(raw.clone()))
    };

    let section = match section {
        Some(s) => s,
        None => {
            return Config {
                config_dir,
                ..Config::default()
            }
        }
    };

    let mut cfg = Config {
        config_dir,
        ..Config::default()
    };

    // ── Rule selection ─────────────────────────────────────────────────────
    if let Some(v) = section.get("select").and_then(|v| v.as_array()) {
        cfg.select = v
            .iter()
            .filter_map(|e| e.as_str())
            .map(str::to_owned)
            .collect();
    }
    if let Some(v) = section.get("ignore").and_then(|v| v.as_array()) {
        cfg.ignore = v
            .iter()
            .filter_map(|e| e.as_str())
            .map(str::to_owned)
            .collect();
    }

    // ── Per-file-ignores ────────────────────────────────────────────
    if let Some(table) = section.get("per_file_ignores").and_then(|v| v.as_table()) {
        for (glob, codes_val) in table {
            if let Some(arr) = codes_val.as_array() {
                let codes: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect();
                cfg.per_file_ignores.insert(glob.clone(), codes);
            }
        }
    }

    // ── Global defaults ────────────────────────────────────────────────────
    if let Some(v) = section.get("level").and_then(|v| v.as_str()) {
        if let Ok(l) = v.parse::<Level>() {
            cfg.level = l;
        }
    }
    if let Some(v) = section.get("cache_dir").and_then(|v| v.as_str()) {
        cfg.cache_dir = v.to_owned();
    }
    if let Some(v) = section.get("workers").and_then(|v| v.as_integer()) {
        cfg.workers = v.max(0) as usize;
    }

    // ── Python interpreter ─────────────────────────────────────────────────
    if let Some(v) = section.get("python").and_then(|v| v.as_str()) {
        cfg.python = Some(v.to_owned());
    }

    // ── Per-category subtables → rules map ────────────────────────────────────
    // Every table-valued key in [tool.konform] is treated as a rule-category
    // config blob (KIS, KPT, …).  Scalar keys are the global settings above.
    if let toml::Value::Table(table) = &section {
        for (key, val) in table {
            if key == "per_file_ignores" {
                continue; // Already parsed above.
            }
            if matches!(val, toml::Value::Table(_)) {
                cfg.rules.insert(key.clone(), val.clone());
            }
        }
    }

    cfg
}

// ---------------------------------------------------------------------------
// Python interpreter discovery
// ---------------------------------------------------------------------------

/// Return the path to the Python interpreter to use for `sys.path` probing,
/// by checking common virtual-environment locations under `project_root`.
///
/// Discovery order:
/// 1. `.venv/bin/python3`  — hatch (`path = ".venv"`), uv, plain `python -m venv`
/// 2. `venv/bin/python3`   — common alternative name
/// 3. `.env/bin/python3`   — another common name
/// 4. `python3` on `$PATH` — system fallback
///
/// On Windows `bin/` is replaced by `Scripts/` and `python3` by `python.exe`.
pub fn discover_python(project_root: &Path) -> PathBuf {
    let (subdir, binary) = if cfg!(windows) {
        ("Scripts", "python.exe")
    } else {
        ("bin", "python3")
    };

    for venv in [".venv", "venv", ".env"] {
        let candidate = project_root.join(venv).join(subdir).join(binary);
        if candidate.is_file() {
            return candidate;
        }
    }

    PathBuf::from(binary)
}

/// Resolve the Python interpreter to use for module probing.
///
/// Priority:
/// 1. `[tool.konform] python = "…"` — explicit config
/// 2. Auto-discovered venv in `config_dir` (see [`discover_python`])
/// 3. `python3` / `python.exe` on `$PATH`
pub fn resolve_python(config: &Config) -> PathBuf {
    if let Some(explicit) = &config.python {
        return PathBuf::from(explicit);
    }
    if let Some(dir) = &config.config_dir {
        return discover_python(dir);
    }
    PathBuf::from(if cfg!(windows) {
        "python.exe"
    } else {
        "python3"
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_all_rules_enabled() {
        let cfg = Config::default();
        assert!(cfg.is_enabled("KIS001"));
        assert!(cfg.is_enabled("KPT001"));
    }

    #[test]
    fn select_prefix_enables_only_matching() {
        let cfg = Config {
            select: vec!["KIS".into()],
            ..Config::default()
        };
        assert!(cfg.is_enabled("KIS001"));
        assert!(!cfg.is_enabled("KPT001"));
    }

    #[test]
    fn ignore_prefix_disables_matching() {
        let cfg = Config {
            ignore: vec!["KIS".into()],
            ..Config::default()
        };
        assert!(!cfg.is_enabled("KIS001"));
        assert!(cfg.is_enabled("KPT001"));
    }

    #[test]
    fn exact_ignore_beats_category_select() {
        let cfg = Config {
            select: vec!["KIS".into()],
            ignore: vec!["KIS001".into()],
            ..Config::default()
        };
        assert!(!cfg.is_enabled("KIS001"));
        assert!(cfg.is_enabled("KIS002")); // hypothetical second KIS rule
    }

    #[test]
    fn rule_config_returns_empty_for_unknown_category() {
        let cfg = Config::default();
        let val = cfg.rule_config("UNKNOWN");
        assert!(val.as_table().is_some_and(|t| t.is_empty()));
    }

    #[test]
    fn rule_config_returns_section_when_present() {
        let mut cfg = Config::default();
        let mut table = toml::map::Map::new();
        table.insert("level".into(), toml::Value::String("warning".into()));
        cfg.rules.insert("KIS".into(), toml::Value::Table(table));

        let val = cfg.rule_config("KIS");
        assert_eq!(val.get("level").and_then(|v| v.as_str()), Some("warning"));
    }

    #[test]
    fn discover_python_finds_venv() {
        // Use a temp dir so the test is hermetic.
        let tmp = tempfile::tempdir().unwrap();
        let bin = if cfg!(windows) { "Scripts" } else { "bin" };
        let exe = if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        };

        let venv_bin = tmp.path().join(".venv").join(bin);
        std::fs::create_dir_all(&venv_bin).unwrap();
        let py = venv_bin.join(exe);
        std::fs::write(&py, "").unwrap();

        let found = discover_python(tmp.path());
        assert_eq!(found, py);
    }

    #[test]
    fn discover_python_falls_back_to_system() {
        let tmp = tempfile::tempdir().unwrap();
        let found = discover_python(tmp.path());
        let expected = if cfg!(windows) {
            "python.exe"
        } else {
            "python3"
        };
        assert_eq!(found, PathBuf::from(expected));
    }

    #[test]
    fn per_file_ignores_parsed_from_pyproject() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform]\nper_file_ignores = {\"tests/**\" = [\"KIS001\", \"KPT\"]}",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        let codes = cfg.per_file_ignores.get("tests/**").expect("glob missing");
        assert!(codes.contains(&"KIS001".to_owned()));
        assert!(codes.contains(&"KPT".to_owned()));
    }

    #[test]
    fn per_file_ignores_not_inserted_into_rules_map() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform]\nper_file_ignores = {\"tests/**\" = [\"KIS001\"]}",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert!(
            !cfg.rules.contains_key("per_file_ignores"),
            "per_file_ignores must not be in the rules map"
        );
    }
}
