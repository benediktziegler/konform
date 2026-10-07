//! The KST matcher algebra.
//!
//! A [`RawMatcher`] is the user-facing TOML/YAML shape; [`Matcher::compile`]
//! validates it (unknown kinds, bad regexes, empty conditions, misplaced
//! `stop_by`) and [`Matcher::matches`] evaluates it against one AST node and
//! its ancestors.
//!
//! All conditions present in one matcher must hold (logical AND), mirroring
//! ast-grep. `not` / `all` / `any` combine matchers, and `inside` / `has`
//! relate a node to its ancestors / descendants.

use super::resolve::Imports;
use regex::Regex;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal};
use ruff_python_ast::{AnyNodeRef, Expr};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Raw (user-facing) shape
// ---------------------------------------------------------------------------

/// A string or a list of strings; a list means "any of".
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn to_vec(&self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s.clone()],
            Self::Many(v) => v.clone(),
        }
    }
}

/// One matcher table as written in the config.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(super) struct RawMatcher {
    /// Node kind(s): see [`Kind::NAMES`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<OneOrMany>,
    /// Regex searched in the node's own identifier (function/class name,
    /// `Name` id, `Attribute` attr, or a call's last callee segment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Import-resolved dotted name of a `Name` / `Attribute` node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualname: Option<OneOrMany>,
    /// Import-resolved dotted name of a call's callee.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<OneOrMany>,
    /// Import-resolved decorator name(s) on a function or class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decorated_with: Option<OneOrMany>,
    /// Some ancestor matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inside: Option<Box<RawMatcher>>,
    /// Some descendant matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has: Option<Box<RawMatcher>>,
    /// The node does not match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not: Option<Box<RawMatcher>>,
    /// Every listed matcher matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all: Option<Vec<RawMatcher>>,
    /// At least one listed matcher matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub any: Option<Vec<RawMatcher>>,
    /// Only valid inside `inside` / `has`: stop the search at the first
    /// ancestor / descendant matching this (that node is still tested
    /// against the relation first).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_by: Option<Box<RawMatcher>>,
}

