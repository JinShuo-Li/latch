//! Markdown rendering: inline spans, tables, and width-aware wrapping.

use super::transcript::{assistant_style, notice_style};
use super::*;

/// Column alignment parsed from a Markdown separator row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TableAlign {
    Left,
    Center,
    Right,
}

/// A small deterministic Markdown subset for assistant text: headings, bullet
/// and numbered lists, fenced code blocks, inline code, bold, links, and
/// aligned tables. Enough that model output stops reading like raw Markdown
/// source; not a browser engine. `width` bounds table column sizing so the
/// paragraph wrapper never has to break an aligned row.
pub(super) fn render_markdown_at(text: &str, width: usize) -> Vec<Line<'static>> {
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut index = 0;
    while index < raw_lines.len() {
        let raw = raw_lines[index];
        let trimmed = raw.trim_end();
        if let Some(rest) = trimmed.trim().strip_prefix("```") {
            let _ = rest;
            in_code = !in_code;
            index += 1;
            continue;
        }
        if in_code {
            out.push(Line::styled(
                format!("  │ {trimmed}"),
                Style::default().fg(Color::Cyan),
            ));
            index += 1;
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        let body = trimmed.trim_start();
        if let Some(rest) = body.strip_prefix('#') {
            let level = rest.chars().take_while(|c| *c == '#').count();
            let heading = rest.trim_start_matches('#').trim_start();
            let style = if level <= 2 {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            };
            out.push(Line::styled(heading.to_owned(), style));
            index += 1;
            continue;
        }
        if let Some(rest) = body.strip_prefix("- ").or_else(|| body.strip_prefix("* ")) {
            let mut spans = vec![Span::styled(
                format!("{}• ", " ".repeat(indent)),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(inline_spans(rest, assistant_style()));
            out.push(Line::from(spans));
            index += 1;
            continue;
        }
        let numbered = body.split_once(". ").is_some_and(|(marker, _)| {
            !marker.is_empty() && marker.chars().all(|c| c.is_ascii_digit())
        });
        if numbered {
            let (marker, rest) = body.split_once(". ").expect("checked above");
            let mut spans = vec![Span::styled(
                format!("{}{marker}.", " ".repeat(indent)),
                Style::default().fg(Color::DarkGray),
            )];
            spans.push(Span::raw(" "));
            spans.extend(inline_spans(rest, assistant_style()));
            out.push(Line::from(spans));
            index += 1;
            continue;
        }
        if body.is_empty() {
            out.push(Line::from(String::new()));
            index += 1;
            continue;
        }
        if body.contains('|') {
            if let Some((consumed, table)) = render_table_block(&raw_lines[index..], width) {
                out.extend(table);
                index += consumed;
                continue;
            }
            // Pipe content that is not an ordinary table keeps the previous
            // single-line treatment; separator rows are still never literal.
            let cells = body
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>();
            if cells.iter().all(|cell| {
                !cell.is_empty() && cell.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
            }) {
                index += 1;
                continue;
            }
            let mut spans = vec![Span::styled("│ ", notice_style())];
            for (cell_index, cell) in cells.iter().enumerate() {
                if cell_index > 0 {
                    spans.push(Span::styled(" │ ", notice_style()));
                }
                spans.extend(inline_spans(cell, assistant_style()));
            }
            spans.push(Span::styled(" │", notice_style()));
            out.push(Line::from(spans));
            index += 1;
            continue;
        }
        out.push(Line::from(inline_spans(body, assistant_style())));
        index += 1;
    }
    out
}

/// Splits one Markdown table row into trimmed cells, honoring `\|` escapes.
/// Returns `None` when the line has no pipe or fewer than two cells, which
/// keeps prose and single-pipe content on the raw path.
pub(super) fn split_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => current.push(chars.next().unwrap_or('\\')),
            '|' => {
                cells.push(current.trim().to_owned());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    cells.push(current.trim().to_owned());
    if cells.len() < 2 {
        return None;
    }
    Some(cells)
}

/// True when every cell is a `---`/`:---:` style delimiter. The width source
/// cell must contain at least one dash.
pub(super) fn is_table_separator(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let cell = cell.trim();
            !cell.is_empty() && cell.contains('-') && cell.chars().all(|ch| ch == '-' || ch == ':')
        })
}

pub(super) fn table_align(cell: &str) -> TableAlign {
    let cell = cell.trim();
    match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => TableAlign::Center,
        (false, true) => TableAlign::Right,
        _ => TableAlign::Left,
    }
}

