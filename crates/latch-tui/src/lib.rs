#![forbid(unsafe_code)]

//! Latch TUI: a quiet, dense, terminal-native coding-agent surface.
//!
//! The transcript is typed ([`TranscriptItem`]) rather than a wall of strings:
//! user messages, assistant messages (rendered with a small deterministic
//! Markdown subset), tool lifecycle rows (one visual item per call, updated
//! from running to passed/failed by `call_id`), kernel notices, and errors.
//! The input is a real editor with cursor motion, multiline, history, and a
//! live slash-command palette whose single command list is shared with the
//! CLI's `/help`.

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
#[cfg(test)]
use latch_protocol::DisplayItem;
use latch_protocol::{Event as DurableEvent, Mode, ToolResult, ToolRunStatus};
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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

mod presentation;
mod session_picker;
pub use presentation::{Cell, CellStatus, ExplorationOperation, PatchFile, PresentationModel};
pub use session_picker::{PickerSelection, SessionItem, SessionPreviewLine, run_session_picker};

/// Visual rows moved per mouse wheel event.
const WHEEL_ROWS: usize = 3;
/// Maximum visual rows the input area may occupy before it scrolls internally.
const MAX_INPUT_ROWS: usize = 8;
/// Maximum slash palette entries shown at once.
const MAX_PALETTE_ROWS: usize = 6;

#[derive(Debug)]
pub enum Input {
    Submit(String),
    Cancel,
    Resume,
    Quit,
}
#[derive(Debug, Clone)]
pub enum Output {
    AssistantDelta(String),
    AssistantDone,
    /// One user-visible transcript element from the shared durable-event
    /// formatter. Used for live kernel events and resume replay alike.
    Event(Box<DurableEvent>),
    ToolResult(ToolResult),
    Notice(String),
    Mode(Mode),
    Header {
        model: String,
        branch: String,
        resumed: bool,
    },
    /// Submitted prompts from the durable session, seeding prompt history on
    /// resume without a second history database.
    History(Vec<String>),
}

/// One slash command: the single source of truth shared by the palette and
/// `/help`, so command names never live in two places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/mode",
        description: "show or switch ASK/PLAN/WORK",
    },
    SlashCommand {
        name: "/resume",
        description: "Resume another saved session",
    },
    SlashCommand {
        name: "/model",
        description: "Show the configured provider model",
    },
    SlashCommand {
        name: "/context",
        description: "Inspect context state",
    },
    SlashCommand {
        name: "/diff",
        description: "Show workspace changes",
    },
    SlashCommand {
        name: "/checkpoint",
        description: "mark a change checkpoint",
    },
    SlashCommand {
        name: "/undo",
        description: "Undo latest safe Latch-owned change",
    },
    SlashCommand {
        name: "/compact",
        description: "Reset active working context",
    },
    SlashCommand {
        name: "/raw",
        description: "Toggle detailed transcript",
    },
    SlashCommand {
        name: "/help",
        description: "Show controls",
    },
    SlashCommand {
        name: "/quit",
        description: "Exit Latch",
    },
    SlashCommand {
        name: "/exit",
        description: "Exit Latch",
    },
];

/// Palette candidates for a filter string like `/mo`, longest-prefix friendly
/// and case-insensitive.
#[must_use]
pub fn filter_commands(filter: &str) -> Vec<&'static SlashCommand> {
    let query = filter.trim_start_matches('/').to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|command| fuzzy_match(command.name.trim_start_matches('/'), &query))
        .collect()
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut query = query.chars();
    let mut wanted = query.next();
    for ch in candidate.chars().flat_map(char::to_lowercase) {
        if wanted == Some(ch) {
            wanted = query.next();
        }
    }
    wanted.is_none()
}

struct Guard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}
impl Guard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            disable_raw_mode().ok();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                disable_raw_mode().ok();
                let mut stdout = io::stdout();
                execute!(stdout, DisableMouseCapture, LeaveAlternateScreen).ok();
                Err(error.into())
            }
        }
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

/// Compatibility-facing transcript item retained for downstream callers while
/// V3 rendering uses [`Cell`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    User { text: String },
    Assistant { text: String, streaming: bool },
    Tool(ToolRow),
    Notice { text: String },
    Error { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    pub call_id: String,
    pub verb: String,
    pub target: String,
    pub detail: String,
    pub status: ToolRunStatus,
}

impl TranscriptItem {
    #[cfg(test)]
    fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::Tool(row) => Some(&row.call_id),
            _ => None,
        }
    }
}

/// Multiline input editor with a real cursor and shell-like prompt history.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputEditor {
    lines: Vec<String>,
    row: usize,
    /// Cursor column as a char offset within `lines[row]`.
    col: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: Option<String>,
}

