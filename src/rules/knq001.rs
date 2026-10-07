//! KNQ001 — Konform NoQa: every `# noqa` comment must say why.
//!
//! A suppression without a reason is a silent promise that someone, once,
//! had a good cause. KNQ001 flags every `# noqa` comment (bare or with
//! codes) that isn't followed by an explanation:
//!
//! ```python
//! # Bad — KNQ001
//! from os.path import join  # noqa: KIS001
//!
//! # Good
//! from os.path import join  # noqa: KIS001  # re-exported for plugins
//! ```
//!
//! The reason is whatever text follows the directive in the same comment,
//! with leading separators (`#`, `-`, `:`) ignored. The second-`#` form is
//! recommended because it reads the same to every tool that parses the
//! comment. See [`super::parse_noqa`] for the exact grammar.
//!
//! The rule is **opt-in** (see [`Rule::opt_in`]): enable it with
//! `extend-select = ["KNQ001"]` or `--extend-select KNQ001`.
//!
//! The rule is deliberately **unsuppressible by inline comments**: it never
//! consults [`super::has_noqa`], so `# noqa: KNQ001` (or a bare `# noqa`)
//! can't excuse itself. Only `select` / `ignore` / `per-file-ignores` turn
//! it off. Only real comment tokens are inspected, so `"# noqa"` inside a
//! string literal is not a violation.

use super::scope::{build_line_starts, offset_to_line_col};
use super::{parse_noqa, FileContext, Rule};
use crate::types::{Level, Violation};
use ruff_python_ast::token::TokenKind;
use ruff_text_size::Ranged;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Knq001Rule;

impl Knq001Rule {
    pub fn new() -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// Rule impl
// ---------------------------------------------------------------------------

impl Rule for Knq001Rule {
    fn code(&self) -> &str {
        "KNQ001"
    }

    fn category(&self) -> &str {
        "KNQ"
    }

    fn config_name(&self) -> &str {
        "noqa-justification"
    }

    fn name(&self) -> &str {
        "noqa justification"
    }

    fn description(&self) -> &str {
        "Requires a reason on every `# noqa` suppression comment."
    }

    fn opt_in(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        // Deliberately no `has_noqa` / `ignore_noqa` handling: this rule
        // polices suppression comments, so one must not be able to excuse
        // itself (see module docs).
        if !ctx.is_python() || !ctx.source.contains("noqa") {
            return Vec::new();
        }
        let level = Knq001Settings::deserialize(cfg.clone())
            .unwrap_or_default()
            .level;
        let source = ctx.source.as_str();
        let line_starts = build_line_starts(source);

        let mut violations = Vec::new();
        for tok in ctx.parsed().tokens().iter() {
            if tok.kind() != TokenKind::Comment {
                continue;
            }
            let range = tok.range();
            let comment_start = range.start().to_usize();
            let Some(noqa) = parse_noqa(&source[comment_start..range.end().to_usize()]) else {
                continue;
            };
            if noqa.has_reason() {
                continue;
            }
            let (line, col) = offset_to_line_col(&line_starts, (comment_start + noqa.start) as u32);
            let (_, end_col) = offset_to_line_col(&line_starts, (comment_start + noqa.end) as u32);
            violations.push(Violation {
                rule: "KNQ001".to_owned(),
                line,
                col,
                end_line: line,
                end_col,
                message: "`# noqa` without a reason".to_owned(),
                help: Some(
                    "Say why the suppression is needed: `# noqa: CODE  # reason`.".to_owned(),
                ),
                level,
                fixable: false,
            });
        }
        violations
    }

