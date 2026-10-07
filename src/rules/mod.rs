//! Rule trait, shared context types, and the rule registry.
//!
//! Every linting rule implements [`Rule`].  The engine calls
//! [`Rule::check`] to find violations and [`Rule::fix`] to rewrite source
//! in-place.  Both the CLI and the LSP build a [`FileContext`] and pass it
//! to the same rule implementations — no duplication of logic.
#![allow(dead_code)]

use crate::config::RuleSelection;
use crate::module_probe::ModuleProbe;
use crate::types::Violation;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::{ModModule, PySourceType, Stmt};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

// ---------------------------------------------------------------------------
// Sub-modules
// ---------------------------------------------------------------------------
pub mod kis001;
pub mod kis002;
pub mod kpt;
pub mod kst;
mod scope;

// ---------------------------------------------------------------------------
// FileContext
// ---------------------------------------------------------------------------

/// Everything a rule needs to know about the file it is checking.
///
/// Constructed once per file and shared across all active rules so that
/// source text is read from disk (or the LSP document store) only once.
#[derive(Debug, Clone)]
pub struct FileContext {
    /// Absolute path to the file being checked.
    pub path: PathBuf,
    /// Full source text (UTF-8).
    pub source: String,
    /// Source split into lines — 0-indexed, no trailing newlines.
    pub lines: Vec<String>,
    /// When `true`, `# noqa` suppression comments are ignored.
    /// Propagated from `CheckInput::ignore_noqa` / `Config::ignore_noqa`.
    pub ignore_noqa: bool,
    /// Alias `# noqa` codes to canonical rule codes / category prefixes.
    /// Propagated from `Config::noqa_aliases`. See [`has_noqa`].
    pub noqa_aliases: HashMap<String, String>,
    /// When set, [`Rule::fix`] must only rewrite the single violation this
    /// identifies (used for per-violation LSP quick-fixes). `None` = fix all.
    pub fix_target: Option<FixTarget>,
    /// `select` / `ignore` lists. Rules that emit several codes (see
    /// [`Rule::gates_per_violation`]) use [`FileContext::is_enabled`] to skip
    /// the codes the user turned off.
    pub selection: RuleSelection,
    /// Lazily parsed AST, shared by every rule and by clones of this context.
    /// Always derived from `source`; see [`FileContext::parsed`].
    parsed: Arc<OnceLock<Parsed<ModModule>>>,
}

/// Identifies one violation by its rule code and 1-based line / 0-based
/// column start, exactly as reported in the [`Violation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixTarget {
    pub rule: String,
    pub line: usize,
    pub col: usize,
}

impl FileContext {
    /// Build a `FileContext` by reading `path` from disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let source = std::fs::read_to_string(path)?;
        Ok(Self::from_source(path.to_path_buf(), source))
    }

    /// Build a `FileContext` from an already-loaded source string.
    ///
    /// Used by the LSP, which keeps documents in memory rather than
    /// reading them on every lint request.
    pub fn from_source(path: PathBuf, source: String) -> Self {
        let lines = source.lines().map(String::from).collect();
        Self {
            path,
            source,
            lines,
            ignore_noqa: false,
            noqa_aliases: HashMap::new(),
            fix_target: None,
            selection: RuleSelection::default(),
            parsed: Arc::new(OnceLock::new()),
        }
    }

    /// The parsed module, computed on first use and then shared by all rules.
    ///
    /// Uses the error-tolerant parser, so tokens (and comments) are available
    /// even for broken files; check [`FileContext::has_valid_syntax`] before
    /// trusting the tree. Must not be called after mutating `source`.
    pub fn parsed(&self) -> &Parsed<ModModule> {
        self.parsed.get_or_init(|| {
            ruff_python_parser::parse_unchecked_source(&self.source, PySourceType::Python)
        })
    }

    /// `true` when `source` parses without syntax errors.
    pub fn has_valid_syntax(&self) -> bool {
        self.parsed().errors().is_empty()
    }

    /// Top-level statements, or an empty slice when the source has syntax
    /// errors (AST rules skip files they cannot parse).
    pub fn stmts(&self) -> &[Stmt] {
        if self.has_valid_syntax() {
            &self.parsed().syntax().body
        } else {
            &[]
        }
    }

    /// Is the violation code `code` enabled by `select` / `ignore`?
    pub fn is_enabled(&self, code: &str) -> bool {
        self.selection.is_enabled(code)
    }

    /// Per-line text to scan for `# noqa` (see [`has_noqa`]).
    ///
    /// For Python files this is only the real comment tokens, so `# noqa`
    /// inside a string literal is ignored. Other file types (KPT can target
    /// them) fall back to the raw line text.
    pub fn noqa_lines(&self) -> Vec<&str> {
        let is_python = self
            .path
            .extension()
            .is_some_and(|e| e == "py" || e == "pyi");
        if is_python && self.source.contains("noqa") {
            comment_lines(&self.source, self.parsed().tokens())
        } else if is_python {
            vec![""; self.lines.len()]
        } else {
            self.lines.iter().map(String::as_str).collect()
        }
    }

    /// Should the fixer rewrite the violation of `rule` starting at
    /// (`line`, `col`)? Always `true` unless a [`FixTarget`] narrows it.
    pub fn wants_fix(&self, rule: &str, line: usize, col: usize) -> bool {
        self.fix_target
            .as_ref()
            .is_none_or(|t| t.rule == rule && t.line == line && t.col == col)
    }
}

