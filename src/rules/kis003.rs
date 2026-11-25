//! KIS003: Konform Import Style: prefer the package for re-exported submodules.
//!
//! When a package publishes a submodule through its `__init__.py`'s
//! `__all__`, import the package and reach the submodule through it.
//!
//! ```python
//! # Bad:   KIS003 (alib/models/__init__.py: __all__ = ["suba", "subb"])
//! from alib.models import suba
//! from alib.models import subb
//!
//! # Good
//! from alib import models
//! models.suba
//! models.subb
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
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

const CODE: &str = "KIS003";

pub struct Kis003Rule {
    probe: Arc<ModuleProbe>,
}

impl Kis003Rule {
    pub fn new(probe: Arc<ModuleProbe>) -> Self {
        Self { probe }
    }
}

impl Rule for Kis003Rule {
    fn code(&self) -> &str {
        CODE
    }

    fn category(&self) -> &str {
        "KIS"
    }

    fn category_title(&self) -> &str {
        "Import style"
    }

    fn config_name(&self) -> &str {
        "prefer-package-imports"
    }

    fn name(&self) -> &str {
        "Prefer package imports"
    }

    fn description(&self) -> &str {
        "Checks that submodules a package re-exports in `__all__` are used through the package."
    }

    fn fixable(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let settings = Kis003Settings::parse(cfg);
        check_imports(ctx, &self.probe, &settings)
    }

    fn fix(&self, ctx: &FileContext, cfg: &toml::Value) -> Result<Option<String>> {
        let settings = Kis003Settings::parse(cfg);
        Ok(apply_fixes(ctx, &self.probe, &settings))
    }

