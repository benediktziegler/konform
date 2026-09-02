//! Shared linting and fixing engine.
//!
//! This is the **only** place that calls into rules.  Both the CLI and the
//! LSP construct a [`CheckInput`] and call [`run_check`] or [`run_fix`];
//! neither path duplicates rule-dispatch logic.
//!
//! ```text
//!                    ┌──────────────────────┐
//!                    │  engine::run_check() │
//!                    │  engine::run_fix()   │
//!                    └──────────┬───────────┘
//!                               │ same rules, same Config
//!              ┌────────────────┴─────────────────┐
//!              │                                  │
//!       CLI check path                    LSP handler path
//!       CheckInput { path, &fs::read() }  CheckInput { path, &session.get() }
//! ```
#![allow(dead_code)]

use crate::config::Config;
use crate::rules::{FileContext, Rule};
use crate::types::Violation;
use anyhow::Result;
use globset::{Glob, GlobMatcher};
use ruff_python_parser::parse_module;
use std::path::Path;

// ---------------------------------------------------------------------------
// CheckInput
// ---------------------------------------------------------------------------

/// Lightweight view of a file passed to the engine.
///
/// Borrowing the source avoids a copy: the CLI passes a reference to the
/// string it read from disk; the LSP passes a reference to the in-memory
/// document text.
pub struct CheckInput<'a> {
    /// Path to the file (used for error messages and cache keys).
    pub path: &'a Path,
    /// Full UTF-8 source text.
    pub source: &'a str,
    /// When `true`, `# noqa` suppression comments are ignored.
    pub ignore_noqa: bool,
}

impl<'a> CheckInput<'a> {
    pub fn new(path: &'a Path, source: &'a str) -> Self {
        Self {
            path,
            source,
            ignore_noqa: false,
        }
    }
}

impl From<&CheckInput<'_>> for FileContext {
    fn from(input: &CheckInput<'_>) -> Self {
        let mut ctx = FileContext::from_source(input.path.to_path_buf(), input.source.to_owned());
        ctx.ignore_noqa = input.ignore_noqa;
        ctx
    }
}

// ---------------------------------------------------------------------------
// run_check
// ---------------------------------------------------------------------------

/// Lint `input` through every active rule and return all violations.
///
/// Rules are filtered by [`Config::is_enabled`] so `--select` / `--ignore`
/// can narrow the active set without changing callers.
/// Per-file ignores from `config.per_file_ignores` are applied after the
/// rule pass and suppress matching violations.
pub fn run_check(
    input: &CheckInput<'_>,
    rules: &[Box<dyn Rule>],
    config: &Config,
) -> Vec<Violation> {
    let mut ctx = FileContext::from(input);
    // Config flag takes precedence over CheckInput flag.
    if config.ignore_noqa {
        ctx.ignore_noqa = true;
    }
    ctx.noqa_aliases = config.noqa_aliases.clone();
    let mut violations: Vec<Violation> = rules
        .iter()
        .filter(|r| config.is_enabled(r.code()))
        .flat_map(|r| r.check(&ctx, config.rule_config(r.category())))
        .collect();

    // Apply per-file-ignores: build GlobMatchers once, then filter.
    if !config.per_file_ignores.is_empty() && !violations.is_empty() {
        let matchers: Vec<(GlobMatcher, Vec<String>)> = config
            .per_file_ignores
            .iter()
            .filter_map(|(pat, codes)| {
                Glob::new(pat)
                    .ok()
                    .map(|g| (g.compile_matcher(), codes.clone()))
            })
            .collect();
        if !matchers.is_empty() {
            let config_dir = config.config_dir.as_deref();
            let cwd = std::env::current_dir().ok();
            violations.retain(|v| {
                !per_file_ignored(input.path, &v.rule, &matchers, config_dir, cwd.as_deref())
            });
        }
    }

    violations
}

// ---------------------------------------------------------------------------
// run_fix
// ---------------------------------------------------------------------------

