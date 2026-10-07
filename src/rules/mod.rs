//! Rule trait, shared context types, and the rule registry.
//!
//! Every linting rule implements [`Rule`].  The engine calls
//! [`Rule::check`] to find violations and [`Rule::fix`] to rewrite source
//! in-place.  Both the CLI and the LSP build a [`FileContext`] and pass it
//! to the same rule implementations — no duplication of logic.

use crate::config::RuleSelection;
use crate::module_probe::ModuleProbe;
use crate::types::Violation;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::{ModModule, PySourceType, Stmt};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

// ---------------------------------------------------------------------------
// Sub-modules
// ---------------------------------------------------------------------------
pub mod docs;
pub mod kis001;
pub mod kis002;
pub mod knq001;
pub mod knq002;
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

    /// `true` for `.py` / `.pyi` files (KPT can also target other file types).
    pub fn is_python(&self) -> bool {
        self.path
            .extension()
            .is_some_and(|e| e == "py" || e == "pyi")
    }

    /// Per-line text to scan for `# noqa` (see [`has_noqa`]).
    ///
    /// For Python files this is only the real comment tokens, so `# noqa`
    /// inside a string literal is ignored. Other file types (KPT can target
    /// them) fall back to the raw line text.
    pub fn noqa_lines(&self) -> Vec<&str> {
        let is_python = self.is_python();
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

/// Outcome of the embedded self-tests of one user-defined rule.
#[derive(Debug)]
pub struct SelfTestReport {
    /// The user rule's id.
    pub code: String,
    pub passed: usize,
    /// One `valid[i]: …` / `invalid[i]: …` line per failed case.
    pub failures: Vec<String>,
}

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

    /// Human-readable name of the category, e.g. `"Import style"`; shown in
    /// the generated rule docs next to the prefix.
    fn category_title(&self) -> &str {
        self.category()
    }

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

    /// `true` for rules that are off by default.
    ///
    /// An opt-in rule runs only when `select` or `extend-select` names it
    /// (or a prefix of its code); an empty `select` does not enable it. See
    /// [`crate::config::RuleSelection::runs_rule`].
    fn opt_in(&self) -> bool {
        false
    }

    /// Whether this rule can automatically rewrite violations in-place.
    ///
    /// Used by the generated rule docs (fix availability).
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

    /// Run the self-tests embedded in user rule definitions (`konform rule
    /// --test`). One report per user rule; empty for built-in rules.
    fn self_tests(&self, _cfg: &toml::Value) -> Vec<SelfTestReport> {
        vec![]
    }

    /// Rewrite the source in `ctx` to fix all violations (or only the one
    /// named by `ctx.fix_target`, see [`FileContext::wants_fix`]), returning
    /// the new source text, or `None` if there is nothing to change.
    ///
    /// The default implementation is a no-op for rules that are not fixable.
    fn fix(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
        Ok(None)
    }

    /// Problems in this rule's user configuration (e.g. a KST rule that
    /// failed to compile), one human-readable message each. Offending
    /// entries are skipped by `check`; the CLI turns these into a hard error
    /// and the LSP surfaces them to the user. Empty when the config is fine.
    fn config_errors(&self, _cfg: &toml::Value) -> Vec<String> {
        Vec::new()
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

    /// Whether the rule runs when no `select` is configured. Listed on the
    /// generated "Default rules" page.
    fn enabled_by_default(&self) -> bool {
        !self.opt_in()
    }

    /// Prose, examples and options for the rule's documentation page.
    ///
    /// Everything else on the page (code, name, config table, fix
    /// availability and safety) is read from the other methods of this trait,
    /// so it can't drift from the rule's behaviour. Every registered rule must
    /// fill in at least `what_it_does` (enforced by a test).
    fn docs(&self) -> docs::RuleDocs {
        docs::RuleDocs::default()
    }

    /// Full Markdown documentation, printed by `konform rule --explain <CODE>`
    /// and published on the docs site. Rendered from [`Rule::docs`].
    fn explain(&self) -> String {
        docs::render_rule_page(self)
    }
}

// ---------------------------------------------------------------------------
// noqa suppression (prefix-aware)
// ---------------------------------------------------------------------------

/// A parsed `# noqa` directive, see [`parse_noqa`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Noqa<'a> {
    /// Byte offset of `# noqa` in the parsed text.
    pub start: usize,
    /// Byte offset just past the directive (`# noqa` or `# noqa: A, B`);
    /// whatever follows is the [`Noqa::reason`].
    pub end: usize,
    /// The listed codes / category prefixes. `None` for a blanket `# noqa`
    /// (bare, or `# noqa:` with nothing after the colon). `Some(vec![])`
    /// means the colon is followed only by text that is not a code
    /// (`# noqa: because`), which suppresses nothing.
    pub codes: Option<Vec<&'a str>>,
    /// Free text after the directive with surrounding separators (`#`, `-`,
    /// `:`, `,`, whitespace) trimmed, e.g. `why` for `# noqa: A  # why`.
    pub reason: &'a str,
}

