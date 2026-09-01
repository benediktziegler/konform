//! KPT001 — Konform Pattern: user-defined regex pattern violations.
//!
//! Checks Python (and other) source files against a set of user-defined
//! regular-expression patterns.  Patterns can be supplied from three sources,
//! tried in priority order:
//!
//! 1. **Inline** `[[tool.konform.KPT.rules]]` inside `pyproject.toml` /
//!    `konform.toml`.
//! 2. **Explicit file** referenced by `rules_file = "path"` in
//!    `[tool.konform.KPT]`  (`.toml` or `.yaml`).
//! 3. **Auto-discovered** `konform_patterns.toml` next to the config file.
//! 4. **Auto-discovered** `konform_patterns.yaml` (legacy / migration compat).
//! 5. **No patterns** — the rule runs but emits zero violations.
//!
//! Each pattern entry carries:
//! * `id`        — violation code used in output and `# noqa` suppression
//! * `message`   — human-readable description
//! * `pattern`   — regular expression matched against each line
//! * `files`     — optional list of glob patterns; when absent the pattern
//!   applies to every file
//! * `level`     — `"error"` or `"warning"`; falls back to
//!   `[tool.konform.KPT].level` (default: `"warning"`)
//! * `help`      — optional guidance text surfaced alongside the violation
//! * `sub_rules` — ordered list of refinements; the first sub-rule whose
//!   pattern(s) match the already-flagged line overrides `message` and `help`

use super::{has_noqa, FileContext, Rule};
use crate::types::{Level, Violation};
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deserialises a field that accepts either a bare string or a list of strings.
///
/// Used for `sub_rules[].pattern` so that both TOML / YAML single-string and
/// list forms are supported:
///
/// ```toml
/// pattern = 'single_regex'
/// pattern = ['regex_a', 'regex_b']   # any match fires the sub-rule
/// ```
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Inner {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Inner::deserialize(deserializer)? {
        Inner::One(s) => vec![s],
        Inner::Many(v) => v,
    })
}

// ---------------------------------------------------------------------------
// Raw pattern types  (deserialised from TOML / YAML)
// ---------------------------------------------------------------------------

/// A refinement that overrides `message` and `help` when any of its patterns
/// matches a line that the parent rule has already flagged.
///
/// Sub-rules are tested in declaration order; the first match wins.
#[derive(Debug, Clone, Deserialize)]
struct RawSubRule {
    /// One or more regexes — a match on **any** of them fires this sub-rule.
    /// Accepts a bare string or a TOML / YAML list of strings.
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pattern: Vec<String>,
    message: String,
    #[serde(default)]
    help: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPattern {
    id: String,
    message: String,
    /// One or more regexes — a match on **any** of them fires this rule.
    /// Accepts a bare string or a TOML / YAML list of strings.
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pattern: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    /// Per-pattern level; falls back to the category-level default when absent.
    level: Option<String>,
    /// Optional guidance shown alongside the violation message.
    #[serde(default)]
    help: Option<String>,
    /// Ordered refinements — first match overrides `message` / `help`.
    #[serde(default)]
    sub_rules: Vec<RawSubRule>,
    /// When `true`, match against the whole file source with the DOTALL flag
    /// so that `.` crosses newlines.  Defaults to `false` (line-by-line).
    #[serde(default)]
    multiline: Option<bool>,
    /// Replacement string applied to each match when fixing.  Rust `regex`
    /// capture syntax (`$1`, `$2`, …) is supported.  When absent the
    /// pattern is non-fixable.
    #[serde(default)]
    replacement: Option<String>,
}

/// Top-level structure for stand-alone pattern files.
#[derive(Debug, Deserialize)]
struct PatternFile {
    #[serde(default)]
    rules: Vec<RawPattern>,
}

// ---------------------------------------------------------------------------
// Compiled pattern
// ---------------------------------------------------------------------------

/// A compiled sub-rule: pre-built regexes plus override message and help.
#[derive(Debug)]
struct CompiledSubRule {
    patterns: Vec<Regex>,
    message: String,
    help: Option<String>,
}

impl CompiledSubRule {
    /// Returns `true` when any of the sub-rule's patterns matches `line`.
    fn matches(&self, line: &str) -> bool {
        self.patterns.iter().any(|re| re.is_match(line))
    }
}

#[derive(Debug)]
struct CompiledPattern {
    id: String,
    message: String,
    help: Option<String>,
    /// One or more compiled regexes — a match on any fires this rule.
    regexes: Vec<Regex>,
    /// `None` → applies to every file; `Some` → only files matching any glob.
    files: Option<GlobSet>,
    level: Level,
    sub_rules: Vec<CompiledSubRule>,
    /// When `true`, the pattern is matched against the full file source
    /// (with DOTALL enabled) rather than line-by-line.
    multiline: bool,
    /// When `Some`, violations are fixable and `fix()` applies this
    /// replacement string (with `$1`, `$2`, … capture syntax).
    replacement: Option<String>,
}

impl CompiledPattern {
    /// Returns `true` when the compiled file-glob set matches `path`.
    ///
    /// Mirrors the multi-candidate strategy used by `engine::per_file_ignored`
    /// so that globs like `src/**/*.py` work whether `path` is:
    /// * already project-root-relative (`src/foo/bar.py`)
    /// * absolute with a `config_dir` prefix (`/project/src/foo/bar.py`)
    /// * absolute with a CWD prefix (same thing reached via a different root)
    /// * a bare filename (`*.py` style)
    fn matches_file(&self, path: &Path, config_dir: Option<&Path>, cwd: Option<&Path>) -> bool {
        let Some(gs) = &self.files else {
            return true;
        };
        // 1. Path as supplied (works when already relative to the project root).
        if gs.is_match(path) {
            return true;
        }
        // 2. Strip config_dir prefix (LSP / absolute-path CLI invocations).
        if let Some(rel) = config_dir.and_then(|d| path.strip_prefix(d).ok()) {
            if gs.is_match(rel) {
                return true;
            }
        }
        // 3. Strip CWD prefix (absolute CLI paths when CWD != config_dir).
        if let Some(rel) = cwd.and_then(|d| path.strip_prefix(d).ok()) {
            if gs.is_match(rel) {
                return true;
            }
        }
        // 4. Bare filename fallback so `*.py` works without any path prefix.
        path.file_name().is_some_and(|n| gs.is_match(n))
    }
}

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

/// KPT001 — user-defined regex pattern rule.
pub struct KptRule {
    /// Directory containing `pyproject.toml` / `konform.toml`.
    /// Used to resolve relative `rules_file` paths and to auto-discover
    /// `konform_patterns.toml` / `konform_patterns.yaml`.
    config_dir: Option<PathBuf>,
}

impl KptRule {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self { config_dir }
    }
}

