//! Shared import rewriter for the import-style rules (KIS001).
//!
//! Both rules replace `from X import name` with an import of a module plus
//! qualified uses of `name`. This module owns the import collection, the
//! "would this fix collide with another import" bookkeeping and the
//! text rewriter; each rule only decides, per alias, which fix to apply.

use super::scope::{collect_all_exports, offset_to_line_col};
use super::FileContext;
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::Ranged;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Internal data structures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(super) struct ParsedAlias {
    pub(super) name: String,
    pub(super) asname: Option<String>,
    /// 1-based line number where this alias appears.
    pub(super) line: usize,
}

impl ParsedAlias {
    /// The name this alias binds: `asname` if present, else `name`.
    pub(super) fn effective(&self) -> &str {
        self.asname.as_deref().unwrap_or(self.name.as_str())
    }
}

#[derive(Debug)]
pub(super) struct ParsedImport {
    pub(super) module: String,
    pub(super) aliases: Vec<ParsedAlias>,
    /// 1-based line of the `from` keyword.
    pub(super) start_line: usize,
    /// 1-based line of the last line of this import statement.
    pub(super) end_line: usize,
    /// 0-based column immediately past the last character of the statement.
    pub(super) end_col: usize,
    /// 0-based column of the `from` keyword.
    pub(super) col: usize,
    /// True when the import lives inside `if TYPE_CHECKING:`.
    /// Tracked for future use (e.g. PT rules may treat TC imports differently).
    #[allow(dead_code)]
    pub(super) in_type_checking: bool,
    /// True when this import sits directly in the module's top-level body
    /// (not nested inside an `if`/`def`/`class` block). New imports must
    /// only ever be inserted at this level.
    pub(super) is_top_level: bool,
    /// Identifies the enclosing compound-statement body this import lives
    /// in: the byte offset of the first statement in that body. All sibling
    /// imports sharing the same body share this id. Meaningless (0) for
    /// top-level imports, which are never at risk of leaving an empty block.
    pub(super) block_id: u32,
    /// Total number of statements (of any kind) in the enclosing body
    /// identified by `block_id`. Used to detect when every statement in a
    /// block turns out to be an import that gets fully removed, in which
    /// case the block would otherwise end up empty (invalid Python) and one
    /// line must be replaced with `pass` instead of being blanked.
    pub(super) block_size: usize,
}

#[derive(Debug, Clone)]
pub(super) struct FixInfo {
    pub(super) import_stmt: String,
    pub(super) import_key: String,
    pub(super) new_qualified: String,
    /// The name this fix's `import_stmt` binds into the file's namespace
    /// (e.g. `plugin` for `from parent import plugin`, or `os` for
    /// `import os.path`). If this name is already assigned/bound elsewhere
    /// in the file, applying the fix would silently shadow it, so callers
    /// must treat that case as unsafe to auto-fix.
    pub(super) new_bound_name: String,
}

// ---------------------------------------------------------------------------
// AST-based import collection
// ---------------------------------------------------------------------------