impl Noqa<'_> {
    /// Does the comment explain itself? Separators alone (`# noqa: A  # -`)
    /// do not count; the reason needs at least one letter or digit.
    pub fn has_reason(&self) -> bool {
        self.reason.chars().any(char::is_alphanumeric)
    }
}

/// A real `# noqa` comment in a Python file, see [`noqa_comments`].
#[derive(Debug, Clone)]
pub struct NoqaComment<'a> {
    /// Byte offset of the whole comment (its leading `#`) in the source.
    pub start: usize,
    /// Byte offset just past the comment (before the newline).
    pub end: usize,
    /// The directive; its offsets are relative to `start`.
    pub noqa: Noqa<'a>,
}

/// Every comment token of a Python file that holds a `# noqa` directive.
///
/// Text inside string literals is never returned, and neither is anything
/// in non-Python files. Works on unparsable sources (the parser is
/// error-tolerant), so suppression-comment rules still see broken files.
pub fn noqa_comments(ctx: &FileContext) -> Vec<NoqaComment<'_>> {
    if !ctx.is_python() || !ctx.source.contains("noqa") {
        return Vec::new();
    }
    let source = ctx.source.as_str();
    ctx.parsed()
        .tokens()
        .iter()
        .filter(|tok| tok.kind() == TokenKind::Comment)
        .filter_map(|tok| {
            let (start, end) = (tok.range().start().to_usize(), tok.range().end().to_usize());
            parse_noqa(&source[start..end]).map(|noqa| NoqaComment { start, end, noqa })
        })
        .collect()
}

/// `A`, `KIS`, `KIS001`, `E501`: uppercase letters then optional digits.
fn is_noqa_code(token: &str) -> bool {
    let letters = token.bytes().take_while(u8::is_ascii_uppercase).count();
    letters > 0 && token[letters..].bytes().all(|b| b.is_ascii_digit())
}

fn is_noqa_separator(c: char) -> bool {
    c.is_whitespace() || c == ','
}

/// Trim list separators and comment punctuation off both ends of a reason.
fn trim_reason(s: &str) -> &str {
    s.trim_matches(|c: char| is_noqa_separator(c) || "#-–—:".contains(c))
}

/// Parse the first `# noqa` directive in `text` (a line or a comment token).
///
/// Codes are separated by commas and/or whitespace and end at the first
/// token that is not a code, so trailing explanations neither break nor
/// widen the suppression: `# noqa: KIS001  # legacy API` lists `KIS001` and
/// carries the reason `legacy API`.
pub fn parse_noqa(text: &str) -> Option<Noqa<'_>> {
    const MARKER: &str = "# noqa";
    let start = text.find(MARKER)?;
    let after_marker = start + MARKER.len();
    let rest = text[after_marker..].trim_start();

    if !rest.starts_with(':') {
        return Some(Noqa {
            start,
            end: after_marker,
            codes: None,
            reason: trim_reason(&text[after_marker..]),
        });
    }

    // Byte offset just past the colon.
    let mut end = text.len() - rest.len() + 1;
    let mut codes = Vec::new();
    loop {
        let tail = text[end..].trim_start_matches(is_noqa_separator);
        let token_start = text.len() - tail.len();
        let token_len = tail
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tail.len());
        let token = &tail[..token_len];
        if !is_noqa_code(token) {
            break;
        }
        codes.push(token);
        end = token_start + token_len;
    }

    // `# noqa:` / `# noqa: ,` with no codes and no text stays a blanket
    // suppression, like Ruff. Empty entries (`A,,B`) are skipped above: an
    // empty prefix would otherwise match every code.
    let blanket = codes.is_empty() && text[end..].chars().all(is_noqa_separator);
    Some(Noqa {
        start,
        end,
        codes: if blanket { None } else { Some(codes) },
        reason: trim_reason(&text[end..]),
    })
}

