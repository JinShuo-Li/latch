//! Reusable startup and in-application session picker.

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::{path::Path, sync::Arc};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionItem {
    pub id: Uuid,
    pub workspace: String,
    pub updated_at: DateTime<Utc>,
    pub mode: String,
    pub model: String,
    pub prompt: String,
    pub event_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewLine {
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerSelection {
    Resume(Uuid),
    StartFresh,
    Exit,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Workspace,
    All,
}

#[derive(Debug)]
struct Picker {
    sessions: Vec<SessionItem>,
    workspace: String,
    scope: Scope,
    query: String,
    selected: usize,
    preview: Vec<SessionPreviewLine>,
    previewed: Option<Uuid>,
}

impl Picker {
    fn new(sessions: Vec<SessionItem>, workspace: &Path) -> Self {
        Self {
            sessions,
            workspace: workspace.to_string_lossy().into_owned(),
            scope: Scope::Workspace,
            query: String::new(),
            selected: 0,
            preview: Vec::new(),
            previewed: None,
        }
    }

    fn filtered(&self) -> Vec<&SessionItem> {
        let query = self.query.to_ascii_lowercase();
        self.sessions
            .iter()
            .filter(|session| {
                (self.scope == Scope::All || session.workspace == self.workspace)
                    && (query.is_empty()
                        || format!(
                            "{} {} {} {} {}",
                            session.id,
                            session.workspace,
                            session.mode,
                            session.model,
                            session.prompt
                        )
                        .to_ascii_lowercase()
                        .contains(&query))
            })
            .collect()
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
    }

    fn selected_id(&self) -> Option<Uuid> {
        self.filtered().get(self.selected).map(|session| session.id)
    }

    fn move_by(&mut self, amount: isize) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(amount).min(len - 1);
        self.previewed = None;
    }
}

/// Opens the session picker. Preview data is requested only for the selected
/// row, and cached until selection changes.
pub async fn run_session_picker(
    sessions: Vec<SessionItem>,
    workspace: &Path,
    load_preview: Arc<dyn Fn(Uuid) -> Vec<SessionPreviewLine> + Send + Sync>,
) -> Result<PickerSelection> {
    let mut guard = super::Guard::enter()?;
    let mut picker = Picker::new(sessions, workspace);
    let mut events = EventStream::new();
    loop {
        if let Some(id) = picker.selected_id()
            && picker.previewed != Some(id)
        {
            picker.preview = load_preview(id);
            picker.previewed = Some(id);
        }
        guard.terminal.draw(|frame| draw(frame, &picker))?;
        let Some(event) = events.next().await.transpose()? else {
            return Ok(PickerSelection::Cancel);
        };
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(PickerSelection::Cancel),
            KeyCode::Enter => {
                return Ok(picker
                    .selected_id()
                    .map_or(PickerSelection::StartFresh, PickerSelection::Resume));
            }
            KeyCode::Up => picker.move_by(-1),
            KeyCode::Down => picker.move_by(1),
            KeyCode::PageUp => picker.move_by(-8),
            KeyCode::PageDown => picker.move_by(8),
            KeyCode::Tab => {
                picker.scope = if picker.scope == Scope::Workspace {
                    Scope::All
                } else {
                    Scope::Workspace
                };
                picker.selected = 0;
                picker.previewed = None;
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
                picker.previewed = None;
            }
            KeyCode::Char('n')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                picker.move_by(1)
            }
            KeyCode::Char('p')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                picker.move_by(-1)
            }
            KeyCode::Char('f')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return Ok(PickerSelection::StartFresh);
            }
            KeyCode::Char('q')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                return Ok(PickerSelection::Exit);
            }
            KeyCode::Char('/') if picker.query.is_empty() => {}
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                picker.query.push(ch);
                picker.selected = 0;
                picker.previewed = None;
            }
            _ => {}
        }
        picker.clamp();
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, picker: &Picker) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(area.height.min(9)),
            Constraint::Length(2),
        ])
        .split(area);
    let scope = if picker.scope == Scope::Workspace {
        "current workspace"
    } else {
        "all sessions"
    };
    let query = if picker.query.is_empty() {
        "type to search"
    } else {
        &picker.query
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                " Resume a saved session",
                Style::default().bold(),
            )),
            Line::from(vec![
                Span::styled(format!(" {scope}"), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("  ·  {query}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        ]),
        sections[0],
    );
    let filtered = picker.filtered();
    let visible_rows = (sections[1].height as usize / 3).max(1);
    let start = picker
        .selected
        .saturating_sub(visible_rows / 2)
        .min(filtered.len().saturating_sub(visible_rows));
    let mut rows = Vec::new();
    if filtered.is_empty() {
        rows.push(Line::styled(
            " No matching sessions",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for (offset, session) in filtered.iter().skip(start).take(visible_rows).enumerate() {
        let selected = start + offset == picker.selected;
        let marker = if selected { "›" } else { " " };
        let style = if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        rows.push(Line::from(vec![Span::styled(
            format!(
                "{marker} {:>10}  {:<5}  {}",
                relative_time(session.updated_at),
                session.mode,
                session.model
            ),
            style,
        )]));
        rows.push(Line::styled(
            format!(
                "   {}  #{} · {} events",
                truncate_width(&session.workspace, area.width.saturating_sub(20) as usize),
                &session.id.to_string()[..8],
                session.event_count
            ),
            Style::default().fg(Color::DarkGray),
        ));
        rows.push(Line::styled(
            format!(
                "   “{}”",
                truncate_width(&session.prompt, area.width.saturating_sub(6) as usize)
            ),
            if selected {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    frame.render_widget(Paragraph::new(rows), sections[1]);
    let preview = picker
        .preview
        .iter()
        .map(|line| {
            Line::from(vec![
                Span::styled(
                    format!("{}: ", line.speaker),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(truncate_width(
                    &line.text,
                    area.width.saturating_sub(12) as usize,
                )),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(preview).block(
            Block::default()
                .borders(Borders::TOP)
                .title(" Preview ")
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        sections[2],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            " ↑↓ select  Enter resume  Tab workspace/all  Ctrl+F fresh  Ctrl+Q exit  Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
        sections[3],
    );
}

fn relative_time(timestamp: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - timestamp).num_seconds().max(0);
    match seconds {
        0..=59 => "now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn truncate_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        if UnicodeWidthStr::width(out.as_str())
            + unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
            >= width
        {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(id: u128, workspace: &str, prompt: &str, age: i64) -> SessionItem {
        SessionItem {
            id: Uuid::from_u128(id),
            workspace: workspace.into(),
            updated_at: Utc::now() - chrono::Duration::seconds(age),
            mode: "PLAN".into(),
            model: "model".into(),
            prompt: prompt.into(),
            event_count: 3,
        }
    }

    #[test]
    fn filters_workspace_searches_and_sorts_input_order() {
        let mut picker = Picker::new(
            vec![
                item(2, "/here", "new bug", 1),
                item(1, "/there", "old bug", 20),
            ],
            Path::new("/here"),
        );
        assert_eq!(picker.filtered().len(), 1);
        picker.scope = Scope::All;
        assert_eq!(picker.filtered().len(), 2);
        picker.query = "old".into();
        assert_eq!(picker.filtered()[0].id, Uuid::from_u128(1));
    }

    #[test]
    fn selection_clamps_for_zero_and_many_rows() {
        let mut picker = Picker::new(vec![], Path::new("/here"));
        picker.move_by(1);
        assert_eq!(picker.selected_id(), None);
        picker.sessions = vec![item(1, "/here", "one", 1), item(2, "/here", "two", 2)];
        picker.move_by(20);
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn width_truncation_handles_cjk() {
        assert!(UnicodeWidthStr::width(truncate_width("你好世界", 5).as_str()) <= 5);
        assert_eq!(truncate_width("", 0), "");
    }
}
