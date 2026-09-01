//! Rule trait, shared context types, and the rule registry.
//!
//! Every linting rule implements [`Rule`].  The engine calls
//! [`Rule::check`] to find violations and [`Rule::fix`] to rewrite source
//! in-place.  Both the CLI and the LSP build a [`FileContext`] and pass it
//! to the same rule implementations — no duplication of logic.
#![allow(dead_code)]

use crate::module_probe::ModuleProbe;
use crate::types::Violation;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Sub-modules
// ---------------------------------------------------------------------------
pub mod kis001;
pub mod kpt;

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

    /// Category prefix, e.g. `"KIS"`.
    ///
    /// Used by [`crate::config::Config::rule_config`] to look up the
    /// per-category configuration section (`[tool.konform.KIS]`).
    fn category(&self) -> &str;

    /// Short human-readable rule name shown in `--list-rules` output.
    fn name(&self) -> &str;

    /// One-line description shown next to the name in `--list-rules` output.
    fn description(&self) -> &str;

    /// Whether this rule can automatically rewrite violations in-place.
    fn fixable(&self) -> bool {
        false
    }

    /// Check `ctx` for violations and return them.
    ///
    /// `cfg` is the raw TOML value for this rule's category section,
    /// e.g. the contents of `[tool.konform.KIS]`.  Rules that need no
    /// configuration can ignore it.
    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation>;

    /// Rewrite the source in `ctx` to fix all violations, returning the
    /// new source text, or `None` if there is nothing to change.
    ///
    /// The default implementation is a no-op for rules that are not fixable.
    fn fix(&self, _ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
        Ok(None)
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

    let codes = rest.trim_start_matches(':');
    codes.split(',').any(|c| {
        let c = c.trim();
        code.starts_with(c)
            || aliases
                .get(c)
                .is_some_and(|target| code.starts_with(target.as_str()))
    })
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
        Box::new(kpt::KptRule::new(config_dir)),
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
