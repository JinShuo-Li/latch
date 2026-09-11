#![forbid(unsafe_code)]

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use latch_protocol::Mode;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::io::{self, Stdout};
use tokio::sync::mpsc;

/// Visual rows moved per mouse wheel event.
const WHEEL_ROWS: usize = 3;

#[derive(Debug)]
pub enum Input {
    Submit(String),
    Cancel,
    Quit,
}
#[derive(Debug, Clone)]
pub enum Output {
    AssistantDelta(String),
    AssistantDone,
    Tool {
        verb: String,
        target: String,
        status: ToolStatus,
    },
    Notice(String),
    Mode(Mode),
    Header {
        model: String,
        branch: String,
        continuity: String,
    },
}
#[derive(Debug, Clone, Copy)]
pub enum ToolStatus {
    Running,
    Passed,
    Failed,
}

struct Guard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}
impl Guard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .ok();
        self.terminal.show_cursor().ok();
    }
}

struct App {
    input: String,
    lines: Vec<DisplayLine>,
    mode: Mode,
    model: String,
    branch: String,
    continuity: String,
    busy: bool,
    /// Offset from the top of the transcript in visual (wrapped) rows.
    scroll: usize,
    /// Whether the viewport is pinned to the newest content.
    follow: bool,
    /// Visual row count of the transcript at the last rendered width.
    content_rows: usize,
    /// Visual row count of the transcript viewport at the last render.
    viewport_rows: usize,
}
impl Default for App {
    fn default() -> Self {
        Self {
            input: String::new(),
            lines: Vec::new(),
            mode: Mode::default(),
            model: String::new(),
            branch: String::new(),
            continuity: String::new(),
            busy: false,
            scroll: 0,
            follow: true,
            content_rows: 0,
            viewport_rows: 0,
        }
    }
}
#[derive(Clone)]
struct DisplayLine {
    text: String,
    kind: LineKind,
}
#[derive(Clone, Copy)]
enum LineKind {
    User,
    Assistant,
    Tool,
    Success,
    Error,
    Muted,
}
impl App {
    fn output(&mut self, out: Output) {
        match out {
            Output::AssistantDelta(t) => {
                if matches!(
                    self.lines.last(),
                    Some(DisplayLine {
                        kind: LineKind::Assistant,
                        ..
                    })
                ) && self.busy
                {
                    if let Some(line) = self.lines.last_mut() {
                        line.text.push_str(&t);
                    }
                } else {
                    self.lines.push(DisplayLine {
                        text: t,
                        kind: LineKind::Assistant,
                    });
                    self.busy = true;
                }
            }
            Output::AssistantDone => self.busy = false,
            Output::Tool {
                verb,
                target,
                status,
            } => self.lines.push(DisplayLine {
                text: format!(" {verb:<8} {target}"),
                kind: match status {
                    ToolStatus::Running => LineKind::Tool,
                    ToolStatus::Passed => LineKind::Success,
                    ToolStatus::Failed => LineKind::Error,
                },
            }),
            Output::Notice(t) => self.lines.push(DisplayLine {
                text: t,
                kind: LineKind::Muted,
            }),
            Output::Mode(m) => self.mode = m,
            Output::Header {
                model,
                branch,
                continuity,
            } => {
                self.model = model;
                self.branch = branch;
                self.continuity = continuity;
            }
        }
    }

    /// Largest valid top offset for the current content and viewport.
    fn max_scroll(&self) -> usize {
        self.content_rows.saturating_sub(self.viewport_rows)
    }

    /// Scrolls `rows` visual rows toward the top, leaving auto-follow only when
    /// the viewport actually moves off the bottom.
    fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
        if self.scroll < self.max_scroll() {
            self.follow = false;
        }
    }

    /// Scrolls `rows` visual rows toward the bottom, re-enabling auto-follow
    /// once the bottom is reached.
    fn scroll_down(&mut self, rows: usize) {
        let max = self.max_scroll();
        self.scroll = self.scroll.saturating_add(rows).min(max);
        if self.scroll >= max {
            self.follow = true;
        }
    }

    /// Jumps to the transcript top and disables auto-follow when there is
    /// scrollback to hold position in.
    fn scroll_home(&mut self) {
        self.scroll = 0;
        if self.max_scroll() > 0 {
            self.follow = false;
        }
    }

    /// Jumps to the transcript bottom and re-enables auto-follow.
    fn scroll_end(&mut self) {
        self.scroll = self.max_scroll();
        self.follow = true;
    }

    /// Reconciles scroll state with the freshly measured transcript. While
    /// following, the viewport is pinned to the newest content. While scrolled
    /// up, the offset is preserved but clamped to the valid range, so a resize
    /// or a shrinking transcript can never produce an invalid offset. Reaching
    /// the bottom naturally restores auto-follow.
    fn sync_viewport(&mut self, content_rows: usize, viewport_rows: usize) {
        self.content_rows = content_rows;
        self.viewport_rows = viewport_rows;
        let max = self.max_scroll();
        if self.follow {
            self.scroll = max;
        } else {
            self.scroll = self.scroll.min(max);
            if self.scroll == max {
                self.follow = true;
            }
        }
    }
}

