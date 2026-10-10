use crate::types::Level;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// LintConfig
// ---------------------------------------------------------------------------

/// `[tool.konform.lint]`: rule selection, suppression, and per-rule settings.
///
/// Mirrors Ruff's `[tool.ruff.lint]` split: project-wide settings
/// (interpreter, cache, search roots) live directly under `[tool.konform]`,
/// while everything about *which rules run and how* lives here.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LintConfig {
    // ── Rule selection (prefix-matched, empty = all rules enabled) ────────
    pub select: Vec<String>,
    /// Codes / prefixes enabled *in addition to* the default set. This is
    /// how opt-in rules (see [`crate::rules::Rule::opt_in`]) are turned on
    /// without narrowing the rules that already run.
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,

    // ── Fix selection (prefix-matched, empty `fixable` = every code) ──────
    /// Codes / prefixes whose violations may be auto-fixed.
    pub fixable: Vec<String>,
    /// Codes / prefixes that are never auto-fixed, even if `fixable` matches.
    pub unfixable: Vec<String>,

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
            extend_select: vec![],
            ignore: vec![],
            fixable: vec![],
            unfixable: vec![],
            level: Level::Error,
            per_file_ignores: HashMap::new(),
            noqa_aliases: HashMap::new(),
            rules: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// RuleSelection
// ---------------------------------------------------------------------------

/// The `select` / `ignore` lists, prefix-matched against rule codes.
///
/// The default (all empty) enables every code except opt-in rules, which
/// need an explicit match in `select` or `extend_select`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSelection {
    pub select: Vec<String>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
}

impl RuleSelection {
    fn explicitly_selected(&self, code: &str) -> bool {
        self.select
            .iter()
            .chain(&self.extend_select)
            .any(|s| code.starts_with(s.as_str()))
    }

    /// `true` when `code` is selected (or nothing is selected explicitly)
    /// and not ignored. All lists use prefix matching.
    pub fn is_enabled(&self, code: &str) -> bool {
        let selected = self.select.is_empty() || self.explicitly_selected(code);
        let ignored = self.ignore.iter().any(|i| code.starts_with(i.as_str()));
        selected && !ignored
    }

    /// Like [`Self::is_enabled`], but an `opt_in` rule additionally needs an
    /// explicit match in `select` / `extend_select`: an empty `select`, or a
    /// `select` naming other rules, does not enable it.
    pub fn runs_rule(&self, code: &str, opt_in: bool) -> bool {
        self.is_enabled(code) && (!opt_in || self.explicitly_selected(code))
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    // ── Global defaults ───────────────────────────────────────────────────
    pub cache_dir: String,
    pub workers: usize,

    // ── File selection ────────────────────────────────────────────────────
    /// Glob patterns of files to skip when walking directories, matched
    /// relative to the config file (or the file name alone). `--exclude`
    /// replaces this list.
    pub exclude: Vec<String>,
    /// Patterns skipped in addition to `exclude`; `--extend-exclude` adds to it.
    pub extend_exclude: Vec<String>,

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
    /// The config file that was loaded, if any.
    #[serde(skip)]
    pub config_file: Option<PathBuf>,

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

    // ── Inheritance ───────────────────────────────────────────────────────
    /// Config files merged in through `extend`, nearest first.
    #[serde(skip)]
    pub extends: Vec<PathBuf>,
    /// Problems with the config itself, such as a broken `extend` chain.
    /// The CLI exits on them; the language server shows them.
    #[serde(skip)]
    pub errors: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_dir: ".konform_cache".into(),
            workers: 0,
            exclude: vec![],
            extend_exclude: vec![],
            lint: LintConfig::default(),
            python: None,
            config_dir: None,
            config_file: None,
            ignore_noqa: false,
            src: vec![".".to_owned(), "src".to_owned()],
            extends: vec![],
            errors: vec![],
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
    #[cfg(test)]
    pub fn is_enabled(&self, code: &str) -> bool {
        self.selection().is_enabled(code)
    }

    /// Return `true` when a whole rule should run. Differs from
    /// [`Self::is_enabled`] only for opt-in rules, which must be selected
    /// explicitly (`select` / `extend-select`).
    pub fn runs_rule(&self, code: &str, opt_in: bool) -> bool {
        self.selection().runs_rule(code, opt_in)
    }

    /// Snapshot of the `select` / `extend-select` / `ignore` lists, for
    /// passing to rules via [`crate::rules::FileContext::selection`].
    pub fn selection(&self) -> RuleSelection {
        RuleSelection {
            select: self.lint.select.clone(),
            extend_select: self.lint.extend_select.clone(),
            ignore: self.lint.ignore.clone(),
        }
    }

    /// The `exclude` and `extend-exclude` globs compiled into one set, or
    /// `None` when there are none. Invalid globs are skipped.
    pub fn exclude_set(&self) -> Option<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        let mut any = false;
        for pattern in self.exclude.iter().chain(&self.extend_exclude) {
            if let Ok(glob) = Glob::new(pattern) {
                builder.add(glob);
                any = true;
            }
        }
        if any {
            builder.build().ok()
        } else {
            None
        }
    }

