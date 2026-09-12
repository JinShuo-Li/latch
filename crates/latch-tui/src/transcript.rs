//! Transcript cell rendering: assistant text, tool activity, validation,
//! exploration, diffs, and patch previews.

use super::markdown::{MARKDOWN_DEFAULT_WIDTH, render_markdown_at};
use super::*;

/// Builds the transcript as Ratatui lines with the item's own styling,
/// splitting embedded newlines so the wrapper and the scroll calculation agree
/// on the visual row layout.
pub(super) fn transcript_lines(
    cells: &[Cell],
    streaming: Option<&str>,
    detail: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for cell in cells {
        for line in cell_lines(cell, detail, width) {
            out.push(line);
        }
        out.push(Line::from(""));
    }
    if let Some(text) = streaming {
        out.extend(
            text.lines()
                .map(|line| Line::styled(line.to_owned(), assistant_style())),
        );
    }
    while out.last().is_some_and(|line| {
        line.spans.is_empty() || line.spans.iter().all(|span| span.content.is_empty())
    }) {
        out.pop();
    }
    out
}

pub(super) fn cell_lines(cell: &Cell, detail: bool, width: usize) -> Vec<Line<'static>> {
    if detail {
        return cell
            .raw_text()
            .lines()
            .map(|line| Line::styled(line.to_owned(), notice_style()))
            .collect();
    }
    match cell {
        Cell::User { text } => text
            .split('\n')
            .enumerate()
            .map(|(index, segment)| {
                let prefix = if index == 0 { "› " } else { "  " };
                Line::styled(
                    format!("{prefix}{segment}"),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
            })
            .collect(),
        Cell::Assistant { text } => render_markdown_at(text, width),
        Cell::Exploration { operations } => exploration_lines(operations),
        Cell::Command {
            command,
            status,
            summary,
            output,
            ..
        } => activity_lines(
            *status,
            if *status == CellStatus::Running {
                "Running"
            } else {
                "Ran"
            },
            command,
            summary,
            output,
        ),
        Cell::Validation {
            command,
            status,
            summary,
            output,
            ..
        } => {
            let title = match status {
                CellStatus::Running => "Validating",
                CellStatus::Passed => "Verified",
                CellStatus::Failed => "Validation failed",
            };
            validation_lines(*status, title, command, summary, output)
        }
        Cell::Patch { files } => patch_lines(files),
        Cell::Diff {
            status, document, ..
        } => diff_cell_lines(*status, document),
        Cell::Notice { text } => text
            .split('\n')
            .map(|segment| Line::styled(format!("· {segment}"), notice_style()))
            .collect(),
        Cell::Error { text } => text
            .split('\n')
            .map(|segment| Line::styled(format!("✗ {segment}"), Style::default().fg(Color::Red)))
            .collect(),
    }
}

pub(super) fn status_marker(status: CellStatus) -> (&'static str, Style) {
    match status {
        CellStatus::Running => ("•", Style::default().fg(Color::Cyan)),
        CellStatus::Passed => ("✓", Style::default().fg(Color::Green)),
        CellStatus::Failed => ("✗", Style::default().fg(Color::Red)),
    }
}

pub(super) fn activity_lines(
    status: CellStatus,
    title: &str,
    subject: &str,
    summary: &str,
    output: &str,
) -> Vec<Line<'static>> {
    let (marker, marker_style) = if status == CellStatus::Passed && title == "Ran" {
        ("•", Style::default().fg(Color::Cyan))
    } else {
        status_marker(status)
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(
            format!("{title} {subject}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])];
    if !summary.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  └ ", notice_style()),
            Span::styled(summary.to_owned(), notice_style()),
        ]));
    }
    if status == CellStatus::Failed && !output.is_empty() {
        lines.push(Line::from(""));
        lines.extend(
            output
                .lines()
                .map(|line| Line::styled(format!("    {line}"), Style::default().fg(Color::Red))),
        );
    }
    lines
}

pub(super) fn validation_lines(
    status: CellStatus,
    title: &str,
    command: &str,
    summary: &str,
    output: &str,
) -> Vec<Line<'static>> {
    let (marker, style) = status_marker(status);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(title.to_owned(), Style::default().bold()),
    ])];
    let detail = if summary.is_empty() {
        command.to_owned()
    } else {
        format!("{command} · {summary}")
    };
    lines.push(Line::from(vec![
        Span::styled("  └ ", notice_style()),
        Span::raw(detail),
    ]));
    if status == CellStatus::Failed && !output.is_empty() {
        lines.push(Line::from(""));
        lines.extend(
            output
                .lines()
                .map(|line| Line::styled(format!("    {line}"), Style::default().fg(Color::Red))),
        );
    }
    lines
}