// ---------------------------------------------------------------------------
// RuleDoc
// ---------------------------------------------------------------------------

/// One row of `konform rule --list` and the text of `konform rule --explain`.
#[derive(Debug, Clone)]
pub struct RuleDoc {
    pub code: String,
    pub category: String,
    pub config_name: String,
    pub name: String,
    pub description: String,
    pub explain: String,
}

impl RuleDoc {
    /// The documentation entry for `rule` itself.
    pub fn of<R: Rule + ?Sized>(rule: &R) -> Self {
        Self {
            code: rule.code().to_owned(),
            category: rule.category().to_owned(),
            config_name: rule.config_name().to_owned(),
            name: rule.name().to_owned(),
            description: rule.description().to_owned(),
            explain: rule.explain(),
        }
    }
}

// ---------------------------------------------------------------------------
// Rule trait
// ---------------------------------------------------------------------------

/// A single linting or formatting rule.
///
/// Implementations must be `Send + Sync` so the engine can run them in
/// parallel via `rayon`.
pub trait Rule: Send + Sync {
    /// Unique violation code, e.g. `"KIS001"`.
    fn code(&self) -> &str;

    /// Category prefix, e.g. `"KIS"`. Used for `select`/`ignore`/`# noqa`
    /// prefix matching and for `--list-rules` display.
    fn category(&self) -> &str;

    /// Stable config-table name, e.g. `"module-only-imports"`.
    ///
    /// Used by [`crate::config::Config::rule_config`] to look up this rule's
    /// settings under `[tool.konform.lint.<config_name>]`. Kept independent
    /// of [`Rule::code`] / [`Rule::category`] so a rule can be renamed (with
    /// `noqa_aliases` covering old suppression comments) without also
    /// forcing every project's config table to be renamed in lockstep.
    fn config_name(&self) -> &str;

    /// Short human-readable rule name shown in `--list-rules` output.
    fn name(&self) -> &str;

    /// One-line description shown next to the name in `--list-rules` output.
    fn description(&self) -> &str;

    /// `true` for rules that emit violation codes other than [`Rule::code`]
    /// (KPT: one code per user-defined pattern).
    ///
    /// The engine then always runs the rule, and the rule itself must honour
    /// [`FileContext::is_enabled`] in both `check` and `fix`, instead of the
    /// engine gating on `code()` alone.
    fn gates_per_violation(&self) -> bool {
        false
    }

    /// Whether this rule can automatically rewrite violations in-place.
    fn fixable(&self) -> bool {
        false
    }

    /// Whether this rule's fix is unsafe, following Ruff's fix-safety model.
    ///
    /// A safe fix (the default) is guaranteed to preserve the exact meaning
    /// of the code and is applied by plain `--fix`. An unsafe fix might, in
    /// some edge cases, change program behavior (e.g. it can't always tell
    /// a renamed binding apart from an unrelated one) and is only applied
    /// when `--unsafe-fixes` is also passed -- see [`crate::engine::run_fix`].
    fn is_unsafe_fix(&self) -> bool {
        false
    }

    /// Check `ctx` for violations and return them.
    ///
    /// `cfg` is the raw TOML value for this rule's config table, e.g. the
    /// contents of `[tool.konform.lint.module-only-imports]`. Rules that
    /// need no configuration can ignore it.
    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation>;