/// Detects and renders an ordinary Markdown table at the head of `lines`.
/// Returns the number of source lines consumed and the rendered rows, or
/// `None` when the block is malformed, too narrow for even minimum columns, or
/// simply not a table.
pub(super) fn render_table_block(
    lines: &[&str],
    width: usize,
) -> Option<(usize, Vec<Line<'static>>)> {
    let header = split_table_row(lines.first()?)?;
    let separator = split_table_row(lines.get(1)?)?;
    if separator.len() != header.len() || !is_table_separator(&separator) {
        return None;
    }
    let aligns: Vec<TableAlign> = separator.iter().map(|cell| table_align(cell)).collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2;
    for line in &lines[2..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("```") || trimmed.starts_with('#') {
            break;
        }
        let Some(mut cells) = split_table_row(line) else {
            break;
        };
        cells.truncate(header.len());
        while cells.len() < header.len() {
            cells.push(String::new());
        }
        rows.push(cells);
        consumed += 1;
    }
    let table = table_lines(&header, &aligns, &rows, width)?;
    Some((consumed, table))
}

/// Renders a detected table with proportional column widths and cell wrapping.
/// `None` means the available width cannot hold readable columns; the caller
/// then falls back to the raw line treatment.
pub(super) fn table_lines(
    header: &[String],
    aligns: &[TableAlign],
    rows: &[Vec<String>],
    width: usize,
) -> Option<Vec<Line<'static>>> {
    const GAP: usize = 2;
    const MIN_CELL: usize = 3;
    const MAX_CELL: usize = 48;
    let columns = header.len();
    if columns < 2 || aligns.len() != columns {
        return None;
    }
    let gap_total = GAP * (columns - 1);
    if width <= gap_total + columns * MIN_CELL {
        return None;
    }
    let available = width - gap_total;
    let natural: Vec<usize> = (0..columns)
        .map(|column| {
            std::iter::once(&header[column])
                .chain(rows.iter().map(|row| &row[column]))
                .map(|cell| display_width(cell))
                .max()
                .unwrap_or(1)
                .clamp(1, MAX_CELL)
        })
        .collect();
    let widths = if natural.iter().sum::<usize>() <= available {
        natural
    } else {
        distribute_widths(&natural, available)
    };
    let mut out = Vec::new();
    let header_style = Style::default().add_modifier(Modifier::BOLD);
    let header_rows = wrap_row(header, &widths);
    let header_height = header_rows.iter().map(Vec::len).max().unwrap_or(1);
    for line_index in 0..header_height {
        out.push(table_row_line(
            &header_rows,
            &widths,
            aligns,
            line_index,
            header_style,
        ));
    }
    let total: usize = widths.iter().sum::<usize>() + gap_total;
    out.push(Line::styled("─".repeat(total), notice_style()));
    for row in rows {
        let wrapped = wrap_row(row, &widths);
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            out.push(table_row_line(
                &wrapped,
                &widths,
                aligns,
                line_index,
                assistant_style(),
            ));
        }
    }
    Some(out)
}

/// Max-min fair column widths: short columns keep their natural width first,
/// and whatever remains is split evenly among the columns that still need to
/// wrap. `available` is the space left after the gutters.
pub(super) fn distribute_widths(natural: &[usize], available: usize) -> Vec<usize> {
    let mut widths = vec![0usize; natural.len()];
    let mut pending: Vec<usize> = (0..natural.len()).collect();
    let mut remaining = available;
    while !pending.is_empty() {
        let share = remaining / pending.len();
        let small: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|column| natural[*column] <= share)
            .collect();
        if small.is_empty() {
            for (position, &column) in pending.iter().enumerate() {
                let per = remaining / (pending.len() - position);
                widths[column] = per;
                remaining -= per;
            }
            break;
        }
        for &column in &small {
            widths[column] = natural[column];
            remaining -= natural[column];
        }
        pending.retain(|column| !small.contains(column));
    }
    widths
}

pub(super) fn wrap_row(cells: &[String], widths: &[usize]) -> Vec<Vec<String>> {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| wrap_cell(cell, *width))
        .collect()
}

/// One physical line of a table row: each column is wrapped separately, then
/// padded to its width and joined with a two-space gutter.
pub(super) fn table_row_line(
    wrapped: &[Vec<String>],
    widths: &[usize],
    aligns: &[TableAlign],
    line_index: usize,
    base: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (column, cells) in wrapped.iter().enumerate() {
        if column > 0 {
            spans.push(Span::raw(" ".repeat(2)));
        }
        let text = cells.get(line_index).map_or("", String::as_str);
        let padded = pad_cell(text, widths[column], aligns[column]);
        spans.extend(inline_spans(&padded, base));
    }
    Line::from(spans)
}

