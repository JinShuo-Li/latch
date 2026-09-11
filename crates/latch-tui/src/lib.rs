#![forbid(unsafe_code)]

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
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
        execute!(stdout, EnterAlternateScreen)?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen).ok();
        self.terminal.show_cursor().ok();
    }
}

#[derive(Default)]
struct App {
    input: String,
    lines: Vec<DisplayLine>,
    mode: Mode,
    model: String,
    branch: String,
    continuity: String,
    busy: bool,
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
        guard.terminal.draw(|frame| draw(frame, &app))?;
        tokio::select! {
         Some(out)=output_rx.recv()=>app.output(out),
         maybe=events.next()=>match maybe.transpose()?{Some(Event::Key(key)) if key.kind==KeyEventKind::Press=>match key.code{KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL)=>{if app.busy{input_tx.send(Input::Cancel).await?;}else{input_tx.send(Input::Quit).await?;break;}},KeyCode::Char(ch)=>app.input.push(ch),KeyCode::Backspace=>{app.input.pop();},KeyCode::Enter=>{let text=std::mem::take(&mut app.input);if !text.trim().is_empty(){app.lines.push(DisplayLine{text:format!("> {text}"),kind:LineKind::User});if text=="/quit"||text=="/exit"{input_tx.send(Input::Quit).await?;break;}input_tx.send(Input::Submit(text)).await?;}},_=>{}},None=>break,_=>{}}
        }
    }
    Ok(())
}
fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);
    let header = Line::from(vec![
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
    ]);
    frame.render_widget(Paragraph::new(header), chunks[0]);
    let height = chunks[1].height as usize;
    let start = app.lines.len().saturating_sub(height);
    let lines = app.lines[start..]
        .iter()
        .map(|l| {
            Line::styled(
                l.text.clone(),
                match l.kind {
                    LineKind::User => Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                    LineKind::Assistant => Style::default().fg(Color::White),
                    LineKind::Tool => Style::default().fg(Color::Cyan),
                    LineKind::Success => Style::default().fg(Color::Green),
                    LineKind::Error => Style::default().fg(Color::Red),
                    LineKind::Muted => Style::default().fg(Color::DarkGray),
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);
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
