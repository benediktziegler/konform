//! `konform ast`: print the tree exactly as KST rules see it.
//!
//! Only nodes that have a konform kind are shown (the others can't be matched
//! by `kind`), indented by their depth among shown nodes. Each line carries
//! the values the matcher conditions compare against: `name`, import-resolved
//! `qualname` / `callee` and `decorators`, so rule authors can copy them.

use super::node::{walk, Flow, Kind, Node, Visitor};
use super::resolve::Imports;
use crate::rules::scope::{build_line_starts, offset_to_line_col};
use crate::rules::FileContext;

/// Render the KST view of `ctx`'s source, one node per line.
///
/// Fails for files with syntax errors, which KST doesn't lint either.
pub fn dump(ctx: &FileContext) -> Result<String, String> {
    if !ctx.has_valid_syntax() {
        return Err("syntax error: KST skips files that do not parse".to_owned());
    }
    let root = Node::root(ctx.parsed().syntax());
    let mut dumper = Dumper {
        source: &ctx.source,
        imports: Imports::collect(root),
        line_starts: build_line_starts(&ctx.source),
        shown: Vec::new(),
        out: String::new(),
    };
    walk(root, &mut dumper);
    Ok(dumper.out)
}

struct Dumper<'a> {
    source: &'a str,
    imports: Imports,
    line_starts: Vec<u32>,
    /// One entry per open node: whether it printed a line (and so indents).
    shown: Vec<bool>,
    out: String,
}

impl<'a> Visitor<'a> for Dumper<'_> {
    fn enter(&mut self, node: Node<'a>) -> Flow {
        let Some(kind) = node.kind() else {
            self.shown.push(false);
            return Flow::Descend;
        };
        let depth = self.shown.iter().filter(|s| **s).count();
        let (line, col) =
            offset_to_line_col(&self.line_starts, node.report_span(self.source).start);
        let kind_name = Kind::NAMES
            .iter()
            .find(|(_, k)| *k == kind)
            .map_or("?", |(n, _)| *n);
        let mut row = format!("{}{kind_name} {line}:{}", "  ".repeat(depth), col + 1);
        if let Some(name) = node.name() {
            row.push_str(&format!("  name={name}"));
        }
        if matches!(kind, Kind::Name | Kind::Attribute) {
            if let Some(q) = self.imports.node_qualname(node) {
                row.push_str(&format!("  qualname={q}"));
            }
        }
        if kind == Kind::Call {
            if let Some(q) = self.imports.callee_qualname(node) {
                row.push_str(&format!("  callee={q}"));
            }
        }
        if matches!(kind, Kind::Function | Kind::Class) {
            let decorators = self.imports.decorator_qualnames(node);
            if !decorators.is_empty() {
                row.push_str(&format!("  decorators=[{}]", decorators.join(", ")));
            }
        }
        self.out.push_str(&row);
        self.out.push('\n');
        self.shown.push(true);
        Flow::Descend
    }

    fn leave(&mut self, _node: Node<'a>) {
        self.shown.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run(src: &str) -> Result<String, String> {
        dump(&FileContext::from_source(
            PathBuf::from("t.py"),
            src.to_owned(),
        ))
    }

    #[test]
    fn shows_nesting_names_and_resolved_qualnames() {
        let out = run("import pytest as pt\n\n@pt.fixture(scope='x')\nasync def f():\n    assert 1\n    pt.skip()\n").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"import 1:1"), "{out}");
        assert!(
            lines.contains(&"function 4:11  name=f  decorators=[pytest.fixture]"),
            "{out}"
        );
        assert!(lines.contains(&"  assert 5:5"), "{out}");
        assert!(
            lines.contains(&"  call 6:5  name=skip  callee=pytest.skip"),
            "{out}"
        );
        assert!(
            lines.contains(&"    attribute 6:5  name=skip  qualname=pytest.skip"),
            "{out}"
        );
    }

    #[test]
    fn binding_names_resolve_to_themselves() {
        let out = run("import os\nx = os.environ\n").unwrap();
        assert!(out.contains("name 2:1  name=x  qualname=x"), "{out}");
    }

    #[test]
    fn syntax_errors_are_reported() {
        assert!(run("def (:\n").unwrap_err().contains("syntax error"));
    }
}
