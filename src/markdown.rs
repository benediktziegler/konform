//! A small Markdown-to-terminal renderer for `konform rule --explain`.
//!
//! It handles the subset of Markdown the rule docs use: headings, paragraphs,
//! bullet and numbered lists, fenced code blocks, pipe tables, and the inline
//! forms `` `code` ``, `**bold**`, `[text](url)` and `<br>`. HTML comments are
//! dropped. Everything else passes through as paragraph text.

use owo_colors::OwoColorize;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Code,
    Bold,
    Link,
    Url,
}

/// A run of inline text; `Code`, `Link` and `Url` words are never split.
struct Span {
    text: String,
    kind: Kind,
}

/// Parse inline Markdown into spans. Relative link targets are dropped (they
/// point into the docs site); absolute ones are kept after the link text.
fn parse_inline(s: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let flush = |plain: &mut String, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span {
                text: std::mem::take(plain),
                kind: Kind::Plain,
            });
        }
    };
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        if let Some(tail) = rest.strip_prefix("<br>") {
            plain.push(' ');
            i += rest.chars().count() - tail.chars().count();
        } else if let Some(tail) = rest.strip_prefix('`') {
            if let Some(end) = tail.find('`') {
                flush(&mut plain, &mut spans);
                spans.push(Span {
                    text: tail[..end].to_owned(),
                    kind: Kind::Code,
                });
                i += 2 + tail[..end].chars().count();
                continue;
            }
            plain.push('`');
            i += 1;
        } else if let Some(tail) = rest.strip_prefix("**") {
            if let Some(end) = tail.find("**") {
                flush(&mut plain, &mut spans);
                spans.push(Span {
                    text: tail[..end].to_owned(),
                    kind: Kind::Bold,
                });
                i += 4 + tail[..end].chars().count();
                continue;
            }
            plain.push_str("**");
            i += 2;
        } else if rest.starts_with('[') {
            let link = rest.find("](").and_then(|mid| {
                let close = rest[mid..].find(')')? + mid;
                Some((
                    rest[1..mid].to_owned(),
                    rest[mid + 2..close].to_owned(),
                    close,
                ))
            });
            if let Some((text, url, close)) = link {
                flush(&mut plain, &mut spans);
                spans.push(Span {
                    text: text.replace('`', ""),
                    kind: Kind::Link,
                });
                if url.starts_with("http") {
                    spans.push(Span {
                        text: format!("({url})"),
                        kind: Kind::Url,
                    });
                }
                i += rest[..=close].chars().count();
                continue;
            }
            plain.push('[');
            i += 1;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush(&mut plain, &mut spans);
    spans
}

fn style(text: &str, kind: Kind, color: bool) -> String {
    if !color || text.is_empty() {
        return text.to_owned();
    }
    match kind {
        Kind::Plain => text.to_owned(),
        Kind::Code => text.cyan().to_string(),
        Kind::Bold => text.bold().to_string(),
        Kind::Link => text.underline().to_string(),
        Kind::Url => text.dimmed().to_string(),
    }
}

/// Visible width of inline Markdown once rendered.
fn inline_width(s: &str) -> usize {
    parse_inline(s)
        .iter()
        .map(|sp| sp.text.chars().count())
        .sum::<usize>()
        + parse_inline(s)
            .windows(2)
            .filter(|w| w[1].kind == Kind::Url)
            .count()
}

/// Render inline Markdown on one line, without wrapping.
fn render_inline(s: &str, color: bool) -> String {
    let mut out = String::new();
    for sp in parse_inline(s) {
        if sp.kind == Kind::Url {
            out.push(' ');
        }
        out.push_str(&style(&sp.text, sp.kind, color));
    }
    out
}

