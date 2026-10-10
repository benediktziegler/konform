//! konform's own view of a Python syntax tree.
//!
//! This is the boundary between KST and the parser. Everything the rest of
//! KST (matcher, walker, docs, user config) knows about the tree is defined
//! here: the [`Kind`] vocabulary, [`Node`] accessors, [`Span`] and the
//! [`Visitor`] traversal. Parser types never cross this boundary, so the
//! user-facing vocabulary can't drift with the parser's naming and the parser
//! can be upgraded or swapped by touching only this module and `resolve.rs`
//! (enforced by `no_parser_types_outside_adapter` in `mod.rs`).

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{AnyNodeRef, Expr, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// A node kind as users write it in `kind = "..."`.
///
/// Each kind may cover several parser node types (e.g. `assign` covers plain,
/// annotated and augmented assignment). The set is konform's public contract:
/// add kinds deliberately and document them in the README.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Function,
    Class,
    Lambda,
    Assert,
    Assign,
    Import,
    Return,
    Raise,
    Yield,
    Try,
    With,
    For,
    While,
    If,
    Call,
    Await,
    Name,
    Attribute,
}

impl Kind {
    pub(super) const NAMES: &'static [(&'static str, Kind)] = &[
        ("function", Kind::Function),
        ("class", Kind::Class),
        ("lambda", Kind::Lambda),
        ("assert", Kind::Assert),
        ("assign", Kind::Assign),
        ("import", Kind::Import),
        ("return", Kind::Return),
        ("raise", Kind::Raise),
        ("yield", Kind::Yield),
        ("try", Kind::Try),
        ("with", Kind::With),
        ("for", Kind::For),
        ("while", Kind::While),
        ("if", Kind::If),
        ("call", Kind::Call),
        ("await", Kind::Await),
        ("name", Kind::Name),
        ("attribute", Kind::Attribute),
    ];

    pub(super) fn parse(s: &str) -> Result<Self, String> {
        Self::NAMES
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, k)| *k)
            .ok_or_else(|| {
                let valid: Vec<&str> = Self::NAMES.iter().map(|(n, _)| *n).collect();
                format!("unknown kind '{s}' (valid: {})", valid.join(", "))
            })
    }
}

/// Half-open byte range into the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Span {
    pub start: u32,
    pub end: u32,
}

impl From<TextRange> for Span {
    fn from(r: TextRange) -> Self {
        Self {
            start: r.start().to_u32(),
            end: r.end().to_u32(),
        }
    }
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

/// One node of the syntax tree.
#[derive(Debug, Clone, Copy)]
pub(super) struct Node<'a>(AnyNodeRef<'a>);

impl<'a> Node<'a> {
    pub(super) fn root(module: &'a ModModule) -> Self {
        Self(AnyNodeRef::from(module))
    }