/// Maximum number of full passes over all fixable rules for a single file.
///
/// One pass may leave behind violations that only became visible *because*
/// of that pass's edits (e.g. fixing rule A's violation can incidentally
/// introduce or expose a violation of rule B, or of A itself) -- mirroring
/// why Ruff's `check --fix` re-parses and re-lints iteratively rather than
/// applying each rule exactly once. Looping here lets those cascades
/// resolve fully within a single `--fix` invocation instead of requiring
/// the user to re-run it repeatedly. The cap is a pure safety guard against
/// a pathological/buggy fix that oscillates forever; well-behaved fixes
/// converge (a pass makes no changes) in 1-2 iterations.
const MAX_FIX_PASSES: u32 = 100;

/// Apply all enabled, fixable rules to `input`, repeating full passes over
/// the rule set until a pass makes no further changes (or [`MAX_FIX_PASSES`]
/// is reached).
///
/// Within a pass, rules are applied one after another so each fix sees the
/// output of the previous one. Returns the final source text if any rule
/// made a change, or `None` if the source is already clean.
///
/// # Safety net
/// After each rule's fix, the resulting source is re-parsed with
/// `ruff_python_parser::parse_module`. A fix is a contract: it must turn
/// valid Python into different, still-valid Python. If a rule ever produces
/// text that no longer parses (a bug in that rule, not something we can fix
/// here), that specific fix is rejected -- the previous, still-valid `src`
/// is kept and the rest of the pipeline continues -- rather than silently
/// writing corrupted code to disk. This check only runs when `src` itself
/// parsed cleanly *before* the rule ran, since we can't meaningfully judge
/// "still valid" against an input that was already unparsable (e.g. a file
/// with a pre-existing syntax error unrelated to any rule).
pub fn run_fix(
    input: &CheckInput<'_>,
    rules: &[Box<dyn Rule>],
    config: &Config,
) -> Result<Option<String>> {
    let mut src = input.source.to_owned();
    let mut changed = false;

    for _pass in 0..MAX_FIX_PASSES {
        let mut pass_changed = false;

        for rule in rules
            .iter()
            .filter(|r| r.fixable() && config.is_enabled(r.code()))
        {
            let mut ctx = FileContext::from_source(input.path.to_path_buf(), src.clone());
            ctx.ignore_noqa = input.ignore_noqa || config.ignore_noqa;
            ctx.noqa_aliases = config.noqa_aliases.clone();
            if let Some(fixed) = rule.fix(&ctx, config.rule_config(rule.category()))? {
                if fixed == src {
                    continue; // no-op fix; nothing to apply or loop on
                }
                let src_was_valid = parse_module(&src).is_ok();
                if src_was_valid && parse_module(&fixed).is_err() {
                    eprintln!(
                        "warning: {}'s fix for {} would produce invalid Python; skipping this \
                         fix (this is a bug in the rule -- please report it)",
                        rule.code(),
                        input.path.display()
                    );
                    continue;
                }
                src = fixed;
                changed = true;
                pass_changed = true;
            }
        }

        if !pass_changed {
            break;
        }
    }

    Ok(changed.then_some(src))
}

// ---------------------------------------------------------------------------
// Per-file-ignores helpers
// ---------------------------------------------------------------------------

