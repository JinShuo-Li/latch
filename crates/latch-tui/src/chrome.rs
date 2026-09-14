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

pub(super) fn focused_accent() -> Style {
    crate::theme::palette().accent()
}

/// Readable secondary text for composer chrome (metadata, footer). Theme-aware
/// so it stays legible on both dark and light terminals.
pub(super) fn muted_style() -> Style {
    crate::theme::palette().muted()
}

/// Idle composer status. Every non-idle state is reported by the transient
/// status row directly above the composer, so the meta line never duplicates
/// it.
pub(super) fn composer_status(app: &App) -> Option<(String, Style)> {
    if app.permission.is_some() || app.busy || app.interrupted {
        None
    } else {
        Some(("ready".into(), muted_style()))
    }
}

/// Compact transient activity row shown directly above the composer. Derived
/// only from authoritative state: running presentation cells, streaming,
/// pending approval, and root-visible child agents. Completed work stays in
/// the transcript; transient work stays here.
pub(super) fn active_status_line(app: &App) -> Option<Line<'static>> {
    let palette = crate::theme::palette();
    let mut label: Option<String> = None;
    let mut detail: Option<String> = None;
    if app.permission.is_some() {
        label = Some("Waiting for approval".into());
    } else if let Some(cell) = app
        .presentation
        .cells()
        .iter()
        .rev()
        .find(|cell| cell_is_running(cell))
    {
        let (text, subject) = running_cell_status(cell);
        label = Some(text);
        detail = subject;
    } else if app.streaming.is_some() {
        label = Some("Writing response".into());
    } else if app.busy {
        label = Some("Working".into());
    } else if app.interrupted {
        label = Some("Interrupted".into());
    }

    // Child agents run in their own sessions and can outlive the root turn.
    let children = app.sidebar.subagents().active();
    if !children.is_empty() {
        let child_text = if children.len() == 1 {
            format!("child `{}` {}", children[0].task_name, children[0].label())
        } else {
            format!("{} child agents running", children.len())
        };
        detail = Some(match detail.filter(|detail| !detail.is_empty()) {
            Some(existing) => format!("{existing} · {child_text}"),
            None => child_text,
        });
        if label.is_none() {
            label = Some(if children.len() == 1 {
                "Child agent running".into()
            } else {
                "Child agents running".into()
            });
        }
    }

    let label = label?;
    // Waiting states are attention (yellow where the theme allows); ordinary
    // work stays quiet with an accent marker.
    let attention = matches!(label.as_str(), "Waiting for approval" | "Interrupted");
    let (marker_style, label_style) = if attention {
        (palette.attention(), palette.attention())
    } else {
        (
            palette.accent(),
            Style::default().add_modifier(Modifier::BOLD),
        )
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("• ", marker_style),
        Span::styled(label, label_style),
    ];
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        spans.push(Span::styled(" · ", notice_style()));
        spans.push(Span::styled(detail, notice_style()));
    }
    Some(Line::from(spans))
}

fn cell_is_running(cell: &Cell) -> bool {
    match cell {
        Cell::Exploration { operations } => operations
            .iter()
            .any(|operation| operation.status == CellStatus::Running),
        Cell::Command { status, .. }
        | Cell::Validation { status, .. }
        | Cell::Diff { status, .. }
        | Cell::AgentTask { status, .. } => *status == CellStatus::Running,
        Cell::Patch { files } => files.iter().any(|file| file.status == CellStatus::Running),
        Cell::User { .. }
        | Cell::Assistant { .. }
        | Cell::AgentReport { .. }
        | Cell::Notice { .. }
        | Cell::Error { .. } => false,
    }
}

fn running_cell_status(cell: &Cell) -> (String, Option<String>) {
    match cell {
        Cell::Exploration { .. } => ("Exploring".into(), None),
        Cell::Command { command, .. } => (
            command_activity(command).into(),
            Some(sidebar::fit(command, 60)),
        ),
        Cell::Validation { requirement, .. } => (
            "Validating".into(),
            (!requirement.is_empty()).then(|| sidebar::fit(requirement, 60)),
        ),
        Cell::Patch { .. } => ("Editing".into(), None),
        Cell::Diff { .. } => ("Reading workspace diff".into(), None),
        Cell::AgentTask { .. } => ("Waiting for child agents".into(), None),
        Cell::User { .. }
        | Cell::Assistant { .. }
        | Cell::AgentReport { .. }
        | Cell::Notice { .. }
        | Cell::Error { .. } => ("Working".into(), None),
    }
}