    /// Parser-level node; only for the adapter layer (`resolve.rs`).
    pub(super) fn raw(self) -> AnyNodeRef<'a> {
        self.0
    }

    /// The konform kind of this node, or `None` if it has no kind (and thus
    /// can't be matched by `kind`).
    pub(super) fn kind(self) -> Option<Kind> {
        Some(match self.0 {
            AnyNodeRef::StmtFunctionDef(_) => Kind::Function,
            AnyNodeRef::StmtClassDef(_) => Kind::Class,
            AnyNodeRef::ExprLambda(_) => Kind::Lambda,
            AnyNodeRef::StmtAssert(_) => Kind::Assert,
            AnyNodeRef::StmtAssign(_)
            | AnyNodeRef::StmtAnnAssign(_)
            | AnyNodeRef::StmtAugAssign(_) => Kind::Assign,
            AnyNodeRef::StmtImport(_) | AnyNodeRef::StmtImportFrom(_) => Kind::Import,
            AnyNodeRef::StmtReturn(_) => Kind::Return,
            AnyNodeRef::StmtRaise(_) => Kind::Raise,
            AnyNodeRef::ExprYield(_) | AnyNodeRef::ExprYieldFrom(_) => Kind::Yield,
            AnyNodeRef::StmtTry(_) => Kind::Try,
            AnyNodeRef::StmtWith(_) => Kind::With,
            AnyNodeRef::StmtFor(_) => Kind::For,
            AnyNodeRef::StmtWhile(_) => Kind::While,
            AnyNodeRef::StmtIf(_) => Kind::If,
            AnyNodeRef::ExprCall(_) => Kind::Call,
            AnyNodeRef::ExprAwait(_) => Kind::Await,
            AnyNodeRef::ExprName(_) => Kind::Name,
            AnyNodeRef::ExprAttribute(_) => Kind::Attribute,
            _ => return None,
        })
    }

    /// Identifier a node introduces or refers to, if it has a single obvious
    /// one: a function/class name, a variable, an attribute, or a call's last
    /// callee segment.
    pub(super) fn name(self) -> Option<&'a str> {
        match self.0 {
            AnyNodeRef::StmtFunctionDef(f) => Some(f.name.as_str()),
            AnyNodeRef::StmtClassDef(c) => Some(c.name.as_str()),
            AnyNodeRef::ExprName(n) => Some(n.id.as_str()),
            AnyNodeRef::ExprAttribute(a) => Some(a.attr.as_str()),
            AnyNodeRef::ExprCall(c) => match &*c.func {
                Expr::Name(n) => Some(n.id.as_str()),
                Expr::Attribute(a) => Some(a.attr.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The whole node (decorators included for functions and classes).
    pub(super) fn span(self) -> Span {
        self.0.range().into()
    }

    /// A call's arguments without the parentheses.
    pub(super) fn call_args_span(self) -> Option<Span> {
        let AnyNodeRef::ExprCall(c) = self.0 else {
            return None;
        };
        let r = c.arguments.range();
        Some(Span {
            start: r.start().to_u32() + 1,
            end: r.end().to_u32() - 1,
        })
    }

    /// A call's function expression.
    pub(super) fn callee_span(self) -> Option<Span> {
        match self.0 {
            AnyNodeRef::ExprCall(c) => Some(c.func.range().into()),
            _ => None,
        }
    }

    /// The object an attribute (or a method call) is looked up on: `a.b` in
    /// `a.b.c` and `a.b.c()`.
    pub(super) fn receiver_span(self) -> Option<Span> {
        match self.0 {
            AnyNodeRef::ExprAttribute(a) => Some(a.value.range().into()),
            AnyNodeRef::ExprCall(c) => match &*c.func {
                Expr::Attribute(a) => Some(a.value.range().into()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The statement this node is, if any: the node itself, or, for an
    /// expression used as a statement (`print(x)` on its own), its `parent`.
    pub(super) fn statement(self, parent: Option<Node<'a>>) -> Option<Node<'a>> {
        if self.0.is_statement() {
            return Some(self);
        }
        match parent?.0 {
            AnyNodeRef::StmtExpr(e) if e.value.range() == self.0.range() => parent,
            _ => None,
        }
    }

    /// Is the statement `self` the only one in its block of `parent` (so
    /// removing it would leave the block empty)? Always `false` at module
    /// level, where an empty body is fine.
    pub(super) fn is_sole_statement_in(self, parent: Node<'a>) -> bool {
        let bodies: &[&[Stmt]] = match parent.0 {
            AnyNodeRef::StmtFunctionDef(f) => &[&f.body],
            AnyNodeRef::StmtClassDef(c) => &[&c.body],
            AnyNodeRef::StmtIf(i) => &[&i.body],
            AnyNodeRef::ElifElseClause(c) => &[&c.body],
            AnyNodeRef::StmtFor(f) => &[&f.body, &f.orelse],
            AnyNodeRef::StmtWhile(w) => &[&w.body, &w.orelse],
            AnyNodeRef::StmtWith(w) => &[&w.body],
            AnyNodeRef::StmtTry(t) => &[&t.body, &t.orelse, &t.finalbody],
            AnyNodeRef::ExceptHandlerExceptHandler(h) => &[&h.body],
            AnyNodeRef::MatchCase(c) => &[&c.body],
            _ => &[],
        };
        let range = self.0.range();
        bodies
            .iter()
            .any(|body| matches!(body, [only] if only.range() == range))
    }

    /// Where a violation for this node is reported: the name for functions
    /// and classes, the header line for other compound statements, else the
    /// whole node.
    pub(super) fn report_span(self, source: &str) -> Span {
        match self.0 {
            AnyNodeRef::StmtFunctionDef(f) => f.name.range().into(),
            AnyNodeRef::StmtClassDef(c) => c.name.range().into(),
            AnyNodeRef::StmtIf(_)
            | AnyNodeRef::StmtFor(_)
            | AnyNodeRef::StmtWhile(_)
            | AnyNodeRef::StmtWith(_)
            | AnyNodeRef::StmtTry(_) => {
                let range = self.0.range();
                let first = source[range].lines().next().unwrap_or("");
                TextRange::at(range.start(), TextSize::of(first)).into()
            }
            _ => self.0.range().into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

/// Whether to visit a node's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flow {
    Descend,
    Skip,
}

/// Source-order traversal. `leave` is called for every node `enter` was
/// called for, including skipped ones, so a path stack can be kept balanced.
pub(super) trait Visitor<'a> {
    fn enter(&mut self, node: Node<'a>) -> Flow;
    fn leave(&mut self, node: Node<'a>);
}

/// Visit `root` and its descendants in source order.
pub(super) fn walk<'a>(root: Node<'a>, visitor: &mut impl Visitor<'a>) {
    root.0.visit_source_order(&mut Adapter(visitor));
}

struct Adapter<'v, V>(&'v mut V);

impl<'a, V: Visitor<'a>> SourceOrderVisitor<'a> for Adapter<'_, V> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        match self.0.enter(Node(node)) {
            Flow::Descend => TraversalSignal::Traverse,
            Flow::Skip => TraversalSignal::Skip,
        }
    }

    fn leave_node(&mut self, node: AnyNodeRef<'a>) {
        self.0.leave(Node(node));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Collect(Vec<(Kind, Option<String>)>);

    impl<'a> Visitor<'a> for Collect {
        fn enter(&mut self, node: Node<'a>) -> Flow {
            if let Some(k) = node.kind() {
                self.0.push((k, node.name().map(str::to_owned)));
            }
            Flow::Descend
        }
        fn leave(&mut self, _node: Node<'a>) {}
    }

    fn kinds(src: &str) -> Vec<(Kind, Option<String>)> {
        let parsed = ruff_python_parser::parse_module(src).unwrap();
        let mut c = Collect(vec![]);
        walk(Node::root(parsed.syntax()), &mut c);
        c.0
    }

    #[test]
    fn every_documented_kind_name_parses() {
        for (name, kind) in Kind::NAMES {
            assert_eq!(Kind::parse(name), Ok(*kind));
        }
        assert!(Kind::parse("StmtAssert").is_err());
        assert!(Kind::parse("FunctionDef").is_err());
    }

    #[test]
    fn assign_covers_plain_annotated_and_augmented() {
        let k = kinds("a = 1\nb: int = 2\nc += 3\n");
        assert_eq!(k.iter().filter(|(k, _)| *k == Kind::Assign).count(), 3);
    }

    #[test]
    fn function_covers_sync_and_async() {
        let k = kinds("def f(): ...\nasync def g(): ...\n");
        let names: Vec<_> = k
            .iter()
            .filter(|(k, _)| *k == Kind::Function)
            .filter_map(|(_, n)| n.as_deref())
            .collect();
        assert_eq!(names, ["f", "g"]);
    }

    #[test]
    fn report_span_of_function_is_its_name() {
        let src = "def foo():\n    pass\n";
        let parsed = ruff_python_parser::parse_module(src).unwrap();
        struct First(Option<Span>);
        impl<'a> Visitor<'a> for First {
            fn enter(&mut self, node: Node<'a>) -> Flow {
                if node.kind() == Some(Kind::Function) {
                    self.0 = Some(node.report_span("def foo():\n    pass\n"));
                }
                Flow::Descend
            }
            fn leave(&mut self, _node: Node<'a>) {}
        }
        let mut v = First(None);
        walk(Node::root(parsed.syntax()), &mut v);
        assert_eq!(v.0, Some(Span { start: 4, end: 7 }));
    }
}
