//! Autofixes for KST rules.
//!
//! A rule may carry a `fix` table that rewrites each matched node:
//!
//! ```toml
//! fix = { replace = "logger.info({args})" }   # replace the matched node
//! fix = { delete = true }                    # delete the matched statement
//! fix = { replace = "...", safe = true }      # applied by plain `--fix`
//! ```
//!
//! `replace` is a template over the matched node; see [`Var`] for the
//! placeholders. A placeholder the matched node doesn't have (`{args}` on a
//! name) makes that one match unfixable rather than producing broken code.
//! Fixes are unsafe unless `safe = true`, since konform can't know what a
//! user-written rewrite does to the program.

use super::node::{Node, Span};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// The `fix` table as written in the config.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawFix {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub safe: bool,
}

/// A template placeholder, written `{name}` in `replace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Var {
    /// The matched node's source text.
    Text,
    /// The node's identifier (function/class/variable/attribute name, or a
    /// call's last callee segment), as matched by the `name` condition.
    Name,
    /// A call's arguments, without the parentheses.
    Args,
    /// A call's function expression.
    Callee,
    /// The object of an attribute or method call (`a.b` in `a.b.c()`).
    Receiver,
}

impl Var {
    const NAMES: &'static [(&'static str, Var)] = &[
        ("text", Var::Text),
        ("name", Var::Name),
        ("args", Var::Args),
        ("callee", Var::Callee),
        ("receiver", Var::Receiver),
    ];

    /// The placeholder's value for `node`, or `None` if the node has none.
    fn value<'s>(self, node: Node<'s>, source: &'s str) -> Option<&'s str> {
        let slice = |span: Span| source.get(span.start as usize..span.end as usize);
        match self {
            Var::Text => slice(node.span()),
            Var::Name => node.name(),
            Var::Args => slice(node.call_args_span()?),
            Var::Callee => slice(node.callee_span()?),
            Var::Receiver => slice(node.receiver_span()?),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Var(Var),
}

#[derive(Debug)]
enum Action {
    Replace(Vec<Piece>),
    Delete,
}

/// A compiled `fix` table.
#[derive(Debug)]
pub(super) struct Fix {
    action: Action,
    /// Applied by plain `--fix` (otherwise only with `--unsafe-fixes`).
    pub safe: bool,
}

/// One source edit: replace `range` with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

impl Fix {
    pub(super) fn compile(raw: &RawFix) -> Result<Self, String> {
        let action = match (&raw.replace, raw.delete) {
            (Some(template), false) => Action::Replace(parse_template(template)?),
            (None, true) => Action::Delete,
            (Some(_), true) => return Err("fix: set either `replace` or `delete`, not both".into()),
            (None, false) => return Err("fix: needs `replace` or `delete = true`".into()),
        };
        Ok(Self {
            action,
            safe: raw.safe,
        })
    }

    /// The edit that fixes the match at `node` (whose parent is `parent`
    /// and grandparent `grandparent`), or `None` when this node can't be
    /// fixed safely (a missing placeholder, or a statement konform can't
    /// delete cleanly).
    pub(super) fn edit<'a>(
        &self,
        node: Node<'a>,
        parent: Option<Node<'a>>,
        grandparent: Option<Node<'a>>,
        source: &'a str,
    ) -> Option<Edit> {
        match &self.action {
            Action::Replace(pieces) => {
                let mut text = String::new();
                for piece in pieces {
                    match piece {
                        Piece::Literal(s) => text.push_str(s),
                        Piece::Var(var) => text.push_str(var.value(node, source)?),
                    }
                }
                let span = node.span();
                Some(Edit {
                    range: span.start as usize..span.end as usize,
                    text,
                })
            }
            Action::Delete => {
                let stmt = node.statement(parent)?;
                // The block holding the statement: for an expression used as
                // a statement, one level further up.
                let block = if node.statement(None).is_some() {
                    parent
                } else {
                    grandparent
                };
                let sole = block.is_some_and(|b| stmt.is_sole_statement_in(b));
                delete_statement(stmt.span(), sole, source)
            }
        }
    }
}