impl InputEditor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Self::default()
        }
    }
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.is_empty())
    }
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }
    fn current_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }
    pub fn insert(&mut self, ch: char) {
        let line = &mut self.lines[self.row];
        let byte = line
            .char_indices()
            .nth(self.col)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len());
        line.insert(byte, ch);
        self.col += 1;
    }
    /// Inserts a newline (Alt+Enter): splits the current line at the cursor.
    pub fn newline(&mut self) {
        let line = self.lines[self.row].clone();
        let byte = line
            .char_indices()
            .nth(self.col)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len());
        let (head, tail) = line.split_at(byte);
        let tail = tail.to_owned();
        self.lines[self.row] = head.to_owned();
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let cursor_byte = char_to_byte(line, self.col);
            let previous = line[..cursor_byte]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(byte, _)| byte);
            let removed_chars = line[previous..cursor_byte].chars().count();
            line.drain(previous..cursor_byte);
            self.col = self.col.saturating_sub(removed_chars);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&line);
        }
    }
    pub fn delete(&mut self) {
        let line = &mut self.lines[self.row];
        if self.col < line.chars().count() {
            let byte = char_to_byte(line, self.col);
            let removed = line[byte..]
                .graphemes(true)
                .next()
                .map(str::len)
                .unwrap_or(0);
            line.drain(byte..byte + removed);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }
    pub fn left(&mut self) {
        if self.col > 0 {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            let step = self.lines[self.row][..byte]
                .graphemes(true)
                .next_back()
                .map_or(1, |g| g.chars().count());
            self.col = self.col.saturating_sub(step);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.current_len();
        }
    }
    pub fn right(&mut self) {
        if self.col < self.current_len() {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            let step = self.lines[self.row][byte..]
                .graphemes(true)
                .next()
                .map_or(1, |g| g.chars().count());
            self.col = (self.col + step).min(self.current_len());
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }
    pub fn line_home(&mut self) {
        self.col = 0;
    }
    pub fn line_end(&mut self) {
        self.col = self.current_len();
    }
    pub fn kill_to_line_start(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = line
            .char_indices()
            .nth(self.col)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len());
        line.drain(..byte);
        self.col = 0;
    }
    pub fn kill_to_line_end(&mut self) {
        let line = &mut self.lines[self.row];
        let byte = line
            .char_indices()
            .nth(self.col)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len());
        line.drain(byte..);
    }
    /// Ctrl+W: delete the word before the cursor.
    pub fn kill_word(&mut self) {
        let line = &mut self.lines[self.row];
        let chars: Vec<char> = line.chars().collect();
        let end = self.col.min(chars.len());
        let mut start = end;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        if start < end {
            let tail: String = chars[end..].iter().collect();
            let head: String = chars[..start].iter().collect();
            *line = format!("{head}{tail}");
            self.col = start;
        } else if self.row > 0 && end == 0 {
            self.backspace();
        }
    }
    /// Up: previous input line when multiline, otherwise prompt history.
    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.current_len());
        } else {
            self.history_previous();
        }
    }
    /// Down: next input line when multiline, otherwise prompt history.
    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.current_len());
        } else {
            self.history_next();
        }
    }
    /// Recalls the previous submitted prompt. Editing a recalled prompt never
    /// mutates the stored history entry.
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = Some(self.text());
                let index = self.history.len() - 1;
                self.history_index = Some(index);
                self.set_text(&self.history[index].clone());
            }
            Some(0) => {}
            Some(index) => {
                self.history_index = Some(index - 1);
                self.set_text(&self.history[index - 1].clone());
            }
        }
    }
    /// Returns toward the newest entry; passing it restores the in-progress
    /// draft unchanged.
    pub fn history_next(&mut self) {
        match self.history_index {
            None => {}
            Some(index) if index + 1 < self.history.len() => {
                self.history_index = Some(index + 1);
                self.set_text(&self.history[index + 1].clone());
            }
            Some(_) => {
                self.history_index = None;
                if let Some(draft) = self.draft.take() {
                    self.set_text(&draft);
                }
            }
        }
    }
    fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        self.row = self.lines.len() - 1;
        self.col = self.current_len();
    }
    /// Seeds prompt history from the durable session (resume).
    pub fn seed_history(&mut self, history: Vec<String>) {
        self.history = history;
    }
    /// Takes the composed text for submission, records it in history, and
    /// resets the editor to empty.
    pub fn take_for_submit(&mut self) -> String {
        let text = self.text();
        if self
            .history
            .last()
            .map(|last| last != &text)
            .unwrap_or(true)
            && !text.trim().is_empty()
        {
            self.history.push(text.clone());
        }
        self.history_index = None;
        self.draft = None;
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        text
    }
}

struct Palette {
    selected: usize,
    dismissed: bool,
}

impl Palette {
    fn new() -> Self {
        Self {
            selected: 0,
            dismissed: false,
        }
    }
    /// The palette is active while the input's first line is a bare command
    /// prefix: starts with `/` and contains no whitespace yet.
    fn active(&self, input: &InputEditor) -> bool {
        let first = &input.lines[0];
        !self.dismissed && first.starts_with('/') && !first.contains(char::is_whitespace)
    }
    fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }
    fn previous(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + len - 1) % len;
        }
    }
    fn next(&mut self, len: usize) {
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }
    fn completion(&self, candidates: &[&SlashCommand]) -> Option<&'static str> {
        candidates.get(self.selected).map(|command| command.name)
    }
}

struct App {
    input: InputEditor,
    palette: Palette,
    presentation: PresentationModel,
    items: Vec<TranscriptItem>,
    streaming: Option<String>,
    mode: Mode,
    model: String,
    branch: String,
    resumed: bool,
    detail: bool,
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
            input: InputEditor::new(),
            palette: Palette::new(),
            presentation: PresentationModel::default(),
            items: Vec::new(),
            streaming: None,
            mode: Mode::default(),
            model: String::new(),
            branch: String::new(),
            resumed: false,
            detail: false,
            busy: false,
            scroll: 0,
            follow: true,
            content_rows: 0,
            viewport_rows: 0,
        }
    }
}
impl App {
    fn output(&mut self, out: Output) {
        match out {
            Output::AssistantDelta(t) => {
                self.streaming.get_or_insert_with(String::new).push_str(&t);
                if let Some(TranscriptItem::Assistant {
                    text,
                    streaming: true,
                }) = self.items.last_mut()
                {
                    text.push_str(&t);
                } else {
                    self.items.push(TranscriptItem::Assistant {
                        text: t,
                        streaming: true,
                    });
                }
                self.busy = true;
            }
            Output::AssistantDone => {
                self.streaming = None;
                if let Some(TranscriptItem::Assistant { streaming, .. }) = self.items.last_mut() {
                    *streaming = false;
                }
                self.busy = false;
            }
            Output::Event(event) => {
                if matches!(
                    &event.payload,
                    latch_protocol::EventPayload::AssistantMessageCompleted { .. }
                ) {
                    self.streaming = None;
                }
                self.presentation.apply_event(&event);
            }
            Output::ToolResult(result) => self.presentation.apply_tool_result(&result),
            Output::Notice(text) => self.presentation.push_notice(text),
            Output::Mode(mode) => self.mode = mode,
            Output::Header {
                model,
                branch,
                resumed,
            } => {
                self.model = model;
                self.branch = branch;
                self.resumed = resumed;
            }
            Output::History(history) => self.input.seed_history(history),
        }
    }