/// Semantic label for a running command, keyed only on the real command
/// string; unrecognized commands stay generic.
fn command_activity(command: &str) -> &'static str {
    let lower = command.to_ascii_lowercase();
    if lower.contains("cargo test")
        || lower.contains("cargo nextest")
        || lower.contains("pytest")
        || lower.contains("npm test")
    {
        "Running tests"
    } else if lower.contains("cargo build")
        || lower.contains("cargo check")
        || lower.contains("cargo clippy")
        || lower.contains("cargo fmt")
        || lower.contains("cargo run")
    {
        "Building"
    } else if lower.starts_with("rg ") || lower.contains("grep") || lower.contains("find ") {
        "Searching"
    } else if lower.starts_with("git ") {
        "Inspecting git"
    } else {
        "Running command"
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

/// A restrained selector and approval surface, rendered directly above the
/// composer in the same visual language: a neutral full-width surface, a clear
/// title, concise context, numbered options, and keyboard hints. Long approval
/// arguments can be inspected in full with Ctrl+O instead of being silently
/// truncated.
pub(super) fn action_surface_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    if let Some(prompt) = &app.permission {
        approval_surface_lines(prompt, width)
    } else if let Some(capture) = &app.capture {
        capture_surface_lines(capture, width)
    } else if let Some(flow) = &app.setup {
        setup_surface_lines(flow, width)
    } else if let Some(selector) = &app.profile_selector {
        profile_surface_lines(selector, width)
    } else if let Some(selector) = app.selector {
        selector_surface_lines(app, selector, width)
    } else {
        Vec::new()
    }
}

/// Render one generic choice surface with an optional review block.
fn choice_surface_lines(
    title: String,
    rows: &[ChoiceRow],
    hint: &str,
    review: &[(String, String)],
    width: usize,
) -> Vec<Line<'static>> {
    let palette = crate::theme::palette();
    let mut lines = vec![surface_blank(width)];
    lines.push(surface_text(title, palette.attention(), width, 2));
    lines.push(surface_blank(width));
    for row in rows {
        let marker = if row.selected { "› " } else { "  " };
        let label_style = if row.selected {
            palette.selected()
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(marker.to_owned(), label_style),
            Span::styled(row.label.clone(), label_style),
        ];
        if row.current {
            spans.push(Span::styled("  (current)".to_owned(), notice_style()));
        }
        if !row.description.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(row.description.clone(), notice_style()));
        }
        lines.push(surface_row(spans, width));
    }
    if !review.is_empty() {
        lines.push(surface_blank(width));
        for (key, value) in review {
            lines.push(surface_row(
                vec![
                    Span::raw("  "),
                    Span::styled(format!("{key:<11}"), notice_style()),
                    Span::styled(value.clone(), Style::default()),
                ],
                width,
            ));
        }
    }
    lines.push(surface_blank(width));
    lines.push(surface_text(hint.to_owned(), notice_style(), width, 2));
    lines
}

fn profile_surface_lines(selector: &ProfileSelector, width: usize) -> Vec<Line<'static>> {
    choice_surface_lines(
        selector.title(),
        &selector.rows(),
        selector.hint(),
        &[],
        width,
    )
}

fn setup_surface_lines(flow: &SetupFlow, width: usize) -> Vec<Line<'static>> {
    let review = if matches!(flow.step(), SetupStep::Review | SetupStep::RemoveConfirm) {
        flow.review_lines()
    } else {
        Vec::new()
    };
    choice_surface_lines(flow.title(), &flow.rows(), flow.hint(), &review, width)
}

fn capture_surface_lines(capture: &CaptureState, width: usize) -> Vec<Line<'static>> {
    let palette = crate::theme::palette();
    let mut lines = vec![surface_blank(width)];
    lines.push(surface_text(
        format!("Setup · {}", capture.spec.label),
        palette.attention(),
        width,
        2,
    ));
    lines.push(surface_blank(width));
    let shown = capture.display_value();
    let value_style = if shown.is_empty() {
        notice_style().add_modifier(Modifier::ITALIC)
    } else {
        Style::default()
    };
    let shown = if shown.is_empty() {
        "(type a value)".to_owned()
    } else {
        shown
    };
    for line in wrap_surface_text(&shown, width.saturating_sub(4).max(1), 2) {
        lines.push(surface_text(line, value_style, width, 2));
    }
    lines.push(surface_blank(width));
    lines.push(surface_text(
        "enter confirm · esc cancel".to_owned(),
        notice_style(),
        width,
        2,
    ));
    lines
}

