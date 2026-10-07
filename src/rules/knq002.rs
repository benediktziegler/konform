//! KNQ002 — Konform NoQa style: put the reason in its own comment.
//!
//! A reason glued onto the directive reads differently to every tool that
//! parses suppression comments (mypy, for one, rejects stray text after
//! `# type: ignore[...]`). Starting a second `#` comment is understood the
//! same way everywhere, and it makes the reason easy to find:
//!
//! ```python
//! # Bad — KNQ002
//! from os.path import join  # noqa: KIS001 re-exported for plugins
//! from os.path import join  # noqa: KIS001 - re-exported for plugins
//!
//! # Good
//! from os.path import join  # noqa: KIS001  # re-exported for plugins
//! ```
//!
//! The fix is **safe**: it only moves existing text behind a `#`, and the
//! set of suppressed codes does not change. Comments that already carry a
//! `#`-separated reason, or no reason at all (that is KNQ001's concern), are
//! left alone. So is `# noqa: <non-code text>`, which may be a mistyped code
//! such as `# noqa: kis001` — rewriting it would hide the typo.
//!
//! Like KNQ001 the rule is **opt-in** and never consults
//! [`super::has_noqa`]: a bare `# noqa` would otherwise suppress the very
//! rule that polices it.

use super::scope::{build_line_starts, offset_to_line_col};
use super::{noqa_comments, rule_settings, FileContext, NoqaComment, Rule};
use crate::types::{Level, Violation};
use anyhow::Result;
use serde::Deserialize;
use std::sync::Once;

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Knq002Rule;

impl Knq002Rule {
    pub fn new() -> Self {
        Self
    }
}

/// One badly placed reason, with everything `check` and `fix` need.
struct Finding {
    line: usize,
    col: usize,
    end_col: usize,
    /// Absolute byte range replaced by the fix: the text after the directive.
    replace: std::ops::Range<usize>,
    replacement: String,
}

fn findings(ctx: &FileContext) -> Vec<Finding> {
    let comments = noqa_comments(ctx);
    if comments.is_empty() {
        return Vec::new();
    }
    let source = ctx.source.as_str();
    let line_starts = build_line_starts(source);

    comments
        .iter()
        .filter_map(|c| finding(c, source, &line_starts))
        .collect()
}

fn finding(c: &NoqaComment, source: &str, line_starts: &[u32]) -> Option<Finding> {
    let noqa = &c.noqa;
    // `# noqa: kis001` lists no code; leave possible typos alone.
    if matches!(&noqa.codes, Some(codes) if codes.is_empty()) || !noqa.has_reason() {
        return None;
    }
    let directive_end = c.start + noqa.end;
    let rest = &source[directive_end..c.end];
    let text = rest.trim();
    if text.starts_with('#') {
        return None;
    }
    let text_start = directive_end + (rest.len() - rest.trim_start().len());
    let (line, col) = offset_to_line_col(line_starts, text_start as u32);
    let (_, end_col) = offset_to_line_col(line_starts, (text_start + text.len()) as u32);
    Some(Finding {
        line,
        col,
        end_col,
        replace: directive_end..c.end,
        replacement: format!("  # {}", noqa.reason),
    })
}

// ---------------------------------------------------------------------------
// Rule impl
// ---------------------------------------------------------------------------

impl Rule for Knq002Rule {
    fn code(&self) -> &str {
        "KNQ002"
    }

    fn category(&self) -> &str {
        "KNQ"
    }

    fn config_name(&self) -> &str {
        "noqa-style"
    }

    fn name(&self) -> &str {
        "noqa style"
    }

    fn description(&self) -> &str {
        "Requires the reason on a `# noqa` comment to be its own `# ...` comment."
    }

    fn opt_in(&self) -> bool {
        true
    }

    fn fixable(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        static WARNED: Once = Once::new();
        let level = rule_settings::<Knq002Settings>(cfg, "noqa-style", &WARNED).level;
        findings(ctx)
            .into_iter()
            .map(|f| Violation {
                rule: "KNQ002".to_owned(),
                line: f.line,
                col: f.col,
                end_line: f.line,
                end_col: f.end_col,
                message: "reason after `# noqa` should be its own comment".to_owned(),
                help: Some("Write `# noqa: CODE  # reason`.".to_owned()),
                level,
                fixable: true,
            })
            .collect()
    }

    fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
        let mut edits: Vec<Finding> = findings(ctx)
            .into_iter()
            .filter(|f| ctx.wants_fix("KNQ002", f.line, f.col))
            .collect();
        if edits.is_empty() {
            return Ok(None);
        }
        // Back to front, so earlier offsets stay valid.
        edits.sort_by_key(|f| std::cmp::Reverse(f.replace.start));
        let mut out = ctx.source.clone();
        for f in edits {
            out.replace_range(f.replace, &f.replacement);
        }
        Ok(Some(out))
    }

    fn explain(&self) -> String {
        "\
KNQ002 — noqa style [fixable]

  Requires the reason on a `# noqa` comment to start its own `#` comment.

  Opt-in: not run by default. Enable it with `extend-select = [\"KNQ002\"]`
  (or `select = [\"KNQ\"]`) in [tool.konform.lint], or pass
  `--extend-select KNQ002`.

  Why: a reason glued onto the directive is read differently by different
  tools (mypy rejects stray text after `# type: ignore[...]`, for example).
  A second `#` comment is understood the same way everywhere.

  Bad:
    from os.path import join  # noqa: KIS001 re-exported for plugins
    value = compute()          # noqa: KPT010 - generated code

  Good:
    from os.path import join  # noqa: KIS001  # re-exported for plugins
    value = compute()          # noqa: KPT010  # generated code

  The fix is safe: it only moves the existing text behind a `#`; the
  suppressed codes stay the same. Comments without a reason are KNQ001's
  concern, and `# noqa: kis001` (lowercase, so not a code) is left alone in
  case it is a typo.

  Like KNQ001 this rule cannot be suppressed with `# noqa` (a bare one would
  silence it) and `--ignore-noqa` does not affect it. Turn it off with
  `ignore` or `per-file-ignores`.

  Configure in [tool.konform.lint.noqa-style]:
    level = \"error\"   # default: \"error\" | \"warning\"
"
        .to_owned()
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

/// `[tool.konform.lint.noqa-style]` settings for KNQ002.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct Knq002Settings {
    level: Level,
}

impl Default for Knq002Settings {
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

    fn cfg() -> toml::Value {
        toml::Value::Table(Default::default())
    }

    fn ctx(src: &str) -> FileContext {
        FileContext::from_source(PathBuf::from("a.py"), src.to_owned())
    }

    fn check(src: &str) -> Vec<Violation> {
        Knq002Rule::new().check(&ctx(src), &cfg())
    }

    fn fixed(src: &str) -> Option<String> {
        Knq002Rule::new().fix(&ctx(src), &cfg()).unwrap()
    }

    #[test]
    fn glued_reasons_are_flagged_and_fixed() {
        for (src, want) in [
            (
                "x = 1  # noqa: KIS001 legacy api\n",
                "x = 1  # noqa: KIS001  # legacy api\n",
            ),
            (
                "x = 1  # noqa: KIS001 - legacy api\n",
                "x = 1  # noqa: KIS001  # legacy api\n",
            ),
            (
                "x = 1  # noqa: KIS001, E501 because legacy\n",
                "x = 1  # noqa: KIS001, E501  # because legacy\n",
            ),
            (
                "x = 1  # noqa: KIS001, legacy\n",
                "x = 1  # noqa: KIS001  # legacy\n",
            ),
            (
                "x = 1  # noqa generated code\n",
                "x = 1  # noqa  # generated code\n",
            ),
            (
                "x = 1  # type: ignore  # noqa: KIS001 why\n",
                "x = 1  # type: ignore  # noqa: KIS001  # why\n",
            ),
        ] {
            assert_eq!(check(src).len(), 1, "{src:?}");
            assert_eq!(fixed(src).as_deref(), Some(want), "{src:?}");
        }
    }

    #[test]
    fn fix_is_idempotent_and_clears_the_violation() {
        let out = fixed("x = 1  # noqa: KIS001 legacy api\n").unwrap();
        assert!(check(&out).is_empty());
        assert_eq!(fixed(&out), None);
    }

