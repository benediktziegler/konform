//! Type conversions between konform internals and lsp-types.
//!
//! All position arithmetic is UTF-16 aware: the LSP protocol uses UTF-16
//! code-unit offsets on each line, while konform works in UTF-8 bytes.

use crate::types::{Level, Violation};
use lsp_types::{Position, Range, TextEdit};

// ---------------------------------------------------------------------------
// UTF-16 position helpers
// ---------------------------------------------------------------------------

/// Convert an [`lsp_types::Position`] (0-based line, 0-based UTF-16 character)
/// to a byte offset into `source`.
pub fn lsp_pos_to_byte_offset(source: &str, pos: Position) -> usize {
    let mut line_start = 0usize;
    for (line_num, line) in source.split('\n').enumerate() {
        if line_num == pos.line as usize {
            return line_start + utf16_col_to_byte(line, pos.character as usize);
        }
        line_start += line.len() + 1; // +1 for the '\n'
    }
    source.len()
}

/// Convert a byte offset in `source` to an [`lsp_types::Position`]
/// (0-based line, 0-based UTF-16 character).
pub fn byte_offset_to_lsp_pos(source: &str, offset: usize) -> Position {
    let safe = offset.min(source.len());
    let before = &source[..safe];
    let line = before.bytes().filter(|&b| b == b'\n').count() as u32;
    let last_nl = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[last_nl..].encode_utf16().count() as u32;
    Position { line, character }
}

/// Convert a UTF-16 column offset to a byte offset within `line`
/// (which must not contain newline characters).
fn utf16_col_to_byte(line: &str, utf16_col: usize) -> usize {
    let mut u16_count = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if u16_count >= utf16_col {
            return byte_idx;
        }
        u16_count += ch.len_utf16();
    }
    line.len()
}

// ---------------------------------------------------------------------------
// Violation → Diagnostic
// ---------------------------------------------------------------------------

/// Convert a typed [`Violation`] to an [`lsp_types::Diagnostic`].
///
/// The diagnostic `message` includes the violation text followed by the
/// rule's help line (e.g. style-guide URL + fixability hint).  This surfaces
/// rule documentation inside the editor's native diagnostic popup without
/// requiring a competing `textDocument/hover` handler.
///
/// The violation's `fixable` flag is embedded in the `data` payload so
/// `codeAction` handlers can retrieve fix metadata without a second lint pass.
pub fn violation_to_diagnostic(v: &Violation) -> lsp_types::Diagnostic {
    // Violations are 1-based; LSP positions are 0-based.
    let start_line = v.line.saturating_sub(1) as u32;
    let end_line = v.end_line.saturating_sub(1) as u32;
    // col is already 0-based in Violation.
    let start_char = v.col as u32;

    let range = Range {
        start: Position {
            line: start_line,
            character: start_char,
        },
        end: Position {
            line: end_line,
            character: v.end_col as u32,
        },
    };

    let severity = match v.level {
        Level::Warning => lsp_types::DiagnosticSeverity::WARNING,
        Level::Error => lsp_types::DiagnosticSeverity::ERROR,
    };

    let code = Some(lsp_types::NumberOrString::String(v.rule.clone()));

    let help = v.help.as_deref().unwrap_or("");

    // Combine violation message with the help line so the editor's diagnostic
    // popup shows the style-guide reference and fixability hint without any
    // hover provider.
    let message = if help.is_empty() {
        v.message.clone()
    } else {
        format!("{}\n{help}", v.message)
    };

    // Embed fix metadata so codeAction needs no second lint pass.
    let data = v.fixable.then(|| {
        serde_json::json!({
            "fixable": true,
            "code":    v.rule.clone(),
            "help":    help,
        })
    });

    lsp_types::Diagnostic {
        range,
        severity: Some(severity),
        code,
        source: Some("konform".to_owned()),
        message,
        data,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Full-document TextEdit helper
// ---------------------------------------------------------------------------

/// Build a single [`TextEdit`] that replaces the entire document content.
///
/// Used by `textDocument/formatting` when the fixer rewrites the whole file.
pub fn full_document_edit(old_source: &str, new_source: &str) -> TextEdit {
    let end = byte_offset_to_lsp_pos(old_source, old_source.len());
    TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end,
        },
        new_text: new_source.to_owned(),
    }
}
