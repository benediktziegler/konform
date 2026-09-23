//! Shared Python AST scope-analysis helpers.
//!
//! Originally written for KIS001's import-rename fixer, these are generic
//! enough to answer the same question any import-rewrite rule needs to ask:
//! "is this identifier bound anywhere else, and if so, where?" KIS002 (and
//! any future rule that renames/removes a binding) reuses them rather than
//! re-implementing the same delicate AST-walking logic.

use ruff_python_ast::{Expr, Pattern, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Line index: byte offset → (line, col)
// ---------------------------------------------------------------------------

/// Build a sorted Vec of byte offsets at which each line starts.
/// `line_starts[0]` is always 0.
pub(crate) fn build_line_starts(source: &str) -> Vec<u32> {
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
pub(crate) fn offset_to_line_col(line_starts: &[u32], offset: u32) -> (usize, usize) {
    let idx = line_starts
        .partition_point(|&s| s <= offset)
        .saturating_sub(1);
    (idx + 1, (offset - line_starts[idx]) as usize)
}

/// Parse `source` and return its top-level statement list, or an empty
/// list on a parse error. A small shared helper for the several places that
/// need the raw AST.
pub(crate) fn parse_module_stmts(source: &str) -> Vec<Stmt> {
    match parse_module(source) {
        Ok(parsed) => parsed.into_suite().into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// AST-based Load-context name collection
// ---------------------------------------------------------------------------

/// A byte-offset half-open span `[start, end)` into the source, paired with
/// the name it covers (a Load-context occurrence of that name).
pub(crate) struct NamedSpan {
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) name: String,
}

/// Recursively collect the byte-offset span and identifier of every
/// `Expr::Name` used in **Load** context (i.e. a value is being *read*).
///
/// Only Load-context occurrences may safely be rewritten (e.g. `join` ->
/// `os.path.join`, or a de-aliased import's old name -> its canonical name):
/// Python does not allow a dotted path or a renamed target as an assignment
/// target, walrus (`:=`) target, `for` target, `as` binding, function
/// parameter, etc. Restricting renames to these exact spans (rather than a
/// textual word-boundary scan) also naturally skips occurrences inside
/// string literals, docstrings and comments, since those never produce
/// `Expr::Name` nodes.
pub(crate) fn collect_load_names(stmts: &[Stmt]) -> Vec<NamedSpan> {
    struct LoadNameVisitor(Vec<NamedSpan>);

    impl<'a> ruff_python_ast::visitor::Visitor<'a> for LoadNameVisitor {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr {
                if name.ctx.is_load() {
                    self.0.push(NamedSpan {
                        start: u32::from(name.range().start()),
                        end: u32::from(name.range().end()),
                        name: name.id.to_string(),
                    });
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
/// A rename is only sound when the name resolves unambiguously to the thing
/// being renamed everywhere it's read. If the same identifier is *also*
/// bound (e.g. shadowed by a local variable) then some `Load` occurrences of
/// that name refer to the local binding instead, and a blind rename would
/// rewrite those too -- silently changing runtime behaviour rather than
/// producing a syntax error. Callers with no scope/binding resolution should
/// treat any such collision as unsafe and skip the fix entirely.
pub(crate) fn collect_bound_names(stmts: &[Stmt]) -> HashSet<String> {
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
// Coarse two-level scope index (module vs. per-top-level-def/class bucket)
// ---------------------------------------------------------------------------
//
// `collect_bound_names` above is deliberately flat: it can tell you *whether*
// a name is bound *somewhere* in the file, but not *where*. That makes shadow
// checks correct-but-overbroad: an unrelated function's parameter or local
// variable (e.g. a `plugin` pytest fixture, or a `server` parameter on some
// other helper) would block a fix that never touches that function at all,
// just because the same identifier happens to be bound *somewhere* in the
// file.
//
// `ScopeIndex` adds just enough location information to fix that: every name
// bound directly at module level (not inside any `def`/`class`) goes into
// `module_names`; everything bound anywhere inside a given top-level `def`/
// `class` -- however deeply nested (inner functions, lambdas, comprehensions,
// nested classes) -- shares *one* bucket keyed by that def/class's start
// offset. This is coarser than real Python scoping (which would give each
// nested function its own scope), but it's sufficient to stop conflating
// unrelated sibling functions, and it never *under*-reports a collision: a
// name is only ever folded into a *larger* bucket than true LEGB scoping
// would use, never a smaller one, so every check built on it stays sound.
pub(crate) struct ScopeIndex {
    pub(crate) module_names: HashSet<String>,
    /// bucket id (a top-level `def`/`class`'s start byte offset) -> every
    /// name bound anywhere in its subtree.
    pub(crate) scope_names: HashMap<u32, HashSet<String>>,
    /// `(start, end)` for every top-level `def`/`class`, used to find which
    /// bucket (if any) contains a given byte offset. The bucket id (the key
    /// into `scope_names`) is always the block's `start` offset.
    pub(crate) buckets: Vec<(u32, u32)>,
}

/// Collect every **Store**-context name bound anywhere within a single
/// expression (assignment/`for`/`with`/walrus targets, which are usually
/// `Expr::Name` but may be `Expr::Tuple`/`Expr::List`/`Expr::Starred` for
/// unpacking). Also incidentally picks up any walrus (`:=`) bindings nested
/// inside a non-target expression (e.g. a `for`/`while`/`if` test), which is
/// exactly what's wanted there too.
pub(crate) fn collect_names_bound_by_expr(expr: &Expr, out: &mut HashSet<String>) {
    struct TargetNameVisitor<'a>(&'a mut HashSet<String>);

    impl<'a, 'ast> ruff_python_ast::visitor::Visitor<'ast> for TargetNameVisitor<'a> {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            if let Expr::Name(name) = expr {
                if !name.ctx.is_load() {
                    self.0.insert(name.id.to_string());
                }
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }
    }

    let mut visitor = TargetNameVisitor(out);
    ruff_python_ast::visitor::Visitor::visit_expr(&mut visitor, expr);
}

/// Collect the names bound by each of `exprs` (assignment/`for`/`with`/
/// walrus targets) and record them all in `bucket` (or module scope). Shared
/// by every `collect_scopes` arm that binds a handful of target expressions
/// before recursing into a body.
pub(crate) fn bind_exprs_scoped(exprs: &[&Expr], bucket: Option<u32>, index: &mut ScopeIndex) {
    let mut names = HashSet::new();
    for expr in exprs {
        collect_names_bound_by_expr(expr, &mut names);
    }
    for n in names {
        insert_scoped(n, bucket, index);
    }
}

pub(crate) fn insert_scoped(name: String, bucket: Option<u32>, index: &mut ScopeIndex) {
    match bucket {
        Some(id) => {
            index.scope_names.entry(id).or_default().insert(name);
        }
        None => {
            index.module_names.insert(name);
        }
    }
}

/// Collect every capture-binding identifier in a `match` pattern (`case x:`,
/// `case [a, *rest]:`, `case {**rest}:`, `case Point(x=0) as p:`, `case a | b:`,
/// ...). Unlike assignment/`for`/`with` targets, pattern-bound names are
/// plain `Identifier`s rather than `Expr::Name` nodes in Store context (and
/// the default AST walk doesn't visit them at all, see `walk_pattern`), so
/// `collect_names_bound_by_expr` can't see them -- this walks the `Pattern`
/// tree by hand instead.
pub(crate) fn collect_names_bound_by_pattern(pattern: &Pattern, out: &mut HashSet<String>) {
    match pattern {
        Pattern::MatchValue(_) | Pattern::MatchSingleton(_) => {}
        Pattern::MatchSequence(p) => {
            for sub in &p.patterns {
                collect_names_bound_by_pattern(sub, out);
            }
        }
        Pattern::MatchMapping(p) => {
            if let Some(rest) = &p.rest {
                out.insert(rest.as_str().to_owned());
            }
            for sub in &p.patterns {
                collect_names_bound_by_pattern(sub, out);
            }
        }
        Pattern::MatchClass(p) => {
            for sub in &p.arguments.patterns {
                collect_names_bound_by_pattern(sub, out);
            }
            for kw in &p.arguments.keywords {
                collect_names_bound_by_pattern(&kw.pattern, out);
            }
        }
        Pattern::MatchStar(p) => {
            if let Some(name) = &p.name {
                out.insert(name.as_str().to_owned());
            }
        }
        Pattern::MatchAs(p) => {
            if let Some(name) = &p.name {
                out.insert(name.as_str().to_owned());
            }
            if let Some(sub) = &p.pattern {
                collect_names_bound_by_pattern(sub, out);
            }
        }
        Pattern::MatchOr(p) => {
            for sub in &p.patterns {
                collect_names_bound_by_pattern(sub, out);
            }
        }
    }
}

/// Recursively collect every name declared in a `global` statement anywhere
/// within `stmts`, including inside further-nested `def`/`class` bodies.
///
/// This exists because `collect_scopes` itself never walks into a `def`/
/// `class` body (it flattens it wholesale into one bucket via
/// `collect_bound_names` instead, see the `FunctionDef`/`ClassDef` arms
/// below) -- so a `global x` written *inside* a function would otherwise
/// never be seen at all, and `x` would incorrectly end up bucketed as local
/// to that function instead of module-wide, even though `global` makes it a
/// real module-scope binding. `collect_bound_names` doesn't help either: it
/// only looks at `Expr::Name` nodes, and a bare `global x` (with no
/// subsequent assignment in that function) never produces one.
pub(crate) fn collect_global_declared_names(stmts: &[Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Global(g) => {
                for n in &g.names {
                    out.insert(n.as_str().to_owned());
                }
            }
            Stmt::FunctionDef(f) => collect_global_declared_names(&f.body, out),
            Stmt::ClassDef(c) => collect_global_declared_names(&c.body, out),
            Stmt::If(node) => {
                collect_global_declared_names(&node.body, out);
                for clause in &node.elif_else_clauses {
                    collect_global_declared_names(&clause.body, out);
                }
            }
            Stmt::For(node) => {
                collect_global_declared_names(&node.body, out);
                collect_global_declared_names(&node.orelse, out);
            }
            Stmt::While(node) => {
                collect_global_declared_names(&node.body, out);
                collect_global_declared_names(&node.orelse, out);
            }
            Stmt::With(node) => collect_global_declared_names(&node.body, out),
            Stmt::Match(node) => {
                for case in &node.cases {
                    collect_global_declared_names(&case.body, out);
                }
            }
            Stmt::Try(node) => {
                collect_global_declared_names(&node.body, out);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_global_declared_names(&h.body, out);
                }
                collect_global_declared_names(&node.orelse, out);
                collect_global_declared_names(&node.finalbody, out);
            }
            _ => {}
        }
    }
}

/// Recursively assign every bound name in `stmts` to `bucket` (`None` means
/// module level), discovering new buckets for any `def`/`class` found along
/// the way -- including ones nested inside `if`/`for`/`while`/`with`/`try`
/// blocks, which don't introduce a scope of their own in Python.
pub(crate) fn collect_scopes(stmts: &[Stmt], bucket: Option<u32>, index: &mut ScopeIndex) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(f) => {
                // The function's own name binds in the *enclosing* scope.
                insert_scoped(f.name.as_str().to_owned(), bucket, index);
                let id = u32::from(f.range().start());
                index.buckets.push((id, u32::from(f.range().end())));
                // Everything inside -- params, body, however deeply nested --
                // becomes this one bucket; reuse the existing flat collector
                // rather than hand-walking every statement/expression kind.
                let names = collect_bound_names(std::slice::from_ref(stmt));
                index.scope_names.entry(id).or_default().extend(names);
                // `global x` anywhere inside (however deeply nested) makes
                // `x` a real module-scope binding, regardless of which
                // bucket it's coarsely filed under above.
                collect_global_declared_names(&f.body, &mut index.module_names);
            }
            Stmt::ClassDef(c) => {
                insert_scoped(c.name.as_str().to_owned(), bucket, index);
                let id = u32::from(c.range().start());
                index.buckets.push((id, u32::from(c.range().end())));
                let names = collect_bound_names(std::slice::from_ref(stmt));
                index.scope_names.entry(id).or_default().extend(names);
                collect_global_declared_names(&c.body, &mut index.module_names);
            }
            Stmt::If(node) => {
                bind_exprs_scoped(&[&node.test], bucket, index);
                collect_scopes(&node.body, bucket, index);
                for clause in &node.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        bind_exprs_scoped(&[test], bucket, index);
                    }
                    collect_scopes(&clause.body, bucket, index);
                }
            }
            Stmt::For(node) => {
                bind_exprs_scoped(&[&node.target, &node.iter], bucket, index);
                collect_scopes(&node.body, bucket, index);
                collect_scopes(&node.orelse, bucket, index);
            }
            Stmt::While(node) => {
                bind_exprs_scoped(&[&node.test], bucket, index);
                collect_scopes(&node.body, bucket, index);
                collect_scopes(&node.orelse, bucket, index);
            }
            Stmt::With(node) => {
                let mut targets: Vec<&Expr> = Vec::new();
                for item in &node.items {
                    if let Some(vars) = &item.optional_vars {
                        targets.push(vars);
                    }
                    targets.push(&item.context_expr);
                }
                bind_exprs_scoped(&targets, bucket, index);
                collect_scopes(&node.body, bucket, index);
            }
            Stmt::Match(node) => {
                bind_exprs_scoped(&[&node.subject], bucket, index);
                for case in &node.cases {
                    let mut names = HashSet::new();
                    collect_names_bound_by_pattern(&case.pattern, &mut names);
                    if let Some(guard) = &case.guard {
                        collect_names_bound_by_expr(guard, &mut names);
                    }
                    for n in names {
                        insert_scoped(n, bucket, index);
                    }
                    collect_scopes(&case.body, bucket, index);
                }
            }
            Stmt::Try(node) => {
                collect_scopes(&node.body, bucket, index);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
                    if let Some(name) = &h.name {
                        insert_scoped(name.as_str().to_owned(), bucket, index);
                    }
                    collect_scopes(&h.body, bucket, index);
                }
                collect_scopes(&node.orelse, bucket, index);
                collect_scopes(&node.finalbody, bucket, index);
            }
            Stmt::Global(g) => {
                // Reached only for a `global` statement that isn't nested
                // inside a `def`/`class` (e.g. directly at module level, or
                // inside a module-level `if`/`for`/`while`/`with`/`match`/
                // `try`) -- the `def`/`class`-nested case is handled by
                // `collect_global_declared_names` in the `FunctionDef`/
                // `ClassDef` arms above, since this function never
                // recurses into their bodies. Either way, be conservative
                // and always treat these as module-wide bound names
                // regardless of the current bucket, matching the old flat
                // (file-wide) behaviour rather than risk under-reporting a
                // real collision.
                for n in &g.names {
                    index.module_names.insert(n.as_str().to_owned());
                }
            }
            Stmt::Nonlocal(n) => {
                for name in &n.names {
                    index.module_names.insert(name.as_str().to_owned());
                }
            }
            _ => {
                let names = collect_bound_names(std::slice::from_ref(stmt));
                for n in names {
                    insert_scoped(n, bucket, index);
                }
            }
        }
    }
}