/// Remove the statement at `span`. A statement alone on its lines takes its
/// lines (and any trailing comment) with it; one sharing a line with a block
/// header (`if x: return`) is only removable when replaced by `pass`. When
/// `sole` the block would be left empty, so a `pass` is left in its place.
fn delete_statement(span: Span, sole: bool, source: &str) -> Option<Edit> {
    let (start, end) = (span.start as usize, span.end as usize);
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = source[end..].find('\n').map_or(source.len(), |i| end + i);
    let before = &source[line_start..start];
    let after = source[end..line_end].trim_start();
    if !(after.is_empty() || after.starts_with('#')) {
        return None; // `a = 1; b = 2`: leave it alone
    }
    if !before.trim().is_empty() {
        // Shares the line with a header: only `pass` keeps that valid.
        return sole.then(|| Edit {
            range: start..end,
            text: "pass".into(),
        });
    }
    let eol_len = usize::from(line_end < source.len());
    Some(Edit {
        range: line_start..line_end + eol_len,
        text: if sole {
            let eol = &source[line_end..line_end + eol_len];
            format!("{before}pass{eol}")
        } else {
            String::new()
        },
    })
}

/// Split `{var}` placeholders out of a `replace` template. `{{` and `}}`
/// stand for literal braces.
fn parse_template(template: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '{' => {
                let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let var = Var::NAMES
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| *v)
                    .ok_or_else(|| {
                        let valid: Vec<String> =
                            Var::NAMES.iter().map(|(n, _)| format!("{{{n}}}")).collect();
                        format!(
                            "fix: unknown placeholder '{{{name}}}' (valid: {}; write {{{{ for a literal brace)",
                            valid.join(", ")
                        )
                    })?;
                if !literal.is_empty() {
                    pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                }
                pieces.push(Piece::Var(var));
            }
            '}' => {
                return Err("fix: unmatched '}' in replace (write }} for a literal brace)".into())
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

/// Placeholder names, for docs and the schema.
pub(super) fn placeholder_names() -> Vec<&'static str> {
    Var::NAMES.iter().map(|(n, _)| *n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_split_into_literals_and_placeholders() {
        assert_eq!(
            parse_template("log.info({args}) {{x}}").unwrap(),
            [
                Piece::Literal("log.info(".into()),
                Piece::Var(Var::Args),
                Piece::Literal(") {x}".into()),
            ]
        );
    }

    #[test]
    fn bad_templates_are_rejected() {
        assert!(parse_template("{nope}")
            .unwrap_err()
            .contains("unknown placeholder '{nope}'"));
        assert!(parse_template("a } b").unwrap_err().contains("unmatched"));
    }

    #[test]
    fn replace_and_delete_are_exclusive() {
        let both = RawFix {
            replace: Some("x".into()),
            delete: true,
            safe: false,
        };
        assert!(Fix::compile(&both).is_err());
        assert!(Fix::compile(&RawFix::default()).is_err());
    }

    fn deleted(src: &str, start: usize, end: usize, sole: bool) -> Option<String> {
        let edit = delete_statement(
            Span {
                start: start as u32,
                end: end as u32,
            },
            sole,
            src,
        )?;
        let mut out = src.to_owned();
        out.replace_range(edit.range, &edit.text);
        Some(out)
    }

    #[test]
    fn delete_takes_the_whole_line_and_its_comment() {
        let src = "a = 1\nprint(a)  # debug\nb = 2\n";
        assert_eq!(
            deleted(src, 6, 14, false).as_deref(),
            Some("a = 1\nb = 2\n")
        );
    }

    #[test]
    fn delete_leaves_pass_in_an_otherwise_empty_block() {
        let src = "def f():\n    print(1)\n";
        assert_eq!(
            deleted(src, 13, 21, true).as_deref(),
            Some("def f():\n    pass\n")
        );
        let one_line = "if x: print(1)\n";
        assert_eq!(
            deleted(one_line, 6, 14, true).as_deref(),
            Some("if x: pass\n")
        );
    }

    #[test]
    fn delete_skips_statements_sharing_a_line() {
        assert_eq!(deleted("a = 1; b = 2\n", 0, 5, false), None);
        assert_eq!(deleted("if x: print(1); y()\n", 6, 14, false), None);
    }

    #[test]
    fn delete_without_trailing_newline() {
        assert_eq!(
            deleted("a = 1\nb = 2", 6, 11, false).as_deref(),
            Some("a = 1\n")
        );
    }
}
