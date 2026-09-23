use crate::types::Level;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// LintConfig
// ---------------------------------------------------------------------------

/// `[tool.konform.lint]` — rule selection, suppression, and per-rule settings.
///
/// Mirrors Ruff's `[tool.ruff.lint]` split: project-wide settings
/// (interpreter, cache, search roots) live directly under `[tool.konform]`,
/// while everything about *which rules run and how* lives here.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LintConfig {
    // ── Rule selection (prefix-matched, empty = all rules enabled) ────────
    pub select: Vec<String>,
    pub ignore: Vec<String>,

    // ── Global default level ───────────────────────────────────────────────
    pub level: Level,

    /// Per-file rule overrides: each key is a glob pattern, the value is a
    /// list of rule codes / category prefixes to suppress for matching files.
    ///
    /// Populated from `[tool.konform.lint] per-file-ignores = {"tests/**" = ["KIS001"]}`
    /// or from `--per-file-ignores` CLI flags.
    pub per_file_ignores: HashMap<String, Vec<String>>,

    /// Alias `# noqa` codes to canonical rule codes, to support migrating
    /// away from an old rule/category name without breaking existing
    /// suppression comments.
    ///
    /// Populated from `[tool.konform.lint] noqa-aliases = {"IS001" = "KIS001"}`.
    /// A `# noqa: IS001` comment then suppresses `KIS001` violations.
    /// Aliases may also target a category prefix (e.g. `"IS" = "KIS"`) to
    /// alias an entire category at once.
    pub noqa_aliases: HashMap<String, String>,

    /// Every other table-valued key under `[tool.konform.lint]`, keyed by
    /// [`crate::rules::Rule::config_name`] (e.g. `"module-only-imports"`,
    /// `"user-defined-patterns"`). Passed verbatim to `Rule::check` / `Rule::fix`.
    #[serde(flatten)]
    pub rules: HashMap<String, toml::Value>,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            select: vec![],
            ignore: vec![],
            level: Level::Error,
            per_file_ignores: HashMap::new(),
            noqa_aliases: HashMap::new(),
            rules: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    // ── Global defaults ───────────────────────────────────────────────────
    pub cache_dir: String,
    pub workers: usize,

    // ── Rule selection / configuration ──────────────────────────────────────
    pub lint: LintConfig,

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

    // ── Module-probe search roots ──────────────────────────────────────────
    /// Directories (relative to `config_dir`) to search when resolving
    /// whether an imported name is a first-party module/package (used by
    /// KIS001's `ModuleProbe`).
    ///
    /// Mirrors Ruff's `src` setting exactly, including its resolution order:
    /// `[tool.konform] src = [...]` takes precedence; if absent, falls back
    /// to `[tool.ruff] src = [...]` (so projects that already configured
    /// Ruff for a non-standard layout don't have to repeat themselves); if
    /// neither is set, defaults to `[".", "src"]`.
    #[serde(skip)]
    pub src: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_dir: ".konform_cache".into(),
            workers: 0,
            lint: LintConfig::default(),
            python: None,
            config_dir: None,
            ignore_noqa: false,
            src: vec![".".to_owned(), "src".to_owned()],
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
    /// [tool.konform.lint]
    /// select = ["KIS"]   # run all KIS* rules
    /// ignore = ["KIS001"] # except KIS001 specifically
    /// ```
    pub fn is_enabled(&self, code: &str) -> bool {
        let selected = self.lint.select.is_empty()
            || self
                .lint
                .select
                .iter()
                .any(|s| code.starts_with(s.as_str()));
        let ignored = self
            .lint
            .ignore
            .iter()
            .any(|i| code.starts_with(i.as_str()));
        selected && !ignored
    }

    /// Return the raw TOML configuration blob for a rule, looked up by its
    /// [`crate::rules::Rule::config_name`].
    ///
    /// For example, calling `rule_config("module-only-imports")` returns the
    /// parsed `[tool.konform.lint.module-only-imports]` section so the rule
    /// can read its own settings. Returns an empty table when the section is
    /// absent.
    pub fn rule_config(&self, config_name: &str) -> &toml::Value {
        static EMPTY: OnceLock<toml::Value> = OnceLock::new();
        self.lint
            .rules
            .get(config_name)
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

    // ── Config migration ────────────────────────────────────────────────────
    // Auto-upgrade an outdated config shape (e.g. the pre-0.3 flat/category
    // format) before parsing. Rewritten with `toml_edit` so comments and
    // formatting elsewhere in the file (other `[tool.*]` sections) survive.
    let content = match crate::migrations::migrate_content(&path, &content) {
        Some((migrated, reports)) => {
            if std::fs::write(&path, &migrated).is_ok() {
                eprintln!(
                    "konform: migrated {} to the current config format:",
                    path.display()
                );
                for report in &reports {
                    eprintln!("  [{}] {}", report.id, report.description);
                    for note in &report.notes {
                        eprintln!("    - {note}");
                    }
                }
            }
            migrated
        }
        None => content,
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

    let mut cfg = Config::deserialize(section.clone()).unwrap_or_default();
    cfg.config_dir = config_dir;

    // ── Module-probe search roots (mirrors Ruff's `src` resolution order) ───
    // `src` is intentionally not part of the typed `Config` struct above: it
    // must fall back to `[tool.ruff] src` only when *absent*, which a plain
    // `#[serde(default)]` field can't distinguish from "present but empty".
    let str_array = |v: &toml::Value| -> Option<Vec<String>> {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str())
                .map(str::to_owned)
                .collect()
        })
    };
    if let Some(v) = section.get("src").and_then(str_array) {
        cfg.src = v;
    } else if let Some(v) = raw
        .get("tool")
        .and_then(|t| t.get("ruff"))
        .and_then(|r| r.get("src"))
        .and_then(str_array)
    {
        cfg.src = v;
    } else {
        cfg.src = Config::default().src;
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
            lint: LintConfig {
                select: vec!["KIS".into()],
                ..LintConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.is_enabled("KIS001"));
        assert!(!cfg.is_enabled("KPT001"));
    }

    #[test]
    fn ignore_prefix_disables_matching() {
        let cfg = Config {
            lint: LintConfig {
                ignore: vec!["KIS".into()],
                ..LintConfig::default()
            },
            ..Config::default()
        };
        assert!(!cfg.is_enabled("KIS001"));
        assert!(cfg.is_enabled("KPT001"));
    }

    #[test]
    fn exact_ignore_beats_category_select() {
        let cfg = Config {
            lint: LintConfig {
                select: vec!["KIS".into()],
                ignore: vec!["KIS001".into()],
                ..LintConfig::default()
            },
            ..Config::default()
        };
        assert!(!cfg.is_enabled("KIS001"));
        assert!(cfg.is_enabled("KIS002")); // hypothetical second KIS rule
    }

    #[test]
    fn rule_config_returns_empty_for_unknown_rule() {
        let cfg = Config::default();
        let val = cfg.rule_config("unknown-rule");
        assert!(val.as_table().is_some_and(|t| t.is_empty()));
    }

    #[test]
    fn rule_config_returns_section_when_present() {
        let mut cfg = Config::default();
        let mut table = toml::map::Map::new();
        table.insert("level".into(), toml::Value::String("warning".into()));
        cfg.lint
            .rules
            .insert("module-only-imports".into(), toml::Value::Table(table));

        let val = cfg.rule_config("module-only-imports");
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
            "[tool.konform.lint]\nper-file-ignores = {\"tests/**\" = [\"KIS001\", \"KPT\"]}",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        let codes = cfg
            .lint
            .per_file_ignores
            .get("tests/**")
            .expect("glob missing");
        assert!(codes.contains(&"KIS001".to_owned()));
        assert!(codes.contains(&"KPT".to_owned()));
    }

    #[test]
    fn per_file_ignores_not_inserted_into_rules_map() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform.lint]\nper-file-ignores = {\"tests/**\" = [\"KIS001\"]}",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert!(
            !cfg.lint.rules.contains_key("per-file-ignores"),
            "per-file-ignores must not be in the rules map"
        );
    }

    #[test]
    fn noqa_aliases_parsed_from_pyproject() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform.lint]\nnoqa-aliases = {\"IS001\" = \"KIS001\", \"IS\" = \"KIS\"}",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert_eq!(
            cfg.lint.noqa_aliases.get("IS001").map(String::as_str),
            Some("KIS001")
        );
        assert_eq!(
            cfg.lint.noqa_aliases.get("IS").map(String::as_str),
            Some("KIS")
        );
    }

    #[test]
    fn noqa_aliases_not_inserted_into_rules_map() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform.lint]\nnoqa-aliases = {\"IS001\" = \"KIS001\"}",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert!(
            !cfg.lint.rules.contains_key("noqa-aliases"),
            "noqa-aliases must not be in the rules map"
        );
    }

    #[test]
    fn rule_config_table_parsed_from_pyproject() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform.lint.module-only-imports]\nlevel = \"warning\"\n",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        let val = cfg.rule_config("module-only-imports");
        assert_eq!(val.get("level").and_then(|v| v.as_str()), Some("warning"));
    }

    #[test]
    fn src_defaults_to_dot_and_src_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(&pyproject, "[tool.konform.lint]\nlevel = \"error\"").unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert_eq!(cfg.src, vec![".".to_owned(), "src".to_owned()]);
    }

    #[test]
    fn src_read_from_tool_konform() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(&pyproject, "[tool.konform]\nsrc = [\"lib\"]").unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert_eq!(cfg.src, vec!["lib".to_owned()]);
    }

    #[test]
    fn src_falls_back_to_tool_ruff_when_konform_src_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform.lint]\nlevel = \"error\"\n\n[tool.ruff]\nsrc = [\"lib\", \"test\"]",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert_eq!(cfg.src, vec!["lib".to_owned(), "test".to_owned()]);
    }

    #[test]
    fn src_prefers_tool_konform_over_tool_ruff() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform]\nsrc = [\"lib\"]\n\n[tool.ruff]\nsrc = [\"should-not-be-used\"]",
        )
        .unwrap();
        let cfg = load_config(Some(tmp.path()), None);
        assert_eq!(cfg.src, vec!["lib".to_owned()]);
    }

    #[test]
    fn old_format_pyproject_is_migrated_on_load_and_rewritten_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = tmp.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[tool.konform]\nselect = [\"KIS\"]\n\n[tool.konform.KIS]\nunresolved_level = \"error\"\n",
        )
        .unwrap();

        let cfg = load_config(Some(tmp.path()), None);
        assert_eq!(cfg.lint.select, vec!["KIS".to_owned()]);
        let kis = cfg.rule_config("module-only-imports");
        assert_eq!(
            kis.get("unresolved-level").and_then(|v| v.as_str()),
            Some("error")
        );

        // The file on disk should now be in the new shape.
        let rewritten = std::fs::read_to_string(&pyproject).unwrap();
        assert!(rewritten.contains("[tool.konform.lint]"));
        assert!(rewritten.contains("[tool.konform.lint.module-only-imports]"));
    }
}