    /// `true` when `path` matches `excluded` (see [`Self::exclude_set`]):
    /// its path relative to the project root (the config directory, else the
    /// current one), or any parent directory below that root, matches either
    /// as a path or by name. So `gen/**`, `*_pb2.py` and a bare directory
    /// name such as `build` all work, while directories above the project
    /// (a checkout under `/home/me/build/`) are never consulted.
    pub fn is_excluded(&self, excluded: &GlobSet, path: &Path) -> bool {
        let cwd = std::env::current_dir().unwrap_or_default();
        let root = cwd.join(self.config_dir.as_deref().unwrap_or(Path::new("")));
        let absolute = cwd.join(path);
        let matches =
            |p: &Path| excluded.is_match(p) || p.file_name().is_some_and(|n| excluded.is_match(n));
        match absolute.strip_prefix(&root) {
            Ok(relative) => relative
                .ancestors()
                .filter(|a| !a.as_os_str().is_empty())
                .any(matches),
            // Outside the project: only the path itself and its file name.
            Err(_) => matches(path.strip_prefix(".").unwrap_or(path)),
        }
    }

    /// The `fixable` / `unfixable` lists as a selection: a code may be fixed
    /// when [`RuleSelection::is_enabled`] says so.
    pub fn fix_selection(&self) -> RuleSelection {
        RuleSelection {
            select: self.lint.fixable.clone(),
            extend_select: vec![],
            ignore: self.lint.unfixable.clone(),
        }
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

    match path {
        Some(path) => Config {
            config_file: Some(path.clone()),
            ..load_config_file(path)
        },
        None => Config::default(),
    }
}

/// Load the config at `path`; on any problem reading it, the defaults.
fn load_config_file(path: PathBuf) -> Config {
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

    let section = match konform_section(&raw, &path) {
        Some(s) => s,
        None => {
            return Config {
                config_dir,
                ..Config::default()
            }
        }
    };

    // ── Inheritance: lay this file over the ones it `extend`s ──────────────
    let mut chain = vec![path.canonicalize().unwrap_or_else(|_| path.clone())];
    let (section, errors) = match resolve_extend(section.clone(), &path, &mut chain) {
        Ok(merged) => (merged, vec![]),
        Err(e) => (section, vec![e]),
    };

    let mut cfg = Config::deserialize(section.clone()).unwrap_or_default();
    cfg.config_dir = config_dir;
    cfg.extends = chain.split_off(1);
    cfg.errors = errors;

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

/// The konform table of a parsed config file: `[tool.konform]` in a
/// `pyproject.toml`, else `[konform]` or the whole file.
fn konform_section(raw: &toml::Value, path: &Path) -> Option<toml::Value> {
    if path.file_name().is_some_and(|n| n == "pyproject.toml") {
        raw.get("tool").and_then(|t| t.get("konform")).cloned()
    } else {
        raw.get("konform").cloned().or(Some(raw.clone()))
    }
}

/// Resolve `extend = "path"` in `section` (read from `path`): load that
/// file's konform table, resolve its own `extend`, then lay `section` over
/// it (see [`merge_toml`]). `chain` holds the files seen so far, starting
/// with the root; each extended file is appended.
fn resolve_extend(
    mut section: toml::Value,
    path: &Path,
    chain: &mut Vec<PathBuf>,
) -> Result<toml::Value, String> {
    let Some(extend) = section.as_table_mut().and_then(|t| t.remove("extend")) else {
        return Ok(section);
    };
    let Some(target) = extend.as_str() else {
        return Err(format!("{}: `extend` must be a path", path.display()));
    };
    let base_path = expand_home(target);
    let base_path = match path.parent() {
        Some(dir) if base_path.is_relative() => dir.join(base_path),
        _ => base_path,
    };
    let base_path = base_path.canonicalize().map_err(|e| {
        format!(
            "{}: cannot read extended config {}: {e}",
            path.display(),
            base_path.display()
        )
    })?;
    if chain.contains(&base_path) {
        return Err(format!(
            "{}: `extend` cycle through {}",
            path.display(),
            base_path.display()
        ));
    }
    chain.push(base_path.clone());

    let content = std::fs::read_to_string(&base_path)
        .map_err(|e| format!("cannot read extended config {}: {e}", base_path.display()))?;
    let raw: toml::Value = toml::from_str(&content)
        .map_err(|e| format!("{}: invalid TOML: {e}", base_path.display()))?;
    let base =
        konform_section(&raw, &base_path).unwrap_or_else(|| toml::Value::Table(Default::default()));
    let mut merged = resolve_extend(base, &base_path, chain)?;
    merge_toml(&mut merged, section);
    Ok(merged)
}

/// Lay `over` onto `base`: tables merge key by key, anything else
/// (strings, lists, arrays of tables) in `over` replaces `base`'s value.
fn merge_toml(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
    }
}

/// Expand a leading `~/` to the home directory.
fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    expand_home_in(path, home)
}