// ---------------------------------------------------------------------------
// Rule impl
// ---------------------------------------------------------------------------

impl Rule for KptRule {
    fn code(&self) -> &str {
        "KPT001"
    }

    fn category(&self) -> &str {
        "KPT"
    }

    fn name(&self) -> &str {
        "Pattern rules"
    }

    fn description(&self) -> &str {
        "Checks files against user-defined regex patterns from konform_patterns.toml."
    }

    fn fixable(&self) -> bool {
        // Without the calling cfg we can only probe auto-discovered pattern
        // files.  For inline rules in pyproject.toml the engine will rely on
        // the per-violation `fixable` field and the `fix()` implementation.
        let empty = toml::Value::Table(toml::map::Map::new());
        load_patterns(&empty, self.config_dir.as_deref(), Level::Warning)
            .into_iter()
            .any(|p| p.replacement.is_some())
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let default_level = parse_default_level(cfg);
        let patterns = load_patterns(cfg, self.config_dir.as_deref(), default_level);
        if patterns.is_empty() {
            return vec![];
        }

        let lines: Vec<&str> = ctx.source.lines().collect();
        let mut violations = Vec::new();
        let cwd = std::env::current_dir().ok();

        for pattern in &patterns {
            if !pattern.matches_file(&ctx.path, self.config_dir.as_deref(), cwd.as_deref()) {
                continue;
            }

            if pattern.multiline {
                // Full-source matching: each regex is applied to the whole
                // source text; one violation is reported per match.
                for re in &pattern.regexes {
                    for m in re.find_iter(&ctx.source) {
                        // Determine start line/col from the byte prefix.
                        let prefix = &ctx.source[..m.start()];
                        let line_0 = prefix.bytes().filter(|&b| b == b'\n').count();
                        let line_num = line_0 + 1;
                        let col = m.start() - prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);

                        // Determine end line/col.
                        let end_prefix = &ctx.source[..m.end()];
                        let end_line_0 = end_prefix.bytes().filter(|&b| b == b'\n').count();
                        let end_line = end_line_0 + 1;
                        let end_col = m.end() - end_prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);

                        // noqa is checked against the first line of the match.
                        let first_line = ctx.lines.get(line_0).map(String::as_str).unwrap_or("");
                        if ctx.ignore_noqa || !has_noqa(first_line, &pattern.id, &ctx.noqa_aliases)
                        {
                            let matched_str = m.as_str();
                            let (message, help) = pattern
                                .sub_rules
                                .iter()
                                .find(|sr| sr.matches(matched_str))
                                .map(|sr| (sr.message.as_str(), sr.help.as_deref()))
                                .unwrap_or((pattern.message.as_str(), pattern.help.as_deref()));

                            violations.push(Violation {
                                rule: pattern.id.clone(),
                                line: line_num,
                                col,
                                end_line,
                                end_col,
                                message: format!("{}: {}", pattern.id, message),
                                help: help.map(str::to_owned),
                                level: pattern.level,
                                fixable: pattern.replacement.is_some(),
                            });
                        }
                    }
                }
            } else {
                for (i, line) in lines.iter().enumerate() {
                    // Try all regexes and report the match with the widest span.
                    if let Some(m) = pattern
                        .regexes
                        .iter()
                        .filter_map(|re| re.find(line))
                        .max_by_key(|m| m.end() - m.start())
                    {
                        if ctx.ignore_noqa || !has_noqa(line, &pattern.id, &ctx.noqa_aliases) {
                            // Apply sub-rules in declaration order; first match wins
                            // and overrides the parent message and help for this line.
                            let (message, help) = pattern
                                .sub_rules
                                .iter()
                                .find(|sr| sr.matches(line))
                                .map(|sr| (sr.message.as_str(), sr.help.as_deref()))
                                .unwrap_or((pattern.message.as_str(), pattern.help.as_deref()));

                            violations.push(Violation {
                                rule: pattern.id.clone(),
                                line: i + 1,
                                col: m.start(),
                                end_line: i + 1,
                                end_col: m.end(),
                                message: format!("{}: {}", pattern.id, message),
                                help: help.map(str::to_owned),
                                level: pattern.level,
                                fixable: pattern.replacement.is_some(),
                            });
                        }
                    }
                }
            }
        }

        violations
    }

    fn fix(&self, ctx: &FileContext, cfg: &toml::Value) -> Result<Option<String>> {
        let default_level = parse_default_level(cfg);
        let patterns = load_patterns(cfg, self.config_dir.as_deref(), default_level);
        let cwd = std::env::current_dir().ok();
        let eol = if ctx.source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };

        let mut current = ctx.source.clone();

        for pattern in &patterns {
            let Some(replacement) = &pattern.replacement else {
                continue;
            };
            if !pattern.matches_file(&ctx.path, self.config_dir.as_deref(), cwd.as_deref()) {
                continue;
            }

            if pattern.multiline {
                // Apply replace_all to the full source for each regex.
                for re in &pattern.regexes {
                    current = re.replace_all(&current, replacement.as_str()).into_owned();
                }
            } else {
                // Apply replace_all per line, then reassemble with original EOL.
                let lines: Vec<&str> = current.lines().collect();
                current = lines
                    .iter()
                    .map(|line| {
                        let mut out = (*line).to_owned();
                        for re in &pattern.regexes {
                            out = re.replace_all(&out, replacement.as_str()).into_owned();
                        }
                        format!("{out}{eol}")
                    })
                    .collect();
            }
        }

        if current == ctx.source {
            Ok(None)
        } else {
            Ok(Some(current))
        }
    }

    fn explain(&self) -> String {
        r#"KPT001 — User-defined pattern rules

  Checks every Python source file against a set of regular expressions
  defined in your project configuration.

  Patterns are loaded from the first available source:
    1. Inline [[tool.konform.KPT.rules]] in pyproject.toml / konform.toml
    2. rules_file = "path" in [tool.konform.KPT]
    3. konform_patterns.toml  (auto-discovered next to the config file)
    4. konform_patterns.yaml  (legacy fallback)

  Each pattern entry:
    id      = "KPT001"
    message = "Use the logger instead of bare print()."
    pattern = '^\s*print\('              # single string
    # or a list — any match fires the rule:
    pattern = ['^\s*print\(', '^\s*breakpoint\(']
    files   = ["src/**/*.py"]   # optional glob filter
    level   = "warning"         # or "error"
    help    = "Use logger.info() instead."  # optional guidance text

  Sub-rules refine the message and help for more specific matches.
  The first sub-rule whose pattern(s) match the already-flagged line wins:

    [[rules]]
    id      = "CTFW001"
    message = "os.environ found — discouraged in test code."
    pattern = 'os\.environ'
    help    = "Contact the project maintainers for guidance."

    [[rules.sub_rules]]
    # pattern accepts a single string or a list; any match fires the sub-rule
    pattern = ['os\.environ\.get\(', 'os\.environ\[']
    message = "os.environ — baseline_handle access detected."
    help    = "Use the 'core_baseline' fixture instead."

  Suppress per-line:
    os.environ.get("X")   # noqa: CTFW001
    os.environ.get("X")   # noqa: KPT      (silences all KPT rules on this line)
"#
        .to_owned()
    }
}