/// Returns `true` when `rule` should be suppressed for `path` based on the
/// compiled per-file-ignore matchers.
///
/// Each matcher is tried against:
/// 1. `path` as-is (works when the path is already project-root-relative).
/// 2. `path` stripped of `config_dir` (covers absolute paths in LSP usage).
/// 3. `path` stripped of the process cwd (covers absolute CLI paths).
fn per_file_ignored(
    path: &Path,
    rule: &str,
    matchers: &[(GlobMatcher, Vec<String>)],
    config_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> bool {
    for (matcher, codes) in matchers {
        let hit = matcher.is_match(path)
            || config_dir
                .and_then(|d| path.strip_prefix(d).ok())
                .is_some_and(|r| matcher.is_match(r))
            || cwd
                .and_then(|d| path.strip_prefix(d).ok())
                .is_some_and(|r| matcher.is_match(r));
        if hit && codes.iter().any(|c| rule.starts_with(c.as_str())) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_probe::ModuleProbe;
    use crate::rules::all_rules;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn probe() -> Arc<ModuleProbe> {
        Arc::new(ModuleProbe::default())
    }

    fn path() -> PathBuf {
        PathBuf::from("test.py")
    }

    #[test]
    fn empty_rules_yields_no_violations() {
        let p = path();
        let input = CheckInput::new(&p, "from os.path import join\n");
        let violations = run_check(&input, &[], &Config::default());
        assert!(violations.is_empty());
    }

    #[test]
    fn empty_rules_fix_returns_none() {
        let p = path();
        let input = CheckInput::new(&p, "from os.path import join\n");
        let result = run_fix(&input, &[], &Config::default()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn run_check_finds_kis001_violation() {
        let rules = all_rules(probe(), None);
        let p = path();
        let input = CheckInput::new(&p, "from os.path import join\n");
        let violations = run_check(&input, &rules, &Config::default());
        assert!(!violations.is_empty());
        assert_eq!(violations[0].rule, "KIS001");
    }

    #[test]
    fn run_check_clean_source_no_violations() {
        let rules = all_rules(probe(), None);
        let p = path();
        // Source is clean for all active rules: valid module import + docstring.
        let input = CheckInput::new(&p, "\"\"\"Clean module.\"\"\"\nimport os.path\n");
        let violations = run_check(&input, &rules, &Config::default());
        assert!(violations.is_empty());
    }

    #[test]
    fn run_fix_rewrites_violation() {
        let rules = all_rules(probe(), None);
        let p = path();
        let source = "from os.path import join\n";
        let input = CheckInput::new(&p, source);
        let fixed = run_fix(&input, &rules, &Config::default()).unwrap();
        assert!(fixed.is_some());
        assert!(!fixed.unwrap().contains("from os.path import join"));
    }

    #[test]
    fn run_fix_clean_source_returns_none() {
        let rules = all_rules(probe(), None);
        let p = path();
        let source = "import os.path\n";
        let input = CheckInput::new(&p, source);
        let result = run_fix(&input, &rules, &Config::default()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn check_input_from_converts_to_file_context() {
        let p = path();
        let input = CheckInput::new(&p, "x = 1\n");
        let ctx = FileContext::from(&input);
        assert_eq!(ctx.path, p);
        assert_eq!(ctx.source, "x = 1\n");
        assert_eq!(ctx.lines, vec!["x = 1"]);
    }

    #[test]
    fn per_file_ignores_suppresses_matching_path() {
        let rules = all_rules(probe(), None);
        // Use a relative path that matches the glob directly.
        // Source with a docstring produces only a KIS001 violation.
        let p = PathBuf::from("tests/test_foo.py");
        let input = CheckInput::new(&p, "\"\"\"Test module.\"\"\"\nfrom os.path import join\n");
        let mut config = Config::default();
        config
            .per_file_ignores
            .insert("tests/**".into(), vec!["KIS001".into()]);
        let violations = run_check(&input, &rules, &config);
        assert!(
            violations.is_empty(),
            "per_file_ignores should suppress KIS001 for tests/**"
        );
    }

    #[test]
    fn per_file_ignores_non_matching_path_still_reports() {
        let rules = all_rules(probe(), None);
        let p = PathBuf::from("src/foo.py");
        let input = CheckInput::new(&p, "from os.path import join\n");
        let mut config = Config::default();
        config
            .per_file_ignores
            .insert("tests/**".into(), vec!["KIS001".into()]);
        let violations = run_check(&input, &rules, &config);
        assert!(!violations.is_empty(), "src/foo.py should not be ignored");
    }

    #[test]
    fn per_file_ignores_category_prefix_suppresses() {
        let rules = all_rules(probe(), None);
        let p = PathBuf::from("tests/test_foo.py");
        // Source with a docstring produces only a KIS001 violation.
        let input = CheckInput::new(&p, "\"\"\"Test module.\"\"\"\nfrom os.path import join\n");
        let mut config = Config::default();
        // Category prefix "KIS" should suppress KIS001.
        config
            .per_file_ignores
            .insert("tests/**".into(), vec!["KIS".into()]);
        let violations = run_check(&input, &rules, &config);
        assert!(
            violations.is_empty(),
            "category prefix KIS should suppress KIS001"
        );
    }

    #[test]
    fn noqa_alias_suppresses_violation() {
        let rules = all_rules(probe(), None);
        let p = path();
        let input = CheckInput::new(&p, "from os.path import join  # noqa: IS001\n");
        let mut config = Config::default();
        config.noqa_aliases.insert("IS001".into(), "KIS001".into());
        let violations = run_check(&input, &rules, &config);
        assert!(
            violations.is_empty(),
            "noqa_aliases should let IS001 suppress KIS001"
        );
    }

    #[test]
    fn noqa_alias_category_suppresses_violation() {
        let rules = all_rules(probe(), None);
        let p = path();
        let input = CheckInput::new(&p, "from os.path import join  # noqa: IS\n");
        let mut config = Config::default();
        config.noqa_aliases.insert("IS".into(), "KIS".into());
        let violations = run_check(&input, &rules, &config);
        assert!(
            violations.is_empty(),
            "category noqa_aliases should let IS suppress KIS001"
        );
    }

    #[test]
    fn noqa_alias_unrelated_code_does_not_suppress() {
        let rules = all_rules(probe(), None);
        let p = path();
        let input = CheckInput::new(&p, "from os.path import join  # noqa: IS002\n");
        let mut config = Config::default();
        config.noqa_aliases.insert("IS001".into(), "KIS001".into());
        let violations = run_check(&input, &rules, &config);
        assert!(
            !violations.is_empty(),
            "unrelated alias should not suppress KIS001"
        );
    }

    // ── AST-validity safety net: a rule's fix must not corrupt syntax ───────

    /// A fake rule whose `fix` always "succeeds" but emits syntactically
    /// invalid Python. Stands in for a buggy rule to exercise the engine's
    /// safety net without depending on any real rule having a bug.
    struct BrokenFixRule;

    impl Rule for BrokenFixRule {
        fn code(&self) -> &str {
            "ZZ999"
        }
        fn category(&self) -> &str {
            "ZZ"
        }
        fn name(&self) -> &str {
            "broken-fix"
        }
        fn description(&self) -> &str {
            "test-only rule that always corrupts the source"
        }
        fn fixable(&self) -> bool {
            true
        }
        fn check(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Vec<Violation> {
            Vec::new()
        }
        fn fix(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
            Ok(Some("def (((( not valid python".to_owned()))
        }
        fn explain(&self) -> String {
            String::new()
        }
    }

    /// A fake rule that always applies a trivial, syntactically valid fix.
    /// Used alongside [`BrokenFixRule`] to prove the engine keeps applying
    /// later rules after rejecting an earlier broken one.
    struct GoodFixRule;

    impl Rule for GoodFixRule {
        fn code(&self) -> &str {
            "ZZ998"
        }
        fn category(&self) -> &str {
            "ZZ"
        }
        fn name(&self) -> &str {
            "good-fix"
        }
        fn description(&self) -> &str {
            "test-only rule that always applies a trivial valid fix"
        }
        fn fixable(&self) -> bool {
            true
        }
        fn check(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Vec<Violation> {
            Vec::new()
        }
        fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
            // Idempotent, like a real rule: nothing left to fix once applied.
            if ctx.source.contains("# fixed by GoodFixRule") {
                return Ok(None);
            }
            Ok(Some(format!(
                "{}\n# fixed by GoodFixRule\n",
                ctx.source.trim_end()
            )))
        }
        fn explain(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn run_fix_rejects_a_fix_that_produces_invalid_python() {
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(BrokenFixRule)];
        let p = path();
        let input = CheckInput::new(&p, "x = 1\n");
        let result = run_fix(&input, &rules, &Config::default()).unwrap();
        assert!(
            result.is_none(),
            "a fix that produces invalid Python must be rejected, not written out"
        );
    }

    #[test]
    fn run_fix_continues_past_a_rejected_fix() {
        // BrokenFixRule's corrupting fix must not prevent GoodFixRule's
        // legitimate, valid fix from still being applied.
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(BrokenFixRule), Box::new(GoodFixRule)];
        let p = path();
        let input = CheckInput::new(&p, "x = 1\n");
        let result = run_fix(&input, &rules, &Config::default()).unwrap();
        let fixed = result.expect("GoodFixRule's valid fix should still be applied");
        assert!(fixed.contains("# fixed by GoodFixRule"), "got: {fixed:?}");
        assert!(
            ruff_python_parser::parse_module(&fixed).is_ok(),
            "final result must remain valid Python"
        );
    }

    /// Fake rule that only fires once its precondition (`NEEDS_A` in the
    /// source) is met, and turns it into `NEEDS_B` -- deliberately modeling
    /// a fix that *creates* a new violation for another rule to pick up.
    struct RuleNeedsA;

    impl Rule for RuleNeedsA {
        fn code(&self) -> &str {
            "ZZ997"
        }
        fn category(&self) -> &str {
            "ZZ"
        }
        fn name(&self) -> &str {
            "needs-a"
        }
        fn description(&self) -> &str {
            "test-only rule: NEEDS_A -> NEEDS_B"
        }
        fn fixable(&self) -> bool {
            true
        }
        fn check(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Vec<Violation> {
            Vec::new()
        }
        fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
            if !ctx.source.contains("NEEDS_A") {
                return Ok(None);
            }
            Ok(Some(ctx.source.replace("NEEDS_A", "FIXED_A NEEDS_B")))
        }
        fn explain(&self) -> String {
            String::new()
        }
    }

    /// Fake rule that fires once `NEEDS_B` appears and resolves it. Listed
    /// *before* [`RuleNeedsA`] in the rule set on purpose, so within a
    /// single pass it runs too early to see the `NEEDS_B` that
    /// `RuleNeedsA` is about to introduce -- only a second pass converges.
    struct RuleNeedsB;

    impl Rule for RuleNeedsB {
        fn code(&self) -> &str {
            "ZZ996"
        }
        fn category(&self) -> &str {
            "ZZ"
        }
        fn name(&self) -> &str {
            "needs-b"
        }
        fn description(&self) -> &str {
            "test-only rule: NEEDS_B -> FIXED_B"
        }
        fn fixable(&self) -> bool {
            true
        }
        fn check(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Vec<Violation> {
            Vec::new()
        }
        fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
            if !ctx.source.contains("NEEDS_B") {
                return Ok(None);
            }
            Ok(Some(ctx.source.replace("NEEDS_B", "FIXED_B")))
        }
        fn explain(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn run_fix_iterates_across_passes_until_stable() {
        // Single-pass semantics would leave this unresolved: within pass 1,
        // RuleNeedsB runs first and finds nothing (NEEDS_B doesn't exist
        // yet), then RuleNeedsA runs and introduces NEEDS_B. Only a second
        // pass -- driven by the engine's fixed-point loop -- lets
        // RuleNeedsB see and resolve it, mirroring Ruff's iterate-to-a-
        // fixed-point `check --fix` behavior.
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(RuleNeedsB), Box::new(RuleNeedsA)];
        let p = path();
        let input = CheckInput::new(&p, "# NEEDS_A\nx = 1\n");
        let result = run_fix(&input, &rules, &Config::default()).unwrap();
        let fixed = result.expect("cascading fix should be applied");
        assert!(
            fixed.contains("FIXED_A") && fixed.contains("FIXED_B"),
            "both the original and the cascaded violation should be resolved \
             within a single run_fix call, got: {fixed:?}"
        );
        assert!(
            !fixed.contains("NEEDS_A") && !fixed.contains("NEEDS_B"),
            "no unresolved placeholder should remain, got: {fixed:?}"
        );
    }

    /// Fake rule whose fix always flips the source between two states,
    /// never converging. Models a hypothetical buggy rule to prove the
    /// engine's iteration cap prevents an infinite loop.
    struct OscillatingRule;

    impl Rule for OscillatingRule {
        fn code(&self) -> &str {
            "ZZ995"
        }
        fn category(&self) -> &str {
            "ZZ"
        }
        fn name(&self) -> &str {
            "oscillating"
        }
        fn description(&self) -> &str {
            "test-only rule that never converges"
        }
        fn fixable(&self) -> bool {
            true
        }
        fn check(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Vec<Violation> {
            Vec::new()
        }
        fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
            if ctx.source.contains("STATE_A") {
                Ok(Some(ctx.source.replace("STATE_A", "STATE_B")))
            } else {
                Ok(Some(ctx.source.replace("STATE_B", "STATE_A")))
            }
        }
        fn explain(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn run_fix_terminates_when_a_rule_never_converges() {
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(OscillatingRule)];
        let p = path();
        let input = CheckInput::new(&p, "# STATE_A\nx = 1\n");
        // Must return rather than loop forever; the iteration cap in
        // run_fix bounds the pathological case.
        let result = run_fix(&input, &rules, &Config::default()).unwrap();
        assert!(
            result.is_some(),
            "an oscillating rule still reports a change"
        );
    }
}
