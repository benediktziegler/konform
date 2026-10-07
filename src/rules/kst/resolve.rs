//! Import-aware qualified-name resolution.
//!
//! KST matchers compare *what a name refers to*, not how it was spelled, so
//! `@fixture`, `@pt.fixture` and `@pytest.fixture` all resolve to
//! `pytest.fixture` given the right imports.
//!
//! The import table is file-wide and scope-agnostic: every `import` /
//! `from … import` anywhere in the file contributes, and local rebinding is
//! not tracked. That is a deliberate trade-off for a linter-grade check.

//! Together with `node.rs` this is the only KST code that sees parser types;
//! the public API here speaks [`Node`].

use super::node::{walk, Flow, Node, Visitor};
use ruff_python_ast::{AnyNodeRef, Expr, ExprAttribute, ExprName};
use std::collections::HashMap;

/// Alias → fully qualified name, e.g. `fx` → `pytest.fixture`.
#[derive(Debug, Default)]
pub(super) struct Imports(HashMap<String, String>);

struct Collector<'m>(&'m mut HashMap<String, String>);

impl<'a> Visitor<'a> for Collector<'_> {
    fn enter(&mut self, node: Node<'a>) -> Flow {
        match node.raw() {
            AnyNodeRef::StmtImport(import) => {
                for alias in &import.names {
                    // `import a.b` binds `a` to itself, which resolution
                    // already assumes; only `import a.b as x` needs a row.
                    if let Some(asname) = &alias.asname {
                        self.0
                            .insert(asname.as_str().to_owned(), alias.name.as_str().to_owned());
                    }
                }
            }
            AnyNodeRef::StmtImportFrom(from) => {
                let mut prefix = ".".repeat(from.level as usize);
                if let Some(module) = &from.module {
                    prefix.push_str(module.as_str());
                    prefix.push('.');
                }
                for alias in &from.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.0.insert(
                        bound.as_str().to_owned(),
                        format!("{prefix}{}", alias.name.as_str()),
                    );
                }
            }
            _ => {}
        }
        Flow::Descend
    }

    fn leave(&mut self, _node: Node<'a>) {}
}

impl Imports {
    pub(super) fn collect(root: Node<'_>) -> Self {
        let mut table = HashMap::new();
        walk(root, &mut Collector(&mut table));
        Self(table)
    }

    /// Resolved dotted name of a `name` / `attribute` node.
    pub(super) fn node_qualname(&self, node: Node<'_>) -> Option<String> {
        match node.raw() {
            AnyNodeRef::ExprName(n) => Some(self.name(n)),
            AnyNodeRef::ExprAttribute(a) => self.attribute(a),
            _ => None,
        }
    }

    /// Resolved dotted name of a `call` node's callee.
    pub(super) fn callee_qualname(&self, node: Node<'_>) -> Option<String> {
        match node.raw() {
            AnyNodeRef::ExprCall(c) => self.qualname(&c.func),
            _ => None,
        }
    }

    /// Resolved names of the decorators of a `function` / `class` node.
    pub(super) fn decorator_qualnames(&self, node: Node<'_>) -> Vec<String> {
        let decorators = match node.raw() {
            AnyNodeRef::StmtFunctionDef(f) => &f.decorator_list[..],
            AnyNodeRef::StmtClassDef(c) => &c.decorator_list[..],
            _ => &[],
        };
        decorators
            .iter()
            .filter_map(|d| self.callable_qualname(&d.expression))
            .collect()
    }

    /// Dotted name of a `Name` / `Attribute` chain with the leading name
    /// resolved through the imports. `None` for anything else (calls,
    /// subscripts, literals, …).
    fn qualname(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Name(name) => Some(self.name(name)),
            Expr::Attribute(attr) => self.attribute(attr),
            _ => None,
        }
    }

    fn name(&self, name: &ExprName) -> String {
        self.0
            .get(name.id.as_str())
            .cloned()
            .unwrap_or_else(|| name.id.to_string())
    }

    fn attribute(&self, attr: &ExprAttribute) -> Option<String> {
        Some(format!("{}.{}", self.qualname(&attr.value)?, attr.attr))
    }

    /// Like [`Imports::qualname`], but sees through one call, so the
    /// decorator `@pytest.fixture(scope="session")` resolves like
    /// `@pytest.fixture`.
    fn callable_qualname(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Call(call) => self.qualname(&call.func),
            other => self.qualname(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::Stmt;

    fn imports(src: &str) -> Imports {
        let parsed = ruff_python_parser::parse_module(src).unwrap();
        Imports::collect(Node::root(parsed.syntax()))
    }

    fn expr_of(src: &str) -> Expr {
        let parsed = ruff_python_parser::parse_module(src).unwrap();
        match parsed.syntax().body.last() {
            Some(Stmt::Expr(e)) => (*e.value).clone(),
            other => panic!("expected expression statement, got {other:?}"),
        }
    }

    fn resolve(src: &str) -> Option<String> {
        let table = imports(src);
        table.callable_qualname(&expr_of(src))
    }

    #[test]
    fn plain_import_keeps_dotted_name() {
        assert_eq!(
            resolve("import pytest\npytest.fixture").as_deref(),
            Some("pytest.fixture")
        );
    }

    #[test]
    fn aliased_import_resolves_to_real_module() {
        assert_eq!(
            resolve("import pytest as pt\npt.fixture").as_deref(),
            Some("pytest.fixture")
        );
    }

    #[test]
    fn from_import_resolves_member() {
        assert_eq!(
            resolve("from pytest import fixture\nfixture").as_deref(),
            Some("pytest.fixture")
        );
        assert_eq!(
            resolve("from pytest import fixture as fx\nfx").as_deref(),
            Some("pytest.fixture")
        );
    }

    #[test]
    fn submodule_import_without_alias() {
        assert_eq!(
            resolve("import os.path\nos.path.join").as_deref(),
            Some("os.path.join")
        );
    }

    #[test]
    fn relative_import_keeps_leading_dots() {
        assert_eq!(
            resolve("from .helpers import fixture\nfixture").as_deref(),
            Some(".helpers.fixture")
        );
        assert_eq!(
            resolve("from . import helpers\nhelpers.f").as_deref(),
            Some(".helpers.f")
        );
    }

    #[test]
    fn unimported_name_is_returned_as_written() {
        assert_eq!(resolve("print").as_deref(), Some("print"));
    }

    #[test]
    fn calls_are_seen_through_once() {
        assert_eq!(
            resolve("import pytest\npytest.fixture(scope='session')").as_deref(),
            Some("pytest.fixture")
        );
    }

    #[test]
    fn non_name_expressions_do_not_resolve() {
        assert_eq!(resolve("(1).real"), None);
        assert_eq!(resolve("a()[0]"), None);
    }

    #[test]
    fn star_import_binds_nothing() {
        assert_eq!(
            resolve("from pytest import *\nfixture").as_deref(),
            Some("fixture")
        );
    }

    #[test]
    fn imports_inside_functions_count() {
        assert_eq!(
            resolve("def f():\n    import pytest as pt\npt.fixture").as_deref(),
            Some("pytest.fixture")
        );
    }
}