pub async fn run(
    input_tx: mpsc::Sender<Input>,
    mut output_rx: mpsc::Receiver<Output>,
    mode: Mode,
    model: String,
) -> Result<()> {
    let mut guard = Guard::enter()?;
    let mut app = App {
        mode,
        model,
        branch: "-".into(),
        continuity: "healthy".into(),
        ..Default::default()
    };
    let mut events = EventStream::new();
    loop {
        guard.terminal.draw(|frame| draw(frame, &mut app))?;
        tokio::select! {
         Some(out)=output_rx.recv()=>app.output(out),
         maybe=events.next()=>match maybe.transpose()?{
            Some(Event::Key(key)) if key.kind==KeyEventKind::Press => match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if app.busy { input_tx.send(Input::Cancel).await?; }
                    else { input_tx.send(Input::Quit).await?; break; }
                }
                KeyCode::Char(ch) => app.input.push(ch),
                KeyCode::Backspace => { app.input.pop(); }
                KeyCode::Enter => {
                    let text=std::mem::take(&mut app.input);
                    if !text.trim().is_empty() {
                        app.lines.push(DisplayLine{text:format!("> {text}"),kind:LineKind::User});
                        if text=="/quit"||text=="/exit" { input_tx.send(Input::Quit).await?; break; }
                        input_tx.send(Input::Submit(text)).await?;
                    }
                }
                KeyCode::PageUp => app.scroll_up(app.viewport_rows.max(1)),
                KeyCode::PageDown => app.scroll_down(app.viewport_rows.max(1)),
                KeyCode::Home => app.scroll_home(),
                KeyCode::End => app.scroll_end(),
                _=>{}
            },
            Some(Event::Mouse(mouse)) => match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(WHEEL_ROWS),
                MouseEventKind::ScrollDown => app.scroll_down(WHEEL_ROWS),
                _=>{}
            },
            None=>break,
            _=>{}
         }
        }
    }
    Ok(())
}

/// Builds the transcript as Ratatui lines, splitting embedded newlines so the
/// wrapper and the scroll calculation agree on the visual row layout.
fn transcript_lines(lines: &[DisplayLine]) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    for line in lines {
        let style = line_style(line.kind);
        for segment in line.text.split('\n') {
            out.push(Line::styled(segment, style));
        }
    }
    out
}

fn line_style(kind: LineKind) -> Style {
    match kind {
        LineKind::User => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        LineKind::Assistant => Style::default().fg(Color::White),
        LineKind::Tool => Style::default().fg(Color::Cyan),
        LineKind::Success => Style::default().fg(Color::Green),
        LineKind::Error => Style::default().fg(Color::Red),
        LineKind::Muted => Style::default().fg(Color::DarkGray),
    }
}