// ---------------------------------------------------------------------------
// Pattern loading
// ---------------------------------------------------------------------------

fn parse_default_level(cfg: &toml::Value) -> Level {
    cfg.get("level")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(Level::Warning)
}

/// Load patterns using the four-source priority order.
fn load_patterns(
    cfg: &toml::Value,
    config_dir: Option<&Path>,
    default_level: Level,
) -> Vec<CompiledPattern> {
    // ── Source 1: inline [[tool.konform.KPT.rules]] ───────────────────────
    if let Some(arr) = cfg.get("rules").and_then(|v| v.as_array()) {
        if !arr.is_empty() {
            let raws: Vec<RawPattern> = arr
                .iter()
                .filter_map(|v| RawPattern::deserialize(v.clone()).ok())
                .collect();
            return compile_patterns(raws, default_level);
        }
    }

    // ── Source 2: explicit rules_file ─────────────────────────────────────
    if let Some(file_path) = cfg.get("rules_file").and_then(|v| v.as_str()) {
        let path = resolve_path(file_path, config_dir);
        if let Some(patterns) = load_from_file(&path, default_level) {
            return patterns;
        }
    }

    // ── Sources 3 & 4: auto-discover next to the config file ──────────────
    if let Some(dir) = config_dir {
        for name in ["konform_patterns.toml", "konform_patterns.yaml"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                if let Some(patterns) = load_from_file(&candidate, default_level) {
                    return patterns;
                }
            }
        }
    }

    // ── Source 5: no patterns ─────────────────────────────────────────────
    vec![]
}