pub(super) fn exploration_lines(operations: &[ExplorationOperation]) -> Vec<Line<'static>> {
    let running = operations.iter().any(|op| op.status == CellStatus::Running);
    let failed = operations.iter().any(|op| op.status == CellStatus::Failed);
    let status = if failed {
        CellStatus::Failed
    } else if running {
        CellStatus::Running
    } else {
        CellStatus::Passed
    };
    let (marker, style) = if status == CellStatus::Passed {
        ("•", Style::default().fg(Color::Cyan))
    } else {
        status_marker(status)
    };
    let title = if running { "Exploring" } else { "Explored" };
    let mut labels = Vec::new();
    let mut reads = Vec::new();
    for operation in operations {
        if operation.status != CellStatus::Failed
            && let Some(path) = operation.label.strip_prefix("Read ")
        {
            reads.push(path);
        } else {
            labels.push(operation.label.clone());
        }
    }
    if !reads.is_empty() {
        let mut read_label = format!(
            "Read {}",
            reads.iter().take(8).copied().collect::<Vec<_>>().join(", ")
        );
        if reads.len() > 8 {
            read_label.push_str(&format!(", … {} more", reads.len() - 8));
        }
        labels.insert(0, read_label);
    }
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(title, Style::default().bold()),
    ])];
    const MAX_VISIBLE_OPERATIONS: usize = 8;
    for (index, label) in labels.iter().take(MAX_VISIBLE_OPERATIONS).enumerate() {
        let prefix = if index == 0 { "  └ " } else { "    " };
        lines.push(Line::from(vec![
            Span::styled(prefix, notice_style()),
            Span::raw(label.clone()),
        ]));
    }
    if labels.len() > MAX_VISIBLE_OPERATIONS {
        lines.push(Line::styled(
            format!("    … and {} more", labels.len() - MAX_VISIBLE_OPERATIONS),
            notice_style(),
        ));
    }
    for operation in operations
        .iter()
        .filter(|op| op.status == CellStatus::Failed)
    {
        lines.push(Line::styled(
            format!("    {} — {}", operation.label, operation.diagnostic),
            Style::default().fg(Color::Red),
        ));
    }
    lines
}

/// One transcript cell for a first-class diff. Bounded so a large model-issued
/// diff never floods the transcript; `/diff` opens the full inspector.
pub(super) fn diff_cell_lines(status: CellStatus, document: &DiffDocument) -> Vec<Line<'static>> {
    let (marker, marker_style) = if status == CellStatus::Passed {
        ("•", Style::default().fg(Color::Cyan))
    } else {
        status_marker(status)
    };
    let title = match status {
        CellStatus::Running => "Diff",
        CellStatus::Passed => "Workspace diff",
        CellStatus::Failed => "Diff failed",
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(title.to_owned(), Style::default().bold()),
    ])];
    if document.is_empty() {
        if status == CellStatus::Passed {
            lines.push(Line::styled("  └ no workspace changes", notice_style()));
        }
        return lines;
    }
    let mut summary = vec![Span::styled(
        format!(
            "  {} file{}  ",
            document.files.len(),
            if document.files.len() == 1 { "" } else { "s" }
        ),
        notice_style(),
    )];
    let additions = document.added_lines();
    let deletions = document.removed_lines();
    summary.push(Span::styled(
        format!("+{additions}"),
        if additions > 0 {
            Style::default().fg(Color::Green)
        } else {
            notice_style()
        },
    ));
    summary.push(Span::styled(" ", notice_style()));
    summary.push(Span::styled(
        format!("−{deletions}"),
        if deletions > 0 {
            Style::default().fg(Color::Red)
        } else {
            notice_style()
        },
    ));
    lines.push(Line::from(summary));
    lines.extend(crate::diff::diff_lines_bounded(document, 30));
    lines
}