pub(super) fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() || width == 0 {
        return vec![String::new()];
    }
    // `wrap_points` returns char offsets (the composer stores them that way in
    // `VisualRow`); convert them to byte offsets before slicing so CJK and
    // other multi-byte cells never split inside a character.
    let mut byte_offsets: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
    byte_offsets.push(text.len());
    let char_count = byte_offsets.len() - 1;
    let points = composer::wrap_points(text, width);
    let mut rows = Vec::new();
    for (index, start) in points.iter().enumerate() {
        let end = points.get(index + 1).copied().unwrap_or(char_count);
        let (Some(&start), Some(&end)) = (byte_offsets.get(*start), byte_offsets.get(end)) else {
            continue;
        };
        rows.push(text[start..end].trim_end().to_owned());
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

pub(super) fn pad_cell(text: &str, width: usize, align: TableAlign) -> String {
    let text_width = display_width(text).min(width);
    let padding = width - text_width;
    match align {
        TableAlign::Left => format!("{text}{}", " ".repeat(padding)),
        TableAlign::Right => format!("{}{text}", " ".repeat(padding)),
        TableAlign::Center => {
            let left = padding / 2;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(padding - left))
        }
    }
}

/// Inline formatting: `` `code` `` → dim cyan, `**bold**` → bold.
pub(super) fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            push_plain_spans(plain, base, spans);
            plain.clear();
        }
    };
    while index < chars.len() {
        if chars[index] == '`'
            && let Some(close) = chars[index + 1..].iter().position(|c| *c == '`')
        {
            flush(&mut plain, &mut spans);
            let code: String = chars[index + 1..index + 1 + close].iter().collect();
            spans.push(Span::styled(code, Style::default().fg(Color::Cyan)));
            index += close + 2;
            continue;
        }
        if chars[index] == '*'
            && chars.get(index + 1) == Some(&'*')
            && let Some(close) = find_double_star(&chars, index + 2)
        {
            flush(&mut plain, &mut spans);
            let bold: String = chars[index + 2..close].iter().collect();
            spans.push(Span::styled(bold, base.add_modifier(Modifier::BOLD)));
            index = close + 2;
            continue;
        }
        if chars[index] == '['
            && let Some(label_end_offset) = chars[index + 1..].iter().position(|ch| *ch == ']')
        {
            let label_end = index + 1 + label_end_offset;
            if chars.get(label_end + 1) == Some(&'(')
                && let Some(url_end_offset) =
                    chars[label_end + 2..].iter().position(|ch| *ch == ')')
            {
                flush(&mut plain, &mut spans);
                let label: String = chars[index + 1..label_end].iter().collect();
                let url_end = label_end + 2 + url_end_offset;
                let url: String = chars[label_end + 2..url_end].iter().collect();
                spans.push(Span::styled(label, base.add_modifier(Modifier::UNDERLINED)));
                spans.push(Span::styled(
                    format!(" ({url})"),
                    Style::default().fg(Color::Cyan),
                ));
                index = url_end + 1;
                continue;
            }
        }
        if matches!(chars[index], '*' | '_')
            && chars.get(index + 1) != Some(&chars[index])
            && let Some(close) = chars[index + 1..].iter().position(|ch| *ch == chars[index])
        {
            flush(&mut plain, &mut spans);
            let italic: String = chars[index + 1..index + 1 + close].iter().collect();
            spans.push(Span::styled(italic, base.add_modifier(Modifier::ITALIC)));
            index += close + 2;
            continue;
        }
        plain.push(chars[index]);
        index += 1;
    }
    flush(&mut plain, &mut spans);
    spans
}

pub(super) fn push_plain_spans(text: &str, base: Style, spans: &mut Vec<Span<'static>>) {
    let mut rest = text;
    while let Some(start) = rest.find("http://").or_else(|| rest.find("https://")) {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_owned(), base));
        }
        let end = rest[start..]
            .find(char::is_whitespace)
            .map_or(rest.len(), |offset| start + offset);
        spans.push(Span::styled(
            rest[start..end].to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        ));
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_owned(), base));
    }
}

pub(super) fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1))
        .find(|&index| chars[index] == '*' && chars.get(index + 1) == Some(&'*'))
}

/// Width assumed when no terminal viewport is available (plain export).
pub(super) const MARKDOWN_DEFAULT_WIDTH: usize = 100;