fn resolve_path(file_path: &str, config_dir: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(file_path);
    if p.is_absolute() {
        p
    } else if let Some(dir) = config_dir {
        dir.join(p)
    } else {
        p
    }
}

fn load_from_file(path: &Path, default_level: Level) -> Option<Vec<CompiledPattern>> {
    let content = std::fs::read_to_string(path).ok()?;
    let pf: PatternFile = if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
        serde_yaml::from_str(&content).ok()?
    } else {
        toml::from_str(&content).ok()?
    };
    Some(compile_patterns(pf.rules, default_level))
}

fn compile_patterns(raws: Vec<RawPattern>, default_level: Level) -> Vec<CompiledPattern> {
    raws.into_iter()
        .filter_map(|r| {
            // Destructure up-front so we can move fields independently.
            let RawPattern {
                id,
                message,
                help,
                pattern: raw_patterns,
                files: file_globs,
                level: raw_level,
                sub_rules: raw_sub_rules,
                multiline: raw_multiline,
                replacement,
            } = r;

            let is_multiline = raw_multiline.unwrap_or(false);

            // Compile all regexes; skip invalid ones and drop the whole rule
            // if none remain.  When multiline is enabled prepend `(?s)` so
            // that `.` matches newline characters.
            let regexes: Vec<Regex> = raw_patterns
                .iter()
                .filter_map(|p| {
                    let pat = if is_multiline {
                        format!("(?s){p}")
                    } else {
                        p.clone()
                    };
                    match Regex::new(&pat) {
                        Ok(re) => Some(re),
                        Err(e) => {
                            eprintln!(
                                "konform: skipping pattern '{}' — invalid regex '{}': {e}",
                                id, p
                            );
                            None
                        }
                    }
                })
                .collect();
            if regexes.is_empty() {
                return None;
            }

            // Compile the file globs; skip bad globs with a diagnostic.
            let files = if file_globs.is_empty() {
                None
            } else {
                let mut builder = GlobSetBuilder::new();
                for glob_str in &file_globs {
                    match Glob::new(glob_str) {
                        Ok(g) => {
                            builder.add(g);
                        }
                        Err(e) => {
                            eprintln!(
                                "konform: skipping glob '{}' in pattern '{}': {e}",
                                glob_str, id
                            );
                        }
                    }
                }
                match builder.build() {
                    Ok(gs) => Some(gs),
                    Err(e) => {
                        eprintln!("konform: failed to build glob set for '{}': {e}", id);
                        None
                    }
                }
            };

            let level = raw_level
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default_level);

            // Compile sub-rules: skip individual patterns with invalid regexes,
            // and drop the whole sub-rule when no valid patterns remain.
            let sub_rules = raw_sub_rules
                .into_iter()
                .filter_map(|sr| {
                    let patterns: Vec<Regex> = sr
                        .pattern
                        .iter()
                        .filter_map(|p| match Regex::new(p) {
                            Ok(re) => Some(re),
                            Err(e) => {
                                eprintln!(
                                    "konform: skipping sub-rule pattern in '{}' \
                                     — invalid regex '{}': {e}",
                                    id, p
                                );
                                None
                            }
                        })
                        .collect();
                    if patterns.is_empty() {
                        None
                    } else {
                        Some(CompiledSubRule {
                            patterns,
                            message: sr.message,
                            help: sr.help,
                        })
                    }
                })
                .collect();

            Some(CompiledPattern {
                id,
                message,
                help,
                regexes,
                files,
                level,
                sub_rules,
                multiline: is_multiline,
                replacement,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(source: &str) -> FileContext {
        FileContext::from_source(PathBuf::from("src/test.py"), source.to_owned())
    }

    fn ctx_path(path: &str, source: &str) -> FileContext {
        FileContext::from_source(PathBuf::from(path), source.to_owned())
    }

    fn rule() -> KptRule {
        KptRule::new(None)
    }

    fn cfg_with_rules(rules_toml: &str) -> toml::Value {
        toml::from_str(rules_toml).unwrap()
    }

    // ── no patterns ────────────────────────────────────────────────────────

    #[test]
    fn no_patterns_yields_no_violations() {
        let cfg = toml::Value::Table(toml::map::Map::new());
        let violations = rule().check(&ctx("print('hello')\n"), &cfg);
        assert!(violations.is_empty());
    }

    // ── inline rules ──────────────────────────────────────────────────────

    #[test]
    fn inline_pattern_fires_on_match() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = '^\s*print\('
"#,
        );
        let violations = rule().check(&ctx("print('hello')\n"), &cfg);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "KPT001");
        assert_eq!(violations[0].line, 1);
        assert!(!violations[0].fixable);
    }

    #[test]
    fn inline_pattern_no_match_is_clean() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = '^\s*print\('
"#,
        );
        let violations = rule().check(&ctx("logger.info('hello')\n"), &cfg);
        assert!(violations.is_empty());
    }

    #[test]
    fn multiple_matching_lines_each_reported() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let src = "print('a')\nlogger.info('b')\nprint('c')\n";
        let violations = rule().check(&ctx(src), &cfg);
        assert_eq!(violations.len(), 2);
        assert_eq!(violations[0].line, 1);
        assert_eq!(violations[1].line, 3);
    }

    // ── glob file filter ──────────────────────────────────────────────────

    #[test]
    fn glob_filter_absolute_path_stripped_by_config_dir() {
        // Simulates a CLI invocation with an absolute path when config_dir
        // equals the project root.  `src/**/*.py` must still match.
        let tmp = tempfile::tempdir().unwrap();
        let abs_src = tmp.path().join("src").join("pkg").join("mod.py");

        let rule = KptRule::new(Some(tmp.path().to_path_buf()));
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
files   = ["src/**/*.py"]
"#,
        );

        // Absolute path — should be stripped to `src/pkg/mod.py` and match.
        let v = rule.check(
            &FileContext::from_source(abs_src, "print('x')\n".to_owned()),
            &cfg,
        );
        assert_eq!(
            v.len(),
            1,
            "absolute path under config_dir should match src/**/*.py"
        );

        // Absolute path outside src/ — must not match.
        let abs_other = tmp.path().join("tests").join("test_mod.py");
        let v2 = rule.check(
            &FileContext::from_source(abs_other, "print('x')\n".to_owned()),
            &cfg,
        );
        assert!(
            v2.is_empty(),
            "absolute path outside src/ must not match src/**/*.py"
        );
    }

    #[test]
    fn glob_filter_matches_correct_path() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
