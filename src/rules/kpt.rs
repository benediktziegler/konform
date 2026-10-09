//! KPT001 — Konform Pattern: user-defined regex pattern violations.
//!
//! Checks Python (and other) source files against a set of user-defined
//! regular-expression patterns.  Patterns can be supplied from three sources,
//! tried in priority order:
//!
//! 1. **Inline** `[[tool.konform.lint.user-defined-patterns.rules]]` inside
//!    `pyproject.toml` / `konform.toml`.
//! 2. **Explicit file** referenced by `rules_file = "path"` in
//!    `[tool.konform.lint.user-defined-patterns]`  (`.toml` or `.yaml`).
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
//!   `[tool.konform.lint.user-defined-patterns].level` (default: `"warning"`)
//! * `help`      — optional guidance text surfaced alongside the violation
//! * `sub_rules` — ordered list of refinements; the first sub-rule whose
//!   pattern(s) match the already-flagged line overrides `message` and `help`

use super::docs::{DocSection, Example, RuleDocs, RuleOption};
use super::{has_noqa, FileContext, FixTarget, Rule, RuleDoc};
use crate::types::{Level, Violation};
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    /// Where the pattern came from: a file path, or `"inline config"`.
    source: String,
    /// The regex sources as written by the user (before `(?s)` prefixing).
    raw_regexes: Vec<String>,
    /// The `files` globs as written by the user (empty = every file).
    raw_files: Vec<String>,
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
    /// Markdown summary printed by `konform rule --explain <ID>`.
    fn explain(&self) -> String {
        let mut out = format!("# {} — {}\n\n", self.id, self.message);
        out.push_str(&format!("- **Level:** {}\n", self.level));
        out.push_str(&format!("- **Source:** `{}`\n", self.source));
        let files = if self.raw_files.is_empty() {
            "all files".to_owned()
        } else {
            format!("`{}`", self.raw_files.join("`, `"))
        };
        out.push_str(&format!("- **Files:** {files}\n"));
        for re in &self.raw_regexes {
            out.push_str(&format!("- **Pattern:** `{re}`\n"));
        }
        if self.multiline {
            out.push_str("- **Match:** whole file (multiline)\n");
        }
        if let Some(r) = &self.replacement {
            out.push_str(&format!("- **Fix:** replace with `{r}`\n"));
        }
        if let Some(h) = &self.help {
            out.push_str(&format!("- **Help:** {h}\n"));
        }
        out
    }

    /// Returns `true` when the compiled file-glob set matches `path`.
    ///
    /// Mirrors the multi-candidate strategy used by `engine::per_file_ignored`
    /// so that globs like `src/**/*.py` work whether `path` is:
    /// * already project-root-relative (`src/foo/bar.py`)
    /// * absolute with a `config_dir` prefix (`/project/src/foo/bar.py`)
    /// * absolute with a CWD prefix (same thing reached via a different root)
    /// * a bare filename (`*.py` style)
    fn matches_file(&self, path: &Path, config_dir: Option<&Path>, cwd: Option<&Path>) -> bool {
        self.files
            .as_ref()
            .is_none_or(|gs| glob_matches(gs, path, config_dir, cwd))
    }
}

