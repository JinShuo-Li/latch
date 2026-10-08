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
    let mut code_indent = 0;
    // Source marker column and rendered body column for each open list item.
    let mut items: Vec<(usize, usize)> = Vec::new();
    let mut after_blank = false;
    let mut index = 0;
    while index < raw_lines.len() {
        let raw = raw_lines[index];
        let trimmed = raw.trim_end();
        let body = trimmed.trim_start();
        let indent = display_width(&trimmed[..trimmed.len() - body.len()]);
        if body.starts_with("```") {
            if !in_code {
                while items.last().is_some_and(|item| indent < item.1) {
                    items.pop();
                }
                code_indent = items.last().map_or(0, |item| item.1);
            }
            in_code = !in_code;
            index += 1;
            continue;
        }
        if in_code {
            let content = trimmed
                .strip_prefix(&" ".repeat(code_indent))
                .unwrap_or(trimmed);
            let prefix = format!("{}  │ ", " ".repeat(code_indent));
            out.extend(wrap_hanging(
                Line::styled(
                    format!("{prefix}{content}"),
                    crate::theme::palette().accent_plain(),
                ),
                width,
                display_width(&prefix),
            ));
            index += 1;
            continue;
        }
        if body.is_empty() {
            separate(&mut out, 1);
            after_blank = true;
            index += 1;
            continue;
        }
        let marker = list_marker(body);
        if let Some((prefix, rest)) = marker {
            while items.last().is_some_and(|item| item.0 >= indent) {
                items.pop();
            }
            if items.is_empty() {
                separate(&mut out, 1);
            }
            let prefix = format!("{}{prefix}", " ".repeat(indent));
            let origin = display_width(&prefix);
            items.push((indent, origin));
            let mut spans = vec![Span::styled(prefix, notice_style())];
            spans.extend(inline_spans(rest, assistant_style()));
            out.extend(wrap_hanging(Line::from(spans), width, origin));
            after_blank = false;
            index += 1;
            continue;
        }
        // An unindented line directly after an item is a lazy continuation.
        // After a blank, only an explicitly indented paragraph belongs to it.
        if after_blank {
            while items.last().is_some_and(|item| indent < item.1) {
                items.pop();
            }
        } else if indent > 0 {
            while items.len() > 1 && items.last().is_some_and(|item| indent < item.1) {
                items.pop();
            }
        }
        if body.starts_with('#') {
            while items.last().is_some_and(|item| indent < item.1) {
                items.pop();
            }
        }
        let origin = items.last().map_or(indent, |item| item.1.max(indent));
        after_blank = false;
        if let Some(rest) = body.strip_prefix('#') {
            let level = 1 + rest.chars().take_while(|c| *c == '#').count();
            let heading = rest.trim_start_matches('#').trim_start();
            let style = if level <= 2 {
                Style::default().bold().underlined()
            } else {
                Style::default().bold()
            };
            separate(&mut out, 2);
            out.extend(wrap_hanging(
                Line::styled(format!("{}{heading}", " ".repeat(origin)), style),
                width,
                origin,
            ));
            separate(&mut out, 1);
            index += 1;
            continue;
        }
        if body.contains('|') {
            if let Some((consumed, table)) =
                render_table_block(&raw_lines[index..], width.saturating_sub(origin))
            {
                separate(&mut out, 1);
                out.extend(table.into_iter().map(|mut line| {
                    if origin > 0 {
                        line.spans.insert(0, Span::raw(" ".repeat(origin)));
                    }
                    line
                }));
                separate(&mut out, 1);
                index += consumed;
                continue;
            }
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
            let mut spans = vec![Span::styled(
                format!("{}│ ", " ".repeat(origin)),
                notice_style(),
            )];
            for (cell_index, cell) in cells.iter().enumerate() {
                if cell_index > 0 {
                    spans.push(Span::styled(" │ ", notice_style()));
                }
                spans.extend(inline_spans(cell, assistant_style()));
            }
            spans.push(Span::styled(" │", notice_style()));
            out.extend(wrap_hanging(Line::from(spans), width, origin + 2));
            index += 1;
            continue;
        }
        let mut spans = vec![Span::raw(" ".repeat(origin))];
        spans.extend(inline_spans(body, assistant_style()));
        out.extend(wrap_hanging(Line::from(spans), width, origin));
        index += 1;
    }
    while out.last().is_some_and(|line| line.width() == 0) {
        out.pop();
    }
    out
}

fn list_marker(body: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = body.strip_prefix(marker) {
            return Some(("• ".into(), rest));
        }
    }
    let end = body.find(['.', ')'])?;
    let digits = &body[..end];
    if digits.is_empty() || !digits.bytes().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let rest = body[end + 1..].strip_prefix(' ')?;
    Some((format!("{} ", &body[..=end]), rest))
}

/// Deliberate block separation, independent of source blank-line count.
fn separate(out: &mut Vec<Line<'static>>, rows: usize) {
    if out.is_empty() {
        return;
    }
    let existing = out
        .iter()
        .rev()
        .take_while(|line| line.width() == 0)
        .count();
    for _ in existing..rows {
        out.push(Line::from(""));
    }
}