    /// Rewrite the source in `ctx` to fix all violations (or only the one
    /// named by `ctx.fix_target`, see [`FileContext::wants_fix`]), returning
    /// the new source text, or `None` if there is nothing to change.
    ///
    /// The default implementation is a no-op for rules that are not fixable.
    fn fix(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
        Ok(None)
    }

    /// Documentation entries this rule contributes to `konform rule --list`
    /// and `--explain`.
    ///
    /// The default is a single entry for [`Rule::code`]. Rules whose codes
    /// come from user configuration (KPT) override this to list each one;
    /// `cfg` is the same rule config passed to [`Rule::check`].
    fn catalog(&self, _cfg: &toml::Value) -> Vec<RuleDoc> {
        vec![RuleDoc::of(self)]
    }

    /// Hash of everything besides the checked file that decides this rule's
    /// output and is not part of `cfg` itself -- e.g. patterns loaded from an
    /// external file. Folded into the cache key so editing such inputs
    /// invalidates cached results. The default `0` suits rules driven only
    /// by `cfg` (which is hashed separately).
    fn fingerprint(&self, _cfg: &toml::Value) -> u64 {
        0
    }

    /// Multi-line human-readable explanation with a bad/good code example.
    ///
    /// Printed by `konform rule --explain <CODE>`.
    fn explain(&self) -> String;
}

// ---------------------------------------------------------------------------
// noqa suppression (prefix-aware)
// ---------------------------------------------------------------------------

/// Return `true` if the violation with code `code` is suppressed on `line`
/// by a `# noqa` comment.
///
/// `aliases` maps alias codes (or category prefixes) to the canonical code
/// (or prefix) they stand in for, e.g. `"IS001" -> "KIS001"` so that
/// `# noqa: IS001` suppresses `KIS001` violations during a rule-rename
/// migration. Aliasing an entire category also works, e.g. `"IS" -> "KIS"`.
pub fn has_noqa(line: &str, code: &str, aliases: &HashMap<String, String>) -> bool {
    let Some(noqa_pos) = line.find("# noqa") else {
        return false;
    };
    let rest = line[noqa_pos + 6..].trim_start();

    if rest.is_empty() || !rest.starts_with(':') {
        return true;
    }

    // Empty entries (`# noqa: A,` / `# noqa: A,,B`) are ignored: an empty
    // prefix would otherwise match every code. A list with no codes at all
    // (`# noqa:`) stays a blanket suppression, like Ruff.
    let mut codes = rest
        .trim_start_matches(':')
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .peekable();
    if codes.peek().is_none() {
        return true;
    }
    codes.any(|c| {
        code.starts_with(c)
            || aliases
                .get(c)
                .is_some_and(|target| code.starts_with(target.as_str()))
    })
}

/// Per-line text in which `# noqa` may appear: one entry per source line,
/// holding that line's comment token (or `""` if it has none).
///
/// Going through tokens keeps `# noqa` inside string literals from
/// suppressing anything. Works on syntactically broken files too.
fn comment_lines<'a>(source: &'a str, tokens: &Tokens) -> Vec<&'a str> {
    let mut out = vec![""; source.lines().count()];
    let starts = scope::build_line_starts(source);
    for tok in tokens.iter() {
        if tok.kind() != TokenKind::Comment {
            continue;
        }
        let range = tok.range();
        let (line, _) = scope::offset_to_line_col(&starts, range.start().to_u32());
        if let Some(slot) = out.get_mut(line - 1) {
            *slot = &source[range.start().to_usize()..range.end().to_usize()];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Rule registry
// ---------------------------------------------------------------------------

/// Return the full list of active rules.
pub fn all_rules(
    probe: Arc<ModuleProbe>,
    config_dir: Option<std::path::PathBuf>,
) -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(kis001::Kis001Rule::new(probe)),
        Box::new(kis002::Kis002Rule::new()),
        Box::new(kpt::KptRule::new(config_dir.clone())),
        Box::new(kst::KstRule::new(config_dir)),
    ]
}

#[cfg(test)]
mod noqa_alias_tests {
    use super::*;

    #[test]
    fn has_noqa_matches_canonical_code_without_aliases() {
        let aliases = HashMap::new();
        assert!(has_noqa("x = 1  # noqa: KIS001", "KIS001", &aliases));
        assert!(!has_noqa("x = 1  # noqa: KIS001", "KPT001", &aliases));
    }