/// Word-wrap inline Markdown to `width` columns. The first line starts after
/// the plain prefix `first`, later lines after `rest` (hanging indent).
fn wrap(text: &str, width: usize, first: &str, rest: &str, color: bool) -> Vec<String> {
    // Unbreakable atoms; `space` is whether whitespace separated it from the
    // previous atom (so punctuation after `code` stays attached).
    let mut atoms: Vec<(String, Kind, bool)> = Vec::new();
    let mut space = false;
    for sp in parse_inline(text) {
        match sp.kind {
            Kind::Plain | Kind::Bold => {
                space |= sp.text.starts_with(char::is_whitespace);
                for w in sp.text.split_whitespace() {
                    atoms.push((w.to_owned(), sp.kind, space));
                    space = true;
                }
                space = sp.text.ends_with(char::is_whitespace);
            }
            Kind::Url => {
                atoms.push((sp.text, sp.kind, true));
                space = false;
            }
            Kind::Code => {
                // Break long code spans at their spaces, never inside a word.
                for (n, w) in sp.text.split_whitespace().enumerate() {
                    atoms.push((w.to_owned(), sp.kind, if n == 0 { space } else { true }));
                }
                space = false;
            }
            Kind::Link => {
                atoms.push((sp.text, sp.kind, space));
                space = false;
            }
        }
    }

    // An atom wider than a whole line is split into line-sized chunks.
    let room = width
        .saturating_sub(first.chars().count().max(rest.chars().count()))
        .max(1);
    let atoms: Vec<(String, Kind, bool)> = atoms
        .into_iter()
        .flat_map(|(w, kind, space)| {
            let chars: Vec<char> = w.chars().collect();
            chars
                .chunks(room)
                .enumerate()
                .map(|(n, c)| (c.iter().collect::<String>(), kind, space && n == 0))
                .collect::<Vec<_>>()
        })
        .collect();

    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    let mut prefix = first;
    for (w, kind, space) in atoms {
        let w_len = w.chars().count();
        let sep = usize::from(space && used > 0);
        if used > 0 && prefix.chars().count() + used + sep + w_len > width {
            lines.push(format!("{prefix}{line}"));
            line.clear();
            used = 0;
            prefix = rest;
        }
        if space && used > 0 {
            line.push(' ');
            used += 1;
        }
        line.push_str(&style(&w, kind, color));
        used += w_len;
    }
    if used > 0 || lines.is_empty() {
        lines.push(format!("{prefix}{line}"));
    }
    lines
}

/// Split a table row on unescaped pipes (`\|` stays inside the cell).
fn split_row(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|');
    let inner = inner
        .strip_suffix('|')
        .filter(|r| !r.ends_with('\\'))
        .unwrap_or(inner);
    let mut cells = vec![String::new()];
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("never empty").push('|');
            }
            '|' => cells.push(String::new()),
            _ => cells.last_mut().expect("never empty").push(c),
        }
    }
    cells.into_iter().map(|c| c.trim().to_owned()).collect()
}

