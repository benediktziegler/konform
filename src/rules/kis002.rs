//! KIS002 — Konform Import Style: unnecessary import aliases.
//!
//! Flags `from X import Y as Z` when the rename to `Z` isn't needed to avoid
//! a naming collision -- i.e. `Y` itself is never bound anywhere else that
//! the alias's uses can see, so `from X import Y` would behave identically.
//!
//! ```python
//! # Bad — KIS002
//! from foo.bar import baz as bar_baz     # `baz` isn't used anywhere else
//!
//! # Good
//! from foo.bar import baz
//! ```
//!
//! Scope, deliberately narrow for this first cut:
//! - Only considers `from X import Y as Z`, not `import X as Z`. Dropping
//!   the alias on a plain `import` can change what gets bound at all (e.g.
//!   `import a.b as c` binds `c` to the submodule; `import a.b` binds only
//!   `a`), so there's no equivalent "was the rename even needed" question.
//! - Skips `from X import Y as Y` (asname identical to the original name):
//!   that's the explicit re-export idiom, already covered by Ruff's
//!   `useless-import-alias` (PLC0414). KIS002 only fires on a genuine
//!   *rename*.
//! - Skips aliases whose new name starts with `_` -- treated as a
//!   deliberate "private, don't re-export" marker rather than a plain
//!   rename.
//! - Skips relative imports (`from . import x as y`): there's no single
//!   stable module identity to key the "is this name already imported
//!   elsewhere" collision check on.

use super::scope::{
    bucket_for_offset, build_line_starts, build_scope_index, collect_load_names, is_name_shadowed,
    offset_to_line_col, parse_module_stmts, shadowed_at_occurrences_of, NamedSpan, ScopeIndex,
};
use super::{has_noqa, FileContext, Rule};
use crate::types::{Level, Violation};
use anyhow::Result;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Kis002Rule;

impl Kis002Rule {
    pub fn new() -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// Rule impl
// ---------------------------------------------------------------------------

impl Rule for Kis002Rule {
    fn code(&self) -> &str {
        "KIS002"
    }

    fn category(&self) -> &str {
        "KIS"
    }

    fn config_name(&self) -> &str {
        "unnecessary-import-alias"
    }

    fn name(&self) -> &str {
        "Unnecessary import alias"
    }

    fn description(&self) -> &str {
        "Checks that `from X import Y as Z` only renames when needed to avoid a collision."
    }

    fn fixable(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let level = parse_kis002_config(cfg);
        check_aliases(&ctx.source, level, ctx.ignore_noqa, &ctx.noqa_aliases)
    }

    fn fix(&self, ctx: &FileContext, _cfg: &toml::Value) -> Result<Option<String>> {
        Ok(apply_fixes(&ctx.source, ctx.ignore_noqa, &ctx.noqa_aliases))
    }

