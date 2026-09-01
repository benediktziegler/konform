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

/// Apply all enabled, fixable rules to `input` in sequence.
///
/// Rules are applied one after another so each fix sees the output of the
/// previous one.  Returns the final source text if any rule made a change,
/// or `None` if the source is already clean.
pub fn run_fix(
    input: &CheckInput<'_>,
    rules: &[Box<dyn Rule>],
    config: &Config,
) -> Result<Option<String>> {
    let mut src = input.source.to_owned();
    let mut changed = false;

    for rule in rules
        .iter()
        .filter(|r| r.fixable() && config.is_enabled(r.code()))
    {
        let mut ctx = FileContext::from_source(input.path.to_path_buf(), src.clone());
        ctx.ignore_noqa = input.ignore_noqa || config.ignore_noqa;
        ctx.noqa_aliases = config.noqa_aliases.clone();
        if let Some(fixed) = rule.fix(&ctx, config.rule_config(rule.category()))? {
            src = fixed;
            changed = true;
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
}
