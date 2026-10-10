//! KNQ003: Konform NoQa: a `# noqa` code must suppress something.
//!
//! A suppression that no longer suppresses anything is dead weight: the
//! violation it once hid was fixed, the code moved, or the rule changed. Left
//! in place, it silently swallows the next real violation on that line.
//!
//! ```python
//! # Bad: KNQ003 (`import os` is a module import, KIS001 has nothing to say)
//! import os  # noqa: KIS001
//!
//! # Good
//! import os
//! ```
//!
//! Only codes that konform itself reports are judged: `# noqa: E501` belongs
//! to another tool and is left alone, and so is a bare `# noqa`. A code only
//! counts as konform's when a rule that is **enabled** can report it, so
//! suppressions for rules turned off in this project are not flagged either.
//! KNQ codes are never flagged.
//!
//! Whether a code is used is decided against what the other rules report
//! with `# noqa` ignored ([`super::NoqaAudit`], filled in by the engine
//! because [`Rule::audits_noqa`] is `true`). Codes match by prefix and
//! through `noqa-aliases`, exactly like the suppression itself.
//!
//! The rule is **opt-in**: enable it with `extend-select = ["KNQ003"]`. Like
//! the other KNQ rules it cannot be suppressed by `# noqa`. The fix (safe)
//! removes the unused codes, or the whole directive and its reason when none
//! are left.

use super::docs::{DocSection, Example, RuleDocs, RuleOption};
use super::scope::{build_line_starts, offset_to_line_col};
use super::{noqa_comments, rule_settings, FileContext, NoqaAudit, NoqaComment, Rule};
use crate::types::{Level, Violation};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Once;

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Knq003Rule;

impl Knq003Rule {
    pub fn new() -> Self {
        Self
    }
}

/// One `# noqa` comment with at least one unused konform code.
struct Finding {
    line: usize,
    col: usize,
    end_col: usize,
    unused: Vec<String>,
    /// Bytes of the source to replace, and what to put there.
    replace: Range<usize>,
    replacement: String,
}

/// The `# noqa` comments in `ctx` that list unused konform codes.
fn findings(ctx: &FileContext) -> Vec<Finding> {
    let Some(audit) = ctx.noqa_audit.as_deref() else {
        return Vec::new();
    };
    let source = ctx.source.as_str();
    let line_starts = build_line_starts(source);

    let mut out = Vec::new();
    for NoqaComment { start, end, noqa } in noqa_comments(ctx) {
        let Some(codes) = noqa.codes.as_deref() else {
            continue; // bare `# noqa`: may serve another tool
        };
        let (line, col) = offset_to_line_col(&line_starts, (start + noqa.start) as u32);
        let (_, end_col) = offset_to_line_col(&line_starts, (start + noqa.end) as u32);
        let (used, unused): (Vec<&str>, Vec<&str>) = codes
            .iter()
            .partition(|code| !is_unused(code, line, audit, &ctx.noqa_aliases));
        if unused.is_empty() {
            continue;
        }

        let directive = start + noqa.start..start + noqa.end;
        let (replace, replacement) = if used.is_empty() {
            // Drop the directive, its reason and the whitespace before it.
            let from = source[..directive.start]
                .trim_end_matches([' ', '\t'])
                .len();
            (from..end, String::new())
        } else {
            (directive, format!("# noqa: {}", used.join(", ")))
        };
        out.push(Finding {
            line,
            col,
            end_col,
            unused: unused.into_iter().map(str::to_owned).collect(),
            replace,
            replacement,
        });
    }
    out
}

/// Is `code` (as written in a `# noqa` on `line`) a konform code that
/// suppresses nothing there?
fn is_unused(
    code: &str,
    line: usize,
    audit: &NoqaAudit,
    aliases: &HashMap<String, String>,
) -> bool {
    let prefixes: Vec<&str> = std::iter::once(code)
        .chain(aliases.get(code).map(String::as_str))
        .collect();
    let covers = |rule: &str| prefixes.iter().any(|p| rule.starts_with(p));
    if prefixes.iter().any(|p| p.starts_with("KNQ")) || !audit.codes.iter().any(|c| covers(c)) {
        return false; // a KNQ code, or not ours to judge
    }
    !audit
        .violations
        .iter()
        .any(|v| v.line == line && covers(&v.rule))
}