/// One padded full-width row on a neutral action surface.
fn surface_row(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let style = crate::theme::palette().surface();
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let mut spans = spans;
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    Line::from(spans).style(style)
}

fn surface_blank(width: usize) -> Line<'static> {
    surface_row(Vec::new(), width)
}

fn surface_text(text: String, style: Style, width: usize, indent: usize) -> Line<'static> {
    surface_row(
        vec![Span::raw(" ".repeat(indent)), Span::styled(text, style)],
        width,
    )
}

fn approval_surface_lines(prompt: &PermissionPrompt, width: usize) -> Vec<Line<'static>> {
    let palette = crate::theme::palette();
    let inner = width.saturating_sub(4).max(1);
    let mut lines = vec![surface_blank(width)];
    lines.push(surface_text(
        "Approval needed".into(),
        palette.attention(),
        width,
        2,
    ));
    lines.push(surface_blank(width));
    lines.push(surface_text(
        prompt.tool.clone(),
        Style::default().bold(),
        width,
        2,
    ));

    let (preview, truncated) = approval_preview_lines(&prompt.arguments, inner, 3);
    for line in preview {
        lines.push(surface_text(line, Style::default(), width, 2));
    }
    if truncated {
        lines.push(surface_text(
            "… full request: Ctrl+O".into(),
            notice_style(),
            width,
            2,
        ));
    }
    if !prompt.reason.trim().is_empty() {
        lines.push(surface_blank(width));
        for line in wrap_surface_text(&prompt.reason, inner, 2) {
            lines.push(surface_text(
                line,
                notice_style().add_modifier(Modifier::ITALIC),
                width,
                2,
            ));
        }
    }
    if !prompt.capabilities.is_empty() {
        let capability = format!("capability: {}", prompt.capabilities.join(", "));
        for line in wrap_surface_text(&capability, inner, 2) {
            lines.push(surface_text(line, palette.attention(), width, 2));
        }
    }
    lines.push(surface_blank(width));
    for (index, (label, description)) in [
        ("Approve", "Run this operation once."),
        ("Deny", "Skip it and tell the model what to do differently."),
    ]
    .iter()
    .enumerate()
    {
        let selected = index == prompt.selected;
        let marker = if selected { "› " } else { "  " };
        let label_style = if selected {
            palette.selected()
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        lines.push(surface_row(
            vec![
                Span::raw("  "),
                Span::styled(marker.to_owned(), label_style),
                Span::styled((*label).to_owned(), label_style),
                Span::raw("  "),
                Span::styled((*description).to_owned(), notice_style()),
            ],
            width,
        ));
    }
    lines.push(surface_blank(width));
    lines.push(surface_text(
        "enter confirm · y approve · n deny · ctrl+o full request · esc deny".into(),
        notice_style(),
        width,
        2,
    ));
    lines.push(surface_blank(width));
    lines
}