// ---------------------------------------------------------------------------
// Node kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
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
    const NAMES: &'static [(&'static str, Kind)] = &[
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

    fn parse(s: &str) -> Result<Self, String> {
        Self::NAMES
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, k)| *k)
            .ok_or_else(|| {
                let valid: Vec<&str> = Self::NAMES.iter().map(|(n, _)| *n).collect();
                format!("unknown kind '{s}' (valid: {})", valid.join(", "))
            })
    }

    fn of(node: AnyNodeRef<'_>) -> Option<Self> {
        Some(match node {
            AnyNodeRef::StmtFunctionDef(_) => Self::Function,
            AnyNodeRef::StmtClassDef(_) => Self::Class,
            AnyNodeRef::ExprLambda(_) => Self::Lambda,
            AnyNodeRef::StmtAssert(_) => Self::Assert,
            AnyNodeRef::StmtAssign(_)
            | AnyNodeRef::StmtAnnAssign(_)
            | AnyNodeRef::StmtAugAssign(_) => Self::Assign,
            AnyNodeRef::StmtImport(_) | AnyNodeRef::StmtImportFrom(_) => Self::Import,
            AnyNodeRef::StmtReturn(_) => Self::Return,
            AnyNodeRef::StmtRaise(_) => Self::Raise,
            AnyNodeRef::ExprYield(_) | AnyNodeRef::ExprYieldFrom(_) => Self::Yield,
            AnyNodeRef::StmtTry(_) => Self::Try,
            AnyNodeRef::StmtWith(_) => Self::With,
            AnyNodeRef::StmtFor(_) => Self::For,
            AnyNodeRef::StmtWhile(_) => Self::While,
            AnyNodeRef::StmtIf(_) => Self::If,
            AnyNodeRef::ExprCall(_) => Self::Call,
            AnyNodeRef::ExprAwait(_) => Self::Await,
            AnyNodeRef::ExprName(_) => Self::Name,
            AnyNodeRef::ExprAttribute(_) => Self::Attribute,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Compiled matcher
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Relation {
    matcher: Box<Matcher>,
    stop_by: Option<Box<Matcher>>,
}

#[derive(Debug)]
pub(super) struct Matcher {
    kinds: Vec<Kind>,
    name: Option<Regex>,
    qualnames: Vec<String>,
    callees: Vec<String>,
    decorators: Vec<String>,
    inside: Option<Relation>,
    has: Option<Relation>,
    not: Option<Box<Matcher>>,
    all: Vec<Matcher>,
    any: Vec<Matcher>,
}

/// Fail with `what` when a list of alternatives is empty.
fn non_empty(list: Vec<String>, what: &str) -> Result<Vec<String>, String> {
    if list.is_empty() {
        Err(format!("'{what}' must not be an empty list"))
    } else {
        Ok(list)
    }
}

impl Matcher {
    pub(super) fn compile(raw: &RawMatcher) -> Result<Self, String> {
        Self::compile_inner(raw, false)
    }

    fn compile_inner(raw: &RawMatcher, stop_by_allowed: bool) -> Result<Self, String> {
        if raw.stop_by.is_some() && !stop_by_allowed {
            return Err("'stop_by' is only valid inside 'inside' or 'has'".into());
        }

        let kinds = match &raw.kind {
            Some(k) => non_empty(k.to_vec(), "kind")?
                .iter()
                .map(|s| Kind::parse(s))
                .collect::<Result<_, _>>()?,
            None => vec![],
        };
        let name = raw
            .name
            .as_deref()
            .map(|p| Regex::new(p).map_err(|e| format!("invalid regex in 'name': {e}")))
            .transpose()?;
        let list = |field: &Option<OneOrMany>, what: &str| match field {
            Some(v) => non_empty(v.to_vec(), what),
            None => Ok(vec![]),
        };
        let all = raw
            .all
            .as_deref()
            .map(|v| Self::compile_list(v, "all"))
            .transpose()?
            .unwrap_or_default();
        let any = raw
            .any
            .as_deref()
            .map(|v| Self::compile_list(v, "any"))
            .transpose()?
            .unwrap_or_default();

        let compiled = Self {
            kinds,
            name,
            qualnames: list(&raw.qualname, "qualname")?,
            callees: list(&raw.callee, "callee")?,
            decorators: list(&raw.decorated_with, "decorated_with")?,
            inside: raw
                .inside
                .as_deref()
                .map(|r| Self::compile_relation(r, "inside"))
                .transpose()?,
            has: raw
                .has
                .as_deref()
                .map(|r| Self::compile_relation(r, "has"))
                .transpose()?,
            not: raw
                .not
                .as_deref()
                .map(|r| {
                    Self::compile(r)
                        .map(Box::new)
                        .map_err(|e| format!("in 'not': {e}"))
                })
                .transpose()?,
            all,
            any,
        };
        if compiled.is_unconstrained() {
            return Err("matcher has no conditions".into());
        }
        Ok(compiled)
    }

    fn compile_list(items: &[RawMatcher], what: &str) -> Result<Vec<Self>, String> {
        if items.is_empty() {
            return Err(format!("'{what}' must not be an empty list"));
        }
        items
            .iter()
            .enumerate()
            .map(|(i, m)| Self::compile(m).map_err(|e| format!("in '{what}[{i}]': {e}")))
            .collect()
    }

    fn compile_relation(raw: &RawMatcher, what: &str) -> Result<Relation, String> {
        let wrap = |e: String| format!("in '{what}': {e}");
        let mut body = raw.clone();
        let stop = body.stop_by.take();
        let matcher = Self::compile(&body).map_err(wrap)?;
        let stop_by = stop
            .as_deref()
            .map(|s| {
                Self::compile(s)
                    .map(Box::new)
                    .map_err(|e| format!("in '{what}.stop_by': {e}"))
            })
            .transpose()?;
        Ok(Relation {
            matcher: Box::new(matcher),
            stop_by,
        })
    }

    fn is_unconstrained(&self) -> bool {
        self.kinds.is_empty()
            && self.name.is_none()
            && self.qualnames.is_empty()
            && self.callees.is_empty()
            && self.decorators.is_empty()
            && self.inside.is_none()
            && self.has.is_none()
            && self.not.is_none()
            && self.all.is_empty()
            && self.any.is_empty()
    }

    // ── evaluation ──────────────────────────────────────────────────────────

    /// Does `node` (whose ancestors, outermost first, are `ancestors`) satisfy
    /// every condition of this matcher? Cheap checks run before the
    /// relational ones.
    pub(super) fn matches<'a>(
        &self,
        node: AnyNodeRef<'a>,
        ancestors: &[AnyNodeRef<'a>],
        imports: &Imports,
    ) -> bool {
        if !self.kinds.is_empty() && !Kind::of(node).is_some_and(|k| self.kinds.contains(&k)) {
            return false;
        }
        if let Some(re) = &self.name {
            if !node_name(node).is_some_and(|n| re.is_match(n)) {
                return false;
            }
        }
        if !self.qualnames.is_empty() {
            let resolved = match node {
                AnyNodeRef::ExprName(n) => Some(imports.name(n)),
                AnyNodeRef::ExprAttribute(a) => imports.attribute(a),
                _ => None,
            };
            if !resolved.is_some_and(|q| self.qualnames.contains(&q)) {
                return false;
            }
        }
        if !self.callees.is_empty() {
            let resolved = match node {
                AnyNodeRef::ExprCall(c) => imports.qualname(&c.func),
                _ => None,
            };
            if !resolved.is_some_and(|q| self.callees.contains(&q)) {
                return false;
            }
        }
        if !self.decorators.is_empty() {
            let decorators = match node {
                AnyNodeRef::StmtFunctionDef(f) => &f.decorator_list[..],
                AnyNodeRef::StmtClassDef(c) => &c.decorator_list[..],
                _ => &[],
            };
            let hit = decorators.iter().any(|d| {
                imports
                    .callable_qualname(&d.expression)
                    .is_some_and(|q| self.decorators.contains(&q))
            });
            if !hit {
                return false;
            }
        }
        if let Some(not) = &self.not {
            if not.matches(node, ancestors, imports) {
                return false;
            }
        }
        if !self.all.iter().all(|m| m.matches(node, ancestors, imports)) {
            return false;
        }
        if !self.any.is_empty() && !self.any.iter().any(|m| m.matches(node, ancestors, imports)) {
            return false;
        }
        if let Some(rel) = &self.inside {
            if !inside(rel, ancestors, imports) {
                return false;
            }
        }
        if let Some(rel) = &self.has {
            if !has(rel, node, ancestors, imports) {
                return false;
            }
        }
        true
    }
}

/// Identifier a node introduces or refers to, if it has a single obvious one.
fn node_name<'a>(node: AnyNodeRef<'a>) -> Option<&'a str> {
    match node {
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

/// Walk the ancestors from the nearest outwards.
fn inside<'a>(rel: &Relation, ancestors: &[AnyNodeRef<'a>], imports: &Imports) -> bool {
    for (i, &ancestor) in ancestors.iter().enumerate().rev() {
        let above = &ancestors[..i];
        if rel.matcher.matches(ancestor, above, imports) {
            return true;
        }
        if rel
            .stop_by
            .as_ref()
            .is_some_and(|s| s.matches(ancestor, above, imports))
        {
            return false;
        }
    }
    false
}

struct HasFinder<'m, 'a> {
    rel: &'m Relation,
    imports: &'m Imports,
    /// Ancestors of the node being visited; kept balanced with
    /// `leave_node`, which the traversal calls even for skipped nodes.
    path: Vec<AnyNodeRef<'a>>,
    found: bool,
}

impl<'a> SourceOrderVisitor<'a> for HasFinder<'_, 'a> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        let signal = if self.found {
            TraversalSignal::Skip
        } else if self.rel.matcher.matches(node, &self.path, self.imports) {
            self.found = true;
            TraversalSignal::Skip
        } else if self
            .rel
            .stop_by
            .as_ref()
            .is_some_and(|s| s.matches(node, &self.path, self.imports))
        {
            TraversalSignal::Skip
        } else {
            TraversalSignal::Traverse
        };
        self.path.push(node);
        signal
    }

    fn leave_node(&mut self, _node: AnyNodeRef<'a>) {
        self.path.pop();
    }
}

fn has<'a>(
    rel: &Relation,
    node: AnyNodeRef<'a>,
    ancestors: &[AnyNodeRef<'a>],
    imports: &Imports,
) -> bool {
    let mut path = ancestors.to_vec();
    path.push(node);
    let mut finder = HasFinder {
        rel,
        imports,
        path,
        found: false,
    };
    node.visit_source_order(&mut finder);
    finder.found
}