/// Extract from the already-parsed `stmts` all absolute `from X import Y` statements
/// together with the module's `__all__` exports.
///
/// Returns `(imports, all_exports)`.  On parse error both collections are
/// empty so the caller silently skips the file.
pub(super) fn parse_ast(
    stmts: &[Stmt],
    line_starts: &[u32],
) -> (Vec<ParsedImport>, HashSet<String>) {
    if stmts.is_empty() {
        return (vec![], HashSet::new());
    }
    let mut imports = Vec::new();
    collect_imports(stmts, false, false, line_starts, &mut imports);
    let exports = collect_all_exports(stmts);
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

/// The fix that imports `package` itself and reaches `attr_name` through
/// it: `from alib import models` and `models.attr_name` for
/// `package = "alib.models"`. `None` for a top-level package, which would
/// need a bare `import alib`.
pub(super) fn package_import_fix(package: &str, attr_name: &str) -> Option<FixInfo> {
    let (parent, child) = package.rsplit_once('.')?;
    Some(FixInfo {
        import_stmt: format!("from {parent} import {child}"),
        import_key: package.to_owned(),
        new_qualified: format!("{child}.{attr_name}"),
        new_bound_name: child.to_owned(),
    })
}

/// For every alias across all `from X import Y` statements in the file, the
/// name it will ultimately bind into the namespace, mapped to the concrete
/// module path(s) it resolves to (`"module.attr"`, or the rewrite `planned`
/// returns for it once konform's own fix is applied).
///
/// Two *different* module paths landing on the same bound name means
/// applying a fix there would create overlapping imports: two `from X
/// import Y` statements silently binding the same name to two different
/// modules. Python has no scoping mechanism to disambiguate that -- unlike a
/// shadowed local variable, there's no rename that fixes it -- so it must be
/// treated as unsafe to auto-fix, the same way a shadowed name is.
pub(super) fn bound_import_targets(
    imports: &[ParsedImport],
    planned: impl Fn(&ParsedImport, &ParsedAlias) -> Option<FixInfo>,
) -> HashMap<String, HashSet<String>> {
    let mut targets: HashMap<String, HashSet<String>> = HashMap::new();
    for imp in imports {
        for alias in &imp.aliases {
            let (bound_name, target) = match planned(imp, alias) {
                Some(fix) => (fix.new_bound_name, fix.import_key),
                None => (
                    alias.effective().to_owned(),
                    format!("{}.{}", imp.module, alias.name),
                ),
            };
            targets.entry(bound_name).or_default().insert(target);
        }
    }
    targets
}

/// Would applying `fix` bind `fix.new_bound_name` to a module other than
/// `fix.import_key` somewhere else in the file?
pub(super) fn overlaps_existing_import(
    fix: &FixInfo,
    bound_targets: &HashMap<String, HashSet<String>>,
) -> bool {
    bound_targets
        .get(&fix.new_bound_name)
        .is_some_and(|paths| paths.iter().any(|path| path != &fix.import_key))
}

// ---------------------------------------------------------------------------
// apply_fixes: six-phase source rewriter
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

/// Fill in the anchor line for an import block whose aliases were all
/// removed: the first pending nested-scope import if there is one, else a
/// `pass` if the block would otherwise be left with an empty body, else a
/// blank line (matching the pre-blanked/absent line already there). Any
/// nested imports beyond the first are returned as deferred
/// `(insert_after_line, text)` pairs for the caller to queue.
fn emptied_import_line(
    nested_new_imports: &[String],
    needs_pass: bool,
    line_idx: usize,
    leading: &str,
    eol: &str,
) -> (String, Vec<(usize, String)>) {
    if let Some(first_stmt) = nested_new_imports.first() {
        let deferred = nested_new_imports[1..]
            .iter()
            .map(|stmt| (line_idx + 1, format!("{leading}{stmt}{eol}")))
            .collect();
        (format!("{leading}{first_stmt}{eol}"), deferred)
    } else if needs_pass {
        // This block would otherwise become empty: a blank body is not
        // valid Python, so leave a `pass` behind.
        (format!("{leading}pass{eol}"), Vec::new())
    } else {
        (eol.to_owned(), Vec::new()) // blank preserves line numbers
    }
}

/// Queue every pending nested-scope import to be inserted right after
/// `after_line`.
fn queue_nested_imports(
    nested_new_imports: &[String],
    after_line: usize,
    leading: &str,
    eol: &str,
    deferred_nested_inserts: &mut Vec<(usize, String)>,
) {
    for stmt in nested_new_imports {
        deferred_nested_inserts.push((after_line, format!("{leading}{stmt}{eol}")));
    }
}

/// Rewrite `ctx`'s source: every alias for which `plan_alias` returns a
/// [`FixInfo`] is removed from its `from X import ...` statement, the fix's
/// import is added (deduplicated, in the same block for nested imports) and
/// every read of the alias's bound name becomes `fix.new_qualified`.
///
/// `skip_statement` drops a whole statement from consideration (exceptions,
/// statement-level `# noqa`); such statements also don't anchor new imports.
/// `plan_alias` must itself check everything that makes a fix unsafe.
///
/// Returns `None` when nothing was planned.
pub(super) fn rewrite_imports(
    ctx: &FileContext,
    imports: &[ParsedImport],
    skip_statement: impl Fn(&ParsedImport) -> bool,
    mut plan_alias: impl FnMut(&ParsedImport, &ParsedAlias) -> Option<FixInfo>,
) -> Option<String> {
    let source = ctx.source.as_str();
    let lines: Vec<&str> = source.lines().collect();
    let load_names = ctx.load_names();

    // ── Phase 1: collect fix instructions ────────────────────────────────
    // aliases_to_remove : 0-based line index → set of alias names to delete
    let mut aliases_to_remove: HashMap<usize, HashSet<String>> = HashMap::new();
    // import_spans       : 0-based start_line → 0-based end_line
    let mut import_spans: HashMap<usize, usize> = HashMap::new();
    // top_level_new_imports : import_key -> import statement string (deduped)
    let mut top_level_new_imports: HashMap<String, String> = HashMap::new();
    // nested_import_anchors : (enclosing block, import statement) -> earliest
    // line in that block where this new import must be inserted.
    let mut nested_import_anchors: HashMap<(u32, String), usize> = HashMap::new();
    // renames            : old_local_name → new_qualified_name
    let mut renames: HashMap<String, String> = HashMap::new();
    let mut last_import_line: Option<usize> = None;

    for imp in imports {
        if skip_statement(imp) {
            continue;
        }
        for alias in &imp.aliases {
            let Some(fix) = plan_alias(imp, alias) else {
                continue;
            };
            renames
                .entry(alias.effective().to_owned())
                .or_insert_with(|| fix.new_qualified.clone());
            if imp.is_top_level {
                top_level_new_imports
                    .entry(fix.import_key.clone())
                    .or_insert_with(|| fix.import_stmt.clone());
            } else {
                nested_import_anchors
                    .entry((imp.block_id, fix.import_stmt.clone()))
                    .and_modify(|line| *line = (*line).min(imp.start_line - 1))
                    .or_insert(imp.start_line - 1);
            }
            aliases_to_remove
                .entry(imp.start_line - 1) // 0-based
                .or_default()
                .insert(alias.name.clone());
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

    let mut nested_new_imports_by_line: HashMap<usize, Vec<String>> = HashMap::new();
    for ((_, stmt), line_idx) in nested_import_anchors {
        nested_new_imports_by_line
            .entry(line_idx)
            .or_default()
            .push(stmt);
    }
    for stmts in nested_new_imports_by_line.values_mut() {
        stmts.sort();
        stmts.dedup();
    }

    let mut block_removed_count: HashMap<u32, usize> = HashMap::new();
    let mut block_first_removed_line: HashMap<u32, usize> = HashMap::new();
    for imp in imports {
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
    for imp in imports {
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

    // ── Phase 2: drop top-level imports already present in the file ───────
    for line in &lines {
        let trimmed = line.trim();
        for key in top_level_new_imports.clone().keys() {
            if key.contains('.') {
                if let Some((parent, child)) = key.split_once('.') {
                    let pat = format!("from {parent} import {child}");
                    if trimmed == pat || trimmed.starts_with(&format!("{pat} ")) {
                        top_level_new_imports.remove(key);
                    }
                }
            } else {
                let pat = format!("import {key}");
                if trimmed == pat || trimmed.starts_with(&format!("{pat} ")) {
                    top_level_new_imports.remove(key);
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
    let mut deferred_nested_inserts: Vec<(usize, String)> = Vec::new();

    for (line_idx, remove_set) in &aliases_to_remove {
        if *line_idx >= lines_out.len() {
            continue;
        }
        let nested_new_imports = nested_new_imports_by_line
            .get(line_idx)
            .cloned()
            .unwrap_or_default();
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
                let (anchor_line, deferred) = emptied_import_line(
                    &nested_new_imports,
                    pass_line.contains(line_idx),
                    *line_idx,
                    leading,
                    eol,
                );
                lines_out[*line_idx] = anchor_line;
                deferred_nested_inserts.extend(deferred);
            } else if survivors.len() == 1 {
                // Collapse to a single line.
                lines_out[*line_idx] =
                    format!("{leading}from {module_part} import {}{eol}", survivors[0]);
                for i in (*line_idx + 1)..=end_line_idx {
                    if i < lines_out.len() {
                        lines_out[i] = eol.to_owned();
                    }
                }
                queue_nested_imports(
                    &nested_new_imports,
                    end_line_idx + 1,
                    leading,
                    eol,
                    &mut deferred_nested_inserts,
                );
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
                queue_nested_imports(
                    &nested_new_imports,
                    end_line_idx + 1,
                    leading,
                    eol,
                    &mut deferred_nested_inserts,
                );
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

                    if survivors.is_empty() {
                        let (anchor_line, deferred) = emptied_import_line(
                            &nested_new_imports,
                            pass_line.contains(line_idx),
                            *line_idx,
                            leading,
                            eol,
                        );
                        lines_out[*line_idx] = anchor_line;
                        deferred_nested_inserts.extend(deferred);
                    } else {
                        lines_out[*line_idx] = format!(
                            "{leading}from {module_part} import {}{eol}",
                            survivors.join(", ")
                        );
                        queue_nested_imports(
                            &nested_new_imports,
                            end_line_idx + 1,
                            leading,
                            eol,
                            &mut deferred_nested_inserts,
                        );
                    }
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
    let line_starts = ctx.line_starts();
    let mut replacements: Vec<(usize, usize, usize, String)> = Vec::new();

    for occ in load_names {
        let Some(new_qualified) = renames.get(&occ.name) else {
            continue;
        };
        let (start_line, col_start) = offset_to_line_col(line_starts, occ.start);
        let (end_line, col_end) = offset_to_line_col(line_starts, occ.end);
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

    // ── Phase 4b: drop top-level import lines that phase 3 emptied ────────
    // Phase 3 blanks a fully removed import instead of deleting it, so line
    // indices stay stable for the phases above. An empty string keeps the
    // slot (later inserts still line up) but writes nothing, so the fix does
    // not leave a stray blank line where the old `from X import y` stood.
    for imp in imports.iter().filter(|imp| imp.is_top_level) {
        let start = imp.start_line - 1;
        if !aliases_to_remove.contains_key(&start) {
            continue;
        }
        for i in start..imp.end_line.min(lines_out.len()) {
            if lines_out[i] == eol && !lines[i].trim().is_empty() {
                lines_out[i].clear();
            }
        }
    }

    // ── Phase 5a: inject deferred nested-scope imports in place ───────────
    deferred_nested_inserts.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (nested_insert_offset, (insert_pos, line)) in deferred_nested_inserts.iter().enumerate() {
        let pos = (*insert_pos + nested_insert_offset).min(lines_out.len());
        lines_out.insert(pos, line.clone());
    }

    // ── Phase 5b: inject top-level imports after the last top-level import ─
    if !top_level_new_imports.is_empty() {
        // When there is no top-level import to anchor on (e.g. every import
        // in the file lives inside `if TYPE_CHECKING:` or a function), the
        // only always-safe place to add a new `import X` statement is the
        // very top of the file (position 0) -- never "after line 0", since
        // line 0 could be the first line of an unrelated block (as in the
        // `if TYPE_CHECKING:` case) rather than a docstring/shebang.
        let base_inject_pos = match last_import_line {
            Some(line) => (line + 1).min(lines_out.len()),
            None => 0,
        };
        let nested_lines_before_anchor = deferred_nested_inserts
            .iter()
            .filter(|(insert_pos, _)| *insert_pos < base_inject_pos)
            .count();
        let inject_pos = (base_inject_pos + nested_lines_before_anchor).min(lines_out.len());

        let mut sorted: Vec<&String> = top_level_new_imports.values().collect();
        sorted.sort();
        for (offset, stmt) in sorted.into_iter().enumerate() {
            lines_out.insert(inject_pos + offset, format!("{stmt}{eol}"));
        }
    }

    // ── Phase 6: write result ──────────────────────────────────────────────
    Some(lines_out.concat())
}