    #[cfg(test)]
    fn apply_item(&mut self, item: DisplayItem) {
        match item {
            DisplayItem::UserMessage { text } => self.items.push(TranscriptItem::User { text }),
            DisplayItem::AssistantMessage { text } => self.items.push(TranscriptItem::Assistant {
                text,
                streaming: false,
            }),
            DisplayItem::KernelNotice { text } => self.items.push(TranscriptItem::Notice { text }),
            DisplayItem::Error { text } => self.items.push(TranscriptItem::Error { text }),
            DisplayItem::ToolActivity {
                call_id,
                verb,
                target,
                detail,
                status,
            } => {
                if let Some(TranscriptItem::Tool(row)) = self
                    .items
                    .iter_mut()
                    .find(|item| item.tool_call_id() == Some(call_id.as_str()))
                {
                    if !verb.is_empty() {
                        row.verb = verb;
                    }
                    if !target.is_empty() {
                        row.target = target;
                    }
                    row.detail = detail;
                    row.status = status;
                } else {
                    self.items.push(TranscriptItem::Tool(ToolRow {
                        call_id,
                        verb,
                        target,
                        detail,
                        status,
                    }));
                }
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

    /// Handles a key press. Returns an action for the session loop.
    fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => Some(if self.busy {
                    Action::Cancel
                } else {
                    Action::Quit
                }),
                KeyCode::Char('a') => {
                    self.input.line_home();
                    None
                }
                KeyCode::Char('e') => {
                    self.input.line_end();
                    None
                }
                KeyCode::Char('w') => {
                    self.input.kill_word();
                    None
                }
                KeyCode::Char('u') => {
                    self.input.kill_to_line_start();
                    None
                }
                KeyCode::Char('k') => {
                    self.input.kill_to_line_end();
                    None
                }
                KeyCode::Char('t') => {
                    self.detail = !self.detail;
                    None
                }
                KeyCode::Char('p') if self.palette.active(&self.input) => {
                    let len = filter_commands(&self.input.lines[0]).len();
                    self.palette.previous(len);
                    None
                }
                KeyCode::Char('n') if self.palette.active(&self.input) => {
                    let len = filter_commands(&self.input.lines[0]).len();
                    self.palette.next(len);
                    None
                }
                _ => None,
            };
        }
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Enter {
            self.input.newline();
            return None;
        }
        let palette_active = self.palette.active(&self.input);
        let candidates = if palette_active {
            filter_commands(&self.input.lines[0])
        } else {
            Vec::new()
        };
        match key.code {
            KeyCode::Enter if palette_active => {
                if let Some(name) = self.palette.completion(&candidates) {
                    self.input.set_text(name);
                    self.submit_action()
                } else {
                    None
                }
            }
            KeyCode::Tab if palette_active => {
                self.complete_palette(&candidates);
                None
            }
            KeyCode::Esc if palette_active => {
                self.palette.selected = 0;
                self.palette.dismissed = true;
                None
            }
            KeyCode::Up if palette_active => {
                self.palette.previous(candidates.len());
                None
            }
            KeyCode::Down if palette_active => {
                self.palette.next(candidates.len());
                None
            }
            KeyCode::Enter => self.submit_action(),
            KeyCode::Backspace => {
                self.input.backspace();
                self.palette.dismissed = false;
                self.palette
                    .clamp(filter_commands(&self.input.lines[0]).len());
                None
            }
            KeyCode::Delete => {
                self.input.delete();
                None
            }
            KeyCode::Left => {
                self.input.left();
                None
            }
            KeyCode::Right => {
                self.input.right();
                None
            }
            KeyCode::Up => {
                self.input.up();
                None
            }
            KeyCode::Down => {
                self.input.down();
                None
            }
            KeyCode::Home => {
                self.scroll_home();
                None
            }
            KeyCode::End => {
                self.scroll_end();
                None
            }
            KeyCode::PageUp => {
                self.scroll_up(self.viewport_rows.max(1));
                None
            }
            KeyCode::PageDown => {
                self.scroll_down(self.viewport_rows.max(1));
                None
            }
            KeyCode::Char(ch) => {
                self.input.insert(ch);
                self.palette.dismissed = false;
                self.palette
                    .clamp(filter_commands(&self.input.lines[0]).len());
                None
            }
            _ => None,
        }
    }
}

enum Action {
    Submit(String),
    Cancel,
    Resume,
    Quit,
}

impl App {
    fn submit_action(&mut self) -> Option<Action> {
        let text = self.input.take_for_submit();
        let command = text.trim();
        if command.is_empty() {
            return None;
        }
        if matches!(command, "/quit" | "/exit") {
            return Some(Action::Quit);
        }
        if command == "/resume" && !self.busy {
            return Some(Action::Resume);
        }
        if command == "/raw" {
            self.detail = !self.detail;
            return None;
        }
        if !command.starts_with('/') {
            self.busy = true;
        }
        Some(Action::Submit(text))
    }