fn is_separator_row(cells: &[String]) -> bool {
    cells
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// Width of the longest word in `cell` (a code span counts as one word).
fn longest_word(cell: &str) -> usize {
    parse_inline(cell)
        .iter()
        .flat_map(|sp| match sp.kind {
            Kind::Plain | Kind::Bold | Kind::Code => sp
                .text
                .split_whitespace()
                .map(|w| w.chars().count())
                .collect::<Vec<_>>(),
            _ => vec![sp.text.chars().count()],
        })
        .max()
        .unwrap_or(0)
}

/// Indent and separator widths of the table layout: `"  "` before the first
/// column and `"  │  "` between columns.
const TABLE_INDENT: usize = 2;
const TABLE_SEP: &str = "  │  ";

/// Render a table to fit `width` columns: as-is when it fits, with wrapped
/// cells when shrinking the widest columns is enough, and as one block per
/// row when even that would squeeze the columns too much.
fn render_table(rows: &[Vec<String>], width: usize, color: bool, out: &mut Vec<String>) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let cell = |row: &Vec<String>, i: usize| row.get(i).map_or(String::new(), Clone::clone);

    let natural: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .map(|r| inline_width(&cell(r, i)))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let floor: Vec<usize> = (0..cols)
        .map(|i| {
            let word = rows
                .iter()
                .map(|r| longest_word(&cell(r, i)))
                .max()
                .unwrap_or(0);
            // Words longer than 24 columns may be split across lines.
            word.min(24).max(natural[i].min(12))
        })
        .collect();

    let overhead = TABLE_INDENT + TABLE_SEP.chars().count() * (cols - 1);
    let avail = width.saturating_sub(overhead);
    if floor.iter().sum::<usize>() > avail {
        render_table_as_list(rows, width, color, out);
        return;
    }

    // Shrink the widest column (above its floor) until the table fits.
    let mut widths = natural.clone();
    while widths.iter().sum::<usize>() > avail {
        let widest = (0..cols)
            .filter(|&i| widths[i] > floor[i])
            .max_by_key(|&i| widths[i])
            .expect("floors fit, so a shrinkable column exists");
        widths[widest] -= 1;
    }
    let wrapped_table = widths != natural;

    for (n, row) in rows.iter().enumerate() {
        let plain: Vec<Vec<String>> = (0..cols)
            .map(|i| wrap(&cell(row, i), widths[i], "", "", false))
            .collect();
        let styled: Vec<Vec<String>> = (0..cols)
            .map(|i| wrap(&cell(row, i), widths[i], "", "", color))
            .collect();
        let height = plain.iter().map(Vec::len).max().unwrap_or(1);
        for h in 0..height {
            let mut line = " ".repeat(TABLE_INDENT);
            for i in 0..cols {
                let text = styled[i].get(h).map_or("", String::as_str);
                let visible = plain[i].get(h).map_or(0, |l| l.chars().count());
                let text = if n == 0 && color && !text.is_empty() {
                    text.bold().to_string()
                } else {
                    text.to_owned()
                };
                line.push_str(&text);
                line.push_str(&" ".repeat(widths[i].saturating_sub(visible)));
                if i + 1 < cols {
                    line.push_str(&style(TABLE_SEP, Kind::Url, color));
                }
            }
            out.push(line.trim_end().to_owned());
        }
        if n == 0 {
            let rule = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("──┼──");
            out.push(format!("  {}", style(&rule, Kind::Url, color)));
        } else if wrapped_table && n + 1 < rows.len() {
            out.push(String::new());
        }
    }
}

/// Narrow-terminal fallback: each body row becomes a bullet (first column)
/// followed by one `Header: value` line per remaining column.
fn render_table_as_list(rows: &[Vec<String>], width: usize, color: bool, out: &mut Vec<String>) {
    let Some((header, body)) = rows.split_first() else {
        return;
    };
    for row in body {
        out.extend(wrap(
            &row.first().cloned().unwrap_or_default(),
            width,
            "  • ",
            "    ",
            color,
        ));
        for (i, value) in row.iter().enumerate().skip(1) {
            if value.is_empty() {
                continue;
            }
            let label = header.get(i).map_or("", String::as_str);
            let first = format!("      {}: ", render_inline(label, false));
            let rest = " ".repeat(first.chars().count());
            out.extend(wrap(value, width, &first, &rest, color));
        }
        out.push(String::new());
    }
    if out.last().is_some_and(String::is_empty) {
        out.pop();
    }
}