    fn docs(&self) -> RuleDocs {
        let defaults = Kis003Settings::default();
        RuleDocs {
            what_it_does: "Flags `from P import N` when `N` is a submodule that package `P` \
                publishes in its `__init__.py`'s `__all__` (and imports there), and suggests \
                importing `P` itself and using `P.N`.",
            why_bad: "A package that lists a submodule in `__all__` makes it part of the \
                package's public surface. Going through the package (`models.suba`) keeps the \
                origin of every name visible at the call site and needs one import for all of \
                the package's submodules instead of one per submodule.",
            example: Some(Example {
                bad: "# alib/models/__init__.py:\n\
                      #   from . import suba, subb\n\
                      #   __all__ = [\"suba\", \"subb\"]\n\
                      from alib.models import suba   # KIS003\n\
                      from alib.models import subb   # KIS003\n\n\
                      suba.run()\n\
                      subb.run()",
                good: "from alib import models\n\nmodels.suba.run()\nmodels.subb.run()",
            }),
            sections: vec![
                DocSection {
                    title: "What is not flagged",
                    body: "- Submodules missing from the package's literal `__all__`, or that \
                        its `__init__.py` never imports (`models.suba` would fail at runtime).\n\
                        - Imports from a top-level package (`from alib import models`): the \
                        rule never suggests a bare `import alib`.\n\
                        - Files inside the package itself, where importing the package invites \
                        circular imports.\n\
                        - Relative imports, star imports and names re-exported through the \
                        file's own `__all__`.\n\
                        - Packages that aren't installed in this environment.",
                },
                DocSection {
                    title: "Fix behavior",
                    body: "konform replaces the import with `from <parent> import <package>` \
                        (reusing it when it is already there) and rewrites every use of the \
                        submodule to `<package>.<submodule>`. Like KIS001, it leaves the \
                        violation to fix by hand when the package's name is already bound \
                        elsewhere in the file. Classes, functions and constants a package \
                        re-exports are covered by [KIS001](kis001.md), whose fix picks the \
                        re-exporting package too.",
                },
            ],
            options: vec![
                RuleOption {
                    name: "exceptions",
                    ty: "list[str]",
                    default: format!("`{:?}`", defaults.exceptions),
                    description: "Packages whose submodules may be imported directly.",
                },
                RuleOption {
                    name: "level",
                    ty: "\"warning\" | \"error\"",
                    default: format!("`\"{}\"`", defaults.level),
                    description: "Severity of violations.",
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// `[tool.konform.lint.prefer-package-imports]` settings for KIS003.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Kis003Settings {
    exceptions: Vec<String>,
    level: Level,
}

impl Default for Kis003Settings {
    fn default() -> Self {
        Self {
            exceptions: Vec::new(),
            level: Level::Warning,
        }
    }
}

impl Kis003Settings {
    fn parse(cfg: &toml::Value) -> Self {
        Self::deserialize(cfg.clone()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// The fix for `alias` of `imp` if it imports a submodule its package
/// re-exports, before any safety checks.
fn planned_fix(
    imp: &ParsedImport,
    alias: &ParsedAlias,
    probe: &ModuleProbe,
    file: &Path,
) -> Option<FixInfo> {
    let package = imp.module.as_str();
    let name = alias.name.as_str();
    if name == "*" || !package.contains('.') {
        return None;
    }
    if probe.check(package, name) != ModuleCheck::Module {
        return None;
    }
    let exports = probe.package_exports(package)?;
    if !exports.all.contains(name) || !exports.bindings.contains_key(name) {
        return None;
    }
    if probe.file_in_package(file, package) {
        return None;
    }
    package_import_fix(package, name)
}

/// Why a planned fix must not be applied automatically.
enum UnsafeReason {
    Shadowed(String),
    ImportOverlap(String),
}

/// Everything about one alias the checker and the fixer both need.
struct Candidate {
    fix: FixInfo,
    unsafe_reason: Option<UnsafeReason>,
}

struct Analysis<'a> {
    ctx: &'a FileContext,
    probe: &'a ModuleProbe,
    settings: &'a Kis003Settings,
    imports: Vec<ParsedImport>,
    all_exports: HashSet<String>,
}

impl<'a> Analysis<'a> {
    fn new(ctx: &'a FileContext, probe: &'a ModuleProbe, settings: &'a Kis003Settings) -> Self {
        let (imports, all_exports) = parse_ast(ctx.stmts(), ctx.line_starts());
        Self {
            ctx,
            probe,
            settings,
            imports,
            all_exports,
        }
    }

    fn noqa_on_line(&self, line: usize) -> bool {
        if self.ctx.ignore_noqa {
            return false;
        }
        let lines = self.ctx.noqa_lines();
        let text = lines.get(line.saturating_sub(1)).copied().unwrap_or("");
        has_noqa(text, CODE, &self.ctx.noqa_aliases)
    }

    fn skip_statement(&self, imp: &ParsedImport) -> bool {
        self.settings.exceptions.iter().any(|e| e == &imp.module)
            || self.noqa_on_line(imp.start_line)
    }

    /// Visit every flagged alias with its planned fix and safety verdict.
    fn for_each_candidate(&self, mut visit: impl FnMut(&ParsedImport, &ParsedAlias, Candidate)) {
        let file = self.ctx.path.as_path();
        let probe = self.probe;
        let bound_targets = bound_import_targets(&self.imports, |imp, alias| {
            planned_fix(imp, alias, probe, file)
        });
        let load_names = self.ctx.load_names();
        let scope_index = self.ctx.scope_index();

        for imp in &self.imports {
            if self.skip_statement(imp) {
                continue;
            }
            for alias in &imp.aliases {
                let effective = alias.effective();
                if self.all_exports.contains(effective) || self.all_exports.contains(&alias.name) {
                    continue; // re-exported by this file on purpose
                }
                if self.noqa_on_line(alias.line) {
                    continue;
                }
                let Some(fix) = planned_fix(imp, alias, probe, file) else {
                    continue;
                };
                let unsafe_reason =
                    if shadowed_at_occurrences_of(effective, effective, load_names, scope_index) {
                        Some(UnsafeReason::Shadowed(effective.to_owned()))
                    } else if shadowed_at_occurrences_of(
                        &fix.new_bound_name,
                        effective,
                        load_names,
                        scope_index,
                    ) {
                        Some(UnsafeReason::Shadowed(fix.new_bound_name.clone()))
                    } else if overlaps_existing_import(&fix, &bound_targets) {
                        Some(UnsafeReason::ImportOverlap(fix.new_bound_name.clone()))
                    } else {
                        None
                    };
                visit(imp, alias, Candidate { fix, unsafe_reason });
            }
        }
    }
}

fn check_imports(
    ctx: &FileContext,
    probe: &ModuleProbe,
    settings: &Kis003Settings,
) -> Vec<Violation> {
    let analysis = Analysis::new(ctx, probe, settings);
    let mut violations = Vec::new();
    analysis.for_each_candidate(|imp, alias, candidate| {
        let suggestion = format!(
            "`{}` publishes `{}` in `__all__`: use `{}` and `{}`",
            imp.module, alias.name, candidate.fix.import_stmt, candidate.fix.new_qualified
        );
        let fixable = candidate.unsafe_reason.is_none();
        let help = match candidate.unsafe_reason {
            None => format!("{suggestion} (fixable)"),
            Some(UnsafeReason::Shadowed(name)) => format!(
                "{suggestion} (not auto-fixed: '{name}' is also assigned/bound elsewhere in this \
                 file, so it must be fixed by hand)"
            ),
            Some(UnsafeReason::ImportOverlap(name)) => format!(
                "{suggestion} (not auto-fixed: '{name}' is already imported from a different \
                 module elsewhere in this file, so it must be fixed by hand)"
            ),
        };
        violations.push(Violation {
            rule: CODE.to_owned(),
            line: imp.start_line,
            col: imp.col,
            end_line: imp.end_line,
            end_col: imp.end_col,
            message: format!(
                "{CODE}: Import package '{}' instead of its re-exported submodule '{}'.",
                imp.module, alias.name
            ),
            help: Some(help),
            level: settings.level,
            fixable,
        });
    });
    violations
}

fn apply_fixes(
    ctx: &FileContext,
    probe: &ModuleProbe,
    settings: &Kis003Settings,
) -> Option<String> {
    let analysis = Analysis::new(ctx, probe, settings);
    // (import start line, alias name) -> fix, for the safe candidates only.
    let mut planned: std::collections::HashMap<(usize, String), FixInfo> =
        std::collections::HashMap::new();
    analysis.for_each_candidate(|imp, alias, candidate| {
        if candidate.unsafe_reason.is_none() && ctx.wants_fix(CODE, imp.start_line, imp.col) {
            planned.insert((imp.start_line, alias.name.clone()), candidate.fix);
        }
    });
    if planned.is_empty() {
        return None;
    }
    rewrite_imports(
        ctx,
        &analysis.imports,
        |imp| analysis.skip_statement(imp),
        |imp, alias| planned.get(&(imp.start_line, alias.name.clone())).cloned(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