files   = ["src/**/*.py"]
"#,
        );
        // matches
        let v = rule().check(&ctx_path("src/foo/bar.py", "print('x')\n"), &cfg);
        assert_eq!(v.len(), 1, "should fire for src/foo/bar.py");

        // does not match
        let v2 = rule().check(&ctx_path("tests/test_bar.py", "print('x')\n"), &cfg);
        assert!(v2.is_empty(), "should not fire for tests/test_bar.py");
    }

    #[test]
    fn no_files_filter_applies_to_all() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx_path("tests/test_foo.py", "print('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
    }

    // ── noqa suppression ─────────────────────────────────────────────────

    #[test]
    fn noqa_exact_code_suppresses() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx("print('x')  # noqa: KPT001\n"), &cfg);
        assert!(v.is_empty());
    }

    #[test]
    fn noqa_category_prefix_suppresses() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx("print('x')  # noqa: KPT\n"), &cfg);
        assert!(v.is_empty());
    }

    #[test]
    fn bare_noqa_suppresses() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx("print('x')  # noqa\n"), &cfg);
        assert!(v.is_empty());
    }

    // ── per-pattern level ─────────────────────────────────────────────────

    #[test]
    fn per_pattern_level_respected() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
level   = "error"
"#,
        );
        let v = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v[0].level, Level::Error);
    }

    #[test]
    fn default_level_is_warning() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v[0].level, Level::Warning);
    }

    #[test]
    fn category_level_propagates_to_patterns() {
        let cfg = cfg_with_rules(
            r#"
level = "error"

[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v[0].level, Level::Error);
    }

    // ── violation fields ──────────────────────────────────────────────────

    #[test]
    fn violation_fields_are_correct() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT042"
message = "Avoid TODO."
pattern = '#\s*TODO'
"#,
        );
        let v = rule().check(&ctx("x = 1  # TODO: fix\n"), &cfg);
        assert_eq!(v.len(), 1);
        let viol = &v[0];
        assert_eq!(viol.rule, "KPT042");
        assert_eq!(viol.line, 1);
        assert_eq!(viol.end_line, 1);
        assert!(!viol.fixable);
        assert!(viol.message.contains("KPT042"));
        assert!(viol.message.contains("Avoid TODO."));
        // col/end_col must cover only the matched regex span, not the whole line.
        assert_eq!(viol.col, 7, "col must be start of match");
        assert_eq!(viol.end_col, 13, "end_col must be end of match");
        assert!(
            viol.end_col < "x = 1  # TODO: fix".len(),
            "end_col must not cover full line"
        );
    }

    // ── invalid regex skipped ─────────────────────────────────────────────

    // ── top-level list pattern ─────────────────────────────────────────

    #[test]
    fn top_level_list_pattern_fires_on_any_match() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Debugging artefact."