pub(super) fn patch_lines(files: &[PatchFile]) -> Vec<Line<'static>> {
    let failed = files.iter().any(|file| file.status == CellStatus::Failed);
    let running = files.iter().any(|file| file.status == CellStatus::Running);
    let status = if failed {
        CellStatus::Failed
    } else if running {
        CellStatus::Running
    } else {
        CellStatus::Passed
    };
    let (marker, style) = if status == CellStatus::Passed {
        ("•", Style::default().fg(Color::Cyan))
    } else {
        status_marker(status)
    };
    let title = if running {
        "Editing"
    } else if failed {
        "Edit failed"
    } else {
        "Edited"
    };
    let mut lines = Vec::new();
    let mut remaining = MAX_PATCH_PREVIEW_LINES;
    let mut omitted = 0usize;
    // The real unified diff recorded with the change is the only source of
    // preview lines; counters are never expanded into diff text.
    let preview_body = |preview: &str| diff::diff_body_lines(&parse_unified_diff(preview));
    if files.len() == 1 {
        let file = &files[0];
        let mut spans = vec![
            Span::styled(format!("{marker} "), style),
            Span::styled(format!("{title} {}", file.path), Style::default().bold()),
            Span::raw("  "),
        ];
        spans.extend(delta_spans(file.additions, file.deletions));
        lines.push(Line::from(spans));
        if !file.preview.is_empty() {
            let body = preview_body(&file.preview);
            if !body.is_empty() {
                lines.push(Line::from(""));
            }
            let take = remaining.min(body.len());
            lines.extend(
                body[..take]
                    .iter()
                    .cloned()
                    .map(|line| indent_preview(line, 2)),
            );
            omitted += body.len() - take;
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::styled(
                format!("{title} {} files", files.len()),
                Style::default().bold(),
            ),
        ]));
        for (index, file) in files.iter().enumerate() {
            let prefix = if index == 0 { "  └ " } else { "    " };
            let mut spans = vec![
                Span::styled(prefix, notice_style()),
                Span::raw(format!("{} {}", file.kind, file.path)),
                Span::raw("  "),
            ];
            spans.extend(delta_spans(file.additions, file.deletions));
            lines.push(Line::from(spans));
            if file.preview.is_empty() {
                continue;
            }
            let body = preview_body(&file.preview);
            let take = remaining.min(body.len());
            lines.extend(
                body[..take]
                    .iter()
                    .cloned()
                    .map(|line| indent_preview(line, 4)),
            );
            remaining -= take;
            omitted += body.len() - take;
        }
    }
    if omitted > 0 {
        lines.push(Line::styled(
            format!("  … {omitted} diff lines omitted · /diff for the full diff"),
            notice_style(),
        ));
    }
    for file in files
        .iter()
        .filter(|file| file.status == CellStatus::Failed)
    {
        lines.push(Line::styled(
            format!("    {}", file.diagnostic),
            Style::default().fg(Color::Red),
        ));
    }
    lines
}

/// Indents one inline preview line and subdues unchanged context so additions
/// and deletions carry the eye.
pub(super) fn indent_preview(mut line: Line<'static>, spaces: usize) -> Line<'static> {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    if text.starts_with(' ') {
        for span in &mut line.spans {
            span.style = notice_style();
        }
    }
    line.spans.insert(0, Span::raw(" ".repeat(spaces)));
    line
}

/// `+N −N` with independent semantic colors. Zero deltas stay dim so the eye
/// lands on the direction that actually changed.
pub(super) fn delta_spans(additions: usize, deletions: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("+{additions}"),
            if additions > 0 {
                Style::default().fg(Color::Green)
            } else {
                notice_style()
            },
        ),
        Span::raw(" "),
        Span::styled(
            format!("−{deletions}"),
            if deletions > 0 {
                Style::default().fg(Color::Red)
            } else {
                notice_style()
            },
        ),
    ]
}

pub(super) fn assistant_style() -> Style {
    Style::default().fg(Color::White)
}

pub(super) fn notice_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
pub(super) fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(1);
    while !text.is_char_boundary(text.floor_char_boundary(end)) {
        end -= 1;
    }
    format!("{}…", &text[..text.floor_char_boundary(end)])
}

/// Number of visual rows the transcript occupies at `width`, using the same
/// wrapping Ratatui renders with.
pub(super) fn semantic_visual_height(
    cells: &[Cell],
    streaming: Option<&str>,
    detail: bool,
    width: u16,
) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(transcript_lines(cells, streaming, detail, width as usize))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

#[cfg(test)]
pub(super) fn visual_height(items: &[TranscriptItem], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    let mut lines = Vec::new();
    for item in items {
        match item {
            TranscriptItem::User { text } => {
                lines.extend(text.lines().map(|line| Line::from(format!("› {line}"))))
            }
            TranscriptItem::Assistant { text, .. } => {
                lines.extend(text.lines().map(|line| Line::from(line.to_owned())))
            }
            TranscriptItem::Tool(row) => lines.push(Line::from(format!(
                "{} {} {}",
                row.verb, row.target, row.detail
            ))),
            TranscriptItem::Notice { text } | TranscriptItem::Error { text } => {
                lines.extend(text.lines().map(|line| Line::from(line.to_owned())))
            }
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Inline preview budget across one edit cell. The full diff stays available
/// through `/diff`; the transcript never floods.
pub(super) const MAX_PATCH_PREVIEW_LINES: usize = 14;

/// Copy-friendly rendering used by tests and transcript export paths.
#[must_use]
pub fn render_cells_plain(cells: &[Cell], detail: bool) -> String {
    transcript_lines(cells, None, detail, MARKDOWN_DEFAULT_WIDTH)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
