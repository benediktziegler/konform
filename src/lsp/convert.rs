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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Level;
    use lsp_types::DiagnosticSeverity;

    // ── lsp_pos_to_byte_offset / byte_offset_to_lsp_pos round-trips ────────

    #[test]
    fn ascii_round_trip() {
        let src = "import os\nfrom os.path import join\n";
        let pos = Position {
            line: 1,
            character: 5,
        };
        let offset = lsp_pos_to_byte_offset(src, pos);
        assert_eq!(offset, "import os\nfrom ".len());
        assert_eq!(byte_offset_to_lsp_pos(src, offset), pos);
    }

    #[test]
    fn position_at_start_of_document() {
        let src = "abc\ndef\n";
        let start = Position {
            line: 0,
            character: 0,
        };
        assert_eq!(lsp_pos_to_byte_offset(src, start), 0);
        assert_eq!(byte_offset_to_lsp_pos(src, 0), start);
    }

    #[test]
    fn position_at_end_of_document() {
        let src = "abc\ndef";
        let end = byte_offset_to_lsp_pos(src, src.len());
        assert_eq!(
            end,
            Position {
                line: 1,
                character: 3
            }
        );
        assert_eq!(lsp_pos_to_byte_offset(src, end), src.len());
    }

    #[test]
    fn out_of_range_position_clamps_to_end() {
        let src = "abc\n";
        let past_end = Position {
            line: 50,
            character: 0,
        };
        assert_eq!(lsp_pos_to_byte_offset(src, past_end), src.len());
    }

    #[test]
    fn out_of_range_offset_clamps_to_end() {
        let src = "abc\n";
        let pos = byte_offset_to_lsp_pos(src, src.len() + 100);
        assert_eq!(byte_offset_to_lsp_pos(src, src.len()), pos);
    }

    #[test]
    fn multi_byte_utf16_surrogate_pairs_are_counted_correctly() {
        // "a" (1 UTF-16 unit) + "\u{1F600}" 😀 (2 UTF-16 units, 4 UTF-8 bytes) + "b".
        let src = "a\u{1F600}b\n";
        // "b" starts after 1 + 2 = 3 UTF-16 units.
        let pos = Position {
            line: 0,
            character: 3,
        };
        let offset = lsp_pos_to_byte_offset(src, pos);
        assert_eq!(&src[offset..offset + 1], "b");
        assert_eq!(byte_offset_to_lsp_pos(src, offset), pos);
    }

    #[test]
    fn multi_line_offsets_account_for_newlines() {
        let src = "one\ntwo\nthree\n";
        let offset = src.find("three").unwrap();
        assert_eq!(
            byte_offset_to_lsp_pos(src, offset),
            Position {
                line: 2,
                character: 0
            }
        );
    }

    // ── violation_to_diagnostic ─────────────────────────────────────────────

    fn sample_violation(level: Level, fixable: bool, help: Option<&str>) -> Violation {
        Violation {
            rule: "KIS001".to_owned(),
            line: 3,
            col: 4,
            end_line: 3,
            end_col: 10,
            message: "Import 'join' from 'os.path' is not a module.".to_owned(),
            help: help.map(str::to_owned),
            level,
            fixable,
        }
    }

    #[test]
    fn violation_line_numbers_convert_from_1_based_to_0_based() {
        let v = sample_violation(Level::Error, true, None);
        let diag = violation_to_diagnostic(&v);
        assert_eq!(diag.range.start.line, 2);
        assert_eq!(diag.range.end.line, 2);
        assert_eq!(diag.range.start.character, 4);
        assert_eq!(diag.range.end.character, 10);
    }

    #[test]
    fn violation_severity_maps_to_lsp_severity() {
        let error = violation_to_diagnostic(&sample_violation(Level::Error, false, None));
        let warning = violation_to_diagnostic(&sample_violation(Level::Warning, false, None));
        assert_eq!(error.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(warning.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn violation_message_appends_help_when_present() {
        let v = sample_violation(Level::Error, true, Some("see the style guide"));
        let diag = violation_to_diagnostic(&v);
        assert!(diag.message.contains("is not a module"));
        assert!(diag.message.contains("see the style guide"));
    }

    #[test]
    fn violation_message_omits_help_when_absent() {
        let v = sample_violation(Level::Error, true, None);
        let diag = violation_to_diagnostic(&v);
        assert_eq!(diag.message, v.message);
    }

    #[test]
    fn fixable_violation_embeds_fix_data() {
        let v = sample_violation(Level::Error, true, Some("help text"));
        let diag = violation_to_diagnostic(&v);
        let data = diag.data.expect("fixable violation should carry data");
        assert_eq!(data["fixable"], serde_json::json!(true));
        assert_eq!(data["code"], serde_json::json!("KIS001"));
    }

    #[test]
    fn unfixable_violation_has_no_data() {
        let v = sample_violation(Level::Warning, false, None);
        let diag = violation_to_diagnostic(&v);
        assert!(diag.data.is_none());
    }

    // ── full_document_edit ──────────────────────────────────────────────────

    #[test]
    fn full_document_edit_spans_the_whole_document() {
        let old = "from os.path import join\n";
        let new = "import os.path\n";
        let edit = full_document_edit(old, new);
        assert_eq!(
            edit.range.start,
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(edit.range.end, byte_offset_to_lsp_pos(old, old.len()));
        assert_eq!(edit.new_text, new);
    }
}