/// Wrap styled text by display cells, retaining its body origin on every row.
/// Graphemes and inline styles survive both word and hard breaks. Spaces at a
/// word break remain at the preceding row so the raw view loses no characters.
pub(super) fn wrap_hanging(line: Line<'static>, width: usize, origin: usize) -> Vec<Line<'static>> {
    wrap_styled(line, width, origin, 0)
}

fn wrap_styled(
    line: Line<'static>,
    width: usize,
    origin: usize,
    minimum_word_break: usize,
) -> Vec<Line<'static>> {
    use unicode_segmentation::UnicodeSegmentation;
    if width == 0 {
        return Vec::new();
    }
    let origin = origin.min(width.saturating_sub(2));
    let units: Vec<_> = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .graphemes(true)
                .map(move |text| (text.to_owned(), span.style))
        })
        .collect();
    if units.is_empty() {
        return vec![line];
    }
    let mut rows = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let padding = if start == 0 { 0 } else { origin };
        let mut used = padding;
        let mut end = start;
        let mut space = None;
        while end < units.len() {
            let cells = display_width(&units[end].0);
            if used + cells > width && end > start {
                break;
            }
            used += cells;
            end += 1;
            if units[end - 1].0.chars().all(char::is_whitespace)
                && used > origin
                && used >= minimum_word_break
            {
                space = Some(end);
            }
        }
        if end < units.len()
            && let Some(boundary) = space
        {
            end = boundary;
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        if padding > 0 {
            spans.push(Span::raw(" ".repeat(padding)));
        }
        for (text, style) in &units[start..end] {
            if let Some(last) = spans.last_mut().filter(|span| span.style == *style) {
                last.content.to_mut().push_str(text);
            } else {
                spans.push(Span::styled(text.clone(), *style));
            }
        }
        rows.push(Line::from(spans).style(line.style));
        start = end;
    }
    rows
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

/// Parse inline Markdown before measuring, wrapping or aligning cells. Visible
/// spans, not source delimiters, are the source of truth for every column.
pub(super) fn table_lines(
    header: &[String],
    aligns: &[TableAlign],
    rows: &[Vec<String>],
    width: usize,
) -> Option<Vec<Line<'static>>> {
    const MIN_CELL: usize = 3;
    let columns = header.len();
    if columns < 2 || aligns.len() != columns {
        return None;
    }
    let gap_total = TABLE_GAP * (columns - 1);
    if width < gap_total + columns * MIN_CELL {
        return None;
    }
    let available = width - gap_total;
    let parse = |cells: &[String], base| {
        cells
            .iter()
            .map(|cell| Line::from(inline_spans(cell, base)))
            .collect::<Vec<_>>()
    };
    let header_cells = parse(header, Style::default().bold());
    let body_cells: Vec<_> = rows
        .iter()
        .map(|row| parse(row, assistant_style()))
        .collect();
    let natural: Vec<usize> = (0..columns)
        .map(|column| {
            std::iter::once(&header_cells[column])
                .chain(body_cells.iter().filter_map(|row| row.get(column)))
                .map(Line::width)
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();
    // Short columns retain their natural width; long descriptions can use the
    // remaining viewport rather than wrapping at an arbitrary fixed cap.
    let widths = if natural.iter().sum::<usize>() <= available {
        natural
    } else {
        distribute_widths(&natural, available)
    };
    let mut out = Vec::new();
    let header_rows = wrap_row(&header_cells, &widths);
    let header_height = header_rows.iter().map(Vec::len).max().unwrap_or(1);
    for line_index in 0..header_height {
        out.push(table_row_line(&header_rows, &widths, aligns, line_index));
    }
    let total = widths.iter().sum::<usize>() + gap_total;
    out.push(Line::styled("─".repeat(total), notice_style()));
    for row in &body_cells {
        // Separate logical records, never the wrapped lines inside one record.
        out.push(Line::from(""));
        let wrapped = wrap_row(row, &widths);
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            out.push(table_row_line(&wrapped, &widths, aligns, line_index));
        }
    }
    Some(out)
}

const TABLE_GAP: usize = 3;

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

fn wrap_row(cells: &[Line<'static>], widths: &[usize]) -> Vec<Vec<Line<'static>>> {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| {
            // In mixed CJK/prose cells an early ASCII space (e.g. "16 个…")
            // must not leave almost an entire row empty. Prefer word breaks
            // only in the latter half; otherwise use a grapheme boundary.
            wrap_styled(cell.clone(), *width, 0, width / 2)
                .into_iter()
                .map(trim_cell_row)
                .collect()
        })
        .collect()
}

/// Remove only wrap-boundary whitespace before aligning the visible spans.
/// Inline styles stay attached to their text, including across hard breaks.
fn trim_cell_row(mut line: Line<'static>) -> Line<'static> {
    for span in &mut line.spans {
        span.content = span.content.trim_start().to_owned().into();
        if !span.content.is_empty() {
            break;
        }
    }
    for span in line.spans.iter_mut().rev() {
        span.content = span.content.trim_end().to_owned().into();
        if !span.content.is_empty() {
            break;
        }
    }
    line.spans.retain(|span| !span.content.is_empty());
    line
}

/// Every physical row uses the same column widths and gutters. Padding is
/// calculated from rendered display cells, including wide and styled text.
fn table_row_line(
    wrapped: &[Vec<Line<'static>>],
    widths: &[usize],
    aligns: &[TableAlign],
    line_index: usize,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (column, cells) in wrapped.iter().enumerate() {
        if column > 0 {
            spans.push(Span::raw(" ".repeat(TABLE_GAP)));
        }
        let cell = cells.get(line_index).cloned().unwrap_or_default();
        let padding = widths[column].saturating_sub(cell.width());
        let left = match aligns[column] {
            TableAlign::Left => 0,
            TableAlign::Center => padding / 2,
            TableAlign::Right => padding,
        };
        if left > 0 {
            spans.push(Span::raw(" ".repeat(left)));
        }
        spans.extend(cell.spans);
        if padding > left {
            spans.push(Span::raw(" ".repeat(padding - left)));
        }
    }
    Line::from(spans)
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
            spans.push(Span::styled(code, crate::theme::palette().accent_plain()));
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
                    crate::theme::palette().accent_plain(),
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
            crate::theme::palette()
                .accent_plain()
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