/// Render `markdown` for a terminal `width` columns wide.
///
/// Prose is wrapped to at most 100 columns for readability; tables may use the
/// whole terminal width.
pub fn render(markdown: &str, width: usize, color: bool) -> String {
    let table_width = width.clamp(40, 240);
    let width = width.clamp(40, 100);
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;

    let blank = |out: &mut Vec<String>| {
        if out.last().is_some_and(|l| !l.is_empty()) {
            out.push(String::new());
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() || (trimmed.starts_with("<!--") && trimmed.ends_with("-->")) {
            i += 1;
        } else if trimmed.starts_with("```") {
            blank(&mut out);
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                let bar = style("│", Kind::Url, color);
                out.push(
                    format!("  {bar} {}", style(lines[i], Kind::Code, color))
                        .trim_end()
                        .to_owned(),
                );
                i += 1;
            }
            i += 1;
            out.push(String::new());
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            blank(&mut out);
            let text = render_inline(h, false);
            out.push(if color {
                text.bold().underline().to_string()
            } else {
                text
            });
            out.push(String::new());
            i += 1;
        } else if let Some(h) = trimmed.strip_prefix("##") {
            blank(&mut out);
            let text = render_inline(h.trim_start_matches('#').trim(), false);
            out.push(if color {
                text.bold().cyan().to_string()
            } else {
                text
            });
            out.push(String::new());
            i += 1;
        } else if trimmed.starts_with('|') {
            blank(&mut out);
            let mut rows = Vec::new();
            while i < lines.len() && lines[i].trim().starts_with('|') {
                let cells = split_row(lines[i]);
                if !is_separator_row(&cells) {
                    rows.push(cells);
                }
                i += 1;
            }
            render_table(&rows, table_width, color, &mut out);
            out.push(String::new());
        } else if let Some(item) = list_item(trimmed) {
            let (marker, body) = item;
            let bullet = if marker == "-" {
                "•".to_owned()
            } else {
                marker
            };
            let first = format!("  {bullet} ");
            let rest = " ".repeat(first.chars().count());
            let mut text = body.to_owned();
            i += 1;
            // Continuation lines of the item (indented, not a new item).
            while i < lines.len()
                && lines[i].starts_with("  ")
                && !lines[i].trim().is_empty()
                && list_item(lines[i].trim()).is_none()
            {
                text.push(' ');
                text.push_str(lines[i].trim());
                i += 1;
            }
            out.extend(wrap(&text, width, &first, &rest, color));
            if lines.get(i).is_none_or(|l| list_item(l.trim()).is_none()) {
                out.push(String::new());
            }
        } else {
            let mut text = String::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if t.is_empty()
                    || t.starts_with("```")
                    || t.starts_with('#')
                    || t.starts_with('|')
                    || list_item(t).is_some()
                {
                    break;
                }
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(t);
                i += 1;
            }
            out.extend(wrap(&text, width, "", "", color));
            out.push(String::new());
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// `- text` / `* text` / `1. text` → (marker, text).
fn list_item(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(("-".to_owned(), rest));
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = line[digits..].strip_prefix(". ") {
            return Some((format!("{}.", &line[..digits]), rest));
        }
    }
    None
}

/// Width of the terminal: an explicit `COLUMNS`, else the size of the
/// terminal attached to stdout, else 80.
pub fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .or_else(|| terminal_size::terminal_size().map(|(w, _)| usize::from(w.0)))
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(md: &str) -> String {
        render(md, 80, false)
    }

    #[test]
    fn headings_lose_their_hashes() {
        let out = plain("# Title\n\n## Section\n\ntext\n");
        assert_eq!(out, "Title\n\nSection\n\ntext\n");
    }

    #[test]
    fn html_comments_are_dropped() {
        assert_eq!(plain("<!-- generated -->\n\n# T\n"), "T\n");
    }

    #[test]
    fn inline_markup_is_unwrapped() {
        let out = plain("Use `x` and **y** or [docs](../a.md) or [web](https://e.com/p).\n");
        assert_eq!(out, "Use x and y or docs or web (https://e.com/p).\n");
    }

    #[test]
    fn code_blocks_are_indented_and_kept_verbatim() {
        let out = plain("```python\nfrom a import b   # c\n  indented\n```\n");
        assert_eq!(out, "  │ from a import b   # c\n  │   indented\n");
    }

    #[test]
    fn paragraphs_wrap_at_the_width() {
        let text = "word ".repeat(30);
        for line in render(&text, 40, false).lines() {
            assert!(line.chars().count() <= 40, "{line:?}");
        }
    }

    #[test]
    fn lists_use_bullets_and_hanging_indent() {
        let out = render(
            "- first item that is rather long and will wrap around\n- second\n",
            40,
            false,
        );
        assert!(
            out.starts_with("  • first item that is rather long and\n    will wrap"),
            "{out}"
        );
        assert!(out.contains("\n  • second\n"), "{out}");
    }

    #[test]
    fn numbered_lists_keep_their_numbers() {
        assert_eq!(plain("1. one\n2. two\n"), "  1. one\n  2. two\n");
    }

    #[test]
    fn tables_are_aligned() {
        let out = plain("| A | Longer |\n| --- | --- |\n| `x` | y |\n");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines[0].contains("A") && lines[0].contains("Longer"));
        assert!(lines[2].starts_with("  x"), "{out}");
        assert_eq!(
            lines[0].find('│'),
            lines[2].find('│'),
            "columns must line up: {out}"
        );
    }

    #[test]
    fn color_adds_ansi_and_plain_does_not() {
        assert!(!plain("# T\n`c`\n").contains('\u{1b}'));
        assert!(render("# T\n`c`\n", 80, true).contains('\u{1b}'));
    }

    #[test]
    fn punctuation_after_inline_code_stays_attached() {
        assert_eq!(plain("see `x`, then `y`.\n"), "see x, then y.\n");
    }

    const WIDE: &str = "| Option | Type | Default | Description |\n\
        | --- | --- | --- | --- |\n\
        | `exceptions` | `list[str]` | `[\"__future__\", \"typing\", \"typing_extensions\"]` | Modules that may import objects directly without being flagged by the rule. |\n\
        | `level` | `\"warning\" \\| \"error\"` | `\"error\"` | Severity of violations. |\n";

    fn all_text_present(out: &str) {
        for needle in [
            "exceptions",
            "list[str]",
            "typing_extensions",
            "directly",
            "flagged",
            "Severity",
            "level",
        ] {
            let squashed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                squashed.contains(needle) || out.contains(needle),
                "{needle} missing in:\n{out}"
            );
        }
    }

    #[test]
    fn wide_table_is_unchanged_when_it_fits() {
        let out = render(WIDE, 200, false);
        assert!(out.lines().all(|l| l.chars().count() <= 200), "{out}");
        assert!(out.contains(
            "Modules that may import objects directly without being flagged by the rule."
        ));
        assert!(
            !out.contains("\n\n"),
            "no row spacing when nothing wraps: {out}"
        );
    }

    #[test]
    fn wide_table_wraps_cells_to_fit_a_narrower_terminal() {
        let out = render(WIDE, 90, false);
        assert!(out.lines().all(|l| l.chars().count() <= 90), "{out}");
        assert!(out.contains('│'), "still a table: {out}");
        assert!(
            out.lines().count() > 4,
            "cells should wrap onto more lines: {out}"
        );
        all_text_present(&out);
    }

    #[test]
    fn wide_table_falls_back_to_a_list_on_a_very_narrow_terminal() {
        let out = render(WIDE, 40, false);
        assert!(out.lines().all(|l| l.chars().count() <= 40), "{out}");
        assert!(!out.contains('│'), "list layout has no columns: {out}");
        assert!(out.contains("  • exceptions"), "{out}");
        assert!(out.contains("Default: "), "{out}");
        all_text_present(&out);
    }

    #[test]
    fn escaped_pipes_stay_inside_their_cell() {
        assert_eq!(
            split_row("| `\"a\" \\| \"b\"` | x |"),
            vec!["`\"a\" | \"b\"`".to_owned(), "x".to_owned()]
        );
    }
}
