//! KIS001: Konform Import Style: module-only imports.
//!
//! Checks that every `from X import Y` statement imports a sub-module rather
//! than a concrete object (function, class, or constant) from within one,
//! following the Google Python Style Guide §2.2.
//!
//! ```python
//! # Bad:   KIS001
//! from os.path import join
//!
//! # Good
//! from os import path        # `path` is a module
//! import os.path             # also fine
//! ```

use super::docs::{DocSection, Example, RuleDocs, RuleOption};
use super::import_fix::{
    bound_import_targets, overlaps_existing_import, package_import_fix, parse_ast, rewrite_imports,
    FixInfo, ParsedAlias, ParsedImport,
};
use super::scope::shadowed_at_occurrences_of;
use super::{has_noqa, FileContext, Rule};
use crate::module_probe::{ModuleCheck, ModuleProbe};
use crate::types::{Level, Violation};
use anyhow::Result;
#[cfg(test)]
use ruff_python_ast::Stmt;
#[cfg(test)]
use ruff_python_parser::parse_module;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Rule struct
// ---------------------------------------------------------------------------

pub struct Kis001Rule {
    probe: Arc<ModuleProbe>,
}

impl Kis001Rule {
    pub fn new(probe: Arc<ModuleProbe>) -> Self {
        Self { probe }
    }
}

// ---------------------------------------------------------------------------
// Rule impl
// ---------------------------------------------------------------------------

impl Rule for Kis001Rule {
    fn code(&self) -> &str {
        "KIS001"
    }

    fn category(&self) -> &str {
        "KIS"
    }

    fn category_title(&self) -> &str {
        "Import style"
    }

    fn config_name(&self) -> &str {
        "module-only-imports"
    }

    fn name(&self) -> &str {
        "Module-only imports"
    }

    fn description(&self) -> &str {
        "Checks that `from X import Y` imports only sub-modules, not objects."
    }

    fn fixable(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let (exceptions, level, unresolved_level) = parse_kis_config(cfg);
        check_imports(ctx, &self.probe, &exceptions, level, unresolved_level)
    }

    fn fix(&self, ctx: &FileContext, cfg: &toml::Value) -> Result<Option<String>> {
        let (exceptions, _level, _unresolved_level) = parse_kis_config(cfg);
        Ok(apply_fixes(ctx, &self.probe, &exceptions))
    }