    #[test]
    fn has_noqa_matches_via_exact_alias() {
        let mut aliases = HashMap::new();
        aliases.insert("IS001".to_owned(), "KIS001".to_owned());
        assert!(has_noqa("x = 1  # noqa: IS001", "KIS001", &aliases));
        assert!(!has_noqa("x = 1  # noqa: IS001", "KPT001", &aliases));
    }

    #[test]
    fn has_noqa_matches_via_category_alias() {
        let mut aliases = HashMap::new();
        aliases.insert("IS".to_owned(), "KIS".to_owned());
        assert!(has_noqa("x = 1  # noqa: IS", "KIS001", &aliases));
    }
}

#[cfg(test)]
mod noqa_matching_tests {
    use super::*;

    fn none() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn trailing_comma_does_not_suppress_unlisted_codes() {
        assert!(has_noqa("x  # noqa: KIS001,", "KIS001", &none()));
        assert!(!has_noqa("x  # noqa: KIS001,", "KPT020", &none()));
    }

    #[test]
    fn empty_entries_between_codes_are_ignored() {
        assert!(!has_noqa("x  # noqa: KIS001,,KIS002", "KPT020", &none()));
        assert!(has_noqa("x  # noqa: KIS001,,KIS002", "KIS002", &none()));
    }

    #[test]
    fn bare_noqa_and_empty_code_list_stay_blanket() {
        assert!(has_noqa("x  # noqa", "KPT020", &none()));
        assert!(has_noqa("x  # noqa:", "KPT020", &none()));
    }

    fn noqa_lines(src: &str) -> Vec<String> {
        FileContext::from_source(PathBuf::from("a.py"), src.to_owned())
            .noqa_lines()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn noqa_lines_keeps_only_comment_text() {
        let src = "a = 1  # noqa: KIS001\nb = '# noqa'\nc = 3\n";
        let lines = noqa_lines(src);
        assert_eq!(lines[0], "# noqa: KIS001");
        assert_eq!(lines[1], "", "string literal is not a comment");
        assert_eq!(lines[2], "");
    }

    #[test]
    fn noqa_lines_ignores_noqa_inside_multiline_string() {
        let src = "s = '''\n# noqa\n'''\nx = 1  # noqa\n";
        let lines = noqa_lines(src);
        assert_eq!(lines[1], "", "inside a triple-quoted string");
        assert_eq!(lines[3], "# noqa");
    }

    #[test]
    fn noqa_lines_handles_no_trailing_newline_and_empty_source() {
        let lines = noqa_lines("x = 1  # noqa");
        assert_eq!(lines.first().map(String::as_str), Some("# noqa"));
        assert!(noqa_lines("").iter().all(|l| l.is_empty()));
    }

    #[test]
    fn noqa_lines_still_works_on_unparsable_source() {
        let lines = noqa_lines("def (:\nx = 1  # noqa\n");
        assert_eq!(lines[1], "# noqa");
    }

    #[test]
    fn file_context_uses_raw_lines_for_non_python_files() {
        let yaml = FileContext::from_source(PathBuf::from("data.yaml"), "key: 1  # noqa\n".into());
        assert_eq!(yaml.noqa_lines()[0], "key: 1  # noqa");
        let py = FileContext::from_source(PathBuf::from("a.py"), "s = '# noqa'\n".into());
        assert_eq!(py.noqa_lines()[0], "");
    }

    // ── shared lazy parse ──────────────────────────────────────────────────

    fn py(src: &str) -> FileContext {
        FileContext::from_source(PathBuf::from("a.py"), src.to_owned())
    }

    #[test]
    fn parse_happens_once_and_is_shared_with_clones() {
        let ctx = py("import os\n");
        let clone = ctx.clone();
        // Parsed through the clone first; the original must see the same tree.
        let via_clone: *const _ = clone.parsed();
        assert!(std::ptr::eq(via_clone, ctx.parsed()));
        assert!(std::ptr::eq(ctx.parsed(), ctx.parsed()));
    }

    #[test]
    fn stmts_are_available_for_valid_source() {
        let ctx = py("import os\nx = 1\n");
        assert!(ctx.has_valid_syntax());
        assert_eq!(ctx.stmts().len(), 2);
    }

    #[test]
    fn stmts_are_empty_for_syntax_errors_but_comments_survive() {
        let ctx = py("def (:\nx = 1  # noqa\n");
        assert!(!ctx.has_valid_syntax());
        assert!(ctx.stmts().is_empty());
        assert_eq!(ctx.noqa_lines()[1], "# noqa");
    }
}