/// Does `gs` match `path`, trying the path as supplied, relative to
/// `config_dir`, relative to `cwd`, and finally just the file name?
/// Shared with KST's `files` filter.
pub(super) fn glob_matches(
    gs: &GlobSet,
    path: &Path,
    config_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> bool {
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

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

/// KPT001 — user-defined regex pattern rule.
pub struct KptRule {
    /// Directory containing `pyproject.toml` / `konform.toml`.
    /// Used to resolve relative `rules_file` paths and to auto-discover
    /// `konform_patterns.toml` / `konform_patterns.yaml`.
    config_dir: Option<PathBuf>,
    /// Compiled patterns for the rule config last seen by `check` / `fix`.
    ///
    /// Loading reads pattern files and compiles every regex and glob, so it
    /// must happen once per run, not once per file. The key is the rule
    /// config the set was built from; a different config rebuilds it.
    compiled: Mutex<Option<(toml::Value, Arc<Vec<CompiledPattern>>)>>,
}

impl KptRule {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            compiled: Mutex::new(None),
        }
    }

    /// Patterns for `cfg`, loaded and compiled on first use.
    ///
    /// The lock is held while compiling so that parallel workers wait for
    /// one load instead of each repeating it (and each printing the same
    /// invalid-regex diagnostics).
    fn patterns(&self, cfg: &toml::Value) -> Arc<Vec<CompiledPattern>> {
        let mut slot = self.compiled.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((key, patterns)) = slot.as_ref() {
            if key == cfg {
                return Arc::clone(patterns);
            }
        }
        let patterns = Arc::new(load_patterns(
            cfg,
            self.config_dir.as_deref(),
            parse_default_level(cfg),
        ));
        *slot = Some((cfg.clone(), Arc::clone(&patterns)));
        patterns
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

    fn category_title(&self) -> &str {
        "User-defined patterns"
    }

    fn gates_per_violation(&self) -> bool {
        true
    }

    fn config_name(&self) -> &str {
        "user-defined-patterns"
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
        let patterns = self.patterns(cfg);
        if patterns.is_empty() {
            return vec![];
        }

        let lines: Vec<&str> = ctx.source.lines().collect();
        let noqa = ctx.noqa_lines();
        let mut violations = Vec::new();
        let cwd = std::env::current_dir().ok();

        for pattern in patterns.iter() {
            if !ctx.is_enabled(&pattern.id)
                || !pattern.matches_file(&ctx.path, self.config_dir.as_deref(), cwd.as_deref())
            {
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
                        let first_line = noqa.get(line_0).copied().unwrap_or("");
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
                        let noqa_text = noqa.get(i).copied().unwrap_or("");
                        if ctx.ignore_noqa || !has_noqa(noqa_text, &pattern.id, &ctx.noqa_aliases) {
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
        let patterns = self.patterns(cfg);
        let cwd = std::env::current_dir().ok();
        let eol = if ctx.source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };

        let mut current = ctx.source.clone();
        let target = ctx.fix_target.as_ref();

        for pattern in patterns.iter() {
            let Some(replacement) = &pattern.replacement else {
                continue;
            };
            if !ctx.is_enabled(&pattern.id) || target.is_some_and(|t| t.rule != pattern.id) {
                continue;
            }
            if !pattern.matches_file(&ctx.path, self.config_dir.as_deref(), cwd.as_deref()) {
                continue;
            }

            if pattern.multiline {
                if let Some(t) = target {
                    current = replace_multiline_match_at(&current, pattern, replacement, t);
                    continue;
                }
                // Apply replace_all to the full source for each regex.
                for re in &pattern.regexes {
                    current = re.replace_all(&current, replacement.as_str()).into_owned();
                }
            } else {
                // Apply replace_all per line, then reassemble with original EOL.
                let lines: Vec<&str> = current.lines().collect();
                current = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let mut out = (*line).to_owned();
                        if target.is_none_or(|t| t.line == i + 1) {
                            for re in &pattern.regexes {
                                out = re.replace_all(&out, replacement.as_str()).into_owned();
                            }
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

    fn fingerprint(&self, cfg: &toml::Value) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = seahash::SeaHasher::new();
        for p in self.patterns(cfg).iter() {
            (&p.id, &p.message, &p.help, &p.raw_regexes, &p.raw_files).hash(&mut h);
            (p.level.to_string(), p.multiline, &p.replacement).hash(&mut h);
            for sr in &p.sub_rules {
                let regexes: Vec<&str> = sr.patterns.iter().map(Regex::as_str).collect();
                (regexes, &sr.message, &sr.help).hash(&mut h);
            }
        }
        h.finish()
    }

    fn catalog(&self, cfg: &toml::Value) -> Vec<RuleDoc> {
        let patterns = self.patterns(cfg);
        if patterns.is_empty() {
            return vec![RuleDoc::of(self)];
        }
        patterns
            .iter()
            .map(|p| RuleDoc {
                code: p.id.clone(),
                category: self.category().to_owned(),
                config_name: self.config_name().to_owned(),
                name: "User pattern".to_owned(),
                description: p.message.clone(),
                explain: format!("{}\n{}", p.explain(), self.explain()),
            })
            .collect()
    }

    fn docs(&self) -> RuleDocs {
        RuleDocs {
            what_it_does: "Checks every source file against regular expressions (Rust \
                [`regex`](https://docs.rs/regex) syntax) defined in your project \
                configuration, line by line. Each pattern has its own code and is addressable \
                in `select`, `ignore`, `per-file-ignores` and `# noqa`.",
            why_bad: "Some conventions are specific to a project (no bare `print()`, no \
                `breakpoint()` left behind, no unticketed `TODO`). A pattern rule enforces them \
                without writing a plugin.",
            example: Some(Example {
                bad: "print(\"hello\")   # KPT001, with the pattern below",
                good: "logger.info(\"hello\")",
            }),
            sections: vec![
                DocSection {
                    title: "Defining patterns",
                    body: "```toml\n\
                        [[tool.konform.lint.user-defined-patterns.rules]]\n\
                        id      = \"KPT001\"\n\
                        message = \"Use the project logger instead of bare print().\"\n\
                        pattern = '^\\s*print\\s*\\('\n\
                        files   = [\"src/**/*.py\"]\n\
                        level   = \"warning\"\n\
                        ```\n\n\
                        Patterns are loaded from the first available source:\n\n\
                        1. Inline `[[tool.konform.lint.user-defined-patterns.rules]]` in the config file.\n\
                        2. `rules_file = \"path\"` in `[tool.konform.lint.user-defined-patterns]`.\n\
                        3. `konform_patterns.toml`, auto-discovered next to the config file.\n\
                        4. `konform_patterns.yaml` (legacy fallback).\n\n\
                        A standalone file uses plain `[[rules]]` tables.",
                },
                DocSection {
                    title: "Pattern entry fields",
                    body: "| Field | Required | Description |\n\
                        | --- | --- | --- |\n\
                        | `id` | yes | Code shown in output and used for `# noqa` |\n\
                        | `message` | yes | Human-readable description |\n\
                        | `pattern` | yes | Regex, or a list of regexes (any match fires) |\n\
                        | `files` | no | Glob filter; omitted means every file |\n\
                        | `level` | no | `\"error\"` or `\"warning\"`; inherits the table's `level` |\n\
                        | `help` | no | Guidance text shown with the violation |\n\
                        | `multiline` | no | Match against the whole file instead of line by line |\n\
                        | `replacement` | no | Replacement string (`$1`, `$2`, ... captures); makes the pattern fixable |\n\
                        | `sub_rules` | no | Refinements overriding `message`/`help`; the first match wins |",
                },
                DocSection {
                    title: "Sub-rules",
                    body: "Sub-rules refine the message and help for more specific matches. The \
                        first sub-rule whose pattern(s) match the already-flagged line wins.\n\n\
                        ```toml\n\
                        [[rules]]\n\
                        id      = \"KPT901\"\n\
                        message = \"os.environ found, discouraged in test code.\"\n\
                        pattern = 'os\\.environ'\n\
                        sub_rules = [\n\
                          {\n\
                            pattern = ['os\\.environ\\.get\\(', 'os\\.environ\\['],\n\
                            message = \"os.environ special_key access detected.\",\n\
                            help    = \"Use the 'shared_fixture' fixture instead.\",\n\
                          },\n\
                        ]\n\
                        ```\n\n\
                        With `[[rules.sub_rules]]` tables, each sub-rule belongs to the most \
                        recently declared `[[rules]]` entry.",
                },
                DocSection {
                    title: "Fix behavior",
                    body: "A pattern with a `replacement` is fixable; patterns without one are \
                        report-only.",
                },
            ],
            options: vec![
                RuleOption {
                    name: "level",
                    ty: "\"warning\" | \"error\"",
                    default: "`\"warning\"`".to_owned(),
                    description: "Default severity for patterns that don't set their own.",
                },
                RuleOption {
                    name: "rules_file",
                    ty: "str",
                    default: "unset".to_owned(),
                    description: "Load patterns from this file instead of auto-discovery.",
                },
                RuleOption {
                    name: "rules",
                    ty: "array of tables",
                    default: "`[]`".to_owned(),
                    description: "Inline pattern entries.",
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Targeted fix helper
// ---------------------------------------------------------------------------

/// Replace only the multiline match that starts at `target`'s line/column
/// (as reported by `check`), leaving every other match untouched.
fn replace_multiline_match_at(
    source: &str,
    pattern: &CompiledPattern,
    replacement: &str,
    target: &FixTarget,
) -> String {
    for re in &pattern.regexes {
        for caps in re.captures_iter(source) {
            let m = caps.get(0).expect("group 0 always matches");
            let prefix = &source[..m.start()];
            let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
            let col = m.start() - prefix.rfind('\n').map_or(0, |p| p + 1);
            if line == target.line && col == target.col {
                let mut expanded = String::new();
                caps.expand(replacement, &mut expanded);
                let mut out = source.to_owned();
                out.replace_range(m.range(), &expanded);
                return out;
            }
        }
    }
    source.to_owned()
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
    // ── Source 1: inline [[tool.konform.lint.user-defined-patterns.rules]] ───
    if let Some(arr) = cfg.get("rules").and_then(|v| v.as_array()) {
        if !arr.is_empty() {
            let raws: Vec<RawPattern> = arr
                .iter()
                .filter_map(|v| RawPattern::deserialize(v.clone()).ok())
                .collect();
            return compile_patterns(raws, default_level, INLINE_SOURCE);
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

pub(super) fn resolve_path(file_path: &str, config_dir: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(file_path);
    if p.is_absolute() {
        p
    } else if let Some(dir) = config_dir {
        dir.join(p)
    } else {
        p
    }
}

const INLINE_SOURCE: &str = "inline config";

fn load_from_file(path: &Path, default_level: Level) -> Option<Vec<CompiledPattern>> {
    let content = std::fs::read_to_string(path).ok()?;
    let pf: PatternFile = if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
        serde_yaml::from_str(&content).ok()?
    } else {
        toml::from_str(&content).ok()?
    };
    Some(compile_patterns(
        pf.rules,
        default_level,
        &path.display().to_string(),
    ))
}

fn compile_patterns(
    raws: Vec<RawPattern>,
    default_level: Level,
    source: &str,
) -> Vec<CompiledPattern> {
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
                source: source.to_owned(),
                raw_regexes: raw_patterns,
                raw_files: file_globs,
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

    // ── compile once ───────────────────────────────────────────────────────

    #[test]
    fn patterns_are_compiled_once_per_config() {
        let cfg = cfg_with_rules("[[rules]]\nid = \"KPT001\"\nmessage = \"m\"\npattern = 'x'\n");
        let r = rule();
        let first = r.patterns(&cfg);
        let second = r.patterns(&cfg);
        assert!(Arc::ptr_eq(&first, &second), "same cfg must reuse the set");
        // check() must go through the same cached set.
        r.check(&ctx("x = 1\n"), &cfg);
        assert!(Arc::ptr_eq(&first, &r.patterns(&cfg)));
    }

    #[test]
    fn patterns_are_rebuilt_when_config_changes() {
        let a = cfg_with_rules("[[rules]]\nid = \"KPT001\"\nmessage = \"m\"\npattern = 'x'\n");
        let b = cfg_with_rules("[[rules]]\nid = \"KPT002\"\nmessage = \"m\"\npattern = 'y'\n");
        let r = rule();
        assert_eq!(r.patterns(&a)[0].id, "KPT001");
        assert_eq!(r.patterns(&b)[0].id, "KPT002");
    }

    // ── catalog (rule --list / --explain) ──────────────────────────────────

    #[test]
    fn catalog_without_patterns_is_the_umbrella_rule() {
        let docs = rule().catalog(&toml::Value::Table(toml::map::Map::new()));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].code, "KPT001");
    }

    #[test]
    fn catalog_lists_each_user_pattern_with_details() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT030"
message = "No x."
pattern = '^x'
files   = ["src/**/*.py"]
level   = "error"
help    = "Rename it."
"#,
        );
        let docs = rule().catalog(&cfg);
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.code, "KPT030");
        assert_eq!(d.description, "No x.");
        for needle in [
            "No x.",
            "error",
            "src/**/*.py",
            "^x",
            "inline config",
            "Rename it.",
        ] {
            assert!(
                d.explain.contains(needle),
                "missing {needle:?}: {}",
                d.explain
            );
        }
    }

    #[test]
    fn catalog_explains_multiline_and_replacement_patterns() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT031"
message     = "Join it."
pattern     = 'a\nb'
multiline   = true
replacement = "ab"
"#,
        );
        let explain = rule().catalog(&cfg).remove(0).explain;
        assert!(explain.contains("- **Files:** all files"), "{explain}");
        assert!(
            explain.contains("- **Match:** whole file (multiline)"),
            "{explain}"
        );
        assert!(
            explain.contains("- **Fix:** replace with `ab`"),
            "{explain}"
        );
    }

    #[test]
    fn catalog_reports_pattern_file_as_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("konform_patterns.toml"),
            "[[rules]]\nid = \"KPT031\"\nmessage = \"m\"\npattern = 'y'\n",
        )
        .unwrap();
        let r = KptRule::new(Some(tmp.path().to_path_buf()));
        let docs = r.catalog(&toml::Value::Table(toml::map::Map::new()));
        assert_eq!(docs[0].code, "KPT031");
        assert!(docs[0].explain.contains("konform_patterns.toml"));
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
    fn glob_matches_strips_the_cwd_prefix_of_absolute_paths() {
        let gs = GlobSetBuilder::new()
            .add(Glob::new("src/**/*.py").unwrap())
            .build()
            .unwrap();
        let cwd = Path::new("/work/project");
        let path = cwd.join("src/pkg/mod.py");
        // Neither the path itself nor the (different) config dir matches, but
        // relative to the CWD it does.
        assert!(glob_matches(
            &gs,
            &path,
            Some(Path::new("/elsewhere")),
            Some(cwd)
        ));
        assert!(!glob_matches(
            &gs,
            &path,
            Some(Path::new("/elsewhere")),
            None
        ));
    }

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

    #[test]
    fn noqa_text_inside_string_literal_does_not_suppress() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT020"
message = "x assign"
pattern = '^x'
"#,
        );
        let v = rule().check(&ctx("x = \"# noqa\"\n"), &cfg);
        assert_eq!(v.len(), 1, "a string is not a comment: {v:?}");
    }

    #[test]
    fn noqa_in_non_python_file_still_uses_raw_line() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT020"
message = "key"
pattern = '^key'
"#,
        );
        let v = rule().check(&ctx_path("data.yaml", "key: 1  # noqa\n"), &cfg);
        assert!(v.is_empty());
    }

    #[test]
    fn noqa_with_trailing_comma_only_suppresses_listed_codes() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT020"
message = "x assign"
pattern = '^x'
"#,
        );
        let v = rule().check(&ctx("x = 1  # noqa: KIS001,\n"), &cfg);
        assert_eq!(v.len(), 1, "KIS001 noqa must not silence KPT020: {v:?}");
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
pattern = 'special_key'
message = "special_key specific hit."
"#,
        );
        let v = rule().check(&ctx("os.environ.get('special_key')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("special_key specific hit."),
            "sub-rule message should win: {:?}",
            v[0].message
        );
    }

    #[test]
    fn inline_sub_rules_field_form_is_bound_to_its_rule() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "Generic hit."