    #[test]
    fn well_formed_and_reasonless_comments_are_left_alone() {
        for src in [
            "x = 1  # noqa: KIS001  # legacy api\n",
            "x = 1  # noqa: KIS001 # legacy api\n",
            "x = 1  # noqa: KIS001#why\n",
            "x = 1  # noqa  # generated\n",
            // No reason at all: KNQ001's job.
            "x = 1  # noqa: KIS001\n",
            "x = 1  # noqa\n",
            "x = 1  # noqa: KIS001  #\n",
        ] {
            assert!(check(src).is_empty(), "{src:?}");
            assert_eq!(fixed(src), None, "{src:?}");
        }
    }

    #[test]
    fn non_code_text_after_the_colon_is_not_rewritten() {
        // Possibly a mistyped code; moving it behind a `#` would hide that.
        for src in ["x = 1  # noqa: kis001\n", "x = 1  # noqa: because\n"] {
            assert!(check(src).is_empty(), "{src:?}");
        }
    }

    #[test]
    fn string_literals_and_non_python_files_are_ignored() {
        assert!(check("s = '# noqa: KIS001 why'\n").is_empty());
        let yaml =
            FileContext::from_source(PathBuf::from("a.yaml"), "k: 1  # noqa: A why\n".into());
        assert!(Knq002Rule::new().check(&yaml, &cfg()).is_empty());
    }

    #[test]
    fn blanket_noqa_cannot_silence_the_rule() {
        // `# noqa generated code` is a blanket suppression; it must not
        // excuse itself.
        assert_eq!(check("x = 1  # noqa generated code\n").len(), 1);
        let mut c = ctx("x = 1  # noqa generated code\n");
        c.ignore_noqa = true;
        assert_eq!(Knq002Rule::new().check(&c, &cfg()).len(), 1);
    }

    #[test]
    fn span_covers_the_reason_text() {
        let v = &check("x = 1  # noqa: KIS001 legacy api\n")[0];
        assert_eq!((v.line, v.col, v.end_line, v.end_col), (1, 22, 1, 32));
        assert!(v.fixable);
    }

    #[test]
    fn fixes_every_line_and_keeps_the_rest() {
        let src =
            "a = 1  # noqa: A why\nb = 2\nc = 3  # noqa: C  # ok\nd = 4  # noqa: D -- later\n";
        let want =
            "a = 1  # noqa: A  # why\nb = 2\nc = 3  # noqa: C  # ok\nd = 4  # noqa: D  # later\n";
        assert_eq!(fixed(src).as_deref(), Some(want));
    }

    #[test]
    fn fix_target_narrows_to_one_violation() {
        let src = "a = 1  # noqa: A why\nb = 2  # noqa: B why\n";
        let mut c = ctx(src);
        let v = &check(src)[1];
        c.fix_target = Some(super::super::FixTarget {
            rule: "KNQ002".to_owned(),
            line: v.line,
            col: v.col,
        });
        let out = Knq002Rule::new().fix(&c, &cfg()).unwrap().unwrap();
        assert_eq!(out, "a = 1  # noqa: A why\nb = 2  # noqa: B  # why\n");
    }

    #[test]
    fn works_on_unparsable_source() {
        assert_eq!(check("def (:\nx = 1  # noqa: A why\n").len(), 1);
    }

    #[test]
    fn level_is_configurable() {
        let cfg: toml::Value = toml::from_str("level = \"warning\"").unwrap();
        let v = Knq002Rule::new().check(&ctx("x = 1  # noqa: A why\n"), &cfg);
        assert_eq!(v[0].level, Level::Warning);
    }

    #[test]
    fn invalid_settings_fall_back_to_defaults() {
        for cfg in ["levle = \"warning\"", "level = 3"] {
            let cfg: toml::Value = toml::from_str(cfg).unwrap();
            let v = Knq002Rule::new().check(&ctx("x = 1  # noqa: A why\n"), &cfg);
            assert_eq!(v[0].level, Level::Error);
        }
    }
}