    fn docs(&self) -> RuleDocs {
        let defaults = Kis001Settings::default();
        RuleDocs {
            what_it_does: "Checks that every `from X import Y` imports a module (a sub-package \
                or `.py` file), not an object such as a function, class or constant.",
            why_bad: "Importing only modules keeps the origin of every name visible at the call \
                site (`path.join(...)` rather than a bare `join(...)`) and avoids name clashes. \
                See the [Google Python Style Guide \u{a7}2.2]\
                (https://google.github.io/styleguide/pyguide.html#22-imports).",
            example: Some(Example {
                bad: "from os.path import join   # KIS001: `join` is a function, not a module",
                good: "import os.path\nfrom os import path        # `path` is a module",
            }),
            sections: vec![
                DocSection {
                    title: "Fix behavior",
                    body: "konform rewrites the import when it is safe to do so. It leaves the \
                        violation for you to fix by hand when the new import's name is already \
                        bound elsewhere in the file: as a local variable, or by a different \
                        import that would then overlap with it.\n\n\
                        When a package publishes the imported object in its `__init__.py`'s \
                        `__all__` (for example `from alib.models.suba import Foo`, with \
                        `alib/models/__init__.py` re-exporting `Foo`), the fix imports the \
                        nearest such package instead: `from alib import models` and \
                        `models.Foo`. It never picks a top-level package or a package that \
                        contains the file being fixed.",
                },
                DocSection {
                    title: "Unresolved imports",
                    body: "When a package isn't installed in this environment, KIS001 can't tell \
                        whether the imported name is a module. `unresolved-level` controls how \
                        that is reported. The extra search roots are described under \
                        [module search roots](../configuration.md#module-search-roots).",
                },
            ],
            options: vec![
                RuleOption {
                    name: "exceptions",
                    ty: "list[str]",
                    default: format!("`{:?}`", defaults.exceptions),
                    description: "Modules that may import objects directly.",
                },
                RuleOption {
                    name: "level",
                    ty: "\"warning\" | \"error\"",
                    default: format!("`\"{}\"`", defaults.level),
                    description: "Severity of violations.",
                },
                RuleOption {
                    name: "unresolved-level",
                    ty: "\"warning\" | \"error\" | \"off\"",
                    default: format!("`\"{}\"`", defaults.unresolved_level),
                    description: "Severity for imports from packages that can't be resolved.",
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

/// `[tool.konform.lint.module-only-imports]` settings for KIS001.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Kis001Settings {
    exceptions: Vec<String>,
    level: Level,
    /// Controls how KIS001 reports imports it can't validate because the
    /// package isn't installed in this environment: `"warning"` (default),
    /// `"error"`, or `"off"` (don't report at all).
    unresolved_level: String,
}

impl Default for Kis001Settings {
    fn default() -> Self {
        Self {
            exceptions: vec![
                "__future__".into(),
                "typing".into(),
                "typing_extensions".into(),
                "collections.abc".into(),
            ],
            level: Level::Error,
            unresolved_level: "warning".into(),
        }
    }
}

impl Kis001Settings {
    /// Parse `unresolved_level` into `Some(Level)`, or `None` for `"off"`.
    fn unresolved_level_enum(&self) -> Option<Level> {
        if self.unresolved_level.eq_ignore_ascii_case("off") {
            None
        } else {
            Some(self.unresolved_level.parse().unwrap_or(Level::Warning))
        }
    }
}

fn parse_kis_config(cfg: &toml::Value) -> (Vec<String>, Level, Option<Level>) {
    let settings = Kis001Settings::deserialize(cfg.clone()).unwrap_or_default();
    let unresolved_level = settings.unresolved_level_enum();
    (settings.exceptions, settings.level, unresolved_level)
}

// ---------------------------------------------------------------------------
// AST-based fix resolution
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Fix resolution
// ---------------------------------------------------------------------------

/// Attempt to find an automatic fix for importing `attr_name` from `module`.
///
/// Strategy 0 (public re-export): the nearest ancestor package of `module`
/// that lists `attr_name` in its `__all__` and binds it to the same object
/// (see [`reexporting_package_fix`]).
/// Strategy 1 (from-parent): walk the dotted module path looking for the
/// deepest parent.child split where child is itself a module.
/// Strategy 2 (bare import): fall back to `import {module}`.
fn can_fix(module: &str, attr_name: &str, probe: &ModuleProbe, file: &Path) -> Option<FixInfo> {
    if let Some(fix) = reexporting_package_fix(module, attr_name, probe, file) {
        return Some(fix);
    }
    let parts: Vec<&str> = module.split('.').collect();

    for split in (1..parts.len()).rev() {
        let parent = parts[..split].join(".");
        let child = parts[split];
        if probe.is_module(&parent, child) {
            return Some(FixInfo {
                import_stmt: format!("from {parent} import {child}"),
                import_key: format!("{parent}.{child}"),
                new_qualified: format!("{child}.{attr_name}"),
                new_bound_name: child.to_owned(),
            });
        }
    }

    Some(FixInfo {
        import_stmt: format!("import {module}"),
        import_key: module.to_owned(),
        new_qualified: format!("{module}.{attr_name}"),
        // `import a.b.c` only binds the top-level name `a` in the
        // namespace; access is always via `a.b.c...`.
        new_bound_name: parts[0].to_owned(),
    })
}

/// `from alib.models.suba import Foo`, where `alib/models/__init__.py`
/// re-exports `Foo` through `__all__`, is better written against the public
/// package: `from alib import models` and `models.Foo`.
///
/// Picks the nearest strict ancestor package of `module` whose `__all__`
/// lists `attr_name` and whose `__init__.py` binds it to the same object
/// (followed through further re-exports). Top-level packages are never
/// picked, so a fix never turns into a bare `import alib`, and neither is a
/// package that contains the file being fixed, since importing your own
/// package from inside it invites circular imports.
fn reexporting_package_fix(
    module: &str,
    attr_name: &str,
    probe: &ModuleProbe,
    file: &Path,
) -> Option<FixInfo> {
    let parts: Vec<&str> = module.split('.').collect();
    if parts.len() < 3 {
        return None; // no strict ancestor below the top level
    }
    let origin = probe.canonical_origin(&format!("{module}.{attr_name}"));
    for len in (2..parts.len()).rev() {
        let package = parts[..len].join(".");
        let Some(exports) = probe.package_exports(&package) else {
            continue;
        };
        if !exports.all.contains(attr_name) {
            continue;
        }
        let Some(bound) = exports.bindings.get(attr_name) else {
            continue;
        };
        if probe.canonical_origin(bound) != origin || probe.file_in_package(file, &package) {
            continue;
        }
        return package_import_fix(&package, attr_name);
    }
    None
}

/// The fix KIS001 plans for `alias` of `imp`, if it is a non-module import
/// at all (no safety checks).
fn planned_fix(
    imp: &ParsedImport,
    alias: &ParsedAlias,
    probe: &ModuleProbe,
    file: &Path,
) -> Option<FixInfo> {
    if probe.check(&imp.module, &alias.name) != ModuleCheck::NotModule {
        return None;
    }
    can_fix(&imp.module, &alias.name, probe, file)
}

// ---------------------------------------------------------------------------
// Violation construction
// ---------------------------------------------------------------------------

/// Source span for a violation: all values are in the same unit as
/// [`ParsedImport`] (lines are 1-based, columns are 0-based byte offsets).
struct ViolationSpan {
    start_line: usize,
    end_line: usize,
    /// 0-based column immediately past the last character (LSP-exclusive end).
    end_col: usize,
    col: usize,
}

/// Why a would-be fix must not be auto-applied even though a rewrite for it
/// was found in principle.
enum UnsafeReason<'a> {
    /// The old alias name, or the fix's new bound name, is also
    /// assigned/bound as a local variable at one of its use sites.
    Shadowed(&'a str),
    /// The fix's new bound name is already bound to a *different* module by
    /// another import elsewhere in the file.
    ImportOverlap(&'a str),
}

fn make_violation(
    span: ViolationSpan,
    module: &str,
    alias_name: &str,
    level: Level,
    fix: Option<&FixInfo>,
    unsafe_to_fix: Option<UnsafeReason<'_>>,
) -> Violation {
    let fixable = fix.is_some();
    let base_help =
        "Use only module imports, see: https://google.github.io/styleguide/pyguide.html#22-imports";
    Violation {
        rule: "KIS001".to_owned(),
        line: span.start_line,
        col: span.col,
        end_line: span.end_line,
        end_col: span.end_col,
        message: format!("KIS001: Import '{alias_name}' from '{module}' is not a module."),
        help: Some(if let Some(fix) = fix {
            format!(
                "{base_help} -- use `{}` and `{}` (fixable)",
                fix.import_stmt, fix.new_qualified
            )
        } else {
            match unsafe_to_fix {
                Some(UnsafeReason::Shadowed(colliding_name)) => format!(
                    "{base_help} (not auto-fixed: '{colliding_name}' is also assigned/bound \
                     elsewhere in this file -- a rename could not reliably tell that binding \
                     apart from the import, so it must be fixed by hand)"
                ),
                Some(UnsafeReason::ImportOverlap(colliding_name)) => format!(
                    "{base_help} (not auto-fixed: '{colliding_name}' is already imported from a \
                     different module elsewhere in this file -- introducing this fix would create \
                     two overlapping imports bound to the same name, so it must be fixed by hand)"
                ),
                None => base_help.to_owned(),
            }
        }),
        level,
        fixable,
    }
}

fn make_unknown_violation(
    span: ViolationSpan,
    module: &str,
    alias_name: &str,
    level: Level,
) -> Violation {
    let root = module.split('.').next().unwrap_or(module);
    Violation {
        rule: "KIS001".to_owned(),
        line: span.start_line,
        col: span.col,
        end_line: span.end_line,
        end_col: span.end_col,
        message: format!(
            "KIS001: Cannot verify whether '{alias_name}' from '{module}' is a module -- '{root}' was not found in this Python environment."
        ),
        help: Some(
            "Install the package in this environment so KIS001 can validate this import, or set `unresolved-level` in [tool.konform.lint.module-only-imports] to \"off\" to silence this warning."
                .to_owned(),
        ),
        level,
        fixable: false,
    }
}

// ---------------------------------------------------------------------------
// check_imports
// ---------------------------------------------------------------------------

fn check_imports(
    ctx: &FileContext,
    probe: &ModuleProbe,
    exceptions: &[String],
    level: Level,
    unresolved_level: Option<Level>,
) -> Vec<Violation> {
    let ignore_noqa = ctx.ignore_noqa;
    let noqa_aliases = &ctx.noqa_aliases;
    let stmts = ctx.stmts();
    let lines = ctx.noqa_lines();
    let (imports, all_exports) = parse_ast(stmts, ctx.line_starts());
    let scope_index = ctx.scope_index();
    let load_names = ctx.load_names();
    let mut violations = Vec::new();

    let exception_set: HashSet<&str> = exceptions.iter().map(String::as_str).collect();
    let file = ctx.path.as_path();
    let bound_targets =
        bound_import_targets(&imports, |imp, alias| planned_fix(imp, alias, probe, file));

    for imp in &imports {
        if exception_set.contains(imp.module.as_str()) {
            continue;
        }
        let start_line_str = lines
            .get(imp.start_line.saturating_sub(1))
            .copied()
            .unwrap_or("");
        if !ignore_noqa && has_noqa(start_line_str, "KIS001", noqa_aliases) {
            continue;
        }

        for alias in &imp.aliases {
            let check = probe.check(&imp.module, &alias.name);
            if check == ModuleCheck::Module {
                continue; // valid: the imported name is itself a module
            }
            let effective = alias.asname.as_deref().unwrap_or(alias.name.as_str());
            if all_exports.contains(effective) || all_exports.contains(&alias.name) {
                continue; // re-export via __all__ -- allowed
            }
            let alias_line_str = lines
                .get(alias.line.saturating_sub(1))
                .copied()
                .unwrap_or("");
            if !ignore_noqa && has_noqa(alias_line_str, "KIS001", noqa_aliases) {
                continue;
            }

            let span = ViolationSpan {
                start_line: imp.start_line,
                end_line: imp.end_line,
                end_col: imp.end_col,
                col: imp.col,
            };
            if check == ModuleCheck::Unknown {
                if let Some(unresolved_level) = unresolved_level {
                    violations.push(make_unknown_violation(
                        span,
                        &imp.module,
                        &alias.name,
                        unresolved_level,
                    ));
                }
                continue;
            }

            let name_shadowed =
                shadowed_at_occurrences_of(effective, effective, load_names, scope_index);
            let candidate_fix = if name_shadowed {
                None
            } else {
                can_fix(&imp.module, &alias.name, probe, file)
            };
            // Even when the *old* alias name isn't shadowed, the fix may
            // introduce a *new* bound name (e.g. `plugin` from
            // `from parent import plugin`) that collides with an existing
            // local binding at one of `effective`'s own call sites -- the
            // qualified replacement text (`plugin.Something`) inserted right
            // there would resolve `plugin` to that local binding instead of
            // our new import. That's equally unsafe to auto-fix -- but the
            // colliding name to report is the *new* one, not the original
            // alias name.
            let new_bound_shadow: Option<String> = candidate_fix
                .as_ref()
                .filter(|f| {
                    shadowed_at_occurrences_of(
                        &f.new_bound_name,
                        effective,
                        load_names,
                        scope_index,
                    )
                })
                .map(|f| f.new_bound_name.clone());
            // Even when the fix's new bound name isn't shadowed by a local
            // variable, it may already be bound to a *different* module by
            // another import elsewhere in the file -- applying the fix
            // would then create two overlapping imports sharing one name.
            // That's equally unsafe to auto-fix, and equally unfixable by
            // konform: there is no rename that disambiguates two imports
            // bound to the same name.
            let import_overlap: Option<String> = candidate_fix
                .as_ref()
                .filter(|_| new_bound_shadow.is_none())
                .filter(|f| overlaps_existing_import(f, &bound_targets))
                .map(|f| f.new_bound_name.clone());
            let fix = if new_bound_shadow.is_some() || import_overlap.is_some() {
                None
            } else {
                candidate_fix
            };
            let unsafe_to_fix = if name_shadowed {
                Some(UnsafeReason::Shadowed(effective))
            } else if let Some(name) = new_bound_shadow.as_deref() {
                Some(UnsafeReason::Shadowed(name))
            } else {
                import_overlap.as_deref().map(UnsafeReason::ImportOverlap)
            };
            violations.push(make_violation(
                span,
                &imp.module,
                &alias.name,
                level,
                fix.as_ref(),
                unsafe_to_fix,
            ));
        }
    }

    violations
}

// ---------------------------------------------------------------------------
// apply_fixes
// ---------------------------------------------------------------------------

fn apply_fixes(ctx: &FileContext, probe: &ModuleProbe, exceptions: &[String]) -> Option<String> {
    let ignore_noqa = ctx.ignore_noqa;
    let noqa_aliases = &ctx.noqa_aliases;
    let noqa = ctx.noqa_lines();
    let (imports, all_exports) = parse_ast(ctx.stmts(), ctx.line_starts());
    let scope_index = ctx.scope_index();
    let load_names = ctx.load_names();
    let exception_set: HashSet<&str> = exceptions.iter().map(String::as_str).collect();
    let file = ctx.path.as_path();
    let bound_targets =
        bound_import_targets(&imports, |imp, alias| planned_fix(imp, alias, probe, file));

    let skip_statement = |imp: &ParsedImport| {
        if exception_set.contains(imp.module.as_str()) {
            return true;
        }
        let start_line_str = noqa
            .get(imp.start_line.saturating_sub(1))
            .copied()
            .unwrap_or("");
        !ignore_noqa && has_noqa(start_line_str, "KIS001", noqa_aliases)
    };

    let plan_alias = |imp: &ParsedImport, alias: &ParsedAlias| -> Option<FixInfo> {
        // A KIS001 violation spans the whole statement, so a targeted fix
        // rewrites every alias in it.
        if !ctx.wants_fix("KIS001", imp.start_line, imp.col) {
            return None;
        }
        if probe.check(&imp.module, &alias.name) != ModuleCheck::NotModule {
            return None;
        }
        let effective = alias.effective();
        if all_exports.contains(effective) || all_exports.contains(&alias.name) {
            return None;
        }
        if shadowed_at_occurrences_of(effective, effective, load_names, scope_index) {
            // `effective`'s own occurrences aren't reliably reads of the
            // import: a local binding of the same name shares the scope
            // of at least one of them, and with no full scope resolution
            // we can't tell which `Load` occurrences refer to the import
            // versus that local binding -- renaming would silently
            // change behaviour rather than raise a syntax error. Skip
            // the fix; `check_imports` reports this same condition as a
            // non-fixable violation with an explanatory `help` message.
            return None;
        }
        let alias_line_str = noqa
            .get(alias.line.saturating_sub(1))
            .copied()
            .unwrap_or("");
        if !ignore_noqa && has_noqa(alias_line_str, "KIS001", noqa_aliases) {
            return None;
        }
        let fix = can_fix(&imp.module, &alias.name, probe, file)?;
        if shadowed_at_occurrences_of(&fix.new_bound_name, effective, load_names, scope_index) {
            // The fix would replace `effective`'s occurrences with
            // `fix.new_bound_name.attr` -- but at (at least) one of
            // those exact spots, `fix.new_bound_name` is itself
            // shadowed by a local binding, so the qualified
            // replacement wouldn't resolve to our new import there.
            // `check_imports` reports this as a non-fixable violation.
            return None;
        }
        if overlaps_existing_import(&fix, &bound_targets) {
            // `fix.new_bound_name` is already bound to a *different*
            // module by another import elsewhere in this file --
            // applying this fix would create two overlapping imports
            // sharing one name, which konform cannot resolve by
            // renaming. `check_imports` reports this as non-fixable.
            return None;
        }
        Some(fix)
    };

    rewrite_imports(ctx, &imports, skip_statement, plan_alias)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::scope::{build_scope_index, parse_module_stmts};
    use std::path::PathBuf;

    fn rule() -> Kis001Rule {
        Kis001Rule::new(Arc::new(ModuleProbe::default()))
    }

    fn ctx(source: &str) -> FileContext {
        FileContext::from_source(PathBuf::from("test.py"), source.to_owned())
    }

    fn empty_cfg() -> toml::Value {
        toml::Value::Table(toml::map::Map::new())
    }

    #[test]
    fn non_module_import_flagged() {
        let violations = rule().check(&ctx("from os.path import join\n"), &empty_cfg());
        assert!(!violations.is_empty(), "expected KIS001 violation");
        assert_eq!(violations[0].rule, "KIS001");
    }

    #[test]
    fn uninstalled_package_produces_warning_not_error() {
        let violations = rule().check(
            &ctx("from totally_not_a_real_package_zzz_kis001 import something\n"),
            &empty_cfg(),
        );
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert_eq!(violations[0].level, Level::Warning);
        assert!(!violations[0].fixable);
    }

    #[test]
    fn future_excepted_by_default() {
        let violations = rule().check(&ctx("from __future__ import annotations\n"), &empty_cfg());
        assert!(violations.is_empty(), "should be excepted by default");
    }

    #[test]
    fn typing_excepted_by_default() {
        // Google Style Guide §2.2 exemption: typing symbols used for static
        // analysis / type checking are allowed via `from typing import X`.
        let violations = rule().check(&ctx("from typing import Optional\n"), &empty_cfg());
        assert!(
            violations.is_empty(),
            "typing should be excepted by default"
        );
    }

    #[test]
    fn typing_extensions_excepted_by_default() {
        let violations = rule().check(
            &ctx("from typing_extensions import TypedDict\n"),
            &empty_cfg(),
        );
        assert!(
            violations.is_empty(),
            "typing_extensions should be excepted by default"
        );
    }

    #[test]
    fn collections_abc_excepted_by_default() {
        let violations = rule().check(&ctx("from collections.abc import Mapping\n"), &empty_cfg());
        assert!(
            violations.is_empty(),
            "collections.abc should be excepted by default"
        );
    }

    #[test]
    fn plain_collections_not_excepted_by_default() {
        // Only `collections.abc` is exempted per the Google Style Guide --
        // `collections` itself (e.g. `defaultdict`, a class) still requires
        // module-only imports.
        let violations = rule().check(&ctx("from collections import defaultdict\n"), &empty_cfg());
        assert!(
            !violations.is_empty(),
            "plain collections should not be excepted by default"
        );
    }

    #[test]
    fn noqa_suppresses() {
        let violations = rule().check(
            &ctx("from os.path import join  # noqa: KIS001\n"),
            &empty_cfg(),
        );
        assert!(violations.is_empty(), "noqa should suppress");
    }

    #[test]
    fn noqa_with_trailing_comma_only_suppresses_listed_codes() {
        let violations = rule().check(
            &ctx("from os.path import join  # noqa: KIS002,\n"),
            &empty_cfg(),
        );
        assert_eq!(violations.len(), 1, "KIS002 noqa must not hide KIS001");
        let violations = rule().check(
            &ctx("from os.path import join  # noqa: KIS001,\n"),
            &empty_cfg(),
        );
        assert!(violations.is_empty());
    }

    #[test]
    fn noqa_text_inside_string_literal_does_not_suppress() {
        let violations = rule().check(
            &ctx("from os.path import join; s = \"# noqa\"\n"),
            &empty_cfg(),
        );
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn category_noqa_suppresses() {
        let violations = rule().check(
            &ctx("from os.path import join  # noqa: KIS\n"),
            &empty_cfg(),
        );
        assert!(violations.is_empty(), "category noqa should suppress");
    }

    #[test]
    fn fix_rewrites_source() {
        let source = "from os.path import join\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(result.is_some(), "expected a fix");
        let fixed = result.unwrap();
        assert!(
            !fixed.contains("from os.path import join"),
            "fix should remove violation"
        );
    }

    #[test]
    fn targeted_fix_rewrites_only_the_target_import() {
        let source = "from os.path import join\nfrom json.decoder import JSONDecoder\n\njoin('a')\nJSONDecoder()\n";
        let mut c = ctx(source);
        c.fix_target = Some(crate::rules::FixTarget {
            rule: "KIS001".to_owned(),
            line: 2,
            col: 0,
        });
        let fixed = rule()
            .fix(&c, &empty_cfg())
            .unwrap()
            .expect("target import fixed");
        assert!(
            fixed.contains("decoder.JSONDecoder()"),
            "target not fixed:\n{fixed}"
        );
        assert!(
            fixed.contains("from os.path import join\n") && fixed.contains("\njoin('a')"),
            "untargeted import must be left alone:\n{fixed}"
        );
    }

    #[test]
    fn violation_fields() {
        let violations = rule().check(&ctx("from os.path import join\n"), &empty_cfg());
        let v = &violations[0];
        assert_eq!(v.rule, "KIS001");
        assert_eq!(v.line, 1);
        assert!(v.fixable);
        assert!(v.message.contains("join"));
    }

    #[test]
    fn custom_exceptions_respected() {
        let mut cfg = toml::map::Map::new();
        cfg.insert(
            "exceptions".to_owned(),
            toml::Value::Array(vec![toml::Value::String("os.path".to_owned())]),
        );
        let violations = rule().check(&ctx("from os.path import join\n"), &toml::Value::Table(cfg));
        assert!(violations.is_empty(), "os.path should be excepted");
    }

    #[test]
    fn unresolved_level_can_be_escalated_to_error() {
        let mut cfg = toml::map::Map::new();
        cfg.insert(
            "unresolved-level".to_owned(),
            toml::Value::String("error".to_owned()),
        );
        let violations = rule().check(
            &ctx("from totally_not_a_real_package_zzz_kis001 import something\n"),
            &toml::Value::Table(cfg),
        );
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert_eq!(violations[0].level, Level::Error);
        assert!(!violations[0].fixable);
    }

    #[test]
    fn unresolved_level_off_suppresses_violation() {
        let mut cfg = toml::map::Map::new();
        cfg.insert(
            "unresolved-level".to_owned(),
            toml::Value::String("off".to_owned()),
        );
        let violations = rule().check(
            &ctx("from totally_not_a_real_package_zzz_kis001 import something\n"),
            &toml::Value::Table(cfg),
        );
        assert!(
            violations.is_empty(),
            "unresolved_level = off should suppress the violation entirely"
        );
    }

    #[test]
    fn test_level_detection() {
        // Verifies that ruff's u32 level field correctly distinguishes absolute
        // from relative imports (replaces the old rustpython Debug-format hack).
        let src = "from os.path import join\n"; // absolute, level == 0
        let stmts = parse_module(src).unwrap().into_suite();
        if let Stmt::ImportFrom(n) = &stmts[0] {
            assert_eq!(n.level, 0, "absolute import should have level 0");
        }
        let src2 = "from .sub import thing\n";
        let stmts2 = parse_module(src2).unwrap().into_suite();
        if let Stmt::ImportFrom(n) = &stmts2[0] {
            assert_ne!(n.level, 0, "relative import should have level != 0");
        }
    }

    // ── multi-line paren import fixer ───────────────────────────────────────

    #[test]
    fn multiline_paren_import_all_removed() {
        // Both aliases are non-modules: all lines should be blanked.
        let source = "from os.path import (\n    join,\n    dirname,\n)\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(result.is_some(), "expected a fix");
        let fixed = result.unwrap();
        // The import statement should be gone.
        assert!(
            !fixed.contains("from os.path import"),
            "import should be removed"
        );
    }

    #[test]
    fn multiline_paren_import_one_survives_collapses() {
        // `join` is a non-module; `exists` is also a non-module.
        // Use `abspath` so we can keep one name that is clearly not a module
        // by mixing with `join` which is definitely flagged.
        // Easier: use a single real non-module + something not flagged.
        // Actually, use two non-modules and suppress one with noqa so only one
        // is in remove_set.
        let source = "from os.path import (\n    join,  # noqa: KIS001\n    dirname,\n)\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(result.is_some(), "expected a fix");
        let fixed = result.unwrap();
        // Should collapse to a single-line import keeping `join` (noqa-suppressed).
        assert!(
            fixed.contains("from os.path import join"),
            "survivor should be kept on one line: {fixed:?}"
        );
        // dirname line should be gone.
        assert!(
            !fixed.contains("dirname"),
            "dirname should be removed: {fixed:?}"
        );
    }

    #[test]
    fn multiline_paren_import_multiple_survive_reconstructed() {
        // Suppress `dirname` with noqa so that only `join` and `exists` survive.
        // We need 2+ survivors so the paren block is reconstructed.
        let source = concat!(
            "from os.path import (\n",
            "    join,  # noqa: KIS001\n",
            "    dirname,\n",
            "    exists,  # noqa: KIS001\n",
            ")\n",
        );
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(result.is_some(), "expected a fix");
        let fixed = result.unwrap();
        // join and exists survive; dirname is removed.
        assert!(fixed.contains("join"), "join should survive: {fixed:?}");
        assert!(fixed.contains("exists"), "exists should survive: {fixed:?}");
        assert!(
            !fixed.contains("dirname"),
            "dirname should be removed: {fixed:?}"
        );
        // Block should still be paren-style (reconstructed).
        assert!(
            fixed.contains("import ("),
            "should be paren-style: {fixed:?}"
        );
    }

    #[test]
    fn multiline_paren_trailing_comma() {
        // Trailing comma on last alias is handled gracefully.
        let source = "from os.path import (\n    join,\n    dirname,\n)\n";
        let result = rule().fix(&ctx(source), &empty_cfg());
        // Should not panic and should produce a result.
        assert!(result.is_ok());
    }

    #[test]
    fn multiline_paren_inline_comments() {
        // Inline comments on alias lines are stripped; aliases still parsed.
        let source = concat!(
            "from os.path import (\n",
            "    join,  # this is a function\n",
            "    dirname,  # also a function\n",
            ")\n",
        );
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(result.is_some(), "expected a fix");
        // Both should be removed (no survivors)
        let fixed = result.unwrap();
        assert!(
            !fixed.contains("from os.path import"),
            "import should be removed"
        );
    }

    #[test]
    fn all_exports_direct_assignment() {
        // __all__ = ['a', 'b'] should suppress re-export imports.
        let source = "from os.path import join\n__all__ = ['join']\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert!(
            violations.is_empty(),
            "join in __all__ should suppress KIS001"
        );
    }

    #[test]
    fn all_exports_augmented_assignment() {
        // __all__ += ['b'] should also suppress re-export imports.
        let source = "from os.path import join\n__all__ = []\n__all__ += ['join']\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert!(
            violations.is_empty(),
            "join in __all__ += should suppress KIS001"
        );
    }

    #[test]
    fn all_exports_augmented_without_direct_still_works() {
        // Even with only __all__ += (no direct __all__ = assignment before), should work.
        let source = "from os.path import join, dirname\n__all__ += ['join']\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        // join is in __all__ += so it should be suppressed; dirname is not.
        let names: Vec<&str> = violations.iter().map(|v| v.message.as_str()).collect();
        assert!(
            violations.iter().all(|v| v.message.contains("dirname")),
            "only dirname should be flagged, got: {names:?}"
        );
    }

    #[test]
    fn multiline_paren_fix_does_not_affect_clean_file() {
        // A file with no KIS001 violations returns None.
        let source = "import os\nx = 1\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(result.is_none(), "clean file should not be modified");
    }

    // ── regression tests: removed top-level imports leave no blank line ───

    #[test]
    fn fully_removed_import_leaves_no_blank_line() {
        let source = "from os.path import join\nprint(join('a'))\n";
        let fixed = rule().fix(&ctx(source), &empty_cfg()).unwrap().unwrap();
        assert_eq!(fixed, "import os.path\nprint(os.path.join('a'))\n");
    }

    #[test]
    fn fully_removed_paren_import_leaves_no_blank_lines() {
        let source = "from os.path import (\n    join,\n    exists,\n)\nimport sys\n\nprint(join('a'), exists('b'), sys)\n";
        let fixed = rule().fix(&ctx(source), &empty_cfg()).unwrap().unwrap();
        assert_eq!(
            fixed,
            "import os.path\nimport sys\n\nprint(os.path.join('a'), os.path.exists('b'), sys)\n"
        );
    }

    // ── regression tests: new-import anchor only uses top-level imports ───

    #[test]
    fn new_import_is_anchored_after_top_level_imports_only() {
        // A nested `if TYPE_CHECKING:` import must never be used as the
        // anchor for inserting a new top-level `import X` statement -- doing
        // so would land the new statement inside the indented block.
        let source = "from os.path import join\n\nif TYPE_CHECKING:\n    from types import ModuleType\n\n\ndef f(m):\n    return join('a', 'b')\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");

        let type_checking_idx = fixed
            .find("if TYPE_CHECKING:")
            .expect("TYPE_CHECKING block should survive");
        let new_import_idx = fixed
            .find("import os.path\n")
            .expect("new import os.path should be inserted");
        assert!(
            new_import_idx < type_checking_idx,
            "new import must be anchored before the TYPE_CHECKING block"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python"
        );
    }

    // ── regression tests: removing a nested import must not empty its block ─

    #[test]
    fn removing_only_import_in_block_inserts_pass() {
        // Removing the sole statement of an indented block must leave a
        // `pass` behind, or the block becomes an IndentationError.
        let source =
            "if TYPE_CHECKING:\n    from types import ModuleType\n\n\ndef f(m):\n    pass\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            fixed.contains("if TYPE_CHECKING:\n    pass\n")
                || fixed.contains("if TYPE_CHECKING:\n    import types\n"),
            "block should either keep a real import or gain a `pass`, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python"
        );
    }

    #[test]
    fn removing_all_imports_in_multi_import_block_stays_valid() {
        // Two fixable imports are the only statements of the block; once
        // both are rewritten, the block must not become empty.
        let source = "if TYPE_CHECKING:\n    from os.path import join\n    from os.path import dirname\n\n\ndef f():\n    pass\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn removing_one_of_two_imports_does_not_insert_pass() {
        // Only one of two imports in the block is rewritten; the block still
        // has real content afterwards, so no `pass` should be injected.
        let source = "if TYPE_CHECKING:\n    from os.path import join\n    import sys\n\n\ndef f():\n    return join('a', 'b')\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            !fixed.contains("    pass\n"),
            "block still has content, no pass should be inserted, got:\n{fixed}"
        );
        assert!(
            fixed.contains("    import sys\n"),
            "surviving import should remain, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    // ── regression tests: rename only touches Load-context AST names ──────

    #[test]
    fn rename_does_not_touch_docstrings_or_comments() {
        // The literal text "join" inside a docstring/comment must survive
        // untouched, while the real call is rewritten to the qualified form.
        let source = "from os.path import join\n\n\ndef f():\n    \"\"\"Calls join() to combine paths.\"\"\"\n    # join here refers to os.path.join\n    return join('a', 'b')\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            fixed.contains("Calls join() to combine paths."),
            "docstring text must be preserved verbatim, got:\n{fixed}"
        );
        assert!(
            fixed.contains("# join here refers to os.path.join"),
            "comment text must be preserved verbatim, got:\n{fixed}"
        );
        assert!(
            fixed.contains("os.path.join('a', 'b')"),
            "the real call site should be rewritten to the qualified form, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    // ── regression tests: shadowed local names must not be auto-fixed ─────

    #[test]
    fn shadowed_import_name_is_reported_as_unsafe_to_fix() {
        // `join` is both the imported alias and a local variable name that
        // is reassigned; a blind rename cannot tell those apart, so this
        // must be reported non-fixable rather than silently rewritten.
        let source =
            "from os.path import join\n\n\ndef f():\n    join = join('a', 'b')\n    return join\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert!(
            !violations[0].fixable,
            "shadowed import must not be marked fixable"
        );
        assert!(
            violations[0]
                .help
                .as_deref()
                .unwrap_or("")
                .contains("also assigned/bound"),
            "help text should explain the shadowing hazard, got: {:?}",
            violations[0].help
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(
            result.is_none(),
            "shadowed import must not be auto-fixed, got: {result:?}"
        );
    }

    #[test]
    fn non_shadowed_import_in_same_file_is_still_fixed() {
        // A shadowed import in one function must not block fixing an
        // unrelated, non-shadowed import elsewhere in the same file.
        let source = "from os.path import join, dirname\n\n\ndef f():\n    join = join('a', 'b')\n    return join\n\n\ndef g():\n    return dirname('/a/b')\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(
            violations.len(),
            2,
            "expected two violations, got: {violations:?}"
        );
        let join_violation = violations
            .iter()
            .find(|v| v.message.contains("'join'"))
            .expect("join violation present");
        let dirname_violation = violations
            .iter()
            .find(|v| v.message.contains("'dirname'"))
            .expect("dirname violation present");
        assert!(
            !join_violation.fixable,
            "join is shadowed, must not be fixable"
        );
        assert!(
            dirname_violation.fixable,
            "dirname is not shadowed, must be fixable"
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("dirname fix should still be applied");
        assert!(
            fixed.contains("os.path.dirname('/a/b')"),
            "dirname call site should be rewritten to the qualified form, got:\n{fixed}"
        );
        assert!(
            fixed.contains("join = join('a', 'b')"),
            "shadowed join code must remain untouched, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn new_bound_name_collision_is_reported_as_unsafe_to_fix() {
        // The fix wants to introduce `from xml.etree import ElementTree`,
        // binding the name `ElementTree` -- but `ElementTree` is already
        // assigned as a local variable elsewhere in this file. The old
        // alias name (`Element`) isn't shadowed at all; it's the *new*
        // name the fix itself would introduce that collides.
        let source = "from xml.etree.ElementTree import Element\n\n\ndef f():\n    ElementTree = Element()\n    return ElementTree\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert!(
            !violations[0].fixable,
            "new-bound-name collision must not be marked fixable"
        );
        assert!(
            violations[0]
                .help
                .as_deref()
                .unwrap_or("")
                .contains("'ElementTree' is also assigned/bound"),
            "help text should name the colliding *new* binding, got: {:?}",
            violations[0].help
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(
            result.is_none(),
            "import with a colliding new-bound-name must not be auto-fixed, got: {result:?}"
        );
    }

    #[test]
    fn overlapping_import_name_is_reported_as_unsafe_to_fix() {
        // `from wsgiref import util` already binds the name `util` in this
        // file. The natural KIS001 fix for `from importlib.util import
        // find_spec` would be `from importlib import util` -- but that
        // would bind the *same* name `util` to a *different* module,
        // creating two overlapping imports. There is no reliable rename
        // that disambiguates the two, so this must be reported as
        // non-fixable rather than silently emitting a colliding import.
        let source = "from wsgiref import util\nfrom importlib.util import find_spec\n\n\ndef f():\n    return util, find_spec\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(
            violations.len(),
            1,
            "expected exactly one violation (the wsgiref import is already valid style)"
        );
        assert!(
            !violations[0].fixable,
            "overlapping import name must not be marked fixable"
        );
        assert!(
            violations[0]
                .help
                .as_deref()
                .unwrap_or("")
                .contains("'util' is already imported from a different module"),
            "help text should explain the overlapping-import name, got: {:?}",
            violations[0].help
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(
            result.is_none(),
            "an import that would overlap with an existing import must not be auto-fixed, got: {result:?}"
        );
    }

    #[test]
    fn non_overlapping_sibling_import_is_still_fixed() {
        // `from importlib.util import find_spec` overlaps with the existing
        // `from wsgiref import util`, but an unrelated, non-colliding
        // violation in the same file must still be fixed normally.
        let source = "from wsgiref import util\nfrom importlib.util import find_spec\nfrom os.path import join\n\n\ndef f():\n    return util, find_spec, join('a', 'b')\n";
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 2, "expected two violations");
        let find_spec_violation = violations
            .iter()
            .find(|v| v.message.contains("find_spec"))
            .expect("find_spec violation present");
        let join_violation = violations
            .iter()
            .find(|v| v.message.contains("join"))
            .expect("join violation present");
        assert!(
            !find_spec_violation.fixable,
            "find_spec overlaps with the existing wsgiref.util import"
        );
        assert!(
            join_violation.fixable,
            "join has no overlap and must still be fixable"
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("join fix should still be applied");
        assert!(
            fixed.contains("import os.path"),
            "join's fix should still be applied, got:\n{fixed}"
        );
        assert!(
            fixed.contains("from importlib.util import find_spec"),
            "the overlapping import must be left untouched, got:\n{fixed}"
        );
    }

    // ── regression tests: shadow checks are scope-aware, not file-wide ────

    #[test]
    fn unrelated_sibling_function_parameter_does_not_block_fix() {
        // `metadata` is a parameter of a completely unrelated function; the
        // fix's own usage site (`distributions()` inside
        // `_installed_packages`) is never affected by it, so a file-wide
        // "is `metadata` bound anywhere" check must not block this fix.
        let source = concat!(
            "from importlib.metadata import distributions\n",
            "\n\n",
            "def _installed_packages():\n",
            "    return list(distributions())\n",
            "\n\n",
            "def pytest_metadata(metadata: dict) -> None:\n",
            "    metadata[\"Packages\"] = _installed_packages()\n",
        );
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert!(
            violations[0].fixable,
            "an unrelated function's parameter must not block this fix, got: {:?}",
            violations[0]
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            fixed.contains("from importlib import metadata"),
            "got:\n{fixed}"
        );
        assert!(fixed.contains("metadata.distributions()"), "got:\n{fixed}");
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn same_new_bound_name_partially_fixable_depending_on_call_site_scope() {
        // Two separate imports would both introduce `from http import
        // server`. `HTTPServer` is used at module level (safe);
        // `BaseHTTPRequestHandler` is used inside a function that has its
        // own `server` parameter, so rewriting it to
        // `server.BaseHTTPRequestHandler` there would resolve `server` to
        // the parameter instead of the new import. A file-wide shadow check
        // would conservatively block *both*; a scope-aware one blocks only
        // the second.
        let source = concat!(
            "from http.server import BaseHTTPRequestHandler\n",
            "from http.server import HTTPServer\n",
            "\n\n",
            "class ThreadedHTTPServer(HTTPServer):\n",
            "    pass\n",
            "\n\n",
            "def build_handler(server):\n",
            "    class Handler(BaseHTTPRequestHandler):\n",
            "        pass\n",
            "    return Handler\n",
        );
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(
            violations.len(),
            2,
            "expected two violations, got: {violations:?}"
        );
        let http_server_violation = violations
            .iter()
            .find(|v| v.message.contains("'HTTPServer'"))
            .expect("HTTPServer violation present");
        let base_handler_violation = violations
            .iter()
            .find(|v| v.message.contains("'BaseHTTPRequestHandler'"))
            .expect("BaseHTTPRequestHandler violation present");
        assert!(
            http_server_violation.fixable,
            "module-level usage is not shadowed, should be fixable"
        );
        assert!(
            !base_handler_violation.fixable,
            "usage inside build_handler is shadowed by its own `server` parameter"
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("partially fixable file should be rewritten");
        assert!(
            fixed.contains("class ThreadedHTTPServer(server.HTTPServer):"),
            "HTTPServer usage should be qualified, got:\n{fixed}"
        );
        assert!(
            fixed.contains("class Handler(BaseHTTPRequestHandler):"),
            "BaseHTTPRequestHandler usage must remain untouched, got:\n{fixed}"
        );
        assert!(
            fixed.contains("from http.server import BaseHTTPRequestHandler\n"),
            "unfixed import line should remain, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn late_function_import_fix_stays_in_function_scope() {
        let source = "def f():\n    from os.path import join\n    return join('a', 'b')\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");

        assert!(
            fixed.starts_with("def f():\n"),
            "late imports should not be moved to module top-level, got:\n{fixed}"
        );
        assert!(
            fixed.contains("def f():\n    import os.path\n"),
            "late import should stay in function scope, got:\n{fixed}"
        );
        assert!(
            fixed.contains("return os.path.join('a', 'b')"),
            "call site should be rewritten to qualified form, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn type_checking_import_fix_stays_in_type_checking_block() {
        let source =
            "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from os.path import join\n";
        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");

        assert!(
            fixed.contains("if TYPE_CHECKING:\n    import os.path\n"),
            "TYPE_CHECKING import should stay in guarded block, got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn match_case_nested_function_gets_own_scope_bucket() {
        // A `def` nested inside a module-level `match`/`case` block must
        // still get its own scope bucket -- just like a plain sibling
        // function does (see `unrelated_sibling_function_parameter_does_not_block_fix`)
        // -- rather than folding its parameter names into the enclosing
        // (module) scope, which would incorrectly report the
        // `distributions()` fix below as unsafe.
        let source = concat!(
            "from importlib.metadata import distributions\n",
            "\n\n",
            "match 'mode':\n",
            "    case 'prod':\n",
            "        def pytest_metadata(metadata: dict) -> None:\n",
            "            metadata['env'] = 'prod'\n",
            "    case _:\n",
            "        def pytest_metadata(metadata: dict) -> None:\n",
            "            metadata['env'] = 'dev'\n",
            "\n\n",
            "def _installed_packages():\n",
            "    return list(distributions())\n",
        );
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert!(
            violations[0].fixable,
            "a match-case-nested function's parameter must not block this fix, got: {:?}",
            violations[0]
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            fixed.contains("from importlib import metadata"),
            "got:\n{fixed}"
        );
        assert!(fixed.contains("metadata.distributions()"), "got:\n{fixed}");
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    #[test]
    fn nested_plain_import_alongside_fixable_import_is_left_untouched() {
        // KIS001 only recognizes `from X import Y` statements (`collect_imports`
        // matches `Stmt::ImportFrom` only) -- a bare `import a, b` is never
        // turned into a fix candidate. Pin that behavior for a nested block
        // that mixes a plain import with a fixable `from` import sharing the
        // same block: the plain import must survive completely untouched
        // even as its sibling is rewritten and a new nested-scope import is
        // anchored into the same block.
        let source = concat!(
            "from typing import TYPE_CHECKING\n",
            "\n",
            "if TYPE_CHECKING:\n",
            "    import os, sys\n",
            "    from os.path import join\n",
            "\n\n",
            "def use():\n",
            "    return join('a', 'b')\n",
        );
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 1, "expected exactly one violation");

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        let fixed = result.expect("fixable file should be rewritten");
        assert!(
            fixed.contains("    import os, sys\n"),
            "plain import must survive untouched, got:\n{fixed}"
        );
        assert!(
            fixed.contains("if TYPE_CHECKING:\n    import os, sys\n    import os.path\n"),
            "new nested import should replace the fixed import in place, right after the untouched plain import, got:\n{fixed}"
        );
        assert!(
            fixed.contains("return os.path.join('a', 'b')"),
            "got:\n{fixed}"
        );
        assert!(
            parse_module(&fixed).is_ok(),
            "fixed source must remain valid Python:\n{fixed}"
        );
    }

    // ── regression tests: `global` inside a function is a real module-scope
    // binding, not just local to that function's bucket ───────────────────

    #[test]
    fn global_declared_name_inside_function_is_bucketed_module_wide() {
        // `counter` is only ever bound via `global counter; counter = 1`
        // inside `bump` -- no top-level assignment at all. Even so, because
        // of `global`, it's a real module-scope name and must show up in
        // `module_names`, not just in `bump`'s own bucket.
        let source = concat!("def bump():\n", "    global counter\n", "    counter = 1\n",);
        let stmts = parse_module_stmts(source);
        let index = build_scope_index(&stmts);
        assert!(
            index.module_names.contains("counter"),
            "expected `global counter` inside a function to register as a \
             module-wide binding, got module_names={:?}",
            index.module_names
        );
    }

    #[test]
    fn global_declared_name_nested_two_levels_deep_is_still_module_wide() {
        // Same as above, but the `global` statement is buried inside an
        // `if` block nested inside a function nested inside a class, to
        // pin that `collect_global_declared_names` recurses through every
        // compound-statement/def/class kind, not just the immediate body.
        let source = concat!(
            "class Counter:\n",
            "    def bump(self, flag):\n",
            "        if flag:\n",
            "            global counter\n",
            "            counter = 1\n",
        );
        let stmts = parse_module_stmts(source);
        let index = build_scope_index(&stmts);
        assert!(
            index.module_names.contains("counter"),
            "expected deeply nested `global counter` to register as a \
             module-wide binding, got module_names={:?}",
            index.module_names
        );
    }

    #[test]
    fn fix_is_blocked_by_global_name_declared_inside_unrelated_function() {
        // Fixing `from importlib.metadata import distributions` would
        // introduce `metadata` as the new bound name (`import importlib.metadata`
        // + `metadata.distributions()`). `metadata` is never assigned at
        // module level and is only ever *read* inside `clear_cache` in a way
        // the old flat `collect_bound_names` check would have caught -- but
        // `clear_cache` declares it `global` and assigns it there, resetting
        // a module-level cache dict. That makes `metadata` a genuine
        // module-scope name despite living entirely inside a function body;
        // applying the fix would silently shadow it for the rest of the
        // module, so the fix must be rejected.
        let source = concat!(
            "from importlib.metadata import distributions\n",
            "\n\n",
            "def clear_cache():\n",
            "    global metadata\n",
            "    metadata = {}\n",
            "\n\n",
            "def use():\n",
            "    return list(distributions())\n",
        );
        let violations = rule().check(&ctx(source), &empty_cfg());
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        assert!(
            !violations[0].fixable,
            "fix introducing `metadata` must be rejected: `clear_cache` \
             declares it `global` and assigns it, so it's a real module-level \
             name, got: {:?}",
            violations[0]
        );

        let result = rule().fix(&ctx(source), &empty_cfg()).unwrap();
        assert!(
            result.is_none(),
            "file has no safely-fixable violations, fix() must return None"
        );
    }

    // ── fix target: the package that re-exports the object ───────────────

    /// A KIS001 rule over a temp tree of `files`, plus a context for
    /// `source` at `rel` inside that tree.
    fn reexport_project(files: &[(&str, &str)]) -> (tempfile::TempDir, Kis001Rule) {
        let dir = tempfile::TempDir::new().unwrap();
        for (path, body) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let probe = ModuleProbe::for_roots(&[dir.path().to_path_buf()]);
        (dir, Kis001Rule::new(Arc::new(probe)))
    }

    fn ctx_in(dir: &tempfile::TempDir, rel: &str, source: &str) -> FileContext {
        FileContext::from_source(dir.path().join(rel), source.to_owned())
    }

    const ALIB: &[(&str, &str)] = &[
        ("alib/__init__.py", ""),
        (
            "alib/models/__init__.py",
            "from .suba import Foo\nfrom .suba import Other as Renamed\n\
             def bar(): ...\n__all__ = ['Foo', 'bar', 'Renamed', 'Gone']\n",
        ),
        (
            "alib/models/suba.py",
            "class Foo: ...\nclass Other: ...\ndef bar(): ...\n",
        ),
    ];

    #[test]
    fn fix_prefers_the_package_that_reexports_the_object() {
        let (dir, rule) = reexport_project(ALIB);
        let source = "from alib.models.suba import Foo\n\nFoo()\n";
        let ctx = ctx_in(&dir, "app.py", source);
        let v = rule.check(&ctx, &empty_cfg());
        assert_eq!(v.len(), 1);
        assert!(v[0].fixable);
        let help = v[0].help.as_deref().unwrap();
        assert!(
            help.contains("use `from alib import models` and `models.Foo`"),
            "{help}"
        );
        let fixed = rule.fix(&ctx, &empty_cfg()).unwrap().unwrap();
        assert_eq!(fixed, "from alib import models\n\nmodels.Foo()\n");
    }

    #[test]
    fn fix_follows_reexport_under_an_alias_name_only_when_it_matches() {
        let (dir, rule) = reexport_project(ALIB);
        // `Renamed` is `suba.Other`; importing `Other` itself is not
        // published under that name, so the fix stays on `suba`.
        let source = "from alib.models.suba import Other\n\nOther()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "app.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(fixed, "from alib.models import suba\n\nsuba.Other()\n");
    }

    #[test]
    fn fix_ignores_a_reexport_of_a_different_object_with_the_same_name() {
        let (dir, rule) = reexport_project(ALIB);
        // `alib.models.bar` is defined in the package, not `suba.bar`.
        let source = "from alib.models.suba import bar\n\nbar()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "app.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(fixed, "from alib.models import suba\n\nsuba.bar()\n");
    }

    #[test]
    fn fix_needs_the_name_in_all() {
        let (dir, rule) = reexport_project(&[
            ("alib/__init__.py", ""),
            ("alib/models/__init__.py", "from .suba import Foo\n"),
            ("alib/models/suba.py", "class Foo: ...\n"),
        ]);
        let source = "from alib.models.suba import Foo\n\nFoo()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "app.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(fixed, "from alib.models import suba\n\nsuba.Foo()\n");
    }

    #[test]
    fn fix_picks_the_nearest_reexporting_package() {
        let (dir, rule) = reexport_project(&[
            ("alib/__init__.py", ""),
            (
                "alib/core/__init__.py",
                "from .models import Foo\n__all__ = ['Foo']\n",
            ),
            (
                "alib/core/models/__init__.py",
                "from .suba import Foo\n__all__ = ['Foo']\n",
            ),
            ("alib/core/models/suba.py", "class Foo: ...\n"),
        ]);
        let source = "from alib.core.models.suba import Foo\n\nFoo()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "app.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(fixed, "from alib.core import models\n\nmodels.Foo()\n");
    }

    #[test]
    fn fix_never_climbs_to_a_top_level_package() {
        let (dir, rule) = reexport_project(&[
            (
                "alib/__init__.py",
                "from .suba import Foo\n__all__ = ['Foo']\n",
            ),
            ("alib/suba.py", "class Foo: ...\n"),
        ]);
        let source = "from alib.suba import Foo\n\nFoo()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "app.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(fixed, "from alib import suba\n\nsuba.Foo()\n");
    }

    #[test]
    fn fix_inside_the_reexporting_package_keeps_the_submodule() {
        let (dir, rule) = reexport_project(ALIB);
        let source = "from alib.models.suba import Foo\n\nFoo()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "alib/models/other.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(fixed, "from alib.models import suba\n\nsuba.Foo()\n");
    }

    #[test]
    fn importing_from_the_reexporting_package_already_fixes_to_it() {
        let (dir, rule) = reexport_project(ALIB);
        let source = "from alib.models import Foo, bar\n\nFoo()\nbar()\n";
        let fixed = rule
            .fix(&ctx_in(&dir, "app.py", source), &empty_cfg())
            .unwrap()
            .unwrap();
        assert_eq!(
            fixed,
            "from alib import models\n\nmodels.Foo()\nmodels.bar()\n"
        );
    }
}
