//! KIS001 — Konform Import Style: module-only imports.
//!
//! Checks that every `from X import Y` statement imports a sub-module rather
//! than a concrete object (function, class, or constant) from within one,
//! following the Google Python Style Guide §2.2.
//!
//! ```python
//! # Bad  — KIS001
//! from os.path import join
//!
//! # Good
//! from os import path        # `path` is a module
//! import os.path             # also fine
//! ```

use super::{has_noqa, FileContext, Rule};
use crate::module_probe::{ModuleCheck, ModuleProbe};
use crate::types::{Level, Violation};
use anyhow::Result;
use ruff_python_ast::{Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use std::collections::{HashMap, HashSet};
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

    fn name(&self) -> &str {
        "Google-style imports"
    }

    fn description(&self) -> &str {
        "Checks that `from X import Y` imports only sub-modules, not objects."
    }

    fn fixable(&self) -> bool {
        true
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let (exceptions, level, unresolved_level) = parse_kis_config(cfg);
        check_imports(
            &ctx.source,
            &self.probe,
            &exceptions,
            level,
            unresolved_level,
            ctx.ignore_noqa,
            &ctx.noqa_aliases,
        )
    }

    fn fix(&self, ctx: &FileContext, cfg: &toml::Value) -> Result<Option<String>> {
        let (exceptions, _level, _unresolved_level) = parse_kis_config(cfg);
        Ok(apply_fixes(
            &ctx.source,
            &self.probe,
            &exceptions,
            ctx.ignore_noqa,
            &ctx.noqa_aliases,
        ))
    }

    fn explain(&self) -> String {
        "\
KIS001 — Google-style imports [fixable]

  Checks that every `from X import Y` imports a module (sub-package or .py
  file), not an object (class, function, or constant) from within one.

  Bad:
    from os.path import join      # join is a function

  Good:
    from os import path           # path is the os.path module
    import os.path                # also fine

  Configure exceptions in [tool.konform.KIS]:
    exceptions = [\"__future__\", \"typing\", \"typing_extensions\", \"collections.abc\"]

  When a package isn't installed in this environment, KIS001 can't tell
  whether the imported name is a module or not. Control how that's reported
  in [tool.konform.KIS]:
    unresolved_level = \"warning\"   # default: \"warning\" | \"error\" | \"off\"

  Suppress per-line:
    from os.path import join   # noqa: KIS001
    from os.path import join   # noqa: KIS      (silences all KIS rules)
"
        .to_owned()
    }
}

// ---------------------------------------------------------------------------
// Config helper
// ---------------------------------------------------------------------------

fn parse_kis_config(cfg: &toml::Value) -> (Vec<String>, Level, Option<Level>) {
    let exceptions = cfg
        .get("exceptions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "__future__".into(),
                "typing".into(),
                "typing_extensions".into(),
                "collections.abc".into(),
            ]
        });
    let level = cfg
        .get("level")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(Level::Error);
    // `unresolved_level` controls how KIS001 reports imports it can't
    // validate because the package isn't installed in this environment:
    // "warning" (default), "error", or "off" (don't report at all).
    let unresolved_level = match cfg.get("unresolved_level").and_then(|v| v.as_str()) {
        Some(s) if s.eq_ignore_ascii_case("off") => None,
        Some(s) => Some(s.parse().unwrap_or(Level::Warning)),
        None => Some(Level::Warning),
    };
    (exceptions, level, unresolved_level)
}

// ---------------------------------------------------------------------------
// Internal data structures
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ParsedAlias {
    name: String,
    asname: Option<String>,
    /// 1-based line number where this alias appears.
    line: usize,
}

#[derive(Debug)]
struct ParsedImport {
    module: String,
    aliases: Vec<ParsedAlias>,
    /// 1-based line of the `from` keyword.
    start_line: usize,
    /// 1-based line of the last line of this import statement.
    end_line: usize,
    /// 0-based column immediately past the last character of the statement.
    end_col: usize,
    /// 0-based column of the `from` keyword.
    col: usize,
    /// True when the import lives inside `if TYPE_CHECKING:`.
    /// Tracked for future use (e.g. PT rules may treat TC imports differently).
    #[allow(dead_code)]
    in_type_checking: bool,
    /// True when this import sits directly in the module's top-level body
    /// (not nested inside an `if`/`def`/`class` block). New imports must
    /// only ever be inserted at this level.
    is_top_level: bool,
    /// Identifies the enclosing compound-statement body this import lives
    /// in: the byte offset of the first statement in that body. All sibling
    /// imports sharing the same body share this id. Meaningless (0) for
    /// top-level imports, which are never at risk of leaving an empty block.
    block_id: u32,
    /// Total number of statements (of any kind) in the enclosing body
    /// identified by `block_id`. Used to detect when every statement in a
    /// block turns out to be an import that gets fully removed, in which
    /// case the block would otherwise end up empty (invalid Python) and one
    /// line must be replaced with `pass` instead of being blanked.
    block_size: usize,
}

