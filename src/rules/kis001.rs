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
use crate::module_probe::ModuleProbe;
use crate::types::{Level, Violation};
use anyhow::Result;
use rustpython_parser::{ast, Parse};
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
        let (exceptions, level) = parse_kis_config(cfg);
        check_imports(
            &ctx.source,
            &self.probe,
            &exceptions,
            level,
            ctx.ignore_noqa,
        )
    }

    fn fix(&self, ctx: &FileContext, cfg: &toml::Value) -> Result<Option<String>> {
        let (exceptions, _level) = parse_kis_config(cfg);
        Ok(apply_fixes(
            &ctx.source,
            &self.probe,
            &exceptions,
            ctx.ignore_noqa,
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

fn parse_kis_config(cfg: &toml::Value) -> (Vec<String>, Level) {
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
    (exceptions, level)
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
}

#[derive(Debug, Clone)]
struct FixInfo {
    import_stmt: String,
    import_key: String,
    #[allow(dead_code)]
    old_local: String,
    new_qualified: String,
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

/// Parse `source` once and extract all absolute `from X import Y` statements
/// together with the module's `__all__` exports.
///
/// Returns `(imports, all_exports)`.  On parse error both collections are
/// empty so the caller silently skips the file.
fn parse_ast(source: &str) -> (Vec<ParsedImport>, HashSet<String>) {
    let stmts = match ast::Suite::parse(source, "<file>") {
        Ok(s) => s,
        Err(_) => return (vec![], HashSet::new()),
    };
    let line_starts = build_line_starts(source);
    let mut imports = Vec::new();
    collect_imports(&stmts, false, &line_starts, &mut imports);
    let exports = collect_all_exports(&stmts);
    (imports, exports)
}

/// Returns `true` iff `expr` is `TYPE_CHECKING` or `typing.TYPE_CHECKING`.
fn is_type_checking_guard(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Name(n) => n.id.as_str() == "TYPE_CHECKING",
        ast::Expr::Attribute(a) => a.attr.as_str() == "TYPE_CHECKING",
        _ => false,
    }
}

/// Recursively walk `stmts` and collect all absolute `from X import Y`
/// statements into `out`, tagging each with whether it is inside an
/// `if TYPE_CHECKING:` block.
fn collect_imports(
    stmts: &[ast::Stmt],
    in_type_checking: bool,
    line_starts: &[u32],
    out: &mut Vec<ParsedImport>,
) {
    for stmt in stmts {
        match stmt {
            // ── from X import Y ──────────────────────────────────────────
            ast::Stmt::ImportFrom(node) => {
                // Skip relative imports.
                // In rustpython-parser 0.4 the level is *always* Some(Int(n)):
                //   absolute: Int(0)  relative: Int(1), Int(2), …
                // Int has no Display and no comparison with primitive integers.
                // We detect non-zero level via its Debug representation, which
                // is stable within the semver-pinned 0.4 dependency.
                // Additionally, a None module (bare `from . import X`) is always
                // relative regardless of the level field.
                let module = match &node.module {
                    Some(m) => m.as_str().to_owned(),
                    None => continue, // bare `from . import X`
                };
                // Relative iff level debug string is not "Int(0)".
                let is_relative = node
                    .level
                    .as_ref()
                    .is_some_and(|l| format!("{l:?}") != "Int(0)");
                if is_relative {
                    continue;
                }

                let start_off = u32::from(node.range.start());
                // end() points one past the last byte; use saturating_sub to
                // stay on the closing token.
                let end_off = u32::from(node.range.end()).saturating_sub(1);
                let (start_line, col) = offset_to_line_col(line_starts, start_off);
                let (end_line, end_col_inclusive) = offset_to_line_col(line_starts, end_off);
                // end_col_inclusive points at the last byte; add 1 so the
                // LSP range is exclusive (covers the final character).
                let end_col = end_col_inclusive + 1;

                let aliases = node
                    .names
                    .iter()
                    .map(|alias| {
                        let alias_off = u32::from(alias.range.start());
                        let (alias_line, _) = offset_to_line_col(line_starts, alias_off);
                        ParsedAlias {
                            name: alias.name.as_str().to_owned(),
                            asname: alias.asname.as_ref().map(|id| id.as_str().to_owned()),
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
                });
            }

            // ── if TYPE_CHECKING: … ──────────────────────────────────────
            ast::Stmt::If(node) => {
                let guard = is_type_checking_guard(&node.test);
                collect_imports(&node.body, in_type_checking || guard, line_starts, out);
                // else / elif branches are NOT considered TYPE_CHECKING scope.
                collect_imports(&node.orelse, in_type_checking, line_starts, out);
            }

            // ── walk into function / class bodies ────────────────────────
            // Unusual but legal: imports can appear in nested scopes.
            ast::Stmt::FunctionDef(node) => {
                collect_imports(&node.body, in_type_checking, line_starts, out);
            }
            ast::Stmt::AsyncFunctionDef(node) => {
                collect_imports(&node.body, in_type_checking, line_starts, out);
            }
            ast::Stmt::ClassDef(node) => {
                collect_imports(&node.body, in_type_checking, line_starts, out);
            }

            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// AST-based __all__ collection
// ---------------------------------------------------------------------------

/// Collect all names from a top-level `__all__` definition.
///
/// Handles:
/// - `__all__ = ['a', 'b']` / `__all__ = ('a', 'b')`  (direct assignment)
/// - `__all__ += ['c', 'd']`                            (augmented assignment)
fn collect_all_exports(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut exports = HashSet::new();

    /// Push all string-literal elements of a list/tuple expression into `out`.
    fn push_str_elts(elts: &[ast::Expr], out: &mut HashSet<String>) {
        for elt in elts {
            if let ast::Expr::Constant(c) = elt {
                if let ast::Constant::Str(s) = &c.value {
                    out.insert(s.clone());
                }
            }
        }
    }

    for stmt in stmts {
        match stmt {
            // __all__ = ['a', 'b']  or  __all__ = ('a', 'b')
            ast::Stmt::Assign(node) => {
                let targets_all = node
                    .targets
                    .iter()
                    .any(|t| matches!(t, ast::Expr::Name(n) if n.id.as_str() == "__all__"));
                if !targets_all {
                    continue;
                }
                match &*node.value {
                    ast::Expr::List(l) => push_str_elts(&l.elts, &mut exports),
                    ast::Expr::Tuple(t) => push_str_elts(&t.elts, &mut exports),
                    _ => {}
                }
            }
            // __all__ += ['c', 'd']
            ast::Stmt::AugAssign(node) => {
                let target_is_all =
                    matches!(&*node.target, ast::Expr::Name(n) if n.id.as_str() == "__all__");
                if !target_is_all {
                    continue;
                }
                match &*node.value {
                    ast::Expr::List(l) => push_str_elts(&l.elts, &mut exports),
                    ast::Expr::Tuple(t) => push_str_elts(&t.elts, &mut exports),
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
            });
        }
    }

    Some(FixInfo {
        import_stmt: format!("import {module}"),
        import_key: module.to_owned(),
        old_local: attr_name.to_owned(),
        new_qualified: format!("{module}.{attr_name}"),
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
        } else {
            base_help.to_owned()
        }),
        level,
        fixable,
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
    ignore_noqa: bool,
) -> Vec<Violation> {
    let lines: Vec<&str> = source.lines().collect();
    let (imports, all_exports) = parse_ast(source);
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
        if !ignore_noqa && has_noqa(start_line_str, "KIS001") {
            continue;
        }

        for alias in &imp.aliases {
            if probe.is_module(&imp.module, &alias.name) {
                continue; // valid: the imported name is itself a module
            }
            let effective = alias.asname.as_deref().unwrap_or(alias.name.as_str());
            if all_exports.contains(effective) || all_exports.contains(&alias.name) {
                continue; // re-export via __all__ — allowed
            }
            let alias_line_str = lines
                .get(alias.line.saturating_sub(1))
                .copied()
                .unwrap_or("");
            if !ignore_noqa && has_noqa(alias_line_str, "KIS001") {
                continue;
            }

            let fix = can_fix(&imp.module, &alias.name, probe);
            violations.push(make_violation(
                ViolationSpan {
                    start_line: imp.start_line,
                    end_line: imp.end_line,
                    end_col: imp.end_col,
                    col: imp.col,
                },
                &imp.module,
                &alias.name,
                level,
                fix.as_ref(),
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
) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let (imports, all_exports) = parse_ast(source);
    let exception_set: HashSet<&str> = exceptions.iter().map(String::as_str).collect();

    // ── Phase 1: collect fix instructions ────────────────────────────────
    // aliases_to_remove : 0-based line index → set of alias names to delete
    let mut aliases_to_remove: HashMap<usize, HashSet<String>> = HashMap::new();
    // import_spans       : 0-based start_line → 0-based end_line
    let mut import_spans: HashMap<usize, usize> = HashMap::new();
    // new_imports        : import_key → import statement string (deduped)
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
        if !ignore_noqa && has_noqa(start_line_str, "KIS001") {
            continue;
        }

        for alias in &imp.aliases {
            if probe.is_module(&imp.module, &alias.name) {
                continue;
            }
            let effective = alias.asname.as_deref().unwrap_or(alias.name.as_str());
            if all_exports.contains(effective) || all_exports.contains(&alias.name) {
                continue;
            }
            let alias_line_str = lines
                .get(alias.line.saturating_sub(1))
                .copied()
                .unwrap_or("");
            if !ignore_noqa && has_noqa(alias_line_str, "KIS001") {
                continue;
            }

            if let Some(fix) = can_fix(&imp.module, &alias.name, probe) {
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
        last_import_line = Some(last_import_line.map_or(end_idx, |prev| prev.max(end_idx)));
    }

    if renames.is_empty() {
        return None;
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
                        eol.to_owned() // blank preserves subsequent line numbers
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

    // ── Phase 4: inject new imports after the last import line ────────────
    if !new_imports.is_empty() {
        let insert_after = last_import_line.unwrap_or(0);
        let inject_pos = (insert_after + 1).min(lines_out.len());
        let mut sorted: Vec<&String> = new_imports.values().collect();
        sorted.sort();
        for (offset, stmt) in sorted.into_iter().enumerate() {
            lines_out.insert(inject_pos + offset, format!("{stmt}{eol}"));
        }
    }

    // ── Phase 5: rename bare name usages (right-to-left per line) ─────────
    let working: Vec<&str> = lines_out.iter().map(String::as_str).collect();
    let mut replacements: Vec<(usize, usize, usize, String)> = Vec::new();

    for (old_name, new_qualified) in &renames {
        for (line_idx, line) in working.iter().enumerate() {
            let trimmed_line = line.trim();
            if trimmed_line.starts_with("import ") || trimmed_line.starts_with("from ") {
                continue;
            }
            let mut search_start = 0;
            while let Some(pos) = line[search_start..].find(old_name.as_str()) {
                let abs_pos = search_start + pos;
                let col_end = abs_pos + old_name.len();
                let bytes = line.as_bytes();
                let before_ok = abs_pos == 0
                    || (!bytes[abs_pos - 1].is_ascii_alphanumeric() && bytes[abs_pos - 1] != b'_');
                let after_ok = col_end >= line.len()
                    || (!bytes[col_end].is_ascii_alphanumeric()
                        && bytes[col_end] != b'_'
                        && bytes[col_end] != b'.'); // skip already-qualified access
                if before_ok && after_ok {
                    replacements.push((line_idx, abs_pos, col_end, new_qualified.clone()));
                }
                search_start = abs_pos + 1;
                if search_start >= line.len() {
                    break;
                }
            }
        }
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
    fn test_int_zero_comparison() {
        // Verifies the Debug-format hack for detecting relative imports still works.
        let src = "from os.path import join\n"; // absolute, level = Int(0)
        let stmts = ast::Suite::parse(src, "<t>").unwrap();
        if let ast::Stmt::ImportFrom(n) = &stmts[0] {
            let level = n.level.as_ref().expect("level should be Some");
            assert_eq!(format!("{level:?}"), "Int(0)");
        }
        let src2 = "from .sub import thing\n";
        let stmts2 = ast::Suite::parse(src2, "<t>").unwrap();
        if let ast::Stmt::ImportFrom(n) = &stmts2[0] {
            let level = n.level.as_ref().expect("level should be Some");
            assert_ne!(format!("{level:?}"), "Int(0)");
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
}