pattern = 'os\.environ'
help    = "Generic help."
sub_rules = [
  { pattern = ['special_key'], message = "Specific hit.", help = "Use shared_fixture fixture." }
]
"#,
        );
        let v = rule().check(&ctx("os.environ['special_key']\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("Specific hit."));
        assert_eq!(v[0].help.as_deref(), Some("Use shared_fixture fixture."));
    }

    #[test]
    fn sub_rules_stay_scoped_to_declaring_rule() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id      = "KPT001"
message = "First generic."
pattern = 'foo'

[[rules.sub_rules]]
pattern = 'special_key'
message = "First specific."

[[rules]]
id      = "KPT002"
message = "Second generic."
pattern = 'foo'
"#,
        );

        let v = rule().check(&ctx("foo special_key\n"), &cfg);
        assert_eq!(v.len(), 2);

        let first = v.iter().find(|x| x.rule == "KPT001").unwrap();
        assert!(first.message.contains("First specific."));

        let second = v.iter().find(|x| x.rule == "KPT002").unwrap();
        assert!(
            second.message.contains("Second generic."),
            "sub-rules from KPT001 must not affect KPT002"
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
pattern = 'special_key'
message = "Specific hit."
help    = "Use shared_fixture fixture."
"#,
        );
        let v = rule().check(&ctx("os.environ['special_key']\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].help.as_deref(), Some("Use shared_fixture fixture."));
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
pattern = 'special_key'
message = "special_key specific hit."
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
pattern = ['special_key', 'some_other_key']
message = "Specific key hit."
"#,
        );
        // First pattern in the list matches.
        let v1 = rule().check(&ctx("os.environ.get('special_key')\n"), &cfg);
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
pattern = 'special'
message = "First sub-rule."

[[rules.sub_rules]]
pattern = 'special_key'
message = "Second sub-rule."
"#,
        );
        // 'special' matches before 'special_key' is even tested.
        let v = rule().check(&ctx("os.environ.get('special_key')\n"), &cfg);
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
  - id: KPT901
    message: "os.environ found."
    pattern: 'os\.environ'
    help: "Contact maintainers."
    sub_rules:
      - pattern:
          - 'special_key'
        message: "special_key access detected."
        help: "Use the shared_fixture fixture."