// ---------------------------------------------------------------------------
// Rule impl
// ---------------------------------------------------------------------------

impl Rule for Knq003Rule {
    fn code(&self) -> &str {
        "KNQ003"
    }

    fn category(&self) -> &str {
        "KNQ"
    }

    fn category_title(&self) -> &str {
        "noqa comments"
    }

    fn config_name(&self) -> &str {
        "unused-noqa"
    }

    fn name(&self) -> &str {
        "unused noqa"
    }

    fn description(&self) -> &str {
        "Flags `# noqa` codes that suppress no konform violation."
    }

    fn opt_in(&self) -> bool {
        true
    }

    fn fixable(&self) -> bool {
        true
    }

    fn audits_noqa(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        // No `has_noqa` handling: a suppression comment must not excuse itself.
        static WARNED: Once = Once::new();
        let level = rule_settings::<Knq003Settings>(cfg, "unused-noqa", &WARNED).level;
        findings(ctx)
            .into_iter()
            .map(|f| {
                let unused: Vec<String> = f.unused.iter().map(|c| format!("`{c}`")).collect();
                Violation {
                    rule: "KNQ003".to_owned(),
                    line: f.line,
                    col: f.col,
                    end_line: f.line,
                    end_col: f.end_col,
                    message: format!("unused `# noqa` (unused: {})", unused.join(", ")),
                    help: Some("Nothing on this line reports these codes; remove them.".to_owned()),
                    level,
                    fixable: true,
                }
            })
            .collect()
    }

    fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
        let mut edits: Vec<Finding> = findings(ctx)
            .into_iter()
            .filter(|f| ctx.wants_fix("KNQ003", f.line, f.col))
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

    fn docs(&self) -> RuleDocs {
        let defaults = Knq003Settings::default();
        RuleDocs {
            what_it_does: "Flags codes in a `# noqa: CODE` comment that suppress nothing: no \
                enabled rule reports a violation with that code on that line. Only codes konform \
                reports are judged, so `# noqa: E501` for another tool and a bare `# noqa` are left \
                alone, and so are codes of rules that are turned off. **Opt-in:** enable it with \
                `extend-select = [\"KNQ003\"]` in `[tool.konform.lint]` (or \
                `--extend-select KNQ003`).",
            why_bad: "A stale suppression hides nothing today but silently swallows the next real \
                violation on that line. Removing it keeps every remaining `# noqa` meaningful.",
            example: Some(Example {
                bad: "import os  # noqa: KIS001\nfrom os.path import join  # noqa: KIS001, KIS002",
                good: "import os\nfrom os.path import join  # noqa: KIS001",
            }),
            sections: vec![
                DocSection {
                    title: "How codes are matched",
                    body: "A code counts as used when a violation on the same line starts with it, \
                        directly or through `noqa-aliases`, the same way the suppression itself \
                        works. A violation filtered out by `per-file-ignores` does not use a code. \
                        KNQ codes are never flagged.",
                },
                DocSection {
                    title: "Suppressing and fixing",
                    body: "This rule cannot be suppressed with `# noqa`; turn it off with `ignore` \
                        or `per-file-ignores`. The fix is safe: it removes the unused codes, or the \
                        whole directive (and its reason) when no code is left.",
                },
            ],
            options: vec![RuleOption {
                name: "level",
                ty: "\"warning\" | \"error\"",
                default: format!("`\"{}\"`", defaults.level),
                description: "Severity of violations.",
            }],
        }
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

/// `[tool.konform.lint.unused-noqa]` settings for KNQ003.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct Knq003Settings {
    level: Level,
}

impl Default for Knq003Settings {
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
    use std::sync::Arc;

    fn cfg() -> toml::Value {
        toml::Value::Table(Default::default())
    }