pub(crate) fn build_scope_index(stmts: &[Stmt]) -> ScopeIndex {
    let mut index = ScopeIndex {
        module_names: HashSet::new(),
        scope_names: HashMap::new(),
        buckets: Vec::new(),
    };
    collect_scopes(stmts, None, &mut index);
    index
}

/// Which bucket (if any) contains byte offset `offset` -- `None` means
/// module level.
pub(crate) fn bucket_for_offset(index: &ScopeIndex, offset: u32) -> Option<u32> {
    index
        .buckets
        .iter()
        .find(|&&(start, end)| start <= offset && offset < end)
        .map(|&(start, _)| start)
}

/// Is `name` bound in `bucket` (or at module level)?
pub(crate) fn is_name_shadowed(name: &str, bucket: Option<u32>, index: &ScopeIndex) -> bool {
    index.module_names.contains(name)
        || bucket.is_some_and(|b| {
            index
                .scope_names
                .get(&b)
                .is_some_and(|names| names.contains(name))
        })
}

/// Is `check_name` shadowed at any Load-context occurrence of
/// `occurrence_name`? Used to answer two different questions with the same
/// per-occurrence machinery:
///   - is `effective`'s own occurrence shadowed, i.e. does it reliably refer
///     to the binding at all (`check_name == occurrence_name == effective`)?
///   - would a rewritten name fail to resolve because the replacement is
///     itself shadowed at that same spot (`check_name == new_name`,
///     `occurrence_name == effective`)?
///
/// Occurrences the name never appears at (e.g. an unused import) can't be
/// shadowed anywhere that matters, so this is vacuously `false` for those.
pub(crate) fn shadowed_at_occurrences_of(
    check_name: &str,
    occurrence_name: &str,
    load_names: &[NamedSpan],
    scope_index: &ScopeIndex,
) -> bool {
    load_names
        .iter()
        .filter(|occ| occ.name == occurrence_name)
        .any(|occ| {
            is_name_shadowed(
                check_name,
                bucket_for_offset(scope_index, occ.start),
                scope_index,
            )
        })
}
