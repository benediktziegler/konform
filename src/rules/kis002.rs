//! KIS002 — Konform Import Style: import alias policy (unnecessary aliases).
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
//! - Skips aliases whose name is listed in the module's `__all__`: that's a
//!   deliberate re-export under that name, and dropping the alias would
//!   silently remove it from the namespace.
//!
//! Two situations make an alias legitimate and exempt it from the rule:
//! - Several aliased imports of the same name from different modules in one
//!   scope (`from a.b import baz as b_baz` / `from c.d import baz as d_baz`):
//!   the aliases are what keeps the names apart.
//! - The alias matches the configured `alias-template` (see [`AliasTemplate`]),
//!   so a project can enforce one aliasing convention even where the rename
//!   isn't strictly needed. The template only exempts aliases; it never
//!   flags any.
//!
//! The fix is marked **unsafe** (see [`super::Rule::is_unsafe_fix`]): it can
//! only see uses of the alias within the file being fixed, so it's applied
//! only when `--unsafe-fixes` is passed alongside `--fix`.

use super::scope::{
    bucket_for_offset, build_line_starts, build_scope_index, collect_all_exports,
    collect_load_names, is_name_shadowed, offset_to_line_col, parse_module_stmts,
    shadowed_at_occurrences_of, NamedSpan, ScopeIndex,
};
use super::{has_noqa, FileContext, Rule};
use crate::types::{Level, Violation};
use anyhow::Result;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Once;

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
        "import-alias-policy"
    }

    fn name(&self) -> &str {
        "Import alias policy"
    }

    fn description(&self) -> &str {
        "Checks that `from X import Y as Z` only renames when needed to avoid a collision."
    }

    fn fixable(&self) -> bool {
        true
    }

    // KIS002's fix renames every use of the alias it drops, but it can only
    // ever see uses within the file being fixed -- it can't rule out, say,
    // reflection-based access to the alias by name (`getattr(mod, "bar_baz")`)
    // elsewhere. Treat it as unsafe so it's only applied with
    // `--unsafe-fixes`, matching Ruff's fix-safety separation.
    fn is_unsafe_fix(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let settings = parse_kis002_config(cfg);
        check_aliases(&ctx.source, &settings, ctx.ignore_noqa, &ctx.noqa_aliases)
    }

    fn fix(&self, ctx: &FileContext, cfg: &toml::Value) -> Result<Option<String>> {
        Ok(apply_fixes(ctx, &parse_kis002_config(cfg)))
    }

    fn explain(&self) -> String {
        "\
KIS002 — Import alias policy [sometimes fixable, unsafe]

  Checks that `from X import Y as Z` only renames the import when the
  rename is actually needed to avoid a naming collision. If `Y` isn't bound
  anywhere else the alias's uses can see, the rename buys nothing.

  Why: an unneeded alias gives one thing two names. Readers must learn that
  `bar_baz` is really `foo.bar.baz`, grepping for `baz` misses its uses, and
  the same import can end up aliased differently from file to file.

  Bad:
    from foo.bar import baz as bar_baz    # `baz` isn't used anywhere else

  Good:
    from foo.bar import baz

  Fix (with --unsafe-fixes) drops the alias and renames its uses:
    from foo.bar import baz as bar_baz   ->   from foo.bar import baz
    bar_baz()                            ->   baz()

  Not flagged:
    from foo.bar import baz as baz        # explicit re-export idiom;
                                           # see Ruff's PLC0414 instead
    from foo.bar import baz as _baz       # leading underscore: deliberate
                                           # \"don't re-export\" marker
    import x.y as z                       # plain `import ... as` is out of
                                           # scope: dropping the alias would
                                           # change what name gets bound

    __all__ = [\"bar_baz\"]
    from foo.bar import baz as bar_baz    # `bar_baz` is part of this
                                           # module's public API

  Also not flagged: several aliased imports of the same name from different
  modules, since the aliases keep them apart:
    from foo.bar import baz as bar_baz
    from nor.kind import baz as kind_baz

  Configure in [tool.konform.lint.import-alias-policy]:
    level = \"warning\"   # default: \"warning\" | \"error\"
    alias-template = \"{module_last}_{name}\"   # optional

  With `alias-template`, an alias equal to the rendered template is always
  allowed -- even when the rename isn't needed -- so a project can use one
  aliasing convention everywhere. With the template above:
    from foo.bar import baz as bar_baz    # allowed: matches the template
    from foo.bar import baz as other      # flagged: no match, not needed
  Placeholders:
    {name}          the imported name           (baz)
    {module}        dotted module, dots -> `_`   (foo_bar)
    {module_first}  first module component       (foo)
    {module_last}   last module component        (bar)
  The template only ever allows aliases; it never flags any. Placeholders
  must be written exactly (no spaces) and no other braces are allowed. An
  invalid template is reported on stderr and ignored.

  Not every violation can be auto-fixed: if the alias is also bound
  elsewhere (shadowed by a local variable, or ambiguous with a different
  import binding the same name), konform reports the violation but leaves
  it for you to fix by hand.

  This fix is marked unsafe: konform can only see uses of the alias within
  the file being fixed, so it can't rule out other, dynamic references to
  it by name (e.g. via `getattr`/`globals()`). Run `konform check --fix
  --unsafe-fixes` (or `--fix-only --unsafe-fixes`) to apply it; plain
  `--fix` reports the violation but leaves it unfixed.

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

/// `[tool.konform.lint.import-alias-policy]` settings for KIS002.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Kis002Settings {
    level: Level,
    /// Optional template; aliases equal to its rendering are always allowed.
    alias_template: Option<String>,
}

impl Default for Kis002Settings {
    fn default() -> Self {
        Self {
            level: Level::Warning,
            alias_template: None,
        }
    }
}

fn parse_kis002_config(cfg: &toml::Value) -> Kis002Settings {
    Kis002Settings::deserialize(cfg.clone()).unwrap_or_default()
}

impl Kis002Settings {
    /// The parsed `alias-template`, if one is configured and valid. An
    /// invalid template is reported on stderr (once per process) and
    /// otherwise ignored.
    fn template(&self) -> Option<AliasTemplate> {
        static WARNED: Once = Once::new();
        let src = self.alias_template.as_deref()?;
        match AliasTemplate::parse(src) {
            Ok(t) => Some(t),
            Err(err) => {
                WARNED.call_once(|| {
                    eprintln!(
                        "konform: ignoring invalid import-alias-policy.alias-template \
                         {src:?}: {err}"
                    );
                });
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Alias template
// ---------------------------------------------------------------------------

const PLACEHOLDERS: [&str; 4] = ["{name}", "{module}", "{module_first}", "{module_last}"];

/// A validated `alias-template`, e.g. `{module_last}_{name}`.
///
/// Placeholders: `{name}`, `{module}` (dots become `_`), `{module_first}`,
/// `{module_last}`. Python identifiers can't contain braces, so any other
/// `{` or `}` makes the template invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AliasTemplate(String);

impl AliasTemplate {
    fn parse(src: &str) -> Result<Self, String> {
        let rest = PLACEHOLDERS
            .iter()
            .fold(src.to_owned(), |acc, p| acc.replace(p, ""));
        if rest.contains(['{', '}']) {
            return Err(format!(
                "unknown placeholder or unbalanced brace (supported: {})",
                PLACEHOLDERS.join(", ")
            ));
        }
        Ok(Self(src.to_owned()))
    }

    fn render(&self, module: &str, name: &str) -> String {
        self.0
            .replace("{module_first}", module.split('.').next().unwrap_or(module))
            .replace("{module_last}", module.rsplit('.').next().unwrap_or(module))
            .replace("{module}", &module.replace('.', "_"))
            .replace("{name}", name)
    }
}

/// Aliases that are legitimate even if dropping them wouldn't collide, keyed
/// by `alias_start`: (a) several aliased imports of one name from different
/// modules in the same scope, (b) aliases equal to the rendered template.
fn collect_allowed_by_convention(
    cands: &[AliasCandidate],
    scope_index: &ScopeIndex,
    template: Option<&AliasTemplate>,
) -> HashSet<u32> {
    let slot_of = |cand: &'_ AliasCandidate| {
        (
            bucket_for_offset(scope_index, cand.alias_start),
            cand.name.clone(),
        )
    };
    let mut modules_by_slot: HashMap<(Option<u32>, String), HashSet<&str>> = HashMap::new();
    for cand in cands {
        modules_by_slot
            .entry(slot_of(cand))
            .or_default()
            .insert(cand.module.as_str());
    }
    cands
        .iter()
        .filter(|cand| {
            modules_by_slot
                .get(&slot_of(cand))
                .is_some_and(|m| m.len() > 1)
                || template.is_some_and(|t| t.render(&cand.module, &cand.name) == cand.asname)
        })
        .map(|cand| cand.alias_start)
        .collect()
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
// Analysis
// ---------------------------------------------------------------------------

/// Everything `check` and `fix` need to decide which aliases are flagged and
/// which of those can be fixed, computed once per file.
struct AliasAnalysis {
    cands: Vec<AliasCandidate>,
    load_names: Vec<NamedSpan>,
    scope_index: ScopeIndex,
    bound_targets: HashMap<String, HashSet<String>>,
    all_exports: HashSet<String>,
    /// `alias_start` of aliases that are legitimate by convention (several
    /// modules sharing a name, or matching the `alias-template`).
    allowed_by_convention: HashSet<u32>,
    /// `alias_start` of candidates whose fix would land on the same name as
    /// an earlier candidate's fix (see [`Self::collect_fix_collisions`]).
    fix_collisions: HashSet<u32>,
}

impl AliasAnalysis {
    /// `None` when the source has no parseable statements.
    fn new(source: &str, template: Option<&AliasTemplate>) -> Option<Self> {
        let stmts = parse_module_stmts(source);
        if stmts.is_empty() {
            return None;
        }
        let scope_index = build_scope_index(&stmts);
        let cands = collect_alias_candidates(&stmts);
        let allowed_by_convention = collect_allowed_by_convention(&cands, &scope_index, template);
        let mut analysis = Self {
            load_names: collect_load_names(&stmts),
            bound_targets: collect_import_bindings(&stmts),
            all_exports: collect_all_exports(&stmts),
            cands,
            scope_index,
            allowed_by_convention,
            fix_collisions: HashSet::new(),
        };
        analysis.fix_collisions = analysis.collect_fix_collisions();
        Some(analysis)
    }

    /// Every candidate KIS002 reports, paired with whether it can be fixed.
    fn flagged(&self) -> impl Iterator<Item = (&AliasCandidate, bool)> {
        self.cands
            .iter()
            .filter(|cand| !self.is_necessary(cand))
            .map(|cand| {
                let fixable =
                    self.is_safe_rename(cand) && !self.fix_collisions.contains(&cand.alias_start);
                (cand, fixable)
            })
    }

    /// Is the rename in `cand` actually needed to avoid a collision -- or is
    /// it otherwise not a genuine "unnecessary alias"? If so, KIS002 must not
    /// flag it at all. Checks both the import's own declaration site (an
    /// unused alias whose original name collides with something is still a
    /// real collision) and every place the alias is actually read.
    ///
    /// Also treats the alias as necessary when `asname` is listed in the
    /// module's `__all__`: that's a deliberate re-export under that name, not
    /// an accidental rename, and rewriting it would remove the exported name
    /// from the namespace entirely.
    fn is_necessary(&self, cand: &AliasCandidate) -> bool {
        self.allowed_by_convention.contains(&cand.alias_start)
            || self.all_exports.contains(&cand.asname)
            || self.bound_targets.contains_key(&cand.name)
            || is_name_shadowed(
                &cand.name,
                bucket_for_offset(&self.scope_index, cand.alias_start),
                &self.scope_index,
            )
            || shadowed_at_occurrences_of(
                &cand.name,
                &cand.asname,
                &self.load_names,
                &self.scope_index,
            )
    }

    /// Would rewriting `cand` (dropping the alias and renaming every use of
    /// `asname` to `name`) be safe, i.e. does `asname` resolve unambiguously
    /// to this exact import everywhere it's read?
    fn is_safe_rename(&self, cand: &AliasCandidate) -> bool {
        let no_local_shadow = !shadowed_at_occurrences_of(
            &cand.asname,
            &cand.asname,
            &self.load_names,
            &self.scope_index,
        );
        let unambiguous_target = self
            .bound_targets
            .get(&cand.asname)
            .is_none_or(|set| set.len() <= 1);
        no_local_shadow && unambiguous_target
    }

    /// Of every individually-eligible candidate, which ones must be rejected
    /// because an *earlier* one already claims the same `(scope bucket,
    /// post-fix name)` slot?
    ///
    /// `is_necessary`/`is_safe_rename` only check each candidate against
    /// imports and names that already exist in the source -- they can't see
    /// that *another* alias from a different module, dropped in the very same
    /// pass, would land on the identical name. E.g. (when both aliases are
    /// from the same module, so the multi-module exemption doesn't apply):
    ///
    /// ```python
    /// from a.plugin import plugin as p1    # unnecessary alias
    /// from a.plugin import plugin as p2    # unnecessary alias
    /// ```
    ///
    /// Naively fixing both independently rewrites them to two `from ...
    /// import plugin` statements bound in the same scope -- a silent
    /// collision. Only one of them can safely drop its alias, so we apply
    /// fixes in source order and let the first candidate to reach a given
    /// slot claim it; later candidates for that same slot are returned here
    /// as unfixable (equivalent to fixing the first, then re-running the
    /// check and finding the rest now genuinely necessary).
    fn collect_fix_collisions(&self) -> HashSet<u32> {
        let mut eligible: Vec<&AliasCandidate> = self
            .cands
            .iter()
            .filter(|cand| !self.is_necessary(cand) && self.is_safe_rename(cand))
            .collect();
        eligible.sort_by_key(|cand| cand.alias_start);

        let mut claimed: HashSet<(Option<u32>, &str)> = HashSet::new();
        let mut rejected: HashSet<u32> = HashSet::new();
        for cand in eligible {
            let key = (
                bucket_for_offset(&self.scope_index, cand.alias_start),
                cand.name.as_str(),
            );
            if !claimed.insert(key) {
                rejected.insert(cand.alias_start);
            }
        }
        rejected
    }
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

fn check_aliases(
    source: &str,
    settings: &Kis002Settings,
    ignore_noqa: bool,
    noqa_aliases: &HashMap<String, String>,
) -> Vec<Violation> {
    let Some(analysis) = AliasAnalysis::new(source, settings.template().as_ref()) else {
        return Vec::new();
    };
    let line_starts = build_line_starts(source);
    let lines: Vec<&str> = source.lines().collect();

    let mut violations = Vec::new();
    for (cand, fixable) in analysis.flagged() {
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
            level: settings.level,
            fixable,
        });
    }
    violations
}

// ---------------------------------------------------------------------------
// Fix
// ---------------------------------------------------------------------------

fn apply_fixes(ctx: &FileContext, settings: &Kis002Settings) -> Option<String> {
    let source = ctx.source.as_str();
    let analysis = AliasAnalysis::new(source, settings.template().as_ref())?;
    let line_starts = build_line_starts(source);
    let lines: Vec<&str> = source.lines().collect();

    let mut renames: HashMap<&str, &str> = HashMap::new();
    let mut splices: Vec<(u32, u32, String)> = Vec::new();

    for (cand, fixable) in analysis.flagged() {
        if !fixable {
            continue;
        }

        let (line, col) = offset_to_line_col(&line_starts, cand.alias_start);
        if !ctx.wants_fix("KIS002", line, col) {
            continue;
        }
        if !ctx.ignore_noqa {
            if let Some(line_text) = lines.get(line - 1) {
                if has_noqa(line_text, "KIS002", &ctx.noqa_aliases) {
                    continue;
                }
            }
        }

        splices.push((cand.alias_start, cand.alias_end, cand.name.clone()));
        renames.insert(cand.asname.as_str(), cand.name.as_str());
    }

    if splices.is_empty() {
        return None;
    }

    for occ in &analysis.load_names {
        if let Some(new_name) = renames.get(occ.name.as_str()) {
            splices.push((occ.start, occ.end, (*new_name).to_owned()));
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
    fn targeted_fix_drops_only_the_target_alias() {
        let src = "from a import x as ax\nfrom b import y as by\n\nax()\nby()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        let second = viols
            .iter()
            .find(|v| v.line == 2)
            .expect("second alias flagged");

        let mut c = ctx(src);
        c.fix_target = Some(crate::rules::FixTarget {
            rule: second.rule.clone(),
            line: second.line,
            col: second.col,
        });
        let fixed = rule().fix(&c, &empty_cfg()).unwrap().expect("target fixed");
        assert_eq!(
            fixed,
            "from a import x as ax\nfrom b import y\n\nax()\ny()\n"
        );
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
    fn alias_necessary_due_to_colliding_name_inside_type_checking_block_not_flagged() {
        // `bar` is bound at module scope by the real (runtime) import; the
        // TYPE_CHECKING-only import of a different `bar` needs the rename
        // to avoid colliding with it, even though the two imports live in
        // different branches of the source.
        let src = "from foo import bar\n\nif TYPE_CHECKING:\n    from hoo import bar as hoo_bar\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(
            viols.is_empty(),
            "alias needed to avoid colliding with the real `bar` import: {viols:?}"
        );
    }

    #[test]
    fn alias_necessary_due_to_type_checking_import_colliding_with_later_runtime_import_not_flagged()
    {
        // Same collision, opposite order: the real import comes after the
        // TYPE_CHECKING-guarded one. Import order shouldn't matter -- both
        // still land in the same (module) scope.
        let src = "if TYPE_CHECKING:\n    from hoo import bar as hoo_bar\n\nfrom foo import bar\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(
            viols.is_empty(),
            "alias needed to avoid colliding with the real `bar` import: {viols:?}"
        );
    }

    #[test]
    fn alias_inside_type_checking_block_flagged_and_fixed_when_genuinely_unnecessary() {
        // No collision this time -- `bar` isn't bound anywhere else, so the
        // alias inside the TYPE_CHECKING block is just as unnecessary as it
        // would be at module level, and should be flagged/fixed the same
        // way.
        let src = "if TYPE_CHECKING:\n    from hoo import bar as hoo_bar\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 1);
        assert!(viols[0].fixable);
        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert!(fixed.contains("    from hoo import bar\n"));
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
    fn several_aliases_of_same_name_from_different_modules_not_flagged() {
        // The aliases are what keeps the two `baz` imports apart.
        let src = "from foo.bar import baz as bar_baz\n\
                   from nor.kind import baz as kind_baz\n\n\
                   bar_baz()\nkind_baz()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty(), "got: {viols:?}");
        assert!(rule().fix(&ctx(src), &empty_cfg()).unwrap().is_none());
    }

    #[test]
    fn same_name_aliases_in_different_scopes_still_flagged() {
        let src = "from a.plugin import plugin as a_plugin\n\n\
                   def f():\n    from b.plugin import plugin as b_plugin\n    return b_plugin()\n\n\
                   a_plugin()\n";
        assert_eq!(rule().check(&ctx(src), &empty_cfg()).len(), 2);
    }

    fn template_cfg(t: &str) -> toml::Value {
        let mut table = toml::map::Map::new();
        table.insert(
            "alias-template".to_owned(),
            toml::Value::String(t.to_owned()),
        );
        toml::Value::Table(table)
    }

    #[test]
    fn alias_matching_template_allowed_even_if_unneeded() {
        let src = "from foo.bar import baz as bar_baz\n\nbar_baz()\n";
        let cfg = template_cfg("{module_last}_{name}");
        assert!(rule().check(&ctx(src), &cfg).is_empty());
        assert!(rule().fix(&ctx(src), &cfg).unwrap().is_none());
    }

    #[test]
    fn alias_not_matching_template_still_flagged() {
        let src = "from foo.bar import baz as other\n\nother()\n";
        let cfg = template_cfg("{module_last}_{name}");
        assert_eq!(rule().check(&ctx(src), &cfg).len(), 1);
    }

    #[test]
    fn invalid_template_falls_back_to_flagging() {
        let src = "from foo.bar import baz as bar_baz\n";
        let cfg = template_cfg("{bogus}_{name}");
        assert_eq!(rule().check(&ctx(src), &cfg).len(), 1);
    }

    #[test]
    fn template_renders_placeholders() {
        let t = AliasTemplate::parse("{module_first}__{module}__{module_last}_{name}").unwrap();
        assert_eq!(t.render("foo.bar.qux", "baz"), "foo__foo_bar_qux__qux_baz");
    }

    #[test]
    fn template_rejects_invalid() {
        for bad in ["{name", "name}", "{nope}", "{ name }", "{{name}}"] {
            assert!(
                AliasTemplate::parse(bad).is_err(),
                "{bad} should be invalid"
            );
        }
    }

    #[test]
    fn same_module_duplicate_names_still_collide_on_fix() {
        let src =
            "from a.plugin import plugin as p1\nfrom a.plugin import plugin as p2\n\np1()\np2()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 2);
        assert!(viols[0].fixable);
        assert!(!viols[1].fixable);
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
    fn alias_exported_via_dunder_all_not_flagged() {
        // `bar_baz` is part of this module's public API via `__all__`.
        // Dropping the alias would rename the binding to `baz`, silently
        // removing `bar_baz` from the namespace even though `__all__`
        // still advertises it -- so this isn't flagged as unnecessary at
        // all, same treatment as the `as baz` self-alias re-export idiom.
        let src = "from foo.bar import baz as bar_baz\n\n__all__ = ['bar_baz']\n\nbar_baz()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert!(viols.is_empty(), "got: {viols:?}");
        assert!(rule().fix(&ctx(src), &empty_cfg()).unwrap().is_none());
    }

    #[test]
    fn fix_applied_when_alias_not_in_dunder_all() {
        // Sanity check: an unrelated `__all__` entry doesn't block fixing
        // an alias that isn't itself exported.
        let src =
            "from foo.bar import baz as bar_baz\n\n__all__ = ['something_else']\n\nbar_baz()\n";
        let viols = rule().check(&ctx(src), &empty_cfg());
        assert_eq!(viols.len(), 1);
        assert!(viols[0].fixable);
        let fixed = rule().fix(&ctx(src), &empty_cfg()).unwrap().unwrap();
        assert!(fixed.contains("from foo.bar import baz\n"));
        assert!(fixed.contains("baz()"));
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