    /// Completes the palette selection in the input. A trailing space is added
    /// so the completed command is ready for arguments and the next Enter
    /// submits it instead of re-opening the palette.
    fn complete_palette(&mut self, candidates: &[&SlashCommand]) {
        if let Some(name) = self.palette.completion(candidates) {
            self.input.set_text(name);
            self.input.insert(' ');
            self.palette.selected = 0;
            self.palette.dismissed = false;
        }
    }
}

/// Builds the transcript as Ratatui lines with the item's own styling,
/// splitting embedded newlines so the wrapper and the scroll calculation agree
/// on the visual row layout.
fn transcript_lines(cells: &[Cell], streaming: Option<&str>, detail: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for cell in cells {
        for line in cell_lines(cell, detail) {
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

/// Copy-friendly rendering used by tests and transcript export paths.
#[must_use]
pub fn render_cells_plain(cells: &[Cell], detail: bool) -> String {
    transcript_lines(cells, None, detail)
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

fn cell_lines(cell: &Cell, detail: bool) -> Vec<Line<'static>> {
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
        Cell::Assistant { text } => render_markdown(text),
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

fn status_marker(status: CellStatus) -> (&'static str, Style) {
    match status {
        CellStatus::Running => ("•", Style::default().fg(Color::Cyan)),
        CellStatus::Passed => ("✓", Style::default().fg(Color::Green)),
        CellStatus::Failed => ("✗", Style::default().fg(Color::Red)),
    }
}

fn activity_lines(
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

fn validation_lines(
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

fn exploration_lines(operations: &[ExplorationOperation]) -> Vec<Line<'static>> {
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

fn patch_lines(files: &[PatchFile]) -> Vec<Line<'static>> {
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
    if files.len() == 1 {
        let file = &files[0];
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::styled(format!("{title} {}", file.path), Style::default().bold()),
            Span::styled(
                format!("  +{} −{}", file.additions, file.deletions),
                notice_style(),
            ),
        ]));
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
            lines.push(Line::from(vec![
                Span::styled(prefix, notice_style()),
                Span::raw(format!("{} {}", file.kind, file.path)),
                Span::styled(
                    format!("  +{} −{}", file.additions, file.deletions),
                    notice_style(),
                ),
            ]));
        }
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

fn assistant_style() -> Style {
    Style::default().fg(Color::White)
}

fn notice_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
fn truncate(text: &str, limit: usize) -> String {
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
fn semantic_visual_height(
    cells: &[Cell],
    streaming: Option<&str>,
    detail: bool,
    width: u16,
) -> usize {
    if width == 0 {
        return 0;
    }
    Paragraph::new(transcript_lines(cells, streaming, detail))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

#[cfg(test)]
fn visual_height(items: &[TranscriptItem], width: u16) -> usize {
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

fn char_to_byte(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(byte, _)| byte)
}

/// Greedy word-wrap of one logical input line into visual rows, each paired
/// with the char offset it starts at. The cursor position uses this exact
/// layout, so wrapping and cursor placement always agree.
fn wrap_input_line(line: &str, width: usize) -> Vec<(usize, String)> {
    if width == 0 {
        return vec![(0, line.to_owned())];
    }
    let mut rows = Vec::new();
    let mut start_chars = 0usize;
    let mut row = String::new();
    let mut row_width = 0usize;
    for grapheme in line.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if !row.is_empty() && row_width + grapheme_width > width {
            rows.push((start_chars, std::mem::take(&mut row)));
            start_chars += rows.last().map_or(0, |(_, text)| text.chars().count());
            row_width = 0;
        }
        row.push_str(grapheme);
        row_width += grapheme_width;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push((start_chars, row));
    }
    rows
}

/// Visual (row, column) of the editor cursor within the wrapped input rows.
fn cursor_position(editor: &InputEditor, width: usize) -> (usize, usize) {
    let mut visual_row = 0usize;
    for (index, line) in editor.lines.iter().enumerate() {
        let rows = wrap_input_line(line, width);
        if index == editor.row {
            for (row_index, (start, content)) in rows.iter().enumerate() {
                let row_len = content.chars().count();
                if editor.col >= *start && editor.col <= start + row_len {
                    let byte = char_to_byte(content, editor.col - start);
                    return (
                        visual_row + row_index,
                        UnicodeWidthStr::width(&content[..byte]),
                    );
                }
            }
            let (_start, content) = rows.last().expect("wrap never yields empty");
            return (
                visual_row + rows.len() - 1,
                UnicodeWidthStr::width(content.as_str()),
            );
        }
        visual_row += rows.len();
    }
    (visual_row, 0)
}

/// A small deterministic Markdown subset for assistant text: headings, bullet
/// and numbered lists, fenced code blocks, inline code, and bold. Enough that
/// model output stops reading like raw Markdown source; not a browser engine.
fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_end();
        if let Some(rest) = trimmed.trim().strip_prefix("```") {
            let _ = rest;
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(Line::styled(
                format!("  │ {trimmed}"),
                Style::default().fg(Color::Cyan),
            ));
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
            continue;
        }
        if let Some(rest) = body.strip_prefix("- ").or_else(|| body.strip_prefix("* ")) {
            let mut spans = vec![Span::styled(
                format!("{}• ", " ".repeat(indent)),
                Style::default().fg(Color::DarkGray),
            )];
            spans.extend(inline_spans(rest, assistant_style()));
            out.push(Line::from(spans));
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
            continue;
        }
        if body.is_empty() {
            out.push(Line::from(String::new()));
            continue;
        }
        if body.contains('|') {
            let cells = body
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>();
            if cells.iter().all(|cell| {
                !cell.is_empty() && cell.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
            }) {
                continue;
            }
            let mut spans = vec![Span::styled("│ ", notice_style())];
            for (index, cell) in cells.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" │ ", notice_style()));
                }
                spans.extend(inline_spans(cell, assistant_style()));
            }
            spans.push(Span::styled(" │", notice_style()));
            out.push(Line::from(spans));
            continue;
        }
        out.push(Line::from(inline_spans(body, assistant_style())));
    }
    out
}