pattern = ['print\(', 'breakpoint\(']
"#,
        );
        // First pattern matches.
        let v1 = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v1.len(), 1, "first list pattern should fire");
        assert_eq!(v1[0].rule, "KPT001");

        // Second pattern matches.
        let v2 = rule().check(&ctx("breakpoint()\n"), &cfg);
        assert_eq!(v2.len(), 1, "second list pattern should fire");

        // Neither matches.
        let v3 = rule().check(&ctx("logger.info('x')\n"), &cfg);
        assert!(v3.is_empty(), "no match should yield no violations");
    }

    #[test]
    fn top_level_list_pattern_col_from_widest_match() {
        // When multiple patterns match the same line, col/end_col must reflect
        // the widest match, not the first one in declaration order.
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "hit."
pattern = ['print', "print\\('x'\\)"]
"#,
        );
        //  line: "    print('x')"
        //  'print'        → col 4, end_col  9  (span = 5)
        //  "print('x')"   → col 4, end_col 14  (span = 10)  ← widest
        let v = rule().check(&ctx("    print('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].col, 4, "col must be start of widest match");
        assert_eq!(v[0].end_col, 14, "end_col must be end of widest match");
    }

    #[test]
    fn top_level_invalid_regex_in_list_skipped_gracefully() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "hit."
pattern = ['[', 'print\(']
"#,
        );
        // Invalid first regex is dropped; valid second regex still fires.
        let v = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(
            v.len(),
            1,
            "valid regex in list should still fire after invalid one is skipped"
        );
    }

    #[test]
    fn top_level_all_invalid_regexes_drops_rule() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "hit."
pattern = ['[', '(']
"#,
        );
        // All patterns invalid — rule is silently dropped, no violations, no panic.
        let v = rule().check(&ctx("anything\n"), &cfg);
        assert!(v.is_empty());
    }

    // ── invalid regex skipped ─────────────────────────────────────────

    #[test]
    fn invalid_regex_skipped_gracefully() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Bad pattern."
pattern = '['   # invalid regex
"#,
        );
        // Should not panic; just emit no violations.
        let v = rule().check(&ctx("anything\n"), &cfg);
        assert!(v.is_empty());
    }

    // ── file loading ──────────────────────────────────────────────────────

    #[test]
    fn load_from_toml_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pattern_file = tmp.path().join("konform_patterns.toml");
        std::fs::write(
            &pattern_file,
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        )
        .unwrap();

        let rule = KptRule::new(Some(tmp.path().to_path_buf()));
        let cfg = toml::Value::Table(toml::map::Map::new()); // no inline rules
        let v = rule.check(&ctx("print('x')\n"), &cfg);
        assert_eq!(
            v.len(),
            1,
            "should load patterns from konform_patterns.toml"
        );
    }

    #[test]
    fn load_from_yaml_file_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let pattern_file = tmp.path().join("konform_patterns.yaml");
        std::fs::write(
            &pattern_file,
            "rules:\n  - id: KPT001\n    message: No bare print.\n    pattern: 'print\\('\n",
        )
        .unwrap();

        let rule = KptRule::new(Some(tmp.path().to_path_buf()));
        let cfg = toml::Value::Table(toml::map::Map::new());
        let v = rule.check(&ctx("print('x')\n"), &cfg);
        assert_eq!(
            v.len(),
            1,
            "should load patterns from konform_patterns.yaml"
        );
    }

    #[test]
    fn explicit_rules_file_takes_priority_over_auto_discover() {
        let tmp = tempfile::tempdir().unwrap();
        // Auto-discover file (should be ignored)
        std::fs::write(
            tmp.path().join("konform_patterns.toml"),
            "[[rules]]\nid = \"KPT099\"\nmessage = \"wrong\"\npattern = 'NEVER_MATCH_THIS'\n",
        )
        .unwrap();
        // Explicit rules file (should be used)
        let explicit = tmp.path().join("my_rules.toml");
        std::fs::write(
            &explicit,
            "[[rules]]\nid = \"KPT001\"\nmessage = \"No bare print.\"\npattern = 'print\\('\n",
        )
        .unwrap();

        let rule = KptRule::new(Some(tmp.path().to_path_buf()));
        let mut cfg_map = toml::map::Map::new();
        cfg_map.insert(
            "rules_file".into(),
            toml::Value::String(explicit.to_string_lossy().into()),
        );
        let cfg = toml::Value::Table(cfg_map);
        let v = rule.check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "KPT001");
    }

    #[test]
    fn inline_rules_take_priority_over_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("konform_patterns.toml"),
            "[[rules]]\nid = \"KPT099\"\nmessage = \"file rule\"\npattern = 'NEVER'\n",
        )
        .unwrap();

        let rule = KptRule::new(Some(tmp.path().to_path_buf()));
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Inline rule."
pattern = 'print\('
"#,
        );
        let v = rule.check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "KPT001", "inline rule should take priority");
    }

    // ── help field ────────────────────────────────────────────────────────

    #[test]
    fn help_text_propagated_to_violation() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