/// Number of visual rows the transcript occupies at `width`, using the same
/// wrapping Ratatui renders with.
fn visual_height(lines: &[DisplayLine], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(transcript_lines(lines))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);
    let mut header = vec![
        Span::styled(
            " latch ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  {}  {}  continuity:{}",
            app.mode, app.model, app.branch, app.continuity
        )),
    ];
    if !app.follow {
        let indicator = if app.scroll > 0 {
            "  ↑ scroll  ↓ newer"
        } else {
            "  ↓ newer"
        };
        header.push(Span::styled(
            indicator,
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

    let viewport = chunks[1];
    let content_rows = visual_height(&app.lines, viewport.width);
    app.sync_viewport(content_rows, viewport.height as usize);
    let offset = app.scroll.min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(transcript_lines(&app.lines))
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(paragraph, viewport);

    let input = Paragraph::new(format!("> {}", app.input)).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(input, chunks[2]);
    frame.set_cursor_position((
        chunks[2].x + 2 + app.input.chars().count() as u16,
        chunks[2].y + 1,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> DisplayLine {
        DisplayLine {
            text: text.into(),
            kind: LineKind::Assistant,
        }
    }

    fn app_with(lines: Vec<DisplayLine>) -> App {
        App {
            lines,
            ..App::default()
        }
    }

    #[test]
    fn empty_transcript_has_no_height() {
        assert_eq!(visual_height(&[], 80), 0);
    }

    #[test]
    fn long_wrapped_message_height_tracks_width() {
        let lines = vec![line(&"word ".repeat(200))];
        let wide = visual_height(&lines, 80);
        let narrow = visual_height(&lines, 20);
        assert!(narrow > wide, "narrow {narrow} should exceed wide {wide}");
        assert!(narrow > 20);
    }

    #[test]
    fn single_message_taller_than_viewport_stays_in_bounds() {
        let lines = vec![line(&"tall ".repeat(80))];
        let content = visual_height(&lines, 10);
        assert!(content > 3);
        let mut app = app_with(lines);
        app.sync_viewport(content, 3);
        assert!(app.follow);
        assert_eq!(app.scroll, content - 3);
        app.scroll_up(3);
        assert!(!app.follow);
        assert_eq!(app.scroll, content - 6);
        app.scroll_down(3);
        assert!(app.follow);
        assert_eq!(app.scroll, content - 3);
    }

    #[test]
    fn page_up_and_page_down_respect_bounds() {
        let mut app = app_with(vec![line(&"x ".repeat(100))]);
        let content = visual_height(&app.lines, 10);
        app.sync_viewport(content, 4);
        let bottom = app.scroll;
        app.scroll_up(4);
        assert_eq!(app.scroll, bottom - 4);
        assert!(!app.follow);
        app.scroll_down(4);
        assert_eq!(app.scroll, bottom);
        assert!(app.follow);
        app.scroll_up(usize::MAX);
        assert_eq!(app.scroll, 0);
        app.scroll_down(usize::MAX);
        assert_eq!(app.scroll, app.max_scroll());
        assert!(app.follow);
    }

    #[test]
    fn home_and_end_jump_to_edges() {
        let mut app = app_with(vec![line(&"y ".repeat(100))]);
        let content = visual_height(&app.lines, 10);
        app.sync_viewport(content, 4);
        app.scroll_up(2);
        assert!(!app.follow);
        app.scroll_home();
        assert_eq!(app.scroll, 0);
        assert!(!app.follow);
        app.scroll_end();
        assert_eq!(app.scroll, app.max_scroll());
        assert!(app.follow);
    }

    #[test]
    fn home_keeps_following_when_everything_fits() {
        let mut app = app_with(vec![line("short")]);
        app.sync_viewport(1, 5);
        app.scroll_home();
        assert_eq!(app.scroll, 0);
        assert!(app.follow);
    }

    #[test]
    fn auto_follow_pins_to_new_output_at_bottom() {
        let mut app = app_with(vec![line("short")]);
        app.sync_viewport(1, 5);
        assert!(app.follow);
        app.lines.push(line(&"more ".repeat(50)));
        let rows = visual_height(&app.lines, 10);
        app.sync_viewport(rows, 5);
        assert!(app.follow);
        assert_eq!(app.scroll, rows - 5);
    }

    #[test]
    fn manual_scroll_is_not_yanked_back_to_bottom() {
        let mut app = app_with(vec![line(&"a ".repeat(100))]);
        let rows = visual_height(&app.lines, 10);
        app.sync_viewport(rows, 4);
        app.scroll_up(3);
        let held = app.scroll;
        assert!(!app.follow);
        app.lines.push(line(&"b ".repeat(100)));
        let rows = visual_height(&app.lines, 10);
        app.sync_viewport(rows, 4);
        assert_eq!(app.scroll, held);
        assert!(!app.follow);
    }

    #[test]
    fn resize_recomputes_rows_and_clamps_offset() {
        let mut app = app_with(vec![line(&"resize ".repeat(100))]);
        let narrow = visual_height(&app.lines, 12);
        let wide = visual_height(&app.lines, 60);
        assert!(narrow > wide);
        app.sync_viewport(narrow, 5);
        app.scroll_home();
        app.scroll_down(2);
        let held = app.scroll;
        assert!(!app.follow);
        app.sync_viewport(wide, 5);
        assert!(app.scroll <= app.max_scroll());
        assert_eq!(app.scroll, held.min(app.max_scroll()));
    }

    #[test]
    fn unicode_content_wraps_without_panicking() {
        let lines = vec![line(&"你好世界".repeat(60))];
        let narrow = visual_height(&lines, 8);
        let wide = visual_height(&lines, 80);
        assert!(narrow > wide);
        let mut app = app_with(lines);
        app.sync_viewport(narrow, 3);
        assert!(app.scroll <= app.max_scroll());
        app.scroll_home();
        assert_eq!(app.scroll, 0);
        app.scroll_end();
        assert_eq!(app.scroll, app.max_scroll());
        assert!(app.follow);
    }

    #[test]
    fn embedded_newlines_count_as_separate_rows() {
        let lines = vec![line("first line\nsecond line\nthird line")];
        assert_eq!(visual_height(&lines, 40), 3);
    }
}