/// Inline formatting: `` `code` `` → dim cyan, `**bold**` → bold.
fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
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

fn push_plain_spans(text: &str, base: Style, spans: &mut Vec<Span<'static>>) {
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

fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1))
        .find(|&index| chars[index] == '*' && chars.get(index + 1) == Some(&'*'))
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();
    let width = area.width.max(1) as usize;
    let palette_active = app.palette.active(&app.input);
    let candidates = if palette_active {
        filter_commands(&app.input.lines[0])
    } else {
        Vec::new()
    };
    app.palette.clamp(candidates.len());

    // Input height: wrapped visual rows (capped) plus the border row.
    let input_rows: usize = app
        .input
        .lines
        .iter()
        .map(|line| wrap_input_line(line, width.saturating_sub(4)).len())
        .sum::<usize>()
        .clamp(1, MAX_INPUT_ROWS);
    let palette_rows = if palette_active {
        candidates.len().min(MAX_PALETTE_ROWS)
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(palette_rows as u16),
            Constraint::Length((input_rows + 1) as u16),
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
        Span::raw(format!("  {}  {}  {}", app.mode, app.model, app.branch)),
    ];
    if app.resumed {
        header.push(Span::styled("  resumed", notice_style()));
    }
    if app.detail {
        header.push(Span::styled("  detail", Style::default().fg(Color::Cyan)));
    }
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
    ))
    .wrap(Wrap { trim: false })
    .scroll((offset, 0));
    frame.render_widget(paragraph, viewport);

    if palette_active && !candidates.is_empty() {
        let palette_area = chunks[2];
        let mut rows = Vec::new();
        let window_start = app
            .palette
            .selected
            .saturating_sub(MAX_PALETTE_ROWS - 1)
            .min(candidates.len().saturating_sub(1));
        for (index, command) in candidates
            .iter()
            .skip(window_start)
            .take(MAX_PALETTE_ROWS)
            .enumerate()
        {
            let selected = window_start + index == app.palette.selected;
            let style = if selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };
            rows.push(Line::from(vec![
                Span::styled(format!(" {:<10}", command.name), style),
                Span::styled(
                    command.description,
                    if selected { style } else { notice_style() },
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(rows), palette_area);
    }

    // Input: wrapped rows windowed around the cursor when longer than the cap.
    let inner_width = width.saturating_sub(4);
    let mut visual: Vec<String> = Vec::new();
    for line in &app.input.lines {
        for (_, row) in wrap_input_line(line, inner_width) {
            visual.push(row);
        }
    }
    let (cursor_row, cursor_col) = cursor_position(&app.input, inner_width);
    let total_visual = visual.len();
    let start_row = if total_visual <= MAX_INPUT_ROWS {
        0
    } else {
        cursor_row
            .saturating_sub(MAX_INPUT_ROWS - 1)
            .min(total_visual - MAX_INPUT_ROWS)
    };
    let visible: Vec<Line<'_>> = visual
        .into_iter()
        .skip(start_row)
        .take(MAX_INPUT_ROWS)
        .map(|row| {
            Line::from(vec![
                Span::styled("❯ ", Style::default().fg(Color::Cyan)),
                Span::raw(row),
            ])
        })
        .collect();
    let input = Paragraph::new(visible).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(input, chunks[3]);

    // The "❯ " prompt occupies two columns; wrapped continuation rows start
    // flush at the inner edge.
    let prompt_indent = if cursor_row.saturating_sub(start_row) == 0 {
        2u16
    } else {
        0u16
    };
    frame.set_cursor_position((
        chunks[3].x + prompt_indent + cursor_col.min(u16::MAX as usize) as u16,
        chunks[3].y + 1 + cursor_row.saturating_sub(start_row).min(MAX_INPUT_ROWS - 1) as u16,
    ));
}

pub async fn run(
    input_tx: mpsc::Sender<Input>,
    mut output_rx: mpsc::Receiver<Output>,
    mode: Mode,
    model: String,
    replay: Vec<DurableEvent>,
    history: Vec<String>,
) -> Result<()> {
    let mut guard = Guard::enter()?;
    let mut app = App {
        mode,
        model,
        branch: "-".into(),
        resumed: !replay.is_empty(),
        ..Default::default()
    };
    for event in replay {
        app.presentation.apply_event(&event);
    }
    app.input.seed_history(history);
    let mut events = EventStream::new();
    loop {
        guard.terminal.draw(|frame| draw(frame, &mut app))?;
        tokio::select! {
         Some(out)=output_rx.recv()=>app.output(out),
         maybe=events.next()=>match maybe.transpose()?{
            Some(Event::Key(key)) if key.kind==KeyEventKind::Press => match app.on_key(key) {
                Some(Action::Submit(text)) => input_tx.send(Input::Submit(text)).await?,
                Some(Action::Cancel) => input_tx.send(Input::Cancel).await?,
                Some(Action::Resume) => { input_tx.send(Input::Resume).await?; break; }
                Some(Action::Quit) => { input_tx.send(Input::Quit).await?; break; }
                None => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> TranscriptItem {
        TranscriptItem::Assistant {
            text: text.into(),
            streaming: false,
        }
    }

    fn app_with(items: Vec<TranscriptItem>) -> App {
        App {
            items,
            ..App::default()
        }
    }

    // ---- transcript scrolling (visual-row mechanics, preserved) ----

    #[test]
    fn empty_transcript_has_no_height() {
        assert_eq!(visual_height(&[], 80), 0);
    }

    #[test]
    fn long_wrapped_message_height_tracks_width() {
        let items = vec![assistant(&"word ".repeat(200))];
        let wide = visual_height(&items, 80);
        let narrow = visual_height(&items, 20);
        assert!(narrow > wide, "narrow {narrow} should exceed wide {wide}");
        assert!(narrow > 20);
    }

    #[test]
    fn single_message_taller_than_viewport_stays_in_bounds() {
        let items = vec![assistant(&"tall ".repeat(80))];
        let content = visual_height(&items, 10);
        assert!(content > 3);
        let mut app = app_with(items);
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
        let mut app = app_with(vec![assistant(&"x ".repeat(100))]);
        let content = visual_height(&app.items, 10);
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
        let mut app = app_with(vec![assistant(&"y ".repeat(100))]);
        let content = visual_height(&app.items, 10);
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
        let mut app = app_with(vec![assistant("short")]);
        app.sync_viewport(1, 5);
        app.scroll_home();
        assert_eq!(app.scroll, 0);
        assert!(app.follow);
    }

    #[test]
    fn auto_follow_pins_to_new_output_at_bottom() {
        let mut app = app_with(vec![assistant("short")]);
        app.sync_viewport(1, 5);
        assert!(app.follow);
        app.items.push(assistant(&"more ".repeat(50)));
        let rows = visual_height(&app.items, 10);
        app.sync_viewport(rows, 5);
        assert!(app.follow);
        assert_eq!(app.scroll, rows - 5);
    }

    #[test]
    fn manual_scroll_is_not_yanked_back_to_bottom() {
        let mut app = app_with(vec![assistant(&"a ".repeat(100))]);
        let rows = visual_height(&app.items, 10);
        app.sync_viewport(rows, 4);
        app.scroll_up(3);
        let held = app.scroll;
        assert!(!app.follow);
        app.items.push(assistant(&"b ".repeat(100)));
        let rows = visual_height(&app.items, 10);
        app.sync_viewport(rows, 4);
        assert_eq!(app.scroll, held);
        assert!(!app.follow);
    }

    #[test]
    fn resize_recomputes_rows_and_clamps_offset() {
        let mut app = app_with(vec![assistant(&"resize ".repeat(100))]);
        let narrow = visual_height(&app.items, 12);
        let wide = visual_height(&app.items, 60);
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
        let items = vec![assistant(&"你好世界".repeat(60))];
        let narrow = visual_height(&items, 8);
        let wide = visual_height(&items, 80);
        assert!(narrow > wide);
        let mut app = app_with(items);
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
        let items = vec![assistant("first line\nsecond line\nthird line")];
        assert_eq!(visual_height(&items, 40), 3);
    }

    // ---- typed items and tool lifecycle ----

    #[test]
    fn tool_activity_upserts_one_row_by_call_id() {
        let mut app = App::default();
        app.apply_item(DisplayItem::ToolActivity {
            call_id: "call-1".into(),
            verb: "patch".into(),
            target: "calc.py".into(),
            detail: String::new(),
            status: ToolRunStatus::Running,
        });
        assert_eq!(app.items.len(), 1);
        app.apply_item(DisplayItem::ToolActivity {
            call_id: "call-1".into(),
            verb: "patch".into(),
            target: "calc.py".into(),
            detail: "+1 -1".into(),
            status: ToolRunStatus::Passed,
        });
        assert_eq!(app.items.len(), 1, "one lifecycle row per call");
        match &app.items[0] {
            TranscriptItem::Tool(row) => {
                assert_eq!(row.status, ToolRunStatus::Passed);
                assert_eq!(row.detail, "+1 -1");
            }
            other => panic!("expected tool row, got {other:?}"),
        }
    }

    #[test]
    fn display_items_map_to_typed_rows() {
        let mut app = App::default();
        app.apply_item(DisplayItem::UserMessage { text: "hi".into() });
        app.apply_item(DisplayItem::AssistantMessage {
            text: "hello".into(),
        });
        app.apply_item(DisplayItem::KernelNotice {
            text: "resumed".into(),
        });
        app.apply_item(DisplayItem::Error {
            text: "boom".into(),
        });
        assert_eq!(
            app.items,
            vec![
                TranscriptItem::User { text: "hi".into() },
                TranscriptItem::Assistant {
                    text: "hello".into(),
                    streaming: false
                },
                TranscriptItem::Notice {
                    text: "resumed".into()
                },
                TranscriptItem::Error {
                    text: "boom".into()
                },
            ]
        );
    }

    #[test]
    fn streaming_assistant_renders_once_after_done() {
        let mut app = App::default();
        app.output(Output::AssistantDelta("he".into()));
        app.output(Output::AssistantDelta("llo".into()));
        assert!(matches!(
            app.items.last(),
            Some(TranscriptItem::Assistant {
                streaming: true,
                ..
            })
        ));
        app.output(Output::AssistantDone);
        assert!(matches!(
            app.items.last(),
            Some(TranscriptItem::Assistant {
                streaming: false,
                ..
            })
        ));
    }

    // ---- markdown rendering ----

    #[test]
    fn markdown_renders_headings_bullets_and_code() {
        let text = "# Title\n\n- bullet `code`\n```py\nx = 1\n```\n1. first\nplain **bold**";
        let lines = render_markdown(text);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(rendered[0], "Title");
        assert!(rendered[2].contains("• bullet"));
        assert!(rendered[2].contains("code"));
        assert_eq!(rendered[3], "  │ x = 1");
        assert!(rendered[4].contains("1. first"));
        assert!(rendered[5].contains("plain"));
        // Heading and code carry their own styles at the line level.
        assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[3].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn markdown_hides_fence_markers() {
        let lines = render_markdown("```\nhello\n```");
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("hello"));
        assert!(!rendered[0].contains("```"));
    }

    #[test]
    fn markdown_renders_links_italics_urls_and_tables() {
        let lines = render_markdown(
            "*note* [Latch](https://example.test)\n\n| A | B |\n|---|---|\n| 你 | https://example.test/x |",
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("note"));
        assert!(rendered.contains("Latch (https://example.test)"));
        assert!(rendered.contains("│ A │ B │"));
        assert!(!rendered.contains("|---"));
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }

    // ---- input editor ----

    fn editor_with(text: &str) -> InputEditor {
        let mut editor = InputEditor::new();
        for ch in text.chars() {
            editor.insert(ch);
        }
        editor
    }

    #[test]
    fn cursor_edits_behave_like_a_line_editor() {
        let mut editor = editor_with("hello");
        assert_eq!(editor.cursor(), (0, 5));
        for _ in 0..3 {
            editor.left();
        }
        assert_eq!(editor.cursor(), (0, 2));
        editor.insert('X');
        assert_eq!(editor.text(), "heXllo");
        editor.backspace();
        assert_eq!(editor.text(), "hello");
        editor.line_home();
        editor.insert('a');
        assert_eq!(editor.text(), "ahello");
        editor.line_end();
        editor.delete();
        assert_eq!(editor.text(), "ahello");
        editor.left();
        editor.delete();
        assert_eq!(editor.text(), "ahell");
    }

    #[test]
    fn ctrl_a_e_w_and_kill_keys_edit_deterministically() {
        let mut editor = editor_with("alpha beta gamma");
        editor.line_end();
        editor.kill_word();
        assert_eq!(editor.text(), "alpha beta ");
        editor.kill_word();
        assert_eq!(editor.text(), "alpha ");
        editor.line_home();
        editor.kill_to_line_end();
        assert!(editor.is_empty());
        let mut editor = editor_with("keep this");
        editor.line_end();
        editor.left();
        editor.kill_to_line_start();
        assert_eq!(editor.text(), "s");
    }

    #[test]
    fn multiline_split_and_merge() {
        let mut editor = editor_with("abc");
        editor.line_home();
        for _ in 0..2 {
            editor.right();
        }
        editor.newline();
        assert_eq!(editor.text(), "ab\nc");
        assert_eq!(editor.cursor(), (1, 0));
        editor.backspace();
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor(), (0, 2));
        let mut editor = editor_with("one");
        editor.line_end();
        editor.newline();
        editor.insert('!');
        assert_eq!(editor.text(), "one\n!");
        editor.delete();
        assert_eq!(editor.text(), "one\n!");
        editor.left();
        editor.down();
        assert_eq!(editor.cursor(), (1, 0));
    }

    #[test]
    fn input_wraps_within_bounded_rows() {
        let editor = editor_with(&"word ".repeat(60));
        let rows = wrap_input_line(&editor.lines[0], 20);
        assert!(rows.len() > 1);
        assert!(rows.len() < 40);
        let (row, col) = cursor_position(&editor, 20);
        assert!(row > 0);
        assert!(col <= 20);
        // Cursor position agrees with the wrapped layout it renders.
        let joined: String = rows
            .iter()
            .map(|(_, content)| content.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, editor.lines[0]);
    }

    #[test]
    fn multiline_wrapping_accounts_for_every_line() {
        let mut editor = InputEditor::new();
        for ch in
            "first line here\nsecond much longer line that wraps around a narrow width".chars()
        {
            editor.insert(ch);
        }
        let (row, _) = cursor_position(&editor, 20);
        assert!(row >= 1, "cursor sits on the second logical line");
    }

    // ---- prompt history ----

    #[test]
    fn history_recalls_without_mutating_and_restores_draft() {
        let mut editor = InputEditor::new();
        editor.seed_history(vec!["first".into(), "second".into()]);
        editor.insert('x');
        editor.history_previous();
        assert_eq!(editor.text(), "second");
        editor.history_previous();
        assert_eq!(editor.text(), "first");
        // Editing a recalled entry must not mutate stored history.
        editor.insert('!');
        assert_eq!(editor.text(), "first!");
        assert_eq!(editor.history, vec!["first", "second"]);
        editor.history_next();
        assert_eq!(editor.text(), "second");
        editor.history_next();
        assert_eq!(editor.text(), "x", "draft restored past newest entry");
        editor.history_previous();
        assert_eq!(editor.text(), "second");
    }

    #[test]
    fn submit_records_history_and_resets() {
        let mut editor = InputEditor::new();
        editor.insert('h');
        editor.insert('i');
        assert_eq!(editor.take_for_submit(), "hi");
        assert!(editor.is_empty());
        assert_eq!(editor.history, vec!["hi"]);
        editor.insert('h');
        editor.insert('i');
        editor.take_for_submit();
        assert_eq!(
            editor.history,
            vec!["hi"],
            "no duplicate consecutive history"
        );
    }

    // ---- slash palette ----

    #[test]
    fn palette_filters_by_prefix() {
        let mo = filter_commands("/mo");
        let names: Vec<&str> = mo.iter().map(|command| command.name).collect();
        assert_eq!(names, vec!["/mode", "/model"]);
        assert!(filter_commands("/mod").iter().any(|c| c.name == "/mode"));
        assert!(filter_commands("/und").iter().any(|c| c.name == "/undo"));
        assert!(filter_commands("/zzz").is_empty());
        assert_eq!(filter_commands("/").len(), SLASH_COMMANDS.len());
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn palette_opens_completes_and_closes() {
        let mut app = App::default();
        for ch in "/mo".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(app.palette.active(&app.input));
        // Enter dispatches the selected command directly.
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit(ref text)) if text.trim() == "/mode"));
        // Typing again reopens; selection can move.
        app.input.set_text("");
        app.input.insert('/');
        app.input.insert('m');
        app.input.insert('o');
        assert!(app.palette.active(&app.input));
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit(ref text)) if text.trim() == "/model"));
    }

    #[test]
    fn palette_tab_completes_and_escape_closes() {
        let mut app = App::default();
        for ch in "/un".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "/undo ");
        let mut app = App::default();
        for ch in "/co".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "/co", "escape keeps the text");
    }

    #[test]
    fn palette_does_not_capture_when_text_has_a_space() {
        let mut app = App::default();
        for ch in "/mode work".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(!app.palette.active(&app.input));
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(action, Some(Action::Submit(ref text)) if text == "/mode work"),
            "submitted, not completed"
        );
    }

    #[test]
    fn up_down_traverse_history_when_palette_closed() {
        let mut app = App::default();
        app.input.seed_history(vec!["earlier".into()]);
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "earlier");
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input.text(), "");
    }

    // ---- single display path for user prompts ----

    #[test]
    fn normal_prompt_is_not_echoed_by_the_tui() {
        let mut app = App::default();
        for ch in "inspect this".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit(ref text)) if text == "inspect this"));
        assert!(
            app.items.is_empty(),
            "normal prompts render from the durable UserMessage event, not a local echo"
        );
        // The durable event, delivered through the shared formatter, is the
        // one authoritative display path: exactly one visible item.
        app.apply_item(DisplayItem::UserMessage {
            text: "inspect this".into(),
        });
        assert_eq!(app.items.len(), 1);
    }

    #[test]
    fn two_identical_normal_prompts_stay_two_visible_items() {
        let mut app = App::default();
        for text in ["same prompt", "same prompt"] {
            for ch in text.chars() {
                app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            let before = app.items.len();
            let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
            assert!(matches!(action, Some(Action::Submit(_))));
            assert_eq!(
                app.items.len(),
                before,
                "no local echo for normal prompts; durable event is pending"
            );
            // One durable UserMessage event per submission.
            app.apply_item(DisplayItem::UserMessage { text: text.into() });
        }
        assert_eq!(app.items.len(), 2);
        assert!(matches!(
            app.items[0],
            TranscriptItem::User { ref text } if text == "same prompt"
        ));
        assert!(matches!(
            app.items[1],
            TranscriptItem::User { ref text } if text == "same prompt"
        ));
    }

    #[test]
    fn slash_command_echoes_exactly_once() {
        let mut app = App::default();
        // Enter dispatches a unique palette match.
        for ch in "/diff".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit(ref text)) if text.trim() == "/diff"));
        assert_eq!(
            app.items.len(),
            0,
            "slash commands are controls, not transcript messages"
        );
        // Slash commands produce no durable UserMessage.
        app.apply_item(DisplayItem::KernelNotice {
            text: "mode: WORK".into(),
        });
        assert_eq!(app.items.len(), 1);
    }

    // ---- shared formatter integration ----

    #[test]
    fn replay_items_rebuild_transcript_without_hidden_data() {
        use latch_protocol::{Event, EventPayload};
        let mut app = App::default();
        let session = uuid::Uuid::new_v4();
        let event = |payload| Event {
            id: uuid::Uuid::new_v4(),
            session_id: session,
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload,
        };
        for item in latch_protocol::display_items(&event(EventPayload::UserMessage {
            text: "fix bug".into(),
        })) {
            app.apply_item(item);
        }
        for item in latch_protocol::display_items(&event(EventPayload::AssistantMessageCompleted {
            text: "done".into(),
            tool_calls: vec![],
            reasoning_content: Some("hidden".into()),
        })) {
            app.apply_item(item);
        }
        assert_eq!(app.items.len(), 2);
        assert!(matches!(app.items[0], TranscriptItem::User { .. }));
        assert!(matches!(app.items[1], TranscriptItem::Assistant { .. }));
    }

    #[test]
    fn truncate_never_panics_on_multibyte_text() {
        let text = "你好世界".repeat(20);
        let cut = truncate(&text, 5);
        assert!(cut.chars().count() <= 5);
        assert!(cut.ends_with('…') || cut.chars().count() < 5);
    }

    #[test]
    fn exit_aliases_are_local_and_graceful() {
        for command in ["/quit", "/exit"] {
            let mut app = App::default();
            for ch in command.chars() {
                app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            assert!(matches!(
                app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Some(Action::Quit)
            ));
            assert!(app.presentation.cells().is_empty());
            assert!(
                app.items.is_empty(),
                "exit commands must never look like user messages"
            );
        }
    }

    #[test]
    fn ctrl_c_cancels_when_busy_and_quits_when_idle() {
        let mut app = App::default();
        assert!(matches!(
            app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        ));
        app.busy = true;
        assert!(matches!(
            app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Cancel)
        ));
    }

    #[test]
    fn grapheme_cursor_uses_terminal_display_width() {
        let mut editor = editor_with("你e\u{301}");
        assert_eq!(cursor_position(&editor, 20), (0, 3));
        editor.left();
        assert_eq!(
            editor.cursor(),
            (0, 1),
            "combining sequence moves as one grapheme"
        );
        assert_eq!(cursor_position(&editor, 20), (0, 2));
        editor.backspace();
        assert_eq!(editor.text(), "e\u{301}");
    }
}