#[derive(Debug, Clone)]
struct FixInfo {
    import_stmt: String,
    import_key: String,
    #[allow(dead_code)]
    old_local: String,
    new_qualified: String,
    /// The name this fix's `import_stmt` binds into the file's namespace
    /// (e.g. `plugin` for `from parent import plugin`, or `os` for
    /// `import os.path`). If this name is already assigned/bound elsewhere
    /// in the file, applying the fix would silently shadow it, so callers
    /// must treat that case as unsafe to auto-fix.
    new_bound_name: String,
}

// ---------------------------------------------------------------------------
// Line index: byte offset → (line, col)
// ---------------------------------------------------------------------------

/// Build a sorted Vec of byte offsets at which each line starts.
/// `line_starts[0]` is always 0.
fn build_line_starts(source: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    let mut offset = 0u32;
    for b in source.bytes() {
        offset += 1;
        if b == b'\n' {
            starts.push(offset);
        }
    }
    starts
}

/// Convert a byte offset to `(1-based line, 0-based column)`.
fn offset_to_line_col(line_starts: &[u32], offset: u32) -> (usize, usize) {
    let idx = line_starts
        .partition_point(|&s| s <= offset)
        .saturating_sub(1);
    (idx + 1, (offset - line_starts[idx]) as usize)
}

// ---------------------------------------------------------------------------
// AST-based import collection
// ---------------------------------------------------------------------------

