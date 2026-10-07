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
//!
//! Two knobs tune it (see [`Knq001Settings`]):
//!
//! * **Placeholder reasons.** A reason that is nothing but `ok`, `todo`,
//!   `fix later`, … says as little as no reason. The match is on the whole
//!   (case- and whitespace-insensitive) reason, never on a substring, so
//!   `legacy api` or `todo: drop in v3` are fine. A short default list can
//!   be replaced (`placeholder-reasons`) or extended
//!   (`extend-placeholder-reasons`).
//! * **Exempt codes.** Comments whose codes are *all* exempt need no reason,
//!   for rules whose suppression explains itself (`exempt-codes`).

use super::docs::{DocSection, Example, RuleDocs, RuleOption};
use super::scope::{build_line_starts, offset_to_line_col};
use super::{noqa_comments, FileContext, NoqaComment, Rule};
use crate::types::{Level, Violation};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

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
        let settings = Knq001Settings::deserialize(cfg.clone()).unwrap_or_default();
        let level = settings.level;
        let policy = Policy::new(&settings);
        let source = ctx.source.as_str();
        let line_starts = build_line_starts(source);

        let mut violations = Vec::new();
        for NoqaComment { start, noqa, .. } in noqa_comments(ctx) {
            if policy.is_exempt(noqa.codes.as_deref(), &ctx.noqa_aliases) {
                continue;
            }
            // Span the directive when the reason is missing, the reason
            // text when it is a placeholder.
            let (message, span) = if !noqa.has_reason() {
                (
                    "`# noqa` without a reason".to_owned(),
                    (start + noqa.start, start + noqa.end),
                )
            } else if policy.is_placeholder(noqa.reason) {
                let comment = &source[start..];
                let from = start + noqa.end + comment[noqa.end..].find(noqa.reason).unwrap_or(0);
                (
                    format!("`# noqa` reason `{}` says nothing", noqa.reason),
                    (from, from + noqa.reason.len()),
                )
            } else {
                continue;
            };
            let (line, col) = offset_to_line_col(&line_starts, span.0 as u32);
            let (_, end_col) = offset_to_line_col(&line_starts, span.1 as u32);
            violations.push(Violation {
                rule: "KNQ001".to_owned(),
                line,
                col,
                end_line: line,
                end_col,
                message,
                help: Some(
                    "Say why the suppression is needed: `# noqa: CODE  # reason`.".to_owned(),
                ),
                level,
                fixable: false,
            });
        }
        violations
    }

    fn category_title(&self) -> &str {
        "noqa comments"
    }

    fn docs(&self) -> RuleDocs {
        let defaults = Knq001Settings::default();
        RuleDocs {
            what_it_does: "Requires every `# noqa` comment to carry a reason. The reason is any text \
                after the directive in the same comment (leading `#`, `-` and `:` are ignored, \
                but it needs at least one letter or digit). The `# noqa: CODE  # reason` form is \
                recommended because it reads the same to every tool that parses the comment. \
                Only real comments count: `# noqa` inside a string literal is not flagged. \
                **Opt-in:** the rule is not run by default; enable it with \
                `extend-select = [\"KNQ001\"]` in `[tool.konform.lint]` (or \
                `--extend-select KNQ001`).",
            why_bad: "A bare suppression hides a violation without recording why that is fine. \
                Months later nobody knows whether it is still needed or safe to remove. A short \
                reason makes the exception reviewable.",
            example: Some(Example {
                bad: "from os.path import join   # noqa: KIS001\nvalue = compute()          # noqa",
                good: "from os.path import join   # noqa: KIS001  # re-exported for plugins\nvalue = compute()          # noqa: KPT010 - generated code",
            }),
            sections: vec![
                DocSection {
                    title: "Placeholder reasons",
                    body: "A reason that only repeats a placeholder is flagged too. The whole reason \
                        has to match (ignoring case, spacing and a trailing `.`/`!`), so \
                        `todo: drop in v3` or `ok for generated code` are fine. Tighten the list \
                        in `[tool.konform.lint.noqa-justification]`:\n\n\
                        ```toml\n\
                        [tool.konform.lint.noqa-justification]\n\
                        extend-placeholder-reasons = [\"legacy\", \"needed\"]  # add to the defaults\n\
                        # placeholder-reasons = [\"ok\", \"todo\"]  # ... or replace them ([] = off)\n\
                        ```",
                },
                DocSection {
                    title: "Exempt codes",
                    body: "Some suppressions explain themselves, such as `F401` on a re-export or \
                        `E501` on a long URL. List those codes (prefixes, like `# noqa`) and their \
                        comments need no reason:\n\n\
                        ```toml\n\
                        [tool.konform.lint.noqa-justification]\n\
                        exempt-codes = [\"F401\", \"E501\"]\n\
                        ```\n\n\
                        The comment is only exempt when **every** listed code is. `# noqa: F401, \
                        KIS001` still needs a reason, and a bare `# noqa` is never exempt. To \
                        exempt whole files instead (say `__init__.py`), use `per-file-ignores`: \
                        `\"**/__init__.py\" = [\"KNQ001\"]`.",
                },
                DocSection {
                title: "Suppressing and fixing",
                body: "This rule cannot be suppressed with `# noqa` (not even `# noqa: KNQ001`), and \
                    `--ignore-noqa` does not affect it. Turn it off with `ignore` or \
                    `per-file-ignores` instead. It is not fixable. `konform check --add-noqa --reason \"...\"` fills in a \
                    missing reason on the comments it flags (see \
                    [Baselining](../suppression.md#baselining-with---add-noqa)).",
                },
            ],
            options: vec![
                RuleOption {
                    name: "level",
                    ty: "\"warning\" | \"error\"",
                    default: format!("`\"{}\"`", defaults.level),
                    description: "Severity of violations. Set `warning` to roll the rule out gradually.",
                },
                RuleOption {
                    name: "placeholder-reasons",
                    ty: "list[str]",
                    default: format!("`{DEFAULT_PLACEHOLDERS:?}`"),
                    description: "Reasons that say nothing. Replaces the defaults; `[]` turns the check off.",
                },
                RuleOption {
                    name: "extend-placeholder-reasons",
                    ty: "list[str]",
                    default: format!("`{:?}`", defaults.extend_placeholder_reasons),
                    description: "Added to the placeholder list (default or replaced).",
                },
                RuleOption {
                    name: "exempt-codes",
                    ty: "list[str]",
                    default: format!("`{:?}`", defaults.exempt_codes),
                    description: "Code prefixes whose `# noqa` comments need no reason.",
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

/// Reasons that say as little as no reason, used unless
/// `placeholder-reasons` replaces them. Kept short on purpose: projects add
/// their own with `extend-placeholder-reasons`.
const DEFAULT_PLACEHOLDERS: &[&str] = &[
    "ok",
    "fine",
    "todo",
    "fixme",
    "tbd",
    "wip",
    "later",
    "fix later",
    "n/a",
    "ignore",
    "noqa",
];

/// `[tool.konform.lint.noqa-justification]` settings for KNQ001.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Knq001Settings {
    level: Level,
    /// Replaces [`DEFAULT_PLACEHOLDERS`] when set; `[]` disables the check.
    placeholder_reasons: Option<Vec<String>>,
    /// Added to the placeholder list (default or replaced).
    extend_placeholder_reasons: Vec<String>,
    /// Code prefixes whose `# noqa` comments need no reason.
    exempt_codes: Vec<String>,
}

impl Default for Knq001Settings {
    fn default() -> Self {
        Self {
            level: Level::Error,
            placeholder_reasons: None,
            extend_placeholder_reasons: Vec::new(),
            exempt_codes: Vec::new(),
        }
    }
}

/// [`Knq001Settings`] prepared for matching.
struct Policy {
    placeholders: HashSet<String>,
    exempt: Vec<String>,
}

/// Lowercase, collapse whitespace and drop a trailing `.` / `!`, so `OK.`
/// and `Fix  later` match the entries `ok` and `fix later`.
fn normalize_reason(s: &str) -> String {
    let words = s.split_whitespace().collect::<Vec<_>>().join(" ");
    words.trim_end_matches(['.', '!']).to_lowercase()
}

impl Policy {
    fn new(settings: &Knq001Settings) -> Self {
        let base: Vec<&str> = match &settings.placeholder_reasons {
            Some(list) => list.iter().map(String::as_str).collect(),
            None => DEFAULT_PLACEHOLDERS.to_vec(),
        };
        let placeholders = base
            .into_iter()
            .chain(
                settings
                    .extend_placeholder_reasons
                    .iter()
                    .map(String::as_str),
            )
            .map(normalize_reason)
            .filter(|s| !s.is_empty())
            .collect();
        // An empty prefix would match every code.
        let exempt = settings
            .exempt_codes
            .iter()
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
            .collect();
        Self {
            placeholders,
            exempt,
        }
    }

    fn is_placeholder(&self, reason: &str) -> bool {
        self.placeholders.contains(&normalize_reason(reason))
    }

    /// Is every listed code covered by an exempt prefix? Blanket comments
    /// (`codes == None`) and ones listing no code at all never are.
    fn is_exempt(&self, codes: Option<&[&str]>, aliases: &HashMap<String, String>) -> bool {
        let Some(codes) = codes.filter(|c| !c.is_empty()) else {
            return false;
        };
        codes.iter().all(|code| {
            // `# noqa: IS001` stands in for the canonical code it aliases.
            let canonical = aliases.get(*code).map_or(*code, String::as_str);
            self.exempt
                .iter()
                .any(|e| code.starts_with(e.as_str()) || canonical.starts_with(e.as_str()))
        })
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
        let src = "a = 1  # noqa: A  # legit\nb = 2  # noqa: B\nc = 3\nd = 4  # noqa\n";
        assert_eq!(lines(src), [2, 4]);
    }

    fn check_cfg(src: &str, cfg: &str) -> Vec<Violation> {
        check_at("a.py", src, &toml::from_str(cfg).unwrap())
    }

    #[test]
    fn placeholder_reasons_are_flagged_by_default() {
        for src in [
            "x = 1  # noqa: A  # ok\n",
            "x = 1  # noqa: A  # OK.\n",
            "x = 1  # noqa: A - TODO\n",
            "x = 1  # noqa  # fix   later!\n",
            "x = 1  # noqa: A, B  # n/a\n",
        ] {
            let v = check(src);
            assert_eq!(v.len(), 1, "{src:?}");
            assert!(v[0].message.contains("says nothing"), "{src:?}");
            assert!(!v[0].fixable);
        }
    }

    #[test]
    fn placeholders_match_the_whole_reason_only() {
        for src in [
            "x = 1  # noqa: A  # todo: drop in v3\n",
            "x = 1  # noqa: A  # ok for generated code\n",
            "x = 1  # noqa: A  # legacy api\n",
            "x = 1  # noqa: A  # tokyo\n",
        ] {
            assert!(check(src).is_empty(), "{src:?}");
        }
    }

    #[test]
    fn placeholder_span_covers_the_reason_text() {
        let v = &check("x = 1  # noqa: KIS001  # ok\n")[0];
        assert_eq!((v.line, v.col, v.end_line, v.end_col), (1, 25, 1, 27));
    }

    #[test]
    fn placeholder_list_can_be_extended_or_replaced() {
        let src = "x = 1  # noqa: A  # legacy\ny = 2  # noqa: A  # ok\n";
        let lines = |cfg| {
            check_cfg(src, cfg)
                .iter()
                .map(|v| v.line)
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(""), [2]);
        // Extending keeps the defaults.
        assert_eq!(lines("extend-placeholder-reasons = [\"Legacy\"]"), [1, 2]);
        // Replacing drops them.
        assert_eq!(lines("placeholder-reasons = [\"legacy\"]"), [1]);
        // `[]` turns the check off.
        assert!(lines("placeholder-reasons = []").is_empty());
        // ... but extending on top of it still works.
        assert_eq!(
            lines("placeholder-reasons = []\nextend-placeholder-reasons = [\"ok\"]"),
            [2]
        );
    }

    #[test]
    fn blank_placeholder_entries_match_nothing() {
        let src = "x = 1  # noqa: A  # real reason\n";
        assert!(check_cfg(src, "extend-placeholder-reasons = [\"\", \"  \"]").is_empty());
    }

    #[test]
    fn exempt_codes_need_no_reason() {
        let cfg = "exempt-codes = [\"F401\", \"E5\"]";
        for src in [
            "x = 1  # noqa: F401\n",
            "x = 1  # noqa: F401, E501\n",
            "x = 1  # noqa: E501 F401\n",
            // A placeholder reason is just as unneeded.
            "x = 1  # noqa: F401  # ok\n",
        ] {
            assert!(check_cfg(src, cfg).is_empty(), "{src:?}");
        }
    }

    #[test]
    fn exemption_needs_every_code_covered() {
        let cfg = "exempt-codes = [\"F401\"]";
        for src in [
            "x = 1  # noqa: F401, KIS001\n",
            "x = 1  # noqa: F811\n",
            // Narrower than the exempt entry.
            "x = 1  # noqa: F\n",
            // Blanket and code-less comments are never exempt.
            "x = 1  # noqa\n",
            "x = 1  # noqa:\n",
        ] {
            assert_eq!(check_cfg(src, cfg).len(), 1, "{src:?}");
        }
    }

    #[test]
    fn exempt_codes_ignore_blank_entries_and_follow_aliases() {
        // A blank entry must not exempt everything.
        assert_eq!(
            check_cfg("x  # noqa: A\n", "exempt-codes = [\"\"]").len(),
            1
        );

        let mut ctx = FileContext::from_source(PathBuf::from("a.py"), "x  # noqa: IS001\n".into());
        ctx.noqa_aliases.insert("IS001".into(), "KIS001".into());
        let cfg: toml::Value = toml::from_str("exempt-codes = [\"KIS001\"]").unwrap();
        assert!(Knq001Rule::new().check(&ctx, &cfg).is_empty());
    }

    #[test]
    fn knq001_itself_can_not_be_exempted_by_a_comment() {
        // Exemption is config-only; `# noqa: KNQ001` stays flagged.
        assert_eq!(check("x = 1  # noqa: KNQ001\n").len(), 1);
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