    fn explain(&self) -> String {
        "\
KNQ001 — noqa justification [not fixable]

  Requires every `# noqa` comment to carry a reason.

  Opt-in: not run by default. Enable it with `extend-select = [\"KNQ001\"]`
  (or `select = [\"KNQ001\"]`) in [tool.konform.lint], or pass
  `--extend-select KNQ001`.

  Why: a bare suppression hides a violation without recording why that is
  fine. Months later nobody knows whether it is still needed or safe to
  remove. A short reason makes the exception reviewable.

  Bad:
    from os.path import join  # noqa: KIS001
    value = compute()          # noqa

  Good:
    from os.path import join  # noqa: KIS001  # re-exported for plugins
    value = compute()          # noqa: KPT010 - generated code

  The reason is any text after the directive in the same comment; leading
  `#`, `-` and `:` are ignored, but it needs at least one letter or digit.
  Prefer the `# noqa: CODE  # reason` form: it is read the same way by every
  tool that parses the comment.

  Not flagged: `# noqa` inside a string literal (only real comments count).

  This rule cannot be suppressed with `# noqa` (not even `# noqa: KNQ001`),
  and `--ignore-noqa` does not affect it. Turn it off with `ignore` or
  `per-file-ignores` instead.

  Configure in [tool.konform.lint.noqa-justification]:
    level = \"error\"   # default: \"error\" | \"warning\"

  `konform check --add-noqa` appends reasonless comments, which this rule
  then flags until a reason is added.
"
        .to_owned()
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

/// `[tool.konform.lint.noqa-justification]` settings for KNQ001.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Knq001Settings {
    level: Level,
}

impl Default for Knq001Settings {
    fn default() -> Self {
        Self {
            level: Level::Error,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn check_at(path: &str, src: &str, cfg: &toml::Value) -> Vec<Violation> {
        Knq001Rule::new().check(
            &FileContext::from_source(PathBuf::from(path), src.to_owned()),
            cfg,
        )
    }

    fn check(src: &str) -> Vec<Violation> {
        check_at("a.py", src, &toml::Value::Table(Default::default()))
    }

    fn lines(src: &str) -> Vec<usize> {
        check(src).iter().map(|v| v.line).collect()
    }

    #[test]
    fn bare_noqa_is_flagged() {
        assert_eq!(lines("x = 1  # noqa\n"), [1]);
    }

    #[test]
    fn codes_without_reason_are_flagged() {
        assert_eq!(lines("x = 1  # noqa: KIS001\n"), [1]);
        assert_eq!(lines("x = 1  # noqa: KIS001, E501\n"), [1]);
        assert_eq!(lines("x = 1  # noqa:\n"), [1]);
    }

    #[test]
    fn reason_forms_are_accepted() {
        for src in [
            "x = 1  # noqa: KIS001  # legacy api\n",
            "x = 1  # noqa: KIS001 # legacy api\n",
            "x = 1  # noqa: KIS001 - legacy api\n",
            "x = 1  # noqa: KIS001, E501 because legacy\n",
            "x = 1  # noqa  # generated code\n",
            "x = 1  # type: ignore  # noqa: KIS001  # why\n",
        ] {
            assert!(check(src).is_empty(), "should be accepted: {src:?}");
        }
    }

    #[test]
    fn separators_alone_are_not_a_reason() {
        assert_eq!(lines("x = 1  # noqa: KIS001  #\n"), [1]);
        assert_eq!(lines("x = 1  # noqa: KIS001 - \n"), [1]);
        assert_eq!(lines("x = 1  # noqa: KIS001  # ...\n"), [1]);
    }

    #[test]
    fn noqa_in_string_literal_is_ignored() {
        assert!(check("s = '# noqa'\n").is_empty());
        assert!(check("s = '''\n# noqa\n'''\n").is_empty());
    }

    #[test]
    fn rule_cannot_suppress_itself() {
        assert_eq!(lines("x = 1  # noqa: KNQ001\n"), [1]);
        assert_eq!(lines("x = 1  # noqa: KNQ\n"), [1]);
        assert_eq!(lines("x = 1  # noqa\n"), [1]);
    }

    #[test]
    fn ignore_noqa_flag_does_not_disable_the_rule() {
        let mut ctx = FileContext::from_source(PathBuf::from("a.py"), "x = 1  # noqa\n".into());
        ctx.ignore_noqa = true;
        let cfg = toml::Value::Table(Default::default());
        assert_eq!(Knq001Rule::new().check(&ctx, &cfg).len(), 1);
    }

    #[test]
    fn reports_each_offending_line_only() {
        let src = "a = 1  # noqa: A  # ok\nb = 2  # noqa: B\nc = 3\nd = 4  # noqa\n";
        assert_eq!(lines(src), [2, 4]);
    }

    #[test]
    fn span_covers_the_directive_not_the_whole_comment() {
        let v = &check("x = 1  # noqa: KIS001\n")[0];
        assert_eq!((v.line, v.col, v.end_line, v.end_col), (1, 7, 1, 21));
        // Text after the directive (here a lone `#`) stays outside the span.
        let v = &check("x = 1  # noqa: KIS001 #\n")[0];
        assert_eq!((v.col, v.end_col), (7, 21));
    }

    #[test]
    fn works_on_unparsable_source() {
        assert_eq!(lines("def (:\nx = 1  # noqa\n"), [2]);
    }

    #[test]
    fn non_python_files_are_skipped() {
        let cfg = toml::Value::Table(Default::default());
        assert!(check_at("data.yaml", "key: 1  # noqa\n", &cfg).is_empty());
    }

    #[test]
    fn level_defaults_to_error_and_is_configurable() {
        assert_eq!(check("x  # noqa\n")[0].level, Level::Error);
        let cfg: toml::Value = toml::from_str("level = \"warning\"").unwrap();
        let v = check_at("a.py", "x  # noqa\n", &cfg);
        assert_eq!(v[0].level, Level::Warning);
    }
}