/// Parse `source` and return its top-level statement list, or an empty
/// list on a parse error. A small shared helper for the several places that
/// need the raw AST rather than the `(imports, exports)` pair `parse_ast`
/// extracts from it.
fn parse_module_stmts(source: &str) -> Vec<Stmt> {
    match parse_module(source) {
        Ok(parsed) => parsed.into_suite().into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

/// Parse `source` once and extract all absolute `from X import Y` statements
/// together with the module's `__all__` exports.
///
/// Returns `(imports, all_exports)`.  On parse error both collections are
/// empty so the caller silently skips the file.
fn parse_ast(source: &str) -> (Vec<ParsedImport>, HashSet<String>) {
    let stmts = parse_module_stmts(source);
    if stmts.is_empty() {
        return (vec![], HashSet::new());
    }
    let line_starts = build_line_starts(source);
    let mut imports = Vec::new();
    collect_imports(&stmts, false, false, &line_starts, &mut imports);
    let exports = collect_all_exports(&stmts);
    (imports, exports)
}

/// Returns `true` iff `expr` is `TYPE_CHECKING` or `typing.TYPE_CHECKING`.
fn is_type_checking_guard(expr: &Expr) -> bool {
    match expr {
        Expr::Name(n) => n.id.as_str() == "TYPE_CHECKING",
        Expr::Attribute(a) => a.attr.as_str() == "TYPE_CHECKING",
        _ => false,
    }
}

/// Recursively walk `stmts` and collect all absolute `from X import Y`
/// statements into `out`, tagging each with whether it is inside an
/// `if TYPE_CHECKING:` block.
fn collect_imports(
    stmts: &[Stmt],
    in_type_checking: bool,
    // True when `stmts` is itself the body of a compound statement (if/def/
    // class/etc.) rather than the module's top-level statement list.
    nested: bool,
    line_starts: &[u32],
    out: &mut Vec<ParsedImport>,
) {
    // Identifies this exact body (shared by every statement directly in
    // it) so fixes can later tell whether *all* statements in the body are
    // imports that end up fully removed, which would otherwise leave an
    // empty (invalid) indented block.
    let block_id = stmts.first().map_or(0, |s| u32::from(s.range().start()));
    let block_size = stmts.len();

    for stmt in stmts {
        match stmt {
            // ── from X import Y ──────────────────────────────────────────
            Stmt::ImportFrom(node) => {
                // Skip relative imports (level > 0) and bare `from . import X`
                // (no module name).
                let module = match &node.module {
                    Some(m) => m.as_str().to_owned(),
                    None => continue, // bare `from . import X`
                };
                if node.level != 0 {
                    continue;
                }

                let start_off = u32::from(node.range().start());
                // end() points one past the last byte; use saturating_sub to
                // stay on the closing token.
                let end_off = u32::from(node.range().end()).saturating_sub(1);
                let (start_line, col) = offset_to_line_col(line_starts, start_off);
                let (end_line, end_col_inclusive) = offset_to_line_col(line_starts, end_off);
                // end_col_inclusive points at the last byte; add 1 so the
                // LSP range is exclusive (covers the final character).
                let end_col = end_col_inclusive + 1;

                let aliases = node
                    .names
                    .iter()
                    .map(|alias| {
                        let alias_off = u32::from(alias.range().start());
                        let (alias_line, _) = offset_to_line_col(line_starts, alias_off);
                        ParsedAlias {
                            name: alias.name.as_str().to_owned(),
                            asname: alias.asname.as_deref().map(str::to_owned),
                            line: alias_line,
                        }
                    })
                    .collect();

                out.push(ParsedImport {
                    module,
                    aliases,
                    start_line,
                    end_line,
                    end_col,
                    col,
                    in_type_checking,
                    is_top_level: !nested,
                    block_id,
                    block_size,
                });
            }

            // ── if TYPE_CHECKING: … ──────────────────────────────────────
            Stmt::If(node) => {
                let guard = is_type_checking_guard(&node.test);
                collect_imports(
                    &node.body,
                    in_type_checking || guard,
                    true,
                    line_starts,
                    out,
                );
                // elif/else branches are NOT considered TYPE_CHECKING scope.
                for clause in &node.elif_else_clauses {
                    collect_imports(&clause.body, in_type_checking, true, line_starts, out);
                }
            }

            // ── walk into function / class bodies ────────────────────────
            // Unusual but legal: imports can appear in nested scopes.
            // In ruff's AST, async functions share `Stmt::FunctionDef` with
            // an `is_async` flag instead of a separate `AsyncFunctionDef`.
            Stmt::FunctionDef(node) => {
                collect_imports(&node.body, in_type_checking, true, line_starts, out);
            }
            Stmt::ClassDef(node) => {
                collect_imports(&node.body, in_type_checking, true, line_starts, out);
            }

            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// AST-based Load-context name collection
// ---------------------------------------------------------------------------

/// Recursively collect the byte-offset span and identifier of every
/// `Expr::Name` used in **Load** context (i.e. a value is being *read*).
///
/// Only Load-context occurrences may safely be rewritten to a dotted
/// qualified name (`join` -> `os.path.join`): Python does not allow a dotted
/// path as an assignment target, walrus (`:=`) target, `for` target, `as`
/// binding, function parameter, etc. Restricting renames to these exact
/// spans (rather than a textual word-boundary scan) also naturally skips
/// occurrences inside string literals, docstrings and comments, since those
/// never produce `Expr::Name` nodes.
fn collect_load_names(stmts: &[Stmt]) -> Vec<(u32, u32, String)> {
    struct LoadNameVisitor(Vec<(u32, u32, String)>);

    impl<'a> ruff_python_ast::visitor::Visitor<'a> for LoadNameVisitor {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr {
                if name.ctx.is_load() {
                    self.0.push((
                        u32::from(name.range().start()),
                        u32::from(name.range().end()),
                        name.id.to_string(),
                    ));
                }
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }
    }

    let mut visitor = LoadNameVisitor(Vec::new());
    for stmt in stmts {
        ruff_python_ast::visitor::Visitor::visit_stmt(&mut visitor, stmt);
    }
    visitor.0
}

/// Collect every identifier that is **bound** (assigned/declared) somewhere
/// in `stmts`, at any scope: assignment targets, walrus (`:=`) targets,
/// `for`/`with`/`except ... as` bindings, function parameters,
/// function/class names, and `global`/`nonlocal` declarations.
///
/// A rename is only sound when the name resolves unambiguously to the
/// import everywhere it's read. If the same identifier is *also* bound
/// (e.g. shadowed by a local variable, as in `plugin = plugin.Foo()`) then
/// some `Load` occurrences of that name refer to the local binding instead
/// of the import, and a blind rename would rewrite those too -- silently
/// changing runtime behaviour rather than producing a syntax error. This
/// rule has no scope/binding resolution, so it treats any such collision as
/// unsafe and skips the fix entirely (see `can_fix`'s caller).
fn collect_bound_names(stmts: &[Stmt]) -> HashSet<String> {
    struct BoundNameVisitor(HashSet<String>);

    impl<'a> ruff_python_ast::visitor::Visitor<'a> for BoundNameVisitor {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr {
                if !name.ctx.is_load() {
                    self.0.insert(name.id.to_string());
                }
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }

        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::FunctionDef(f) => {
                    self.0.insert(f.name.as_str().to_owned());
                }
                Stmt::ClassDef(c) => {
                    self.0.insert(c.name.as_str().to_owned());
                }
                Stmt::Global(g) => {
                    self.0.extend(g.names.iter().map(|n| n.as_str().to_owned()));
                }
                Stmt::Nonlocal(n) => {
                    self.0.extend(n.names.iter().map(|n| n.as_str().to_owned()));
                }
                _ => {}
            }
            ruff_python_ast::visitor::walk_stmt(self, stmt);
        }

        fn visit_parameter(&mut self, parameter: &'a ruff_python_ast::Parameter) {
            self.0.insert(parameter.name.as_str().to_owned());
            ruff_python_ast::visitor::walk_parameter(self, parameter);
        }

        fn visit_except_handler(&mut self, except_handler: &'a ruff_python_ast::ExceptHandler) {
            let ruff_python_ast::ExceptHandler::ExceptHandler(h) = except_handler;
            if let Some(name) = &h.name {
                self.0.insert(name.as_str().to_owned());
            }
            ruff_python_ast::visitor::walk_except_handler(self, except_handler);
        }
    }

    let mut visitor = BoundNameVisitor(HashSet::new());
    for stmt in stmts {
        ruff_python_ast::visitor::Visitor::visit_stmt(&mut visitor, stmt);
    }
    visitor.0
}

// ---------------------------------------------------------------------------
// AST-based __all__ collection
// ---------------------------------------------------------------------------

/// Collect all names from a top-level `__all__` definition.
///
/// Handles:
/// - `__all__ = ['a', 'b']` / `__all__ = ('a', 'b')`  (direct assignment)
/// - `__all__ += ['c', 'd']`                            (augmented assignment)
fn collect_all_exports(stmts: &[Stmt]) -> HashSet<String> {
    let mut exports = HashSet::new();

    /// Push all string-literal elements of a list/tuple expression into `out`.
    fn push_str_elts(elts: &[Expr], out: &mut HashSet<String>) {
        for elt in elts {
            if let Expr::StringLiteral(s) = elt {
                out.insert(s.value.to_str().to_owned());
            }
        }
    }

    for stmt in stmts {
        match stmt {
            // __all__ = ['a', 'b']  or  __all__ = ('a', 'b')
            Stmt::Assign(node) => {
                let targets_all = node
                    .targets
                    .iter()
                    .any(|t| matches!(t, Expr::Name(n) if n.id.as_str() == "__all__"));
                if !targets_all {
                    continue;
                }
                match &*node.value {
                    Expr::List(l) => push_str_elts(&l.elts, &mut exports),
                    Expr::Tuple(t) => push_str_elts(&t.elts, &mut exports),
                    _ => {}
                }
            }
            // __all__ += ['c', 'd']
            Stmt::AugAssign(node) => {
                let target_is_all =
                    matches!(node.target.as_ref(), Expr::Name(n) if n.id.as_str() == "__all__");
                if !target_is_all {
                    continue;
                }
                match &*node.value {
                    Expr::List(l) => push_str_elts(&l.elts, &mut exports),
                    Expr::Tuple(t) => push_str_elts(&t.elts, &mut exports),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    exports
}

// ---------------------------------------------------------------------------
// Fix resolution
// ---------------------------------------------------------------------------

/// Attempt to find an automatic fix for importing `attr_name` from `module`.
///
/// Strategy 1 — from-parent: walk the dotted module path looking for the
/// deepest parent.child split where child is itself a module.
/// Strategy 2 — bare import: fall back to `import {module}`.
fn can_fix(module: &str, attr_name: &str, probe: &ModuleProbe) -> Option<FixInfo> {
    let parts: Vec<&str> = module.split('.').collect();

    for split in (1..parts.len()).rev() {
        let parent = parts[..split].join(".");
        let child = parts[split];
        if probe.is_module(&parent, child) {
            return Some(FixInfo {
                import_stmt: format!("from {parent} import {child}"),
                import_key: format!("{parent}.{child}"),
                old_local: attr_name.to_owned(),
                new_qualified: format!("{child}.{attr_name}"),
                new_bound_name: child.to_owned(),
            });
        }
    }

    Some(FixInfo {
        import_stmt: format!("import {module}"),
        import_key: module.to_owned(),
        old_local: attr_name.to_owned(),
        new_qualified: format!("{module}.{attr_name}"),
        // `import a.b.c` only binds the top-level name `a` in the
        // namespace; access is always via `a.b.c...`.
        new_bound_name: parts[0].to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Violation construction
// ---------------------------------------------------------------------------

/// Source span for a violation — all values are in the same unit as
/// [`ParsedImport`] (lines are 1-based, columns are 0-based byte offsets).
struct ViolationSpan {
    start_line: usize,
    end_line: usize,
    /// 0-based column immediately past the last character (LSP-exclusive end).
    end_col: usize,
    col: usize,
}

fn make_violation(
    span: ViolationSpan,
    module: &str,
    alias_name: &str,
    level: Level,
    fix: Option<&FixInfo>,
    unsafe_to_fix: Option<&str>,
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
        help: Some(if fixable {
            format!("{base_help} (fixable)")
        } else if let Some(colliding_name) = unsafe_to_fix {
            format!(
                "{base_help} (not auto-fixed: '{colliding_name}' is also assigned/bound elsewhere \
                 in this file -- a rename could not reliably tell that binding apart from the \
                 import, so it must be fixed by hand)"
            )
        } else {
            base_help.to_owned()
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
            "Install the package in this environment so KIS001 can validate this import, or set `unresolved_level` in [tool.konform.KIS] to \"off\" to silence this warning."
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
    source: &str,
    probe: &ModuleProbe,
    exceptions: &[String],
    level: Level,
    unresolved_level: Option<Level>,
    ignore_noqa: bool,
    noqa_aliases: &HashMap<String, String>,
) -> Vec<Violation> {
    let lines: Vec<&str> = source.lines().collect();
    let (imports, all_exports) = parse_ast(source);
    let bound_names = collect_bound_names(&parse_module_stmts(source));
    let mut violations = Vec::new();

    let exception_set: HashSet<&str> = exceptions.iter().map(String::as_str).collect();

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

            let name_shadowed = bound_names.contains(effective);
            let candidate_fix = if name_shadowed {
                None
            } else {
                can_fix(&imp.module, &alias.name, probe)
            };
            // Even when the *old* alias name isn't shadowed, the fix may
            // introduce a *new* bound name (e.g. `plugin` from
            // `from parent import plugin`) that collides with an existing
            // local binding elsewhere in the file. That's equally unsafe to
            // auto-fix -- but the colliding name to report is the *new*
            // one, not the original alias name.
            let new_bound_shadow: Option<String> = candidate_fix
                .as_ref()
                .filter(|f| bound_names.contains(&f.new_bound_name))
                .map(|f| f.new_bound_name.clone());
            let fix = if new_bound_shadow.is_some() {
                None
            } else {
                candidate_fix
            };
            let unsafe_to_fix = if name_shadowed {
                Some(effective)
            } else {
                new_bound_shadow.as_deref()
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
// apply_fixes — six-phase source rewriter
// ---------------------------------------------------------------------------
//
// The rewriter operates on the source *text* (not the AST) for phases 3-6;
// the AST is used only in phase 1 to discover which imports to fix.

/// Strip a trailing `#` comment from a code line.  Used by phase 3 only.
fn strip_comment(s: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double => return s[..i].trim_end(),
            _ => {}
        }
    }
    s.trim_end()
}

fn apply_fixes(
    source: &str,
    probe: &ModuleProbe,
    exceptions: &[String],
    ignore_noqa: bool,
    noqa_aliases: &HashMap<String, String>,
) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let (imports, all_exports) = parse_ast(source);
    let stmts = parse_module_stmts(source);
    let bound_names = collect_bound_names(&stmts);
    let exception_set: HashSet<&str> = exceptions.iter().map(String::as_str).collect();

    // ── Phase 1: collect fix instructions ────────────────────────────────
    // aliases_to_remove : 0-based line index → set of alias names to delete
    let mut aliases_to_remove: HashMap<usize, HashSet<String>> = HashMap::new();
    // import_spans       : 0-based start_line → 0-based end_line
    let mut import_spans: HashMap<usize, usize> = HashMap::new();
    // new_imports        : import_key -> import statement string (deduped)
    let mut new_imports: HashMap<String, String> = HashMap::new();
    // renames            : old_local_name → new_qualified_name
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut last_import_line: Option<usize> = None;

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
            if check != ModuleCheck::NotModule {
                continue;
            }
            let effective = alias.asname.as_deref().unwrap_or(alias.name.as_str());
            if all_exports.contains(effective) || all_exports.contains(&alias.name) {
                continue;
            }
            if bound_names.contains(effective) {
                // `effective` is also assigned/bound somewhere else in this
                // file (e.g. shadowed by a local variable of the same
                // name). We have no scope resolution, so we can't tell
                // which `Load` occurrences of that name refer to the import
                // versus the local binding -- renaming would silently
                // change behaviour rather than raise a syntax error. Skip
                // the fix; `check_imports` reports this same condition as a
                // non-fixable violation with an explanatory `help` message.
                continue;
            }
            let alias_line_str = lines
                .get(alias.line.saturating_sub(1))
                .copied()
                .unwrap_or("");
            if !ignore_noqa && has_noqa(alias_line_str, "KIS001", noqa_aliases) {
                continue;
            }

            if let Some(fix) = can_fix(&imp.module, &alias.name, probe) {
                if bound_names.contains(&fix.new_bound_name) {
                    // The fix would bind `fix.new_bound_name` at module
                    // scope (e.g. `plugin` from `from parent import
                    // plugin`), but that name is already assigned/bound
                    // elsewhere in the file. Applying the fix would
                    // silently shadow that binding (or, if the collision is
                    // inside a function, turn every reference in that
                    // function into a local before its assignment -- an
                    // `UnboundLocalError` at runtime). Skip the fix;
                    // `check_imports` reports this same condition as a
                    // non-fixable violation.
                    continue;
                }
                let old_local = alias
                    .asname
                    .as_deref()
                    .unwrap_or(alias.name.as_str())
                    .to_owned();
                renames
                    .entry(old_local)
                    .or_insert_with(|| fix.new_qualified.clone());
                new_imports
                    .entry(fix.import_key.clone())
                    .or_insert(fix.import_stmt);
                aliases_to_remove
                    .entry(imp.start_line - 1) // 0-based
                    .or_default()
                    .insert(alias.name.clone());
            }
        }

        let end_idx = imp.end_line - 1;
        import_spans.insert(imp.start_line - 1, end_idx);
        if imp.is_top_level {
            // Only a module-top-level import is a safe place to anchor the
            // insertion of new `import X` statements (phase 5). Imports
            // nested inside `if TYPE_CHECKING:` / a function / a class body
            // must never be used as the anchor, or the new statement would
            // land inside that indented block at column 0.
            last_import_line = Some(last_import_line.map_or(end_idx, |prev| prev.max(end_idx)));
        }
    }

    if renames.is_empty() {
        return None;
    }

    let mut block_removed_count: HashMap<u32, usize> = HashMap::new();
    let mut block_first_removed_line: HashMap<u32, usize> = HashMap::new();
    for imp in &imports {
        if imp.is_top_level {
            continue;
        }
        let line_idx = imp.start_line - 1;
        let removed = match aliases_to_remove.get(&line_idx) {
            Some(r) => r,
            None => continue,
        };
        if removed.len() != imp.aliases.len() {
            continue;
        }
        *block_removed_count.entry(imp.block_id).or_insert(0) += 1;
        block_first_removed_line
            .entry(imp.block_id)
            .and_modify(|l| *l = (*l).min(line_idx))
            .or_insert(line_idx);
    }
    let mut pass_line: HashSet<usize> = HashSet::new();
    for imp in &imports {
        if imp.is_top_level {
            continue;
        }
        let removed_in_block = block_removed_count.get(&imp.block_id).copied().unwrap_or(0);
        if removed_in_block == imp.block_size {
            if let Some(first) = block_first_removed_line.get(&imp.block_id) {
                pass_line.insert(*first);
            }
        }
    }

    // ── Phase 2: drop imports already present in the file ────────────────
    for line in &lines {
        let trimmed = line.trim();
        for key in new_imports.clone().keys() {
            if key.contains('.') {
                if let Some((parent, child)) = key.split_once('.') {
                    let pat = format!("from {parent} import {child}");
                    if trimmed == pat || trimmed.starts_with(&format!("{pat} ")) {
                        new_imports.remove(key);
                    }
                }
            } else {
                let pat = format!("import {key}");
                if trimmed == pat || trimmed.starts_with(&format!("{pat} ")) {
                    new_imports.remove(key);
                }
            }
        }
    }

    // ── Phase 3: rewrite import lines ──────────────────────────────────
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut lines_out: Vec<String> = source.lines().map(|l| format!("{l}{eol}")).collect();

    for (line_idx, remove_set) in &aliases_to_remove {
        if *line_idx >= lines_out.len() {
            continue;
        }
        let original = lines[*line_idx];
        let trimmed = original.trim_start();
        let leading = &original[..original.len() - trimmed.len()];
        let code = strip_comment(trimmed);

        if code.contains('(') {
            // Paren-style import: single-line `(a, b)` or multi-line block.
            let end_line_idx = import_spans.get(line_idx).copied().unwrap_or(*line_idx);

            // Collect all text between '(' and ')' across the span, stripping
            // inline comments from each line.  Tokens are comma-separated.
            let mut raw_content = String::new();
            for i in *line_idx..=end_line_idx {
                if i >= lines.len() {
                    break;
                }
                let raw_line = lines[i];
                let stripped = strip_comment(raw_line.trim_end());
                let seg: &str = if i == *line_idx && i == end_line_idx {
                    // Open and close on the same line.
                    let a = stripped.find('(').map(|p| p + 1).unwrap_or(stripped.len());
                    let b = stripped.rfind(')').unwrap_or(stripped.len());
                    if a <= b {
                        &stripped[a..b]
                    } else {
                        ""
                    }
                } else if i == *line_idx {
                    let a = stripped.find('(').map(|p| p + 1).unwrap_or(stripped.len());
                    &stripped[a..]
                } else if i == end_line_idx {
                    let b = stripped.rfind(')').unwrap_or(stripped.len());
                    &stripped[..b]
                } else {
                    stripped
                };
                raw_content.push_str(seg);
                // Inject a comma between lines so adjacent line-tokens split
                // cleanly (extra empty tokens from trailing commas are filtered).
                if i < end_line_idx {
                    raw_content.push(',');
                }
            }

            // Parse alias tokens from the collected content.
            let alias_tokens: Vec<String> = raw_content
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();

            // Determine the module name from the start line.
            let module_part = code
                .strip_prefix("from ")
                .and_then(|s| s.split_once(" import "))
                .map(|(m, _)| m)
                .unwrap_or("");

            // Filter survivors: keep aliases NOT in the remove set.
            let survivors: Vec<&str> = alias_tokens
                .iter()
                .map(String::as_str)
                .filter(|s| {
                    let name = s.split_whitespace().next().unwrap_or(s);
                    !remove_set.contains(name)
                })
                .collect();

            if survivors.is_empty() {
                // Blank every line in the span.
                for i in *line_idx..=end_line_idx {
                    if i < lines_out.len() {
                        lines_out[i] = eol.to_owned();
                    }
                }
                if pass_line.contains(line_idx) {
                    // This block would otherwise become empty: a blank body
                    // is not valid Python, so leave a `pass` behind.
                    lines_out[*line_idx] = format!("{leading}pass{eol}");
                }
            } else if survivors.len() == 1 {
                // Collapse to a single line.
                lines_out[*line_idx] =
                    format!("{leading}from {module_part} import {}{eol}", survivors[0]);
                for i in (*line_idx + 1)..=end_line_idx {
                    if i < lines_out.len() {
                        lines_out[i] = eol.to_owned();
                    }
                }
            } else {
                // Reconstruct a parenthesised block.
                let mut new_block: Vec<String> = Vec::new();
                new_block.push(format!("{leading}from {module_part} import ({eol}"));
                for alias in &survivors {
                    new_block.push(format!("{leading}    {alias},{eol}"));
                }
                new_block.push(format!("{leading}){eol}"));

                // Overwrite existing lines with the new block.
                for (offset, new_line) in new_block.iter().enumerate() {
                    let target = line_idx + offset;
                    if target <= end_line_idx && target < lines_out.len() {
                        lines_out[target] = new_line.clone();
                    }
                }
                // Blank any trailing lines that are no longer needed.
                for i in (line_idx + new_block.len())..=end_line_idx {
                    if i < lines_out.len() {
                        lines_out[i] = eol.to_owned();
                    }
                }
            }
        } else {
            // No parens: handle only single-line imports.
            // Multi-line backslash-continuation imports are skipped.
            let end_line_idx = import_spans.get(line_idx).copied().unwrap_or(*line_idx);
            if *line_idx != end_line_idx {
                continue; // backslash-continuation: leave as-is
            }

            if let Some(after_from) = code.strip_prefix("from ") {
                if let Some((module_part, names_part)) = after_from.split_once(" import ") {
                    let survivors: Vec<&str> = names_part
                        .split(',')
                        .map(str::trim)
                        .filter(|s| {
                            let name = s.split_whitespace().next().unwrap_or(*s);
                            !remove_set.contains(name)
                        })
                        .collect();

                    lines_out[*line_idx] = if survivors.is_empty() {
                        if pass_line.contains(line_idx) {
                            // This block would otherwise become empty: a
                            // blank body is not valid Python, so leave a
                            // `pass` behind.
                            format!("{leading}pass{eol}")
                        } else {
                            eol.to_owned() // blank preserves subsequent line numbers
                        }
                    } else {
                        format!(
                            "{leading}from {module_part} import {}{eol}",
                            survivors.join(", ")
                        )
                    };
                }
            }
        }
    }

    // ── Phase 4: rename bare name usages via precise AST Load-context spans ──
    //
    // This runs *before* new imports are injected (phase 5 below) because the
    // spans below are byte offsets into the original `source`, which map
    // 1:1 onto `lines_out`'s current line numbers only as long as no lines
    // have been inserted or removed yet (phase 3 only ever rewrites content
    // in place, never changing the line count).
    //
    // Renaming is restricted to `Expr::Name` occurrences in **Load** context
    // collected straight from the AST, rather than a textual word-boundary
    // scan. This is deliberate: a plain-text scan cannot distinguish a value
    // being *read* (safe to qualify, e.g. `join(...)` -> `os.path.join(...)`)
    // from a name being *bound* (a plain assignment target, a `:=` walrus
    // target, a `for`/`as`/parameter binding) -- Python does not allow a
    // dotted path in any of those binding positions, so rewriting them
    // produces a `SyntaxError`. It also cannot tell a real code reference
    // apart from the same text appearing inside a string, docstring or
    // comment. AST Load-context spans have neither problem.
    let line_starts = build_line_starts(source);
    let mut replacements: Vec<(usize, usize, usize, String)> = Vec::new();

    for (start_off, end_off, name) in collect_load_names(&stmts) {
        let Some(new_qualified) = renames.get(&name) else {
            continue;
        };
        let (start_line, col_start) = offset_to_line_col(&line_starts, start_off);
        let (end_line, col_end) = offset_to_line_col(&line_starts, end_off);
        if start_line != end_line {
            continue; // a bare Name never spans multiple lines
        }
        replacements.push((start_line - 1, col_start, col_end, new_qualified.clone()));
    }

    replacements.sort_by_key(|&(li, col, _, _)| (li, std::cmp::Reverse(col)));
    for (line_idx, col_start, col_end, replacement) in replacements {
        if line_idx >= lines_out.len() {
            continue;
        }
        let line = &lines_out[line_idx];
        let line_eol = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let bare = &line[..line.len() - line_eol.len()];
        if col_end <= bare.len() {
            lines_out[line_idx] = format!(
                "{}{}{}{line_eol}",
                &bare[..col_start],
                replacement,
                &bare[col_end..]
            );
        }
    }

    // ── Phase 5: inject new imports after the last top-level import line ──
    if !new_imports.is_empty() {
        // When there is no top-level import to anchor on (e.g. every import
        // in the file lives inside `if TYPE_CHECKING:` or a function), the
        // only always-safe place to add a new `import X` statement is the
        // very top of the file (position 0) -- never "after line 0", since
        // line 0 could be the first line of an unrelated block (as in the
        // `if TYPE_CHECKING:` case) rather than a docstring/shebang.
        let inject_pos = match last_import_line {
            Some(line) => (line + 1).min(lines_out.len()),
            None => 0,
        };
        let mut sorted: Vec<&String> = new_imports.values().collect();
        sorted.sort();
        for (offset, stmt) in sorted.into_iter().enumerate() {
            lines_out.insert(inject_pos + offset, format!("{stmt}{eol}"));
        }
    }

    // ── Phase 6: write result ──────────────────────────────────────────────
    Some(lines_out.concat())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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
    fn noqa_suppresses() {
        let violations = rule().check(
            &ctx("from os.path import join  # noqa: KIS001\n"),
            &empty_cfg(),
        );
        assert!(violations.is_empty(), "noqa should suppress");
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
            "unresolved_level".to_owned(),
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
            "unresolved_level".to_owned(),
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
        // Both aliases are non-modules — all lines should be blanked.
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
}