help    = "Use logger.info() instead."
"#,
        );
        let v = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].help.as_deref(), Some("Use logger.info() instead."));
    }

    #[test]
    fn no_help_field_yields_none_in_violation() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\('
"#,
        );
        let v = rule().check(&ctx("print('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(v[0].help.is_none());
    }

    // ── sub_rules ─────────────────────────────────────────────────────────

    #[test]
    fn sub_rule_overrides_message_on_specific_match() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic os.environ hit."
pattern = 'os\.environ'

[[rules.sub_rules]]
pattern = 'baseline_handle'
message = "baseline_handle specific hit."
"#,
        );
        let v = rule().check(&ctx("os.environ.get('baseline_handle')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("baseline_handle specific hit."),
            "sub-rule message should win: {:?}",
            v[0].message
        );
    }

    #[test]
    fn sub_rule_overrides_help() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic hit."
pattern = 'os\.environ'
help    = "Generic help."

[[rules.sub_rules]]
pattern = 'baseline_handle'
message = "Specific hit."
help    = "Use core_baseline fixture."
"#,
        );
        let v = rule().check(&ctx("os.environ['baseline_handle']\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].help.as_deref(), Some("Use core_baseline fixture."));
    }

    #[test]
    fn sub_rule_no_match_falls_back_to_parent() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic os.environ hit."
pattern = 'os\.environ'
help    = "Parent help."

[[rules.sub_rules]]
pattern = 'baseline_handle'
message = "baseline_handle specific hit."
help    = "Sub-rule help."
"#,
        );
        // Line matches parent pattern but not the sub-rule.
        let v = rule().check(&ctx("os.environ.get('OTHER_KEY')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("Generic os.environ hit."),
            "parent message should be used: {:?}",
            v[0].message
        );
        assert_eq!(v[0].help.as_deref(), Some("Parent help."));
    }

    #[test]
    fn sub_rule_list_pattern_fires_on_any_match() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic hit."
pattern = 'os\.environ'

[[rules.sub_rules]]
pattern = ['baseline_handle', 'some_other_key']
message = "Specific key hit."
"#,
        );
        // First pattern in the list matches.
        let v1 = rule().check(&ctx("os.environ.get('baseline_handle')\n"), &cfg);
        assert!(
            v1[0].message.contains("Specific key hit."),
            "first list pattern should fire"
        );
        // Second pattern in the list matches.
        let v2 = rule().check(&ctx("os.environ['some_other_key']\n"), &cfg);
        assert!(
            v2[0].message.contains("Specific key hit."),
            "second list pattern should fire"
        );
        // Neither matches → parent message.
        let v3 = rule().check(&ctx("os.environ.get('unrelated')\n"), &cfg);
        assert!(
            v3[0].message.contains("Generic hit."),
            "parent message when no list pattern matches"
        );
    }

    #[test]
    fn first_matching_sub_rule_wins() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic hit."
pattern = 'os\.environ'

[[rules.sub_rules]]
pattern = 'baseline'
message = "First sub-rule."

[[rules.sub_rules]]
pattern = 'baseline_handle'
message = "Second sub-rule."
"#,
        );
        // 'baseline' matches before 'baseline_handle' is even tested.
        let v = rule().check(&ctx("os.environ.get('baseline_handle')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("First sub-rule."),
            "first matching sub-rule should win: {:?}",
            v[0].message
        );
    }

    #[test]
    fn invalid_sub_rule_regex_skipped_gracefully() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic hit."
pattern = 'os\.environ'