/// Bounded, readable rendering of the raw approval arguments. A single
/// `command` field is unwrapped to `$ command`; anything else stays JSON.
fn approval_preview_lines(arguments: &str, width: usize, max_rows: usize) -> (Vec<String>, bool) {
    let value: Option<serde_json::Value> = serde_json::from_str(arguments).ok();
    let command = value
        .as_ref()
        .and_then(|value| value.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let source = command.map_or_else(|| arguments.to_owned(), |command| format!("$ {command}"));
    let mut rows: Vec<String> = Vec::new();
    let mut truncated = false;
    for logical in source.lines() {
        for row in wrap_surface_text(logical, width, 0) {
            if rows.len() == max_rows {
                truncated = true;
                break;
            }
            rows.push(row);
        }
        if truncated {
            break;
        }
    }
    (rows, truncated)
}

fn wrap_surface_text(text: &str, width: usize, indent: usize) -> Vec<String> {
    let width = width.saturating_sub(indent).max(1);
    let chars: Vec<char> = text.chars().collect();
    let points = composer::wrap_points(text, width);
    let mut rows = Vec::new();
    for (index, start) in points.iter().enumerate() {
        let end = points.get(index + 1).copied().unwrap_or(chars.len());
        rows.push(chars[*start..end].iter().collect());
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn selector_surface_lines(app: &App, selector: PolicySelector, width: usize) -> Vec<Line<'static>> {
    let palette = crate::theme::palette();
    let mut lines = vec![surface_blank(width)];
    lines.push(surface_text(
        selector.kind.title().into(),
        Style::default().add_modifier(Modifier::BOLD),
        width,
        2,
    ));
    lines.push(surface_blank(width));
    let disabled = selector.kind.disabled_reason(app);
    let current = selector.kind.current(app);
    for (index, option) in selector.kind.options().iter().enumerate() {
        let selected = index == selector.selected;
        let marker = if selected { "› " } else { "  " };
        let label_style = if disabled.is_some() {
            notice_style()
        } else if selected {
            palette.selected()
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let mut label = option.label.to_owned();
        if index == current {
            label.push_str(" (current)");
        }
        if let Some(reason) = disabled {
            label.push_str(&format!(" (unavailable: {reason})"));
        }
        lines.push(surface_row(
            vec![
                Span::raw("  "),
                Span::styled(marker.to_owned(), label_style),
                Span::styled(label, label_style),
                Span::raw("  "),
                Span::styled(option.description.to_owned(), notice_style()),
            ],
            width,
        ));
    }
    lines.push(surface_blank(width));
    if let Some(reason) = disabled {
        lines.push(surface_text(
            format!("finish or cancel the {reason} before changing policy"),
            notice_style(),
            width,
            2,
        ));
    } else {
        lines.push(surface_text(
            "↑↓ select · enter apply · esc cancel".into(),
            notice_style(),
            width,
            2,
        ));
    }
    lines.push(surface_blank(width));
    lines
}

/// Full-request inspector for an approval (Ctrl+O). This is deliberately a
/// separate full-width view: the human must be able to read everything being
/// approved, not only the bounded preview on the action surface.
pub(super) fn draw_request_overlay(
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
    let Some(prompt) = app.permission.as_mut() else {
        return;
    };
    let palette = crate::theme::palette();
    let title = Line::from(vec![
        Span::styled(" full request ", palette.accent()),
        Span::raw(" "),
        Span::styled(prompt.tool.clone(), Style::default().bold()),
        Span::raw("  "),
        Span::styled(
            if prompt.capabilities.is_empty() {
                String::new()
            } else {
                format!("capability: {}", prompt.capabilities.join(", "))
            },
            palette.attention(),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), rows[0]);
    let body_lines: Vec<Line<'static>> = prompt
        .arguments
        .lines()
        .map(|line| Line::raw(line.to_owned()))
        .collect();
    let body = Paragraph::new(body_lines).wrap(Wrap { trim: false });
    let body_rows = body.line_count(rows[1].width);
    let max_scroll = body_rows.saturating_sub(rows[1].height as usize);
    prompt.scroll = prompt.scroll.min(max_scroll);
    let offset = prompt.scroll.min(u16::MAX as usize) as u16;
    frame.render_widget(body.scroll((offset, 0)), rows[1]);
    frame.render_widget(
        Paragraph::new(Line::styled(
            " ↑/↓ PgUp/PgDn Home/End scroll · Ctrl+O or Esc close · approval stays pending",
            notice_style(),
        )),
        rows[2],
    );
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
    // The approval surface owns the keyboard and carries its own hints; a
    // contradictory `enter send` row directly beneath it would mislead.
    if app.permission.is_some() {
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
        // Model and effort are always adjacent so the active inference profile
        // is visible at a glance, never on a second row.
        fields.push((
            format!("{}/{}", app.model, app.effort.short()),
            muted_style(),
        ));
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
    let status = composer_status(app);
    let status_width = status.as_ref().map_or(0, |(text, _)| display_width(text));

    // Longest left prefix that still leaves room for the status word, falling
    // back to a prefix without it, and finally to just the mode.
    let with_status = status.as_ref().and_then(|_| {
        (0..=fields.len())
            .rev()
            .find(|count| mode_width + field_width(*count) + 2 + status_width <= width)
    });
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
    let Some((status, status_style)) = status.filter(|_| include_status && left_width <= width)
    else {
        return Line::from(spans);
    };

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
    let palette = crate::theme::palette();
    let focused = app.permission.is_none() && app.diff_overlay.is_none();
    let surface = palette.surface();
    let prompt_style = if focused {
        palette.accent()
    } else {
        notice_style()
    };
    // The composer is a neutral band, not a box: a two-column prompt gutter and
    // a one-column right margin keep text aligned with user-message rows.
    const GUTTER: usize = 2;
    let inner_width = (area.width as usize).saturating_sub(GUTTER + 1).max(1);
    let body_height = chrome.body.max(1) as usize;
    app.last_input_width = inner_width;
    app.last_input_height = body_height;
    app.composer_body = area;
    app.input.reconcile_viewport(inner_width, body_height);

    let layout = app.input.layout(inner_width);
    let viewport = app.input.viewport();
    let (cursor_row, cursor_col) = app.input.cursor_visual(&layout);

    let band = |content: Vec<Span<'static>>, indent: usize| -> Line<'static> {
        let used: usize = indent
            + content
                .iter()
                .map(|span| display_width(&span.content))
                .sum::<usize>();
        let mut spans = vec![Span::raw(" ".repeat(indent))];
        spans.extend(content);
        if used < area.width as usize {
            spans.push(Span::raw(" ".repeat(area.width as usize - used)));
        }
        Line::from(spans).style(surface)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    for _ in 0..chrome.spacer {
        lines.push(Line::from(""));
    }
    for _ in 0..chrome.top {
        lines.push(band(Vec::new(), 0));
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
        let prompt = if index == 0 { "› " } else { "  " };
        let mut spans = vec![Span::styled(
            prompt.to_owned(),
            if index == 0 {
                prompt_style
            } else {
                Style::default()
            },
        )];
        spans.extend(content);
        lines.push(band(spans, 0));
    }
    for _ in 0..chrome.gap {
        lines.push(band(Vec::new(), 0));
    }
    if chrome.meta > 0 {
        let meta = composer_meta_line(
            app,
            (area.width as usize).saturating_sub(GUTTER),
            sidebar_shown,
        );
        lines.push(band(meta.spans, GUTTER));
    }
    for _ in 0..chrome.rule {
        lines.push(band(Vec::new(), 0));
    }
    frame.render_widget(Paragraph::new(lines), area);

    // The terminal cursor is only placed while it is inside the visible body;
    // a viewport scrolled away hides it rather than pinning it to an edge.
    if focused && cursor_row >= viewport && cursor_row < viewport + body_height {
        let row = body_start + (cursor_row - viewport);
        // The prompt gutter precedes the first text cell; clamp to the right
        // margin so it never runs past the band.
        let col = (GUTTER + cursor_col).min(area.width.saturating_sub(2) as usize) as u16;
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
    let request_open = app
        .permission
        .as_ref()
        .is_some_and(|prompt| prompt.expanded);
    let sidebar_shown =
        sidebar_visible(area.width, app.sidebar_override) && !overlay_open && !request_open;
    let sidebar_cols = sidebar_width(area.width, sidebar_shown);

    let composer_inner = area.width.saturating_sub(3).max(1) as usize;
    let content_rows = app.input.total_visual_rows(composer_inner);
    let palette_rows = if palette_active {
        candidates.len().min(MAX_PALETTE_ROWS) as u16
    } else {
        0
    };
    // Action surfaces render at their natural height, bounded so the transcript
    // above them always keeps most of the screen.
    let action_lines = action_surface_lines(app, area.width as usize);
    let action_rows = (action_lines.len() as u16).min(area.height / 2);
    let status_line = active_status_line(app);
    let status_rows = u16::from(status_line.is_some());
    // Pending image attachments sit directly above the composer, in the same
    // restrained metadata language as the transcript summary.
    let attachment_line = app.attachment_summary();
    let attachment_rows = u16::from(attachment_line.is_some());
    let chrome = ComposerChrome::responsive(area.height, content_rows);
    // The approval surface carries its own hints; drop the composer hint row
    // so no contradictory shortcut row sits underneath it.
    let hints_rows = if app.permission.is_some() {
        0
    } else {
        chrome.hints
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(action_rows),
            Constraint::Length(palette_rows),
            Constraint::Length(status_rows),
            Constraint::Length(attachment_rows),
            Constraint::Length(
                chrome.spacer + chrome.top + chrome.body + chrome.gap + chrome.meta + chrome.rule,
            ),
            Constraint::Length(hints_rows),
            Constraint::Length(chrome.footer),
        ])
        .split(area);
    let transcript_area = chunks[0];
    let action_area = chunks[1];
    let palette_area = chunks[2];
    let status_area = chunks[3];
    let attachment_area = chunks[4];
    let composer_area = chunks[5];
    let hints_area = chunks[6];
    let footer_area = chunks[7];

    if request_open {
        draw_request_overlay(frame, app, transcript_area);
    } else if overlay_open {
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
    if !action_lines.is_empty() {
        frame.render_widget(Paragraph::new(action_lines), action_area);
    }
    if let Some(status) = status_line {
        frame.render_widget(Paragraph::new(status), status_area);
    }
    if let Some(summary) = attachment_line {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" attachments: {summary}"),
                notice_style(),
            )),
            attachment_area,
        );
    }
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