    /// A violation of `rule` on `line`, as another rule would report it.
    fn hit(rule: &str, line: usize) -> Violation {
        Violation {
            rule: rule.to_owned(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            message: String::new(),
            help: None,
            level: Level::Error,
            fixable: false,
        }
    }

    fn ctx(src: &str, hits: Vec<Violation>) -> FileContext {
        let mut ctx = FileContext::from_source(PathBuf::from("a.py"), src.to_owned());
        ctx.noqa_audit = Some(Arc::new(NoqaAudit {
            violations: hits,
            codes: vec!["KIS001".into(), "KIS002".into(), "KPT001".into()],
        }));
        ctx
    }

    fn check(src: &str, hits: Vec<Violation>) -> Vec<String> {
        Knq003Rule::new()
            .check(&ctx(src, hits), &cfg())
            .into_iter()
            .map(|v| format!("{}:{} {}", v.line, v.col, v.message))
            .collect()
    }

    fn fixed(src: &str, hits: Vec<Violation>) -> Option<String> {
        Knq003Rule::new().fix(&ctx(src, hits), &cfg()).unwrap()
    }

    #[test]
    fn used_codes_are_fine() {
        assert!(check("x = 1  # noqa: KIS001\n", vec![hit("KIS001", 1)]).is_empty());
        // A category prefix is used by any violation it covers.
        assert!(check("x = 1  # noqa: KIS\n", vec![hit("KIS002", 1)]).is_empty());
    }

    #[test]
    fn unused_codes_are_flagged_on_the_directive() {
        assert_eq!(
            check("x = 1  # noqa: KIS001\n", vec![]),
            ["1:7 unused `# noqa` (unused: `KIS001`)"]
        );
        // A violation on another line does not use the code.
        assert_eq!(
            check("x = 1  # noqa: KIS001\n", vec![hit("KIS001", 2)]).len(),
            1
        );
    }

    #[test]
    fn foreign_bare_disabled_and_knq_codes_are_ignored() {
        for src in [
            "x = 1  # noqa: E501\n",
            "x = 1  # noqa\n",
            "x = 1  # noqa: KST001\n", // no enabled KST rule
            "x = 1  # noqa: KNQ001\n",
        ] {
            assert!(check(src, vec![]).is_empty(), "{src}");
        }
    }

    #[test]
    fn aliases_count_as_the_code_they_stand_for() {
        let mut ctx = ctx("x = 1  # noqa: IS001\n", vec![hit("KIS001", 1)]);
        ctx.noqa_aliases.insert("IS001".into(), "KIS001".into());
        assert!(Knq003Rule::new().check(&ctx, &cfg()).is_empty());
        ctx.noqa_audit = Some(Arc::new(NoqaAudit {
            violations: vec![],
            codes: vec!["KIS001".into()],
        }));
        assert_eq!(Knq003Rule::new().check(&ctx, &cfg()).len(), 1);
    }

    #[test]
    fn no_audit_means_no_findings() {
        let ctx = FileContext::from_source(PathBuf::from("a.py"), "x = 1  # noqa: KIS001\n".into());
        assert!(Knq003Rule::new().check(&ctx, &cfg()).is_empty());
    }

    #[test]
    fn fix_keeps_used_and_foreign_codes() {
        assert_eq!(
            fixed(
                "x = 1  # noqa: KIS001, E501, KIS002  # why\n",
                vec![hit("KIS002", 1)]
            )
            .as_deref(),
            Some("x = 1  # noqa: E501, KIS002  # why\n")
        );
    }

    #[test]
    fn fix_drops_the_whole_directive_when_nothing_is_left() {
        assert_eq!(
            fixed("x = 1  # noqa: KIS001  # legacy\ny = 2\n", vec![]).as_deref(),
            Some("x = 1\ny = 2\n")
        );
        // Text before the directive in the same comment survives.
        assert_eq!(
            fixed("x = 1  # type: ignore  # noqa: KIS001\n", vec![]).as_deref(),
            Some("x = 1  # type: ignore\n")
        );
        // A comment on its own line leaves an empty line, not a stray `#`.
        assert_eq!(
            fixed("# noqa: KIS001\nx = 1\n", vec![]).as_deref(),
            Some("\nx = 1\n")
        );
    }

    #[test]
    fn fix_with_nothing_unused_is_none() {
        assert_eq!(
            fixed("x = 1  # noqa: KIS001\n", vec![hit("KIS001", 1)]),
            None
        );
    }
}