/// Return `true` if the violation with code `code` is suppressed on `line`
/// by a `# noqa` comment.
///
/// `aliases` maps alias codes (or category prefixes) to the canonical code
/// (or prefix) they stand in for, e.g. `"IS001" -> "KIS001"` so that
/// `# noqa: IS001` suppresses `KIS001` violations during a rule-rename
/// migration. Aliasing an entire category also works, e.g. `"IS" -> "KIS"`.
pub fn has_noqa(line: &str, code: &str, aliases: &HashMap<String, String>) -> bool {
    let Some(noqa) = parse_noqa(line) else {
        return false;
    };
    let Some(codes) = noqa.codes else {
        return true;
    };
    codes.into_iter().any(|c| {
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
        Box::new(knq001::Knq001Rule::new()),
        Box::new(knq002::Knq002Rule::new()),
        Box::new(kpt::KptRule::new(config_dir.clone())),
        Box::new(kst::KstRule::new(config_dir)),
    ]
}

#[cfg(test)]
mod docs_tests {
    use super::*;

    #[test]
    fn every_registered_rule_is_documented() {
        for rule in all_rules(Arc::new(ModuleProbe::default()), None) {
            let docs = rule.docs();
            assert!(
                !docs.what_it_does.trim().is_empty(),
                "{} has no `what_it_does` docs",
                rule.code()
            );
            assert!(
                !docs.options.is_empty(),
                "{} documents no options",
                rule.code()
            );
            assert!(
                !rule.category_title().is_empty(),
                "{} has no category title",
                rule.code()
            );
        }
    }
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

    #[test]
    fn reason_after_codes_does_not_break_suppression() {
        for line in [
            "x  # noqa: KIS001  # legacy api",
            "x  # noqa: KIS001 - legacy api",
            "x  # noqa: KIS001, KPT020 because legacy",
            "x  # noqa:KIS001#why",
        ] {
            assert!(has_noqa(line, "KIS001", &none()), "{line}");
        }
        assert!(!has_noqa("x  # noqa: KIS001  # reason", "KPT020", &none()));
    }

    #[test]
    fn reason_after_blanket_noqa_stays_blanket() {
        assert!(has_noqa("x  # noqa  # generated", "KPT020", &none()));
    }

    #[test]
    fn non_code_text_after_colon_suppresses_nothing() {
        assert!(!has_noqa("x  # noqa: because", "KIS001", &none()));
        assert!(!has_noqa("x  # noqa: kis001", "KIS001", &none()));
    }

    #[test]
    fn parse_noqa_splits_directive_from_reason() {
        let line = "x  # noqa: KIS001, E501  # legacy api";
        let n = parse_noqa(line).unwrap();
        assert_eq!(n.codes.as_deref(), Some(["KIS001", "E501"].as_slice()));
        assert_eq!(&line[n.start..n.end], "# noqa: KIS001, E501");
        assert_eq!(n.reason, "legacy api");
        assert!(n.has_reason());
    }

    #[test]
    fn parse_noqa_without_reason() {
        for line in [
            "# noqa",
            "# noqa:",
            "# noqa: A,",
            "# noqa: A  #",
            "# noqa: A - ",
        ] {
            let n = parse_noqa(line).unwrap();
            assert!(!n.has_reason(), "{line}: {n:?}");
        }
        assert_eq!(parse_noqa("# noqa").unwrap().codes, None);
        assert_eq!(parse_noqa("# noqa:").unwrap().codes, None);
        assert!(parse_noqa("# just a comment").is_none());
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