"#,
        )
        .unwrap();

        let rule_inst = KptRule::new(Some(tmp.path().to_path_buf()));
        let cfg = toml::Value::Table(toml::map::Map::new());

        // Sub-rule message for special_key access.
        let v = rule_inst.check(&ctx("os.environ.get('special_key')\n"), &cfg);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("special_key access detected."),
            "YAML sub-rule should fire: {:?}",
            v[0].message
        );
        assert_eq!(
            v[0].help.as_deref(),
            Some("Use the shared_fixture fixture.")
        );

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

    fn target(rule: &str, line: usize, col: usize) -> FixTarget {
        FixTarget {
            rule: rule.to_owned(),
            line,
            col,
        }
    }

    #[test]
    fn targeted_fix_rewrites_only_the_target_line_and_pattern_id() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT901"
message     = "Use logger."
pattern     = 'print\((.*?)\)'
replacement = "logger.info($1)"

[[rules]]
id          = "KPT902"
message     = "No foo."
pattern     = 'foo'
replacement = "bar"
"#,
        );
        let mut c = ctx("print(1)\nprint(foo)\n");
        c.fix_target = Some(target("KPT901", 2, 0));
        let fixed = rule().fix(&c, &cfg).unwrap().expect("target line fixed");
        assert_eq!(fixed, "print(1)\nlogger.info(foo)\n");
    }

    #[test]
    fn targeted_multiline_fix_rewrites_only_the_matching_occurrence() {
        let cfg = cfg_with_rules(
            r#"
[[rules]]
id          = "KPT001"
message     = "Replace sequence."
pattern     = 'foo\n(ba.)'
replacement = "foo_$1"
multiline   = true
"#,
        );
        let mut c = ctx("foo\nbar\nfoo\nbaz\n");
        c.fix_target = Some(target("KPT001", 3, 0));
        let fixed = rule().fix(&c, &cfg).unwrap().expect("second match fixed");
        assert_eq!(fixed, "foo\nbar\nfoo_baz\n");
    }
}