    fn explain(&self) -> String {
        "\
KIS002 — Unnecessary import alias [sometimes fixable]

  Checks that `from X import Y as Z` only renames the import when the
  rename is actually needed to avoid a naming collision. If `Y` isn't bound
  anywhere else the alias's uses can see, the rename buys nothing.

  Bad:
    from foo.bar import baz as bar_baz    # `baz` isn't used anywhere else

  Good:
    from foo.bar import baz

  Not flagged:
    from foo.bar import baz as baz        # explicit re-export idiom;
                                           # see Ruff's PLC0414 instead
    from foo.bar import baz as _baz       # leading underscore: deliberate
                                           # \"don't re-export\" marker
    import x.y as z                       # plain `import ... as` is out of
                                           # scope: dropping the alias would
                                           # change what name gets bound

  Configure the severity in [tool.konform.lint.unnecessary-import-alias]:
    level = \"warning\"   # default: \"warning\" | \"error\"

  Not every violation can be auto-fixed: if the alias is also bound
  elsewhere (shadowed by a local variable, or ambiguous with a different
  import binding the same name), konform reports the violation but leaves
  it for you to fix by hand.

  Suppress per-line:
    from foo.bar import baz as bar_baz   # noqa: KIS002
    from foo.bar import baz as bar_baz   # noqa: KIS      (all KIS rules)
"
        .to_owned()
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

/// `[tool.konform.lint.unnecessary-import-alias]` settings for KIS002.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Kis002Settings {
    level: Level,
}

impl Default for Kis002Settings {
    fn default() -> Self {
        Self {
            level: Level::Warning,
        }
    }
}

fn parse_kis002_config(cfg: &toml::Value) -> Level {
    Kis002Settings::deserialize(cfg.clone())
        .unwrap_or_default()
        .level
}

// ---------------------------------------------------------------------------
// Alias candidates
// ---------------------------------------------------------------------------

/// A single `Y as Z` clause inside a `from X import ...` statement, where
/// `asname` genuinely renames `name` (already excludes `as Y` self-aliases
/// and `_`-prefixed asnames -- see module docs).
struct AliasCandidate {
    module: String,
    name: String,
    asname: String,
    /// Byte range of the `Y as Z` clause itself (an AST `Alias` node),
    /// used both for the violation span and, when fixable, as the exact
    /// text to splice down to just `Y`.
    alias_start: u32,
    alias_end: u32,
}

/// Recursively collect every `AliasCandidate` in `stmts` (including inside
/// `if`/`def`/`class` bodies -- imports can legally appear anywhere).
fn collect_alias_candidates(stmts: &[Stmt]) -> Vec<AliasCandidate> {
    let mut out = Vec::new();
    collect_alias_candidates_rec(stmts, &mut out);
    out
}

fn collect_alias_candidates_rec(stmts: &[Stmt], out: &mut Vec<AliasCandidate>) {
    for stmt in stmts {
        match stmt {
            Stmt::ImportFrom(node) => {
                if node.level != 0 {
                    continue; // relative import: no stable module identity
                }
                let Some(module) = &node.module else {
                    continue;
                };
                for alias in &node.names {
                    let Some(asname) = &alias.asname else {
                        continue;
                    };
                    let asname_s = asname.as_str();
                    let name_s = alias.name.as_str();
                    if asname_s == name_s || asname_s.starts_with('_') {
                        continue;
                    }
                    out.push(AliasCandidate {
                        module: module.as_str().to_owned(),
                        name: name_s.to_owned(),
                        asname: asname_s.to_owned(),
                        alias_start: u32::from(alias.range().start()),
                        alias_end: u32::from(alias.range().end()),
                    });
                }
            }
            Stmt::If(node) => {
                collect_alias_candidates_rec(&node.body, out);
                for clause in &node.elif_else_clauses {
                    collect_alias_candidates_rec(&clause.body, out);
                }
            }
            Stmt::FunctionDef(node) => collect_alias_candidates_rec(&node.body, out),
            Stmt::ClassDef(node) => collect_alias_candidates_rec(&node.body, out),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Import-binding collision map
// ---------------------------------------------------------------------------

/// Every name bound into the namespace by *any* import statement in the
/// file, mapped to the set of distinct "targets" (what it's actually
/// imported from) it's bound to. Import aliases are plain `Identifier`s in
/// the AST, not `Expr::Name` nodes, so `super::scope`'s Store-context
/// collectors never see them -- this fills that gap for the two questions
/// KIS002 needs answered:
///   - is the *original* name already bound by some other import (so the
///     rename is in fact necessary to avoid a collision)?
///   - is the *alias* itself bound to more than one distinct import
///     somewhere in the file (so renaming its uses would be ambiguous)?
fn collect_import_bindings(stmts: &[Stmt]) -> HashMap<String, HashSet<String>> {
    let mut out = HashMap::new();
    collect_import_bindings_rec(stmts, &mut out);
    out
}

fn collect_import_bindings_rec(stmts: &[Stmt], out: &mut HashMap<String, HashSet<String>>) {
    for stmt in stmts {
        match stmt {
            Stmt::ImportFrom(node) => {
                let module = node.module.as_ref().map_or("", |m| m.as_str());
                let dots = ".".repeat(node.level as usize);
                for alias in &node.names {
                    let bound = alias.asname.as_deref().unwrap_or(alias.name.as_str());
                    let target = format!("{dots}{module}::{}", alias.name.as_str());
                    out.entry(bound.to_owned()).or_default().insert(target);
                }
            }
            Stmt::Import(node) => {
                for alias in &node.names {
                    let dotted = alias.name.as_str();
                    let bound = alias
                        .asname
                        .as_deref()
                        .unwrap_or_else(|| dotted.split('.').next().unwrap_or(dotted));
                    out.entry(bound.to_owned())
                        .or_default()
                        .insert(dotted.to_owned());
                }
            }
            Stmt::If(node) => {
                collect_import_bindings_rec(&node.body, out);
                for clause in &node.elif_else_clauses {
                    collect_import_bindings_rec(&clause.body, out);
                }
            }
            Stmt::FunctionDef(node) => collect_import_bindings_rec(&node.body, out),
            Stmt::ClassDef(node) => collect_import_bindings_rec(&node.body, out),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Necessity / fixability checks
// ---------------------------------------------------------------------------

/// Is the rename in `cand` actually needed to avoid a collision? If so,
/// KIS002 must not flag it at all. Checks both the import's own declaration
/// site (an unused alias whose original name collides with something is
/// still a real collision) and every place the alias is actually read.
fn is_necessary(
    cand: &AliasCandidate,
    load_names: &[NamedSpan],
    scope_index: &ScopeIndex,
    bound_targets: &HashMap<String, HashSet<String>>,
) -> bool {
    bound_targets.contains_key(&cand.name)
        || is_name_shadowed(
            &cand.name,
            bucket_for_offset(scope_index, cand.alias_start),
            scope_index,
        )
        || shadowed_at_occurrences_of(&cand.name, &cand.asname, load_names, scope_index)
}

/// Would rewriting `cand` (dropping the alias and renaming every use of
/// `asname` to `name`) be safe, i.e. does `asname` resolve unambiguously to
/// this exact import everywhere it's read?
fn is_fixable(
    cand: &AliasCandidate,
    load_names: &[NamedSpan],
    scope_index: &ScopeIndex,
    bound_targets: &HashMap<String, HashSet<String>>,
) -> bool {
    let no_local_shadow =
        !shadowed_at_occurrences_of(&cand.asname, &cand.asname, load_names, scope_index);
    let unambiguous_target = bound_targets
        .get(&cand.asname)
        .is_none_or(|set| set.len() <= 1);
    no_local_shadow && unambiguous_target
}

/// Of every individually-eligible candidate in `cands`, which ones must be
/// rejected because an *earlier* one already claims the same `(scope bucket,
/// post-fix name)` slot?
///
/// `is_necessary`/`is_fixable` only check each candidate against imports and
/// names that already exist in the source -- they can't see that *another*
/// alias from a different module, dropped in the very same pass, would land
/// on the identical name. E.g.:
///
/// ```python
/// from a.plugin import plugin as a_plugin    # unnecessary alias
/// from b.plugin import plugin as b_plugin    # unnecessary alias
/// ```
///
/// Naively fixing both independently rewrites them to two `from ... import
/// plugin` statements bound in the same scope -- a silent collision. Only
/// one of them can safely drop its alias, so we apply fixes in source order
/// and let the first candidate to reach a given slot claim it; later
/// candidates for that same slot are returned here as unfixable (equivalent
/// to fixing the first, then re-running the check and finding the rest now
/// genuinely necessary).
fn collect_rename_collisions(
    cands: &[AliasCandidate],
    load_names: &[NamedSpan],
    scope_index: &ScopeIndex,
    bound_targets: &HashMap<String, HashSet<String>>,
) -> HashSet<u32> {
    let mut eligible: Vec<&AliasCandidate> = cands
        .iter()
        .filter(|cand| {
            !is_necessary(cand, load_names, scope_index, bound_targets)
                && is_fixable(cand, load_names, scope_index, bound_targets)
        })
        .collect();
    eligible.sort_by_key(|cand| cand.alias_start);

    let mut claimed: HashSet<(Option<u32>, String)> = HashSet::new();
    let mut rejected: HashSet<u32> = HashSet::new();
    for cand in eligible {
        let key = (
            bucket_for_offset(scope_index, cand.alias_start),
            cand.name.clone(),
        );
        if !claimed.insert(key) {
            rejected.insert(cand.alias_start);
        }
    }
    rejected
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

fn check_aliases(
    source: &str,
    level: Level,
    ignore_noqa: bool,
    noqa_aliases: &HashMap<String, String>,
) -> Vec<Violation> {
    let stmts = parse_module_stmts(source);
    if stmts.is_empty() {
        return Vec::new();
    }
    let line_starts = build_line_starts(source);
    let lines: Vec<&str> = source.lines().collect();
    let scope_index = build_scope_index(&stmts);
    let load_names = collect_load_names(&stmts);
    let bound_targets = collect_import_bindings(&stmts);
    let cands = collect_alias_candidates(&stmts);
    let collisions = collect_rename_collisions(&cands, &load_names, &scope_index, &bound_targets);

    let mut violations = Vec::new();
    for cand in cands {
        if is_necessary(&cand, &load_names, &scope_index, &bound_targets) {
            continue;
        }

        let (start_line, col) = offset_to_line_col(&line_starts, cand.alias_start);
        let (end_line, end_col_inclusive) =
            offset_to_line_col(&line_starts, cand.alias_end.saturating_sub(1));
        let end_col = end_col_inclusive + 1;

        if !ignore_noqa {
            if let Some(line_text) = lines.get(start_line - 1) {
                if has_noqa(line_text, "KIS002", noqa_aliases) {
                    continue;
                }
            }
        }

        let fixable = is_fixable(&cand, &load_names, &scope_index, &bound_targets)
            && !collisions.contains(&cand.alias_start);
        let help = Some(if fixable {
            format!(
                "Remove the alias: `from {} import {}`.",
                cand.module, cand.name
            )
        } else {
            format!(
                "`{}` is shadowed or bound to more than one import in this file, so konform \
                 can't safely rename every use; remove ` as {}` and rename its uses by hand.",
                cand.asname, cand.asname
            )
        });

        violations.push(Violation {
            rule: "KIS002".to_owned(),
            line: start_line,
            col,
            end_line,
            end_col,
            message: format!(
                "Unnecessary alias: `{}` is imported as `{}`, but `{}` isn't otherwise used in \
                 this module",
                cand.name, cand.asname, cand.name
            ),
            help,
            level,
            fixable,
        });
    }
    violations
}

// ---------------------------------------------------------------------------
// Fix
// ---------------------------------------------------------------------------

fn apply_fixes(
    source: &str,
    ignore_noqa: bool,
    noqa_aliases: &HashMap<String, String>,
) -> Option<String> {
    let stmts = parse_module_stmts(source);
    if stmts.is_empty() {
        return None;
    }
    let line_starts = build_line_starts(source);
    let lines: Vec<&str> = source.lines().collect();
    let scope_index = build_scope_index(&stmts);
    let load_names = collect_load_names(&stmts);
    let bound_targets = collect_import_bindings(&stmts);
    let cands = collect_alias_candidates(&stmts);
    let collisions = collect_rename_collisions(&cands, &load_names, &scope_index, &bound_targets);

    let mut renames: HashMap<String, String> = HashMap::new();
    let mut splices: Vec<(u32, u32, String)> = Vec::new();

    for cand in cands {
        if is_necessary(&cand, &load_names, &scope_index, &bound_targets) {
            continue;
        }
        if !is_fixable(&cand, &load_names, &scope_index, &bound_targets) {
            continue;
        }
        if collisions.contains(&cand.alias_start) {
            continue;
        }

        let (line, _) = offset_to_line_col(&line_starts, cand.alias_start);
        if !ignore_noqa {
            if let Some(line_text) = lines.get(line - 1) {
                if has_noqa(line_text, "KIS002", noqa_aliases) {
                    continue;
                }
            }
        }

        splices.push((cand.alias_start, cand.alias_end, cand.name.clone()));
        renames.insert(cand.asname, cand.name);
    }

    if splices.is_empty() {
        return None;
    }

    for occ in &load_names {
        if let Some(new_name) = renames.get(&occ.name) {
            splices.push((occ.start, occ.end, new_name.clone()));
        }
    }

    // Apply from the end of the file backwards so earlier byte offsets stay
    // valid as later ones are spliced in.
    splices.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut out = source.to_owned();
    for (start, end, replacement) in splices {
        out.replace_range(start as usize..end as usize, &replacement);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule() -> Kis002Rule {
        Kis002Rule::new()
    }

    fn ctx(source: &str) -> FileContext {
        FileContext::from_source(PathBuf::from("test.py"), source.to_owned())
    }

    fn empty_cfg() -> toml::Value {
        toml::Value::Table(toml::map::Map::new())
    }

    #[test]
    fn unnecessary_alias_flagged() {
        let src = "from foo.bar import baz as bar_baz\n\nbar_baz()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 1);
        assert_eq!(viols[0].rule, "KIS002");
        assert!(viols[0].fixable);
    }

    #[test]
    fn self_alias_not_flagged() {
        let src = "from foo.bar import baz as baz\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn underscore_alias_not_flagged() {
        let src = "from foo.bar import baz as _baz\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn plain_import_as_not_flagged() {
        let src = "import foo.bar as fb\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn relative_import_not_flagged() {
        let src = "from . import baz as bar_baz\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn alias_necessary_due_to_local_collision_not_flagged() {
        let src = "from foo.bar import baz as bar_baz\n\ndef baz():\n    return 1\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn alias_necessary_due_to_other_import_binding_same_name_not_flagged() {
        let src = "from foo.bar import baz as bar_baz\nfrom qux import baz\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn unused_alias_still_flagged() {
        let src = "from foo.bar import baz as bar_baz\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 1);
    }

    #[test]
    fn noqa_suppresses() {
        let src = "from foo.bar import baz as bar_baz  # noqa: KIS002\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn category_noqa_suppresses() {
        let src = "from foo.bar import baz as bar_baz  # noqa: KIS\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty());
    }

    #[test]
    fn fix_rewrites_source_and_renames_usages() {
        let src = "from foo.bar import baz as bar_baz\n\nbar_baz()\n";
        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert_eq!(fixed, "from foo.bar import baz\n\nbaz()\n");
    }

    #[test]
    fn first_of_two_colliding_aliases_is_fixed_second_is_not() {
        // Regression: `plugin` isn't bound anywhere else in the file, so
        // each alias looks independently fixable -- but dropping *both*
        // would bind `plugin` twice in the same (module) scope. The first
        // declared alias can still be fixed safely; only the later one(s)
        // sharing its slot must stay aliased.
        let src = "from a.plugin import plugin as a_plugin\n\
                   from b.plugin import plugin as b_plugin\n\n\
                   a_plugin()\nb_plugin()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 2);
        assert!(viols[0].fixable, "first alias should still be fixable");
        assert!(
            !viols[1].fixable,
            "second alias collides with the first's fix"
        );

        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert!(fixed.contains("from a.plugin import plugin\n"));
        assert!(fixed.contains("from b.plugin import plugin as b_plugin\n"));
        assert!(fixed.contains("plugin()\nb_plugin()\n"));
    }

    #[test]
    fn fix_applied_when_colliding_alias_is_in_a_different_scope() {
        // Same post-fix name (`plugin`), but one alias lives in a nested
        // function scope -- no collision, so it should still be fixed.
        let src = "from a.plugin import plugin as a_plugin\n\n\
                   def f():\n    from b.plugin import plugin as b_plugin\n    return b_plugin()\n\n\
                   a_plugin()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 2);
        assert!(viols.iter().all(|v| v.fixable));
        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert!(fixed.contains("from a.plugin import plugin\n"));
        assert!(fixed.contains("    from b.plugin import plugin\n"));
    }

    #[test]
    fn fix_only_touches_flagged_alias_in_multi_name_import() {
        let src = "from foo.bar import baz as bar_baz, qux\n\nbar_baz()\nqux()\n";
        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert_eq!(fixed, "from foo.bar import baz, qux\n\nbaz()\nqux()\n");
    }

    #[test]
    fn fix_preserves_other_names_in_parenthesized_multiline_import() {
        // Regression test: fixing one aliased name inside a parenthesized,
        // multi-line `from X import (...)` statement must not drop or
        // otherwise disturb sibling names on their own lines.
        let src = "from foo.bar import (\n    baz as bar_baz,\n    qux,\n)\n\nprint(bar_baz)\n";
        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert_eq!(
            fixed,
            "from foo.bar import (\n    baz,\n    qux,\n)\n\nprint(baz)\n"
        );
    }

    #[test]
    fn fix_skipped_when_alias_shadowed_locally() {
        let src = "from foo.bar import baz as bar_baz\n\n\
                   def f():\n    bar_baz = 1\n    return bar_baz\n\n\
                   bar_baz()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 1);
        assert!(!viols[0].fixable);
        // No fix applied since the only violation isn't safely fixable.
        assert!(rule().fix(&ctx(src), &empty_cfg()).unwrap().is_none());
    }

    #[test]
    fn fix_skipped_when_alias_ambiguous_with_other_import() {
        let src =
            "from foo.bar import baz as bar_baz\nfrom other import thing as bar_baz\n\nbar_baz()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        // Both aliases are unnecessary in isolation, but neither is safely
        // fixable since `bar_baz` is bound to two different imports.
        assert!(viols.iter().all(|v| !v.fixable));
        assert!(rule().fix(&ctx(src), &empty_cfg()).unwrap().is_none());
    }

    #[test]
    fn violation_fields() {
        let src = "from foo.bar import baz as bar_baz\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 1);
        let v = &viols[0];
        assert_eq!(v.line, 1);
        assert_eq!(v.level, Level::Warning);
        assert!(v.message.contains("bar_baz"));
    }

    #[test]
    fn level_configurable_to_error() {
        let src = "from foo.bar import baz as bar_baz\n";
        let mut table = toml::map::Map::new();
        table.insert("level".to_owned(), toml::Value::String("error".to_owned()));
        let cfg = toml::Value::Table(table);
        let viols = rule().check(&ctx(src), &cfg);
        assert_eq!(viols[0].level, Level::Error);
    }
}
