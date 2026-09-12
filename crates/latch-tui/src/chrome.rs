//! Frame composition: transcript viewport, composer, overlays, palette,
//! and the welcome/footer chrome.

use super::transcript::{notice_style, semantic_visual_height, transcript_lines};
use super::*;

/// Responsive chrome rows around the composer.
///
/// Rows are dropped in priority order as the terminal shrinks: footer, top
/// spacer, hints, gap, then the metadata row. The rounded frame and the editor
/// body are only ever reduced to a single row, never removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ComposerChrome {
    pub(super) spacer: u16,
    pub(super) top: u16,
    pub(super) body: u16,
    pub(super) gap: u16,
    pub(super) meta: u16,
    pub(super) rule: u16,
    pub(super) hints: u16,
    pub(super) footer: u16,
}

impl ComposerChrome {
    fn total(self) -> u16 {
        self.spacer
            + self.top
            + self.body
            + self.gap
            + self.meta
            + self.rule
            + self.hints
            + self.footer
    }

    /// The editor keeps at least this many rows on terminals tall enough for
    /// it, so the idle screen opens with a roomy input box instead of a
    /// single hairline row. Heights that cannot afford it fall back to a
    /// smaller body.
    const MIN_BODY: u16 = 3;

    pub(super) fn responsive(height: u16, content_rows: usize) -> Self {
        if height < 3 {
            return Self {
                body: 1,
                ..Self::default()
            };
        }
        let max_body = (u32::from(height) * 2 / 5).saturating_sub(4).clamp(1, 16) as u16;
        let mut chrome = Self {
            spacer: u16::from(height >= 14),
            top: 1,
            body: (content_rows.max(1) as u16)
                .max(Self::MIN_BODY)
                .min(max_body),
            gap: u16::from(height >= 9),
            meta: 1,
            rule: 1,
            hints: u16::from(height >= 10),
            footer: u16::from(height >= 16),
        };
        let floor = if height >= 10 { 3 } else { 1 };
        for optional in ["footer", "spacer", "hints", "gap", "meta"] {
            if height.saturating_sub(chrome.total()) >= floor {
                break;
            }
            match optional {
                "footer" => chrome.footer = 0,
                "spacer" => chrome.spacer = 0,
                "hints" => chrome.hints = 0,
                "gap" => chrome.gap = 0,
                "meta" => chrome.meta = 0,
                _ => {}
            }
        }
        chrome
    }
}

pub(super) fn yellow() -> Style {
    crate::theme::palette().attention()
}

pub(super) fn focused_accent() -> Style {
    crate::theme::palette().accent()
}

/// Readable secondary text for composer chrome (metadata, footer). Theme-aware
/// so it stays legible on both dark and light terminals.
pub(super) fn muted_style() -> Style {
    crate::theme::palette().muted()
}

/// Composer status word and color, derived from real run state.
pub(super) fn composer_status(app: &App) -> (String, Style) {
    if app.permission.is_some() {
        ("approval needed".into(), yellow())
    } else if app.busy {
        ("● working".into(), focused_accent())
    } else if app.interrupted {
        ("interrupted".into(), yellow())
    } else {
        ("ready".into(), muted_style())
    }
}

pub(super) fn hint_spans(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", notice_style()));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(Color::Gray),
        ));
        spans.push(Span::styled(format!(" {label}"), notice_style()));
    }
    spans
}