fn expand_home_in(path: &str, home: Option<std::ffi::OsString>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

// ---------------------------------------------------------------------------
// Python interpreter discovery
// ---------------------------------------------------------------------------

/// Return the path to the Python interpreter to use for `sys.path` probing,
/// by checking common virtual-environment locations under `project_root`.
///
/// Discovery order:
/// 1. `.venv/bin/python3`:   hatch (`path = ".venv"`), uv, plain `python -m venv`
/// 2. `venv/bin/python3`:    common alternative name
/// 3. `.env/bin/python3`:    another common name
/// 4. `python3` on `$PATH`: system fallback
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
/// 1. `[tool.konform] python = "…"`: explicit config
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

    // ── extend ───────────────────────────────────────────────────────────

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn extend_lays_the_child_over_the_base() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "shared/konform.toml",
            "exclude = [\"gen/**\"]\n\
             [lint]\nselect = [\"KIS\"]\nignore = [\"KIS002\"]\n\
             [lint.module-only-imports]\nexceptions = [\"typing\"]\nlevel = \"warning\"\n",
        );
        let child = write(
            tmp.path(),
            "proj/pyproject.toml",
            "[tool.konform]\nextend = \"../shared/konform.toml\"\n\
             [tool.konform.lint]\nignore = []\n\
             [tool.konform.lint.module-only-imports]\nlevel = \"error\"\n",
        );
        let cfg = load_config(None, Some(&child));
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        assert_eq!(cfg.lint.select, ["KIS"]);
        assert!(
            cfg.lint.ignore.is_empty(),
            "lists in the child replace the base's"
        );
        assert_eq!(cfg.exclude, ["gen/**"]);
        let rule = cfg.rule_config("module-only-imports");
        assert_eq!(rule["level"].as_str(), Some("error"));
        assert_eq!(
            rule["exceptions"][0].as_str(),
            Some("typing"),
            "tables merge"
        );
        // Paths stay relative to the project, not the shared file.
        assert_eq!(cfg.config_dir.as_deref(), child.parent());
        assert_eq!(
            cfg.extends,
            [tmp.path()
                .join("shared/konform.toml")
                .canonicalize()
                .unwrap()]
        );
    }

    #[test]
    fn extend_chains_and_reads_pyproject_bases() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "org/pyproject.toml",
            "[tool.konform.lint]\nselect = [\"KIS\"]\nlevel = \"warning\"\n",
        );
        write(
            tmp.path(),
            "team/konform.toml",
            "extend = \"../org/pyproject.toml\"\n[lint]\nlevel = \"error\"\n",
        );
        let child = write(
            tmp.path(),
            "proj/konform.toml",
            "extend = \"../team/konform.toml\"\n",
        );
        let cfg = load_config(None, Some(&child));
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        assert_eq!(cfg.lint.select, ["KIS"]);
        assert_eq!(cfg.lint.level, Level::Error);
        assert_eq!(cfg.extends.len(), 2);
        assert!(cfg.extends[0].ends_with("team/konform.toml"));
    }

    #[test]
    fn extend_cycles_and_missing_files_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(
            tmp.path(),
            "a.toml",
            "extend = \"b.toml\"\n[lint]\nselect = [\"KIS\"]\n",
        );
        write(tmp.path(), "b.toml", "extend = \"a.toml\"\n");
        let cfg = load_config(None, Some(&a));
        assert!(cfg.errors[0].contains("`extend` cycle"), "{:?}", cfg.errors);
        // The file's own settings still apply.
        assert_eq!(cfg.lint.select, ["KIS"]);

        let c = write(tmp.path(), "c.toml", "extend = \"missing.toml\"\n");
        let cfg = load_config(None, Some(&c));
        assert!(
            cfg.errors[0].contains("cannot read extended config"),
            "{:?}",
            cfg.errors
        );
    }

    #[test]
    fn extend_must_be_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "konform.toml", "extend = 3\n");
        let cfg = load_config(None, Some(&path));
        assert!(
            cfg.errors[0].contains("`extend` must be a path"),
            "{:?}",
            cfg.errors
        );
    }

    #[test]
    fn extend_accepts_an_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base = write(
            tmp.path(),
            "shared/base.toml",
            "[lint]\nselect = [\"KIS\"]\n",
        );
        let child = write(
            tmp.path(),
            "proj/konform.toml",
            &format!("extend = {:?}\n", base.to_str().unwrap()),
        );
        let cfg = load_config(None, Some(&child));
        assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
        assert_eq!(cfg.lint.select, ["KIS"]);
    }

    #[test]
    fn a_leading_tilde_means_the_home_directory() {
        let home = Some(std::ffi::OsString::from("/home/me"));
        assert_eq!(
            expand_home_in("~/shared/konform.toml", home.clone()),
            PathBuf::from("/home/me").join("shared/konform.toml")
        );
        // No home known, or no tilde: the path is used as written.
        assert_eq!(expand_home_in("~/x.toml", None), PathBuf::from("~/x.toml"));
        assert_eq!(
            expand_home_in("../x.toml", home),
            PathBuf::from("../x.toml")
        );
    }

    #[test]
    fn merge_toml_merges_tables_and_replaces_everything_else() {
        let mut base: toml::Value =
            toml::from_str("a = 1\nlist = [1, 2]\n[t]\nx = 1\ny = 2\n").unwrap();
        let over: toml::Value = toml::from_str("list = [3]\n[t]\ny = 3\nz = 4\n").unwrap();
        merge_toml(&mut base, over);
        let want: toml::Value =
            toml::from_str("a = 1\nlist = [3]\n[t]\nx = 1\ny = 3\nz = 4\n").unwrap();
        assert_eq!(base, want);
    }

    #[test]
    fn exclude_globs_match_relative_to_the_config_dir() {
        let cfg = Config {
            exclude: vec!["gen/**".into()],
            extend_exclude: vec!["*_pb2.py".into()],
            config_dir: Some(PathBuf::from("/proj")),
            ..Config::default()
        };
        let gs = cfg.exclude_set().unwrap();
        assert!(cfg.is_excluded(&gs, Path::new("/proj/gen/a.py")));
        assert!(cfg.is_excluded(&gs, Path::new("./gen/a.py")));
        assert!(cfg.is_excluded(&gs, Path::new("pkg/msg_pb2.py")));
        assert!(!cfg.is_excluded(&gs, Path::new("/proj/src/a.py")));
    }

    #[test]
    fn exclude_matches_parent_directories_but_only_inside_the_project() {
        let cfg = Config {
            exclude: vec!["build".into()],
            config_dir: Some(PathBuf::from("/home/me/build/proj")),
            ..Config::default()
        };
        let gs = cfg.exclude_set().unwrap();
        assert!(cfg.is_excluded(&gs, Path::new("/home/me/build/proj/build/sub/x.py")));
        assert!(cfg.is_excluded(&gs, Path::new("/home/me/build/proj/src/build")));
        // `build` above the project root must not exclude everything.
        assert!(!cfg.is_excluded(&gs, Path::new("/home/me/build/proj/src/a.py")));
    }

    #[test]
    fn no_exclude_globs_means_no_set() {
        assert!(Config::default().exclude_set().is_none());
    }

    #[test]
    fn exclude_keys_are_read_from_the_top_level_table() {
        let cfg: Config = toml::from_str("exclude = [\"a\"]\nextend-exclude = [\"b\"]\n").unwrap();
        assert_eq!(cfg.exclude, ["a"]);
        assert_eq!(cfg.extend_exclude, ["b"]);
        assert!(cfg.lint.rules.is_empty());
    }

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

    fn sel(select: &[&str], extend: &[&str], ignore: &[&str]) -> RuleSelection {
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect();
        RuleSelection {
            select: own(select),
            extend_select: own(extend),
            ignore: own(ignore),
        }
    }

    #[test]
    fn opt_in_rule_is_off_unless_explicitly_selected() {
        assert!(!sel(&[], &[], &[]).runs_rule("KNQ001", true));
        assert!(!sel(&["KIS"], &[], &[]).runs_rule("KNQ001", true));
        for s in [
            sel(&["KNQ"], &[], &[]),
            sel(&["KNQ001"], &[], &[]),
            sel(&[], &["KNQ001"], &[]),
        ] {
            assert!(s.runs_rule("KNQ001", true), "{s:?}");
        }
    }

    #[test]
    fn ignore_beats_opt_in_selection() {
        assert!(!sel(&[], &["KNQ"], &["KNQ001"]).runs_rule("KNQ001", true));
    }

    #[test]
    fn extend_select_does_not_narrow_default_rules() {
        let s = sel(&[], &["KNQ001"], &[]);
        assert!(s.runs_rule("KIS001", false));
        assert!(s.runs_rule("KPT001", false));
    }

    #[test]
    fn non_opt_in_rules_ignore_the_opt_in_gate() {
        assert!(sel(&[], &[], &[]).runs_rule("KIS001", false));
        assert!(!sel(&["KPT"], &[], &[]).runs_rule("KIS001", false));
    }

    #[test]
    fn extend_select_is_read_from_toml() {
        let cfg: Config = toml::from_str("[lint]\nextend-select = [\"KNQ001\"]\n").unwrap();
        assert_eq!(cfg.lint.extend_select, vec!["KNQ001".to_owned()]);
        assert!(cfg.runs_rule("KNQ001", true));
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