[[rules.sub_rules]]
pattern = '['
message = "Should be skipped."
"#,
        );
        // Must not panic; sub-rule is dropped, parent message is used.
        let v = rule().check(&ctx("os.environ.get('x')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("Generic hit."));
    }

    #[test]
    fn yaml_sub_rules_loaded_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pattern_file = tmp.path().join("konform_patterns.yaml");
        std::fs::write(
            &pattern_file,
            r#"rules:
  - id: CTFW001
    message: "os.environ found."
    pattern: 'os\.environ'
    help: "Contact maintainers."
    sub_rules:
      - pattern:
          - 'baseline_handle'
        message: "baseline_handle access detected."
        help: "Use the core_baseline fixture."
"#,
        )
        .unwrap();

        let rule_inst = KptRule::new(Some(tmp.path().to_path_buf()));
        let cfg = toml::Value::Table(toml::map::Map::new());

        // Sub-rule message for baseline_handle access.
        let v = rule_inst.check(&ctx("os.environ.get('baseline_handle')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("baseline_handle access detected."),
            "YAML sub-rule should fire: {:?}",
            v[0].message
        );
        assert_eq!(v[0].help.as_deref(), Some("Use the core_baseline fixture."));

        // Parent message for an unrelated environ access.
        let v2 = rule_inst.check(&ctx("os.environ.get('OTHER')\n"), &cfg);
        assert_eq!(v2.len(), 1);
        assert!(v2[0].message.contains("os.environ found."));
        assert_eq!(v2[0].help.as_deref(), Some("Contact maintainers."));
    }

    // ── multiline pattern matching ─────────────────────────────────────────

    #[test]
    fn multiline_false_does_not_match_across_lines() {
        // \n in a regex only matches when applied to the full source;
        // line-by-line mode splits on \n so the pattern cannot fire.
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Forbidden sequence."
pattern = 'foo\nbar'
multiline = false
"#,
        );
        let v = rule().check(&ctx("foo\nbar\n"), &cfg);
        assert!(v.is_empty());
    }

    #[test]
    fn multiline_true_matches_across_lines() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Forbidden sequence."
pattern = 'foo\nbar'
multiline = true
"#,
        );
        let v = rule().check(&ctx("foo\nbar\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "KPT001");
    }

    #[test]
    fn multiline_violation_line_number_is_correct() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Forbidden sequence."
pattern = 'foo\nbar'
multiline = true
"#,
        );
        // Match starts on line 2 (1-based) because "header\n" precedes it.
        let v = rule().check(&ctx("header\nfoo\nbar\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].line, 2);
    }

    #[test]
    fn multiline_violation_noqa_respected() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Forbidden sequence."
pattern = 'foo\nbar'
multiline = true
"#,
        );
        // noqa on the first line of the match suppresses the violation.
        let v = rule().check(&ctx("foo  # noqa: KPT001\nbar\n"), &cfg);
        assert!(v.is_empty());
    }

    // ── replacement / auto-fix ────────────────────────────────────────────

    #[test]
    fn replacement_fixes_source() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT001"
message     = "Use logger."
pattern     = 'print\((.*?)\)'
replacement = "logger.info($1)"
"#,
        );
        let src = "print('hello')\n";
        let result = rule().fix(&ctx(src), &cfg).unwrap();
        assert!(result.is_some());
        assert!(result.unwrap().contains("logger.info("));
    }

    #[test]
    fn no_replacement_leaves_fixable_false() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "No bare print."
pattern = 'print\(.*?\)'
"#,
        );
        let v = rule().check(&ctx("print('hello')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(!v[0].fixable);
    }

    #[test]
    fn fixable_true_when_replacement_present() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT001"
message     = "No bare print."
pattern     = 'print\(.*?\)'
replacement = "logger.info()"
"#,
        );
        let v = rule().check(&ctx("print('hello')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(v[0].fixable);
    }

    #[test]
    fn replacement_with_capture_group() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT001"
message     = "Use logger."
pattern     = 'print\((.*?)\)'
replacement = "log($1)"
"#,
        );
        let src = "print('hello')\n";
        let result = rule().fix(&ctx(src), &cfg).unwrap().unwrap();
        assert!(result.contains("log('hello')"));
    }

    #[test]
    fn multiline_replacement_rewrites_source() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT001"
message     = "Replace sequence."
pattern     = 'foo\nbar'
replacement = "foobar"
multiline   = true
"#,
        );
        let src = "foo\nbar\n";
        let result = rule().fix(&ctx(src), &cfg).unwrap();
        assert!(result.is_some());
        let rewritten = result.unwrap();
        assert!(!rewritten.contains("foo\nbar"));
        assert!(rewritten.contains("foobar"));
    }

    #[test]
    fn replacement_no_match_returns_none() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT001"
message     = "Use logger."
pattern     = 'print\(.*?\)'
replacement = "logger.info()"
"#,
        );
        // Source has no match — fix should be a no-op.
        let result = rule().fix(&ctx("x = 1\n"), &cfg).unwrap();
        assert!(result.is_none());
    }
}