/// A restrained selector for `/safety` and `/permissions`, rendered above the
/// palette slot with the same visual language as the command palette.
pub(super) fn draw_policy_selector(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: ratatui::layout::Rect,
) {
    let Some(selector) = app.selector else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut rows = vec![Line::styled(
        format!("  {}", selector.kind.title().to_lowercase()),
        notice_style(),
    )];
    for (index, (label, _)) in selector.kind.options().iter().enumerate() {
        let selected = index == selector.selected;
        let style = if selected {
            crate::theme::palette().selected()
        } else {
            Style::default()
        };
        let marker = if selected { "› " } else { "  " };
        rows.push(Line::styled(format!("  {marker}{label}"), style));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

pub(super) fn draw_palette(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: ratatui::layout::Rect,
    candidates: &[&SlashCommand],
) {
    if area.height == 0 || area.width == 0 || candidates.is_empty() {
        return;
    }
    let window_start = app
        .palette
        .selected
        .saturating_sub(MAX_PALETTE_ROWS - 1)
        .min(candidates.len().saturating_sub(1));
    let mut rows = Vec::new();
    for (index, command) in candidates
        .iter()
        .skip(window_start)
        .take(MAX_PALETTE_ROWS)
        .enumerate()
    {
        let selected = window_start + index == app.palette.selected;
        let selected_style = crate::theme::palette().selected();
        let name_style = if selected {
            selected_style
        } else {
            Style::default()
        };
        let description_style = if selected {
            selected_style
        } else {
            notice_style()
        };
        rows.push(Line::from(vec![
            Span::styled(format!("  {:<12}", command.name), name_style),
            Span::styled(command.description, description_style),
        ]));
    }
    frame.render_widget(Paragraph::new(rows), area);
}

pub(super) fn wordmark_lines() -> Vec<Line<'static>> {
    (0..5)
        .map(|row| {
            let mut spans = Vec::new();
            for (index, (_, glyph)) in WORDMARK.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(
                    glyph[row].to_owned(),
                    Style::default().fg(WORDMARK_COLORS[index]),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

pub(super) fn draw_welcome(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.extend(wordmark_lines());
    lines.push(Line::styled("a quiet terminal coding agent", muted_style()));
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Ask Latch to inspect, change, or verify code. /help lists commands.",
        notice_style(),
    ));
    let top = area.height.saturating_sub(lines.len() as u16) / 3;
    let mut text = vec![Line::from(""); top as usize];
    text.append(&mut lines);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        area,
    );
}

pub(super) fn draw_footer(frame: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let workspace = if app.workspace.is_empty() {
        ".".to_owned()
    } else {
        app.workspace.clone()
    };
    let right = format!("latch v{}", env!("CARGO_PKG_VERSION"));
    let right_width = display_width(&right);
    let left = sidebar::fit(
        &workspace,
        width.saturating_sub(right_width).saturating_sub(1),
    );
    let left_width = display_width(&left);
    let line = if left_width + right_width < width {
        Line::from(vec![
            Span::styled(left, muted_style()),
            Span::raw(" ".repeat(width - left_width - right_width)),
            Span::styled(right, notice_style()),
        ])
    } else {
        Line::styled(sidebar::fit(&workspace, width), notice_style())
    };
    frame.render_widget(Paragraph::new(line), area);
}

pub(super) fn draw_composer_hints(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: ratatui::layout::Rect,
) {
    if area.height == 0 || area.width < 28 {
        return;
    }
    let width = area.width as usize;
    // Secondary shortcuts are abbreviated before the row is clipped.
    let hints: &[(&str, &str)] = if width < 64 {
        &[("ctrl+p", "commands")]
    } else {
        &[
            ("enter", if app.busy { "steer" } else { "send" }),
            ("ctrl+j", "newline"),
            ("ctrl+p", "commands"),
        ]
    };
    let left = hint_spans(hints);
    let left_width: usize = left.iter().map(|span| display_width(&span.content)).sum();
    let right = if !app.follow {
        "shift+pgup/pgdn scroll".to_owned()
    } else if !app.sidebar_visible_now() && app.last_width >= SIDEBAR_MIN_AUTO_WIDTH {
        "ctrl+b sidebar".to_owned()
    } else if app.detail {
        "detail view".to_owned()
    } else {
        String::new()
    };
    let right_width = display_width(&right);
    let mut spans = left;
    if !right.is_empty() && left_width + right_width + 2 <= width {
        spans.push(Span::raw(" ".repeat(width - left_width - right_width)));
        spans.push(Span::styled(right, notice_style()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Metadata row inside the composer: mode, model, branch on the left; status
/// and (when the sidebar is hidden) a compact working-set summary on the
/// right. Secondary detail is dropped before the status.
pub(super) fn composer_meta_line(app: &App, width: usize, sidebar_shown: bool) -> Line<'static> {
    let mode_style = match app.mode {
        Mode::Work => focused_accent().add_modifier(Modifier::BOLD),
        _ => muted_style().add_modifier(Modifier::BOLD),
    };
    let mode_text = app.mode.to_string();
    let mode_width = display_width(&mode_text);
    let mut fields: Vec<(String, Style)> = Vec::new();
    if !app.model.is_empty() {
        fields.push((app.model.clone(), muted_style()));
    }
    if !app.branch.is_empty() && app.branch != "-" {
        fields.push((app.branch.clone(), notice_style()));
    }
    // Safety and permissions are part of the session state, not decoration;
    // they drop before model/branch when the terminal is narrow.
    fields.push((app.safety.short().to_owned(), notice_style()));
    fields.push((app.permissions.short().to_owned(), notice_style()));
    if app.resumed {
        fields.push(("resumed".to_owned(), notice_style()));
    }
    let field_width = |count: usize| -> usize {
        fields
            .iter()
            .take(count)
            .map(|(text, _)| 3 + display_width(text))
            .sum()
    };
    let (status, status_style) = composer_status(app);
    let status_width = display_width(&status);

    // Longest left prefix that still leaves room for the status word, falling
    // back to a prefix without it, and finally to just the mode.
    let with_status = (0..=fields.len())
        .rev()
        .find(|count| mode_width + field_width(*count) + 2 + status_width <= width);
    let without_status = (0..=fields.len())
        .rev()
        .find(|count| mode_width + field_width(*count) <= width);
    let (field_count, include_status) = match (with_status, without_status) {
        (Some(count), _) => (count, true),
        (None, Some(count)) => (count, false),
        (None, None) => (0, false),
    };

    let mut spans: Vec<Span<'static>> = vec![Span::styled(mode_text, mode_style)];
    for (text, style) in fields.iter().take(field_count) {
        spans.push(Span::styled(" · ".to_owned(), notice_style()));
        spans.push(Span::styled(text.clone(), *style));
    }
    let left_width: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    if !include_status || left_width > width {
        return Line::from(spans);
    }

    // Prefer status + context + cost, then status + context, then status.
    let mut context_span = None;
    if !sidebar_shown && let Some(context) = app.sidebar.context() {
        context_span = Some(Span::styled(
            format!(
                "≈{}/{} tok",
                sidebar::format_tokens(context.total_tokens as u64),
                sidebar::format_tokens(context.window_tokens as u64)
            ),
            notice_style(),
        ));
    }
    let cost_span = app.sidebar.estimated_cost().map(|cost| {
        Span::styled(
            format!(
                "est. {}",
                sidebar::format_money(cost.amount, &cost.currency)
            ),
            notice_style(),
        )
    });
    let mut variants: Vec<Vec<Span<'static>>> = Vec::new();
    let mut full = Vec::new();
    if let Some(context) = &context_span {
        full.push(context.clone());
    }
    if let Some(cost) = &cost_span {
        if !full.is_empty() {
            full.push(Span::styled(" · ".to_owned(), notice_style()));
        }
        full.push(cost.clone());
    }
    if !full.is_empty() {
        full.push(Span::styled(" · ".to_owned(), notice_style()));
    }
    full.push(Span::styled(status.clone(), status_style));
    variants.push(full);
    let mut context_only = Vec::new();
    if let Some(context) = &context_span {
        context_only.push(context.clone());
        context_only.push(Span::styled(" · ".to_owned(), notice_style()));
    }
    context_only.push(Span::styled(status.clone(), status_style));
    if context_only.len() > 1 {
        variants.push(context_only);
    }
    variants.push(vec![Span::styled(status, status_style)]);
    for variant in variants {
        let right_width: usize = variant
            .iter()
            .map(|span| display_width(&span.content))
            .sum();
        if left_width + right_width + 2 <= width {
            spans.push(Span::raw(" ".repeat(width - left_width - right_width)));
            spans.extend(variant);
            return Line::from(spans);
        }
    }
    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_composer(
    frame: &mut ratatui::Frame<'_>,
    app: &mut App,
    area: ratatui::layout::Rect,
    hints_area: ratatui::layout::Rect,
    footer_area: ratatui::layout::Rect,
    chrome: &ComposerChrome,
    sidebar_shown: bool,
) {
    draw_composer_hints(frame, app, hints_area);
    draw_footer(frame, app, footer_area);
    if area.height == 0 || area.width == 0 {
        return;
    }
    let focused = app.permission.is_none() && app.diff_overlay.is_none();
    let border_style = if focused {
        focused_accent()
    } else {
        notice_style()
    };
    // A closed rounded frame: one border column and one padding column on each
    // side of the editor content.
    let framed = chrome.top > 0;
    let inner_width = area.width.saturating_sub(4).max(1) as usize;
    let body_height = chrome.body.max(1) as usize;
    app.last_input_width = inner_width;
    app.last_input_height = body_height;
    app.composer_body = area;
    app.input.reconcile_viewport(inner_width, body_height);

    let layout = app.input.layout(inner_width);
    let viewport = app.input.viewport();
    let (cursor_row, cursor_col) = app.input.cursor_visual(&layout);

    let plain = |content: Vec<Span<'static>>, inner: usize| -> Line<'static> {
        let used: usize = content
            .iter()
            .map(|span| display_width(&span.content))
            .sum();
        let mut spans = vec![Span::styled("│ ".to_owned(), border_style)];
        spans.extend(content);
        if used < inner {
            spans.push(Span::raw(" ".repeat(inner - used)));
        }
        if framed {
            spans.push(Span::styled(" │".to_owned(), border_style));
        }
        Line::from(spans)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for _ in 0..chrome.spacer {
        lines.push(Line::from(""));
    }
    if framed {
        lines.push(Line::styled(
            format!("╭{}╮", "─".repeat(area.width.saturating_sub(2) as usize)),
            border_style,
        ));
    }
    let body_start = lines.len();
    for offset in 0..body_height {
        let index = viewport + offset;
        let content = match layout.get(index) {
            None => Vec::new(),
            Some(_) if app.input.is_empty() && index == 0 => {
                vec![Span::styled("Ask Latch…".to_owned(), notice_style())]
            }
            Some(row) => {
                let text = sidebar::fit(&app.input.row_text(row), inner_width);
                let above = index == viewport && viewport > 0;
                let below = index + 1 == layout.len() && viewport + body_height < layout.len();
                let indicator = match (above, below) {
                    (true, true) => Some("↕"),
                    (true, false) => Some("↑"),
                    (false, true) => Some("↓"),
                    _ => None,
                };
                let text_width = display_width(&text);
                let mut content = vec![Span::raw(text)];
                if let Some(symbol) = indicator
                    && text_width + 2 <= inner_width
                {
                    content.push(Span::raw(" ".repeat(inner_width - text_width - 1)));
                    content.push(Span::styled(symbol.to_owned(), notice_style()));
                }
                content
            }
        };
        lines.push(plain(content, inner_width));
    }
    for _ in 0..chrome.gap {
        lines.push(plain(Vec::new(), inner_width));
    }
    if chrome.meta > 0 {
        let meta = composer_meta_line(app, inner_width, sidebar_shown);
        lines.push(plain(meta.spans, inner_width));
    }
    if chrome.rule > 0 {
        let (left, right) = if framed { ("╰", "╯") } else { ("╰", "") };
        let mut rule = format!(
            "{left}{}",
            "─".repeat(area.width.saturating_sub(2) as usize)
        );
        rule.push_str(right);
        lines.push(Line::styled(rule, border_style));
    }
    frame.render_widget(Paragraph::new(lines), area);

    // The terminal cursor is only placed while it is inside the visible body;
    // a viewport scrolled away hides it rather than pinning it to an edge.
    if focused && cursor_row >= viewport && cursor_row < viewport + body_height {
        let row = body_start + (cursor_row - viewport);
        // Frame column + padding column, then the cursor cell. It is clamped to
        // the right padding so it never covers the border.
        let col = (2 + cursor_col).min(area.width.saturating_sub(2) as usize) as u16;
        let position = (area.x + col, area.y + row as u16);
        frame.set_cursor_position(position);
        app.last_cursor = Some(position);
    }
}

/// Full-width diff inspector with its own scrolling and a raw toggle.
pub(super) fn draw_diff_overlay(
    frame: &mut ratatui::Frame<'_>,
    app: &mut App,
    area: ratatui::layout::Rect,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let (title, body_lines, parsed) = {
        let document = app
            .diff_overlay
            .as_ref()
            .expect("draw_diff_overlay requires an open document");
        let additions = document.added_lines();
        let deletions = document.removed_lines();
        let palette = crate::theme::palette();
        let mut title = vec![
            Span::styled(" diff ", palette.accent()),
            Span::raw(" "),
            Span::styled(
                format!(
                    "{} file{}",
                    document.files.len(),
                    if document.files.len() == 1 { "" } else { "s" }
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("+{additions}"),
                if additions > 0 {
                    palette.success()
                } else {
                    notice_style()
                },
            ),
            Span::raw(" "),
            Span::styled(
                format!("−{deletions}"),
                if deletions > 0 {
                    palette.failure()
                } else {
                    notice_style()
                },
            ),
        ];
        if !document.parsed && !document.raw.trim().is_empty() {
            title.push(Span::styled("  raw (unparsed)", notice_style()));
        }
        let body = if app.diff_raw {
            crate::diff::raw_diff_lines(document)
        } else {
            crate::diff::diff_lines(document)
        };
        (Line::from(title), body, document.parsed)
    };
    frame.render_widget(Paragraph::new(title), rows[0]);
    let body = Paragraph::new(body_lines).wrap(Wrap { trim: false });
    let body_rows = body.line_count(rows[1].width);
    app.diff_viewport_rows = rows[1].height as usize;
    app.diff_max_scroll = body_rows.saturating_sub(rows[1].height as usize);
    app.diff_scroll = app.diff_scroll.min(app.diff_max_scroll);
    let offset = app.diff_scroll.min(u16::MAX as usize) as u16;
    frame.render_widget(body.scroll((offset, 0)), rows[1]);
    let hint = format!(
        " ↑/↓ PgUp/PgDn Home/End scroll · Ctrl+T {} · Esc close{}",
        if app.diff_raw { "semantic" } else { "raw" },
        if parsed {
            ""
        } else {
            " · unparsed input shown raw"
        }
    );
    frame.render_widget(Paragraph::new(Line::styled(hint, notice_style())), rows[2]);
}

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.last_width = area.width;
    app.last_cursor = None;
    let palette_active = app.palette.active(&app.input);
    let candidates = if palette_active {
        filter_commands(app.input.first_line())
    } else {
        Vec::new()
    };
    app.palette.clamp(candidates.len());

    let overlay_open = app.diff_overlay.is_some();
    let sidebar_shown = sidebar_visible(area.width, app.sidebar_override) && !overlay_open;
    let sidebar_cols = sidebar_width(area.width, sidebar_shown);

    let composer_inner = area.width.saturating_sub(4).max(1) as usize;
    let content_rows = app.input.total_visual_rows(composer_inner);
    let palette_rows = if palette_active {
        candidates.len().min(MAX_PALETTE_ROWS) as u16
    } else {
        0
    };
    let selector_rows = if app.selector.is_some() { 4 } else { 0 };
    let chrome = ComposerChrome::responsive(area.height, content_rows);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(selector_rows),
            Constraint::Length(palette_rows),
            Constraint::Length(
                chrome.spacer + chrome.top + chrome.body + chrome.gap + chrome.meta + chrome.rule,
            ),
            Constraint::Length(chrome.hints),
            Constraint::Length(chrome.footer),
        ])
        .split(area);
    let transcript_area = chunks[0];
    let selector_area = chunks[1];
    let palette_area = chunks[2];
    let composer_area = chunks[3];
    let hints_area = chunks[4];
    let footer_area = chunks[5];

    if overlay_open {
        draw_diff_overlay(frame, app, transcript_area);
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(20), Constraint::Length(sidebar_cols)])
            .split(transcript_area);
        let viewport = panes[0];
        if app.presentation.cells().is_empty() && app.streaming.is_none() {
            app.sync_viewport(0, viewport.height as usize);
            draw_welcome(frame, viewport);
        } else {
            let content_rows = semantic_visual_height(
                app.presentation.cells(),
                app.streaming.as_deref(),
                app.detail,
                viewport.width,
            );
            app.sync_viewport(content_rows, viewport.height as usize);
            let offset = app.scroll.min(u16::MAX as usize) as u16;
            let paragraph = Paragraph::new(transcript_lines(
                app.presentation.cells(),
                app.streaming.as_deref(),
                app.detail,
                viewport.width as usize,
                true,
            ))
            .wrap(Wrap { trim: false })
            .scroll((offset, 0));
            frame.render_widget(paragraph, viewport);
        }
        if sidebar_cols > 0 && panes.len() > 1 {
            let sidebar_area = panes[1];
            let inner_width = sidebar_area.width.saturating_sub(2);
            let lines = app.sidebar.render_lines(inner_width, sidebar_area.height);
            let sidebar = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(notice_style()),
            );
            frame.render_widget(sidebar, sidebar_area);
        }
    }
    draw_permission_modal(frame, app, transcript_area);
    draw_policy_selector(frame, app, selector_area);
    draw_palette(frame, app, palette_area, &candidates);
    draw_composer(
        frame,
        app,
        composer_area,
        hints_area,
        footer_area,
        &chrome,
        sidebar_shown,
    );
}

/// Five-row pixel letterforms for the startup wordmark. Every letter occupies
/// the same six columns so the rows align without per-letter padding; the
/// extra width keeps the glyphs from looking narrow against terminal cells,
/// which are roughly twice as tall as they are wide.
pub(super) const WORDMARK: [(&str, [&str; 5]); 5] = [
    ("L", ["███   ", "███   ", "███   ", "███   ", "██████"]),
    ("A", [" ████ ", "██  ██", "██████", "██  ██", "██  ██"]),
    ("T", ["██████", "  ██  ", "  ██  ", "  ██  ", "  ██  "]),
    ("C", [" █████", "██    ", "██    ", "██    ", " █████"]),
    ("H", ["██  ██", "██  ██", "██████", "██  ██", "██  ██"]),
];

/// One muted tone per letter; all distinct, none neon.
pub(super) const WORDMARK_COLORS: [Color; 5] = [
    Color::Rgb(186, 142, 120),
    Color::Rgb(158, 176, 134),
    Color::Rgb(134, 160, 190),
    Color::Rgb(184, 164, 126),
    Color::Rgb(172, 146, 178),
];

/// Centered approval prompt. Human approval is the only path that lets an
/// `Ask` policy decision execute; the model never controls this surface.
pub(super) fn draw_permission_modal(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    area: ratatui::layout::Rect,
) {
    let Some(prompt) = &app.permission else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(24, 84).min(area.width);
    // Six content rows plus the top and bottom border.
    let height = 8.min(area.height).max(3);
    let rect = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let inner = width.saturating_sub(2) as usize;
    let lines = vec![
        Line::styled(
            "Permission required",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![
            Span::styled(format!("{} ", prompt.tool), Style::default().bold()),
            Span::raw(crate::sidebar::fit(
                &prompt.arguments,
                inner.saturating_sub(prompt.tool.len() + 1),
            )),
        ]),
        Line::styled(crate::sidebar::fit(&prompt.reason, inner), notice_style()),
        Line::styled(
            crate::sidebar::fit(
                &format!("capability: {}", prompt.capabilities.join(", ")),
                inner,
            ),
            Style::default().fg(Color::Yellow),
        ),
        Line::from(""),
        Line::styled(
            "[y] approve   [n] deny   [Ctrl+C] cancel",
            Style::default().fg(Color::Cyan),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        rect,
    );
}
