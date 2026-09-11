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
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
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
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::io::{self, Stdout, Write};
use tokio::sync::mpsc;

mod composer;
mod diff;
mod presentation;
mod session_picker;
mod sidebar;
use composer::display_width;
pub use composer::{Composer, VisualRow};
pub use diff::{DiffDocument, DiffFile, DiffHunk, DiffLine, DiffLineKind, parse_unified_diff};
pub use presentation::{Cell, CellStatus, ExplorationOperation, PatchFile, PresentationModel};
pub use session_picker::{PickerSelection, SessionItem, SessionPreviewLine, run_session_picker};
pub use sidebar::{Pricing, SidebarModel, SidebarSession};

/// Visual rows moved per mouse wheel event.
const WHEEL_ROWS: usize = 3;
/// Maximum slash palette entries shown at once.
const MAX_PALETTE_ROWS: usize = 6;

#[derive(Debug)]
pub enum Input {
    Submit(String),
    Cancel,
    Resume,
    Quit,
    /// A real human decision for a kernel approval request.
    Permission {
        request_id: uuid::Uuid,
        approved: bool,
    },
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
        /// Friendly provider label, for example `OpenCode Go` or `Anthropic`.
        provider: String,
        /// Session workspace, shown in the welcome state and composer footer.
        workspace: String,
        branch: String,
        resumed: bool,
        /// Optional user-configured pricing for the session model. `None` means
        /// the sidebar must show estimated cost as unavailable.
        pricing: Option<Pricing>,
    },
    /// A workspace diff to open in the full-width inspector.
    Diff(String),
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
        description: "Open workspace diff inspector",
    },
    SlashCommand {
        name: "/sidebar",
        description: "Toggle state sidebar (Ctrl+B)",
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

fn enter_screen(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
}

fn leave_screen(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
}

impl Guard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = enter_screen(&mut stdout) {
            leave_screen(&mut stdout).ok();
            disable_raw_mode().ok();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                disable_raw_mode().ok();
                let mut stdout = io::stdout();
                leave_screen(&mut stdout).ok();
                Err(error.into())
            }
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        leave_screen(self.terminal.backend_mut()).ok();
        disable_raw_mode().ok();
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Palette {
    selected: usize,
    dismissed: bool,
    /// Opened with Ctrl+P rather than by typing `/`, so it shows commands for
    /// any single-line input until the user edits, closes, or runs one.
    forced: bool,
}

impl Palette {
    fn new() -> Self {
        Self {
            selected: 0,
            dismissed: false,
            forced: false,
        }
    }
    /// The palette is active while the input's first line is a bare command
    /// prefix (starts with `/`, no whitespace yet) or was opened explicitly.
    fn active(&self, input: &Composer) -> bool {
        if self.dismissed || input.line_count() != 1 {
            return false;
        }
        if self.forced {
            return true;
        }
        let first = input.first_line();
        first.starts_with('/') && !first.contains(char::is_whitespace)
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
    input: Composer,
    palette: Palette,
    presentation: PresentationModel,
    items: Vec<TranscriptItem>,
    streaming: Option<String>,
    mode: Mode,
    model: String,
    provider: String,
    workspace: String,
    branch: String,
    resumed: bool,
    detail: bool,
    busy: bool,
    /// True after a cancelled or errored run, until the next prompt starts.
    interrupted: bool,
    /// Offset from the top of the transcript in visual (wrapped) rows.
    scroll: usize,
    /// Whether the viewport is pinned to the newest content.
    follow: bool,
    /// Visual row count of the transcript at the last rendered width.
    content_rows: usize,
    /// Visual row count of the transcript viewport at the last render.
    viewport_rows: usize,
    /// Authoritative observability state derived from durable events.
    sidebar: SidebarModel,
    /// Explicit user override for sidebar visibility; `None` follows width.
    sidebar_override: Option<bool>,
    /// Last rendered terminal width, so key handling can toggle responsively.
    last_width: u16,
    /// Full-width workspace diff inspector, when opened with `/diff`.
    diff_overlay: Option<DiffDocument>,
    diff_raw: bool,
    diff_scroll: usize,
    diff_max_scroll: usize,
    diff_viewport_rows: usize,
    /// A pending kernel approval request awaiting a human decision.
    permission: Option<PermissionPrompt>,
    /// Composer body width/height from the last render, for visual navigation.
    last_input_width: usize,
    last_input_height: usize,
    /// Screen rect of the composer body, for mouse-wheel routing.
    composer_body: ratatui::layout::Rect,
    /// Screen position of the terminal cursor from the last render.
    last_cursor: Option<(u16, u16)>,
}

/// Human-visible form of a `PermissionRequested` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub request_id: uuid::Uuid,
    pub tool: String,
    pub arguments: String,
    pub reason: String,
}
impl Default for App {
    fn default() -> Self {
        Self {
            input: Composer::new(),
            palette: Palette::new(),
            presentation: PresentationModel::default(),
            items: Vec::new(),
            streaming: None,
            mode: Mode::default(),
            model: String::new(),
            provider: String::new(),
            workspace: String::new(),
            branch: String::new(),
            resumed: false,
            detail: false,
            busy: false,
            interrupted: false,
            scroll: 0,
            follow: true,
            content_rows: 0,
            viewport_rows: 0,
            sidebar: SidebarModel::new(SidebarSession::default()),
            sidebar_override: None,
            last_width: 0,
            diff_overlay: None,
            diff_raw: false,
            diff_scroll: 0,
            diff_max_scroll: 0,
            diff_viewport_rows: 0,
            permission: None,
            last_input_width: 0,
            last_input_height: 0,
            composer_body: ratatui::layout::Rect::default(),
            last_cursor: None,
        }
    }
}
impl App {
    fn on_paste(&mut self, text: &str) {
        self.input.insert_text(text);
        self.palette.dismissed = false;
        self.palette.forced = false;
        self.palette
            .clamp(filter_commands(self.input.first_line()).len());
    }

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
                self.interrupted = false;
            }
            Output::AssistantDone => {
                self.streaming = None;
                if let Some(TranscriptItem::Assistant { streaming, .. }) = self.items.last_mut() {
                    *streaming = false;
                }
                self.busy = false;
            }
            Output::Event(event) => {
                match &event.payload {
                    latch_protocol::EventPayload::PermissionRequested {
                        request_id,
                        tool,
                        arguments,
                        reason,
                    } => {
                        self.permission = Some(PermissionPrompt {
                            request_id: *request_id,
                            tool: tool.clone(),
                            arguments: crate::sidebar::fit(
                                &serde_json::to_string(arguments).unwrap_or_default(),
                                160,
                            ),
                            reason: reason.clone(),
                        });
                    }
                    latch_protocol::EventPayload::PermissionResolved { request_id, .. }
                        if self
                            .permission
                            .as_ref()
                            .is_some_and(|prompt| prompt.request_id == *request_id) =>
                    {
                        self.permission = None;
                    }
                    _ => {}
                }
                if matches!(
                    &event.payload,
                    latch_protocol::EventPayload::AssistantMessageCompleted { .. }
                ) {
                    self.streaming = None;
                }
                self.presentation.apply_event(&event);
                self.sidebar.apply_event(&event);
            }
            Output::ToolResult(result) => self.presentation.apply_tool_result(&result),
            Output::Notice(text) => {
                if text.starts_with("error:") {
                    self.interrupted = true;
                    self.busy = false;
                }
                self.presentation.push_notice(text);
            }
            Output::Mode(mode) => {
                self.mode = mode;
                let mut session = self.sidebar.session().clone();
                session.mode = mode;
                self.sidebar.set_session(session);
            }
            Output::Header {
                model,
                provider,
                workspace,
                branch,
                resumed,
                pricing,
            } => {
                self.model = model.clone();
                self.provider = provider;
                self.workspace = workspace;
                self.branch = branch.clone();
                self.resumed = resumed;
                let mut session = self.sidebar.session().clone();
                session.model = model;
                session.branch = branch;
                session.resumed = resumed;
                session.pricing = pricing;
                self.sidebar.set_session(session);
            }
            Output::Diff(raw) => self.open_diff(raw),
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

    /// Opens the full-width diff inspector.
    fn open_diff(&mut self, raw: String) {
        self.diff_overlay = Some(parse_unified_diff(&raw));
        self.diff_raw = false;
        self.diff_scroll = 0;
        self.diff_max_scroll = 0;
    }

    fn close_diff(&mut self) {
        self.diff_overlay = None;
        self.diff_scroll = 0;
        self.diff_max_scroll = 0;
    }

    fn sidebar_visible_now(&self) -> bool {
        sidebar_visible(self.last_width, self.sidebar_override)
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_override = Some(!self.sidebar_visible_now());
    }

    fn diff_scroll_up(&mut self, rows: usize) {
        self.diff_scroll = self.diff_scroll.saturating_sub(rows);
    }

    fn diff_scroll_down(&mut self, rows: usize) {
        self.diff_scroll = (self.diff_scroll.saturating_add(rows)).min(self.diff_max_scroll);
    }

    /// Diff inspector keys. Editing keys are swallowed while the inspector is
    /// open; Ctrl+C still reaches normal cancel/quit handling.
    fn on_diff_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.diff_raw = !self.diff_raw;
            return None;
        }
        match key.code {
            KeyCode::Esc => self.close_diff(),
            KeyCode::Up => self.diff_scroll_up(1),
            KeyCode::Down => self.diff_scroll_down(1),
            KeyCode::PageUp => self.diff_scroll_up(self.diff_viewport_rows.max(1)),
            KeyCode::PageDown => self.diff_scroll_down(self.diff_viewport_rows.max(1)),
            KeyCode::Home => self.diff_scroll = 0,
            KeyCode::End => self.diff_scroll = self.diff_max_scroll,
            _ => {}
        }
        None
    }

    /// Handles a key press. Returns an action for the session loop.
    fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        // A pending approval owns the keyboard: approve, deny, or cancel.
        if let Some(request_id) = self.permission.as_ref().map(|prompt| prompt.request_id) {
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
                _ => None,
            };
            if let Some(approved) = decision {
                self.permission = None;
                return Some(Action::Permission {
                    request_id,
                    approved,
                });
            }
            if !(key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')) {
                return None;
            }
        }
        if self.diff_overlay.is_some()
            && !(key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
        {
            return self.on_diff_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') => {
                    if self.busy {
                        self.interrupted = true;
                        Some(Action::Cancel)
                    } else {
                        Some(Action::Quit)
                    }
                }
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
                KeyCode::Home => {
                    self.input.buffer_home();
                    None
                }
                KeyCode::End => {
                    self.input.buffer_end();
                    None
                }
                KeyCode::Char('t') => {
                    self.detail = !self.detail;
                    None
                }
                KeyCode::Char('b') => {
                    self.toggle_sidebar();
                    None
                }
                KeyCode::Char('p') => {
                    if self.palette.active(&self.input) {
                        let len = filter_commands(self.input.first_line()).len();
                        self.palette.previous(len);
                    } else if self.input.line_count() == 1 {
                        // Ctrl+P opens the command palette for the current
                        // single line; Esc restores it untouched.
                        self.palette.forced = true;
                        self.palette.dismissed = false;
                        self.palette.selected = 0;
                    }
                    None
                }
                KeyCode::Char('n') if self.palette.active(&self.input) => {
                    let len = filter_commands(self.input.first_line()).len();
                    self.palette.next(len);
                    None
                }
                // Ctrl+J is a literal line feed, so unlike Alt+Enter it reaches
                // every terminal (Windows Terminal reserves Alt+Enter for
                // fullscreen). Ctrl+Enter only arrives where the terminal can
                // report modified keys.
                KeyCode::Char('j') | KeyCode::Enter => {
                    self.input.newline();
                    None
                }
                _ => None,
            };
        }
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Enter {
            self.input.newline();
            return None;
        }
        // Shift+Home/End and Shift+PageUp/PageDown stay with the transcript so
        // full scrollback navigation survives the composer owning plain keys.
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            match key.code {
                KeyCode::Enter => {
                    self.input.newline();
                    return None;
                }
                KeyCode::Home => {
                    self.scroll_home();
                    return None;
                }
                KeyCode::End => {
                    self.scroll_end();
                    return None;
                }
                KeyCode::PageUp => {
                    self.scroll_up(self.viewport_rows.max(1));
                    return None;
                }
                KeyCode::PageDown => {
                    self.scroll_down(self.viewport_rows.max(1));
                    return None;
                }
                _ => {}
            }
        }
        let palette_active = self.palette.active(&self.input);
        let candidates = if palette_active {
            filter_commands(self.input.first_line())
        } else {
            Vec::new()
        };
        match key.code {
            KeyCode::Enter if palette_active => {
                self.palette.forced = false;
                match self.palette.completion(&candidates) {
                    Some(name) => {
                        self.input.set_text(name);
                        self.submit_action()
                    }
                    // An explicit palette with no matching command still sends
                    // what the user typed instead of swallowing Enter.
                    None => self.submit_action(),
                }
            }
            KeyCode::Tab if palette_active => {
                self.complete_palette(&candidates);
                None
            }
            KeyCode::Esc if palette_active => {
                self.palette.selected = 0;
                self.palette.dismissed = true;
                self.palette.forced = false;
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
                    .clamp(filter_commands(self.input.first_line()).len());
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
                self.input.up(self.last_input_width.max(1));
                None
            }
            KeyCode::Down => {
                self.input.down(self.last_input_width.max(1));
                None
            }
            KeyCode::Home => {
                self.input.line_home();
                None
            }
            KeyCode::End => {
                self.input.line_end();
                None
            }
            KeyCode::PageUp => {
                self.page_composer_or_transcript(-1);
                None
            }
            KeyCode::PageDown => {
                self.page_composer_or_transcript(1);
                None
            }
            KeyCode::Char(ch) => {
                self.input.insert(ch);
                self.palette.dismissed = false;
                self.palette
                    .clamp(filter_commands(self.input.first_line()).len());
                None
            }
            _ => None,
        }
    }

    /// Page keys move through the composer when it overflows; otherwise they
    /// keep their long-standing transcript-scroll role.
    fn page_composer_or_transcript(&mut self, direction: i32) {
        let width = self.last_input_width.max(1);
        let height = self.last_input_height.max(1);
        if self.input.is_scrollable(width, height) {
            if direction < 0 {
                self.input.page_up(width, height);
            } else {
                self.input.page_down(width, height);
            }
            self.input.reconcile_viewport(width, height);
        } else if direction < 0 {
            self.scroll_up(self.viewport_rows.max(1));
        } else {
            self.scroll_down(self.viewport_rows.max(1));
        }
    }

    fn composer_scroll_up(&mut self, rows: usize) {
        let width = self.last_input_width.max(1);
        let height = self.last_input_height.max(1);
        self.input.scroll_lines(-(rows as isize), width, height);
    }

    fn composer_scroll_down(&mut self, rows: usize) {
        let width = self.last_input_width.max(1);
        let height = self.last_input_height.max(1);
        self.input.scroll_lines(rows as isize, width, height);
    }
}

enum Action {
    Submit(String),
    Cancel,
    Resume,
    Quit,
    Permission {
        request_id: uuid::Uuid,
        approved: bool,
    },
}

impl App {
    fn submit_action(&mut self) -> Option<Action> {
        self.palette.forced = false;
        let text = self.input.take_for_submit();
        let command = text.trim();
        let slash_command = !text.contains(['\n', '\r']) && command.starts_with('/');
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
        if command == "/sidebar" {
            self.toggle_sidebar();
            return None;
        }
        if !slash_command {
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
            self.palette.forced = false;
        }
    }
}

/// Builds the transcript as Ratatui lines with the item's own styling,
/// splitting embedded newlines so the wrapper and the scroll calculation agree
/// on the visual row layout.
fn transcript_lines(
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

fn cell_lines(cell: &Cell, detail: bool, width: usize) -> Vec<Line<'static>> {
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

/// One transcript cell for a first-class diff. Bounded so a large model-issued
/// diff never floods the transcript; `/diff` opens the full inspector.
fn diff_cell_lines(status: CellStatus, document: &DiffDocument) -> Vec<Line<'static>> {
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

/// Inline preview budget across one edit cell. The full diff stays available
/// through `/diff`; the transcript never floods.
const MAX_PATCH_PREVIEW_LINES: usize = 14;

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
fn indent_preview(mut line: Line<'static>, spaces: usize) -> Line<'static> {
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
fn delta_spans(additions: usize, deletions: usize) -> Vec<Span<'static>> {
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
    Paragraph::new(transcript_lines(cells, streaming, detail, width as usize))
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

/// Width assumed when no terminal viewport is available (plain export).
const MARKDOWN_DEFAULT_WIDTH: usize = 100;

/// A small deterministic Markdown subset for assistant text: headings, bullet
/// and numbered lists, fenced code blocks, inline code, bold, links, and
/// aligned tables. Enough that model output stops reading like raw Markdown
/// source; not a browser engine. `width` bounds table column sizing so the
/// paragraph wrapper never has to break an aligned row.
fn render_markdown_at(text: &str, width: usize) -> Vec<Line<'static>> {
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

/// Column alignment parsed from a Markdown separator row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableAlign {
    Left,
    Center,
    Right,
}

/// Splits one Markdown table row into trimmed cells, honoring `\|` escapes.
/// Returns `None` when the line has no pipe or fewer than two cells, which
/// keeps prose and single-pipe content on the raw path.
fn split_table_row(line: &str) -> Option<Vec<String>> {
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
fn is_table_separator(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let cell = cell.trim();
            !cell.is_empty() && cell.contains('-') && cell.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn table_align(cell: &str) -> TableAlign {
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
fn render_table_block(lines: &[&str], width: usize) -> Option<(usize, Vec<Line<'static>>)> {
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
fn table_lines(
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
fn distribute_widths(natural: &[usize], available: usize) -> Vec<usize> {
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

fn wrap_row(cells: &[String], widths: &[usize]) -> Vec<Vec<String>> {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| wrap_cell(cell, *width))
        .collect()
}

/// One physical line of a table row: each column is wrapped separately, then
/// padded to its width and joined with a two-space gutter.
fn table_row_line(
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

fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() || width == 0 {
        return vec![String::new()];
    }
    let points = composer::wrap_points(text, width);
    let mut rows = Vec::new();
    for (index, start) in points.iter().enumerate() {
        let end = points.get(index + 1).copied().unwrap_or(text.len());
        rows.push(text[*start..end].trim_end().to_owned());
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn pad_cell(text: &str, width: usize, align: TableAlign) -> String {
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

/// Below this width the sidebar auto-collapses; narrower terminals stay clean
/// unless the user explicitly toggles it.
pub const SIDEBAR_MIN_AUTO_WIDTH: u16 = 110;

/// Whether the sidebar should be shown, honoring an explicit user override.
#[must_use]
pub fn sidebar_visible(width: u16, override_state: Option<bool>) -> bool {
    override_state.unwrap_or(width >= SIDEBAR_MIN_AUTO_WIDTH)
}

/// Responsive sidebar width in columns. Never a fixed third of the terminal:
/// wide screens get ~32% clamped to 44, mid screens ~27%, compact screens ~24%.
#[must_use]
pub fn sidebar_width(width: u16, visible: bool) -> u16 {
    if !visible {
        return 0;
    }
    match width {
        w if w >= 160 => (w as u32 * 32 / 100).clamp(28, 44) as u16,
        w if w >= 130 => (w as u32 * 27 / 100).clamp(26, 40) as u16,
        w if w >= 110 => (w as u32 * 24 / 100).clamp(22, 30) as u16,
        w => (w as u32 * 24 / 100)
            .clamp(20, 28)
            .min(u32::from((w / 2).max(1))) as u16,
    }
}

/// Centered approval prompt. Human approval is the only path that lets an
/// `Ask` policy decision execute; the model never controls this surface.
fn draw_permission_modal(frame: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let Some(prompt) = &app.permission else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(24, 84).min(area.width);
    // Five content rows plus the top and bottom border.
    let height = 7.min(area.height).max(3);
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

/// Full-width diff inspector with its own scrolling and a raw toggle.
fn draw_diff_overlay(frame: &mut ratatui::Frame<'_>, app: &mut App, area: ratatui::layout::Rect) {
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
        let mut title = vec![
            Span::styled(
                " diff ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
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

/// Responsive chrome rows around the composer.
///
/// Rows are dropped in priority order as the terminal shrinks: footer, top
/// spacer, hints, gap, then the metadata row. The rounded frame and the editor
/// body are only ever reduced to a single row, never removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ComposerChrome {
    spacer: u16,
    top: u16,
    body: u16,
    gap: u16,
    meta: u16,
    rule: u16,
    hints: u16,
    footer: u16,
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

    fn responsive(height: u16, content_rows: usize) -> Self {
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

fn yellow() -> Style {
    Style::default().fg(Color::Yellow)
}

fn focused_accent() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Readable secondary text for composer chrome (metadata, footer). `DarkGray`
/// is nearly invisible on dark themes, so chrome text steps up to `Gray`.
fn muted_style() -> Style {
    Style::default().fg(Color::Gray)
}

/// Composer status word and color, derived from real run state.
fn composer_status(app: &App) -> (String, Style) {
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

fn hint_spans(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
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

fn draw_palette(
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
        let name_style = if selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let description_style = if selected {
            Style::default().fg(Color::White).bg(Color::DarkGray)
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

/// Five-row pixel letterforms for the startup wordmark. Every letter occupies
/// the same five columns so the rows align without per-letter padding; the
/// extra width keeps the glyphs from looking narrow against terminal cells,
/// which are roughly twice as tall as they are wide.
const WORDMARK: [(&str, [&str; 5]); 5] = [
    ("L", ["██   ", "██   ", "██   ", "██   ", "█████"]),
    ("A", [" ███ ", "██ ██", "█████", "██ ██", "██ ██"]),
    ("T", ["█████", "  ██ ", "  ██ ", "  ██ ", "  ██ "]),
    ("C", [" ████", "██   ", "██   ", "██   ", " ████"]),
    ("H", ["██ ██", "██ ██", "█████", "██ ██", "██ ██"]),
];

/// One muted tone per letter; all distinct, none neon.
const WORDMARK_COLORS: [Color; 5] = [
    Color::Rgb(186, 142, 120),
    Color::Rgb(158, 176, 134),
    Color::Rgb(134, 160, 190),
    Color::Rgb(184, 164, 126),
    Color::Rgb(172, 146, 178),
];

fn wordmark_lines() -> Vec<Line<'static>> {
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

fn draw_welcome(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
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

fn draw_footer(frame: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
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

fn draw_composer_hints(frame: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    if area.height == 0 || area.width < 28 {
        return;
    }
    let width = area.width as usize;
    // Secondary shortcuts are abbreviated before the row is clipped.
    let hints: &[(&str, &str)] = if width < 64 {
        &[("ctrl+p", "commands")]
    } else {
        &[
            ("enter", "send"),
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
fn composer_meta_line(app: &App, width: usize, sidebar_shown: bool) -> Line<'static> {
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
fn draw_composer(
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

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
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
    let chrome = ComposerChrome::responsive(area.height, content_rows);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(palette_rows),
            Constraint::Length(
                chrome.spacer + chrome.top + chrome.body + chrome.gap + chrome.meta + chrome.rule,
            ),
            Constraint::Length(chrome.hints),
            Constraint::Length(chrome.footer),
        ])
        .split(area);
    let transcript_area = chunks[0];
    let palette_area = chunks[1];
    let composer_area = chunks[2];
    let hints_area = chunks[3];
    let footer_area = chunks[4];

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
    let session = SidebarSession {
        model: app.model.clone(),
        mode: app.mode,
        branch: app.branch.clone(),
        resumed: app.resumed,
        pricing: None,
    };
    for event in &replay {
        app.presentation.apply_event(event);
    }
    app.sidebar = SidebarModel::from_events(session, &replay);
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
                Some(Action::Permission { request_id, approved }) => {
                    input_tx.send(Input::Permission { request_id, approved }).await?;
                }
                None => {}
            },
            Some(Event::Mouse(mouse)) => {
                // The wheel scrolls whichever surface is under the pointer:
                // the composer when it overflows, otherwise the transcript.
                let over_composer = {
                    let r = app.composer_body;
                    r.width > 0
                        && mouse.column >= r.x
                        && mouse.column < r.x + r.width
                        && mouse.row >= r.y
                        && mouse.row < r.y + r.height
                };
                let composer_scrollable = app
                    .input
                    .is_scrollable(app.last_input_width.max(1), app.last_input_height.max(1));
                match mouse.kind {
                    MouseEventKind::ScrollUp if over_composer && composer_scrollable => {
                        app.composer_scroll_up(WHEEL_ROWS)
                    }
                    MouseEventKind::ScrollDown if over_composer && composer_scrollable => {
                        app.composer_scroll_down(WHEEL_ROWS)
                    }
                    MouseEventKind::ScrollUp => app.scroll_up(WHEEL_ROWS),
                    MouseEventKind::ScrollDown => app.scroll_down(WHEEL_ROWS),
                    _ => {}
                }
            }
            Some(Event::Paste(text)) => app.on_paste(&text),
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
        let lines = render_markdown_at(text, MARKDOWN_DEFAULT_WIDTH);
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
        let lines = render_markdown_at("```\nhello\n```", MARKDOWN_DEFAULT_WIDTH);
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
        let lines = render_markdown_at(
            "*note* [Latch](https://example.test)\n\n| A | B |\n|---|---|\n| 你 | https://example.test/x |",
            MARKDOWN_DEFAULT_WIDTH,
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("note"));
        assert!(rendered.contains("Latch (https://example.test)"));
        assert!(rendered.contains("A"));
        assert!(rendered.contains("你"));
        assert!(!rendered.contains("|---"), "separator rows are not literal");
        assert!(
            !rendered.contains("│ A │ B │"),
            "tables are aligned, not raw"
        );
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

    #[test]
    fn markdown_tables_align_columns_and_hide_separators() {
        let lines = render_markdown_at(
            "| Name | Qty |\n|:-----|----:|\n| apple | 12 |\n| kiwi | 3 |",
            40,
        );
        assert_eq!(
            lines_text(&lines),
            "Name   Qty\n──────────\napple   12\nkiwi     3"
        );
        // The rule after the header is dim and bold header cells stay bold.
        assert_eq!(lines[0].spans[0].style.add_modifier, Modifier::BOLD);
        assert_eq!(lines[1].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn markdown_tables_wrap_wide_cells_within_the_width() {
        let lines = render_markdown_at(
            "| Feature | Description |\n|---|---|\n| alpha | a moderately long description that wraps |",
            30,
        );
        for line in &lines {
            let text = lines_text(std::slice::from_ref(line));
            assert!(
                display_width(&text) <= 30,
                "table line {text:?} exceeds the viewport"
            );
        }
        assert!(lines.len() > 4, "the wide cell wrapped onto extra rows");
        assert!(lines_text(&lines).contains("alpha"));
    }

    #[test]
    fn markdown_tables_measure_cjk_by_display_width() {
        let lines = render_markdown_at("| 名称 | 数量 |\n|---|---|\n| 苹果 | 12 |", 40);
        assert_eq!(lines_text(&lines), "名称  数量\n──────────\n苹果  12  ");
        for line in &lines {
            let text = lines_text(std::slice::from_ref(line));
            assert_eq!(display_width(&text), 10);
        }
    }

    #[test]
    fn malformed_pipe_content_keeps_the_raw_treatment() {
        let lines = render_markdown_at("| a | b |\nno separator here", MARKDOWN_DEFAULT_WIDTH);
        let rendered = lines_text(&lines);
        assert!(rendered.contains("│ a │ b │"), "{rendered}");
        assert!(!rendered.contains('─'), "{rendered}");

        // A single-column pipe line is not an ordinary table.
        let single = render_markdown_at("| solo |\n|---|", MARKDOWN_DEFAULT_WIDTH);
        let rendered = lines_text(&single);
        assert!(rendered.contains("│ solo │"), "{rendered}");
        assert!(!rendered.contains("---"), "{rendered}");
    }

    // ---- input editor ----

    fn editor_with(text: &str) -> Composer {
        let mut editor = Composer::new();
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
        editor.down(40);
        assert_eq!(editor.cursor(), (1, 0));
    }

    #[test]
    fn input_wraps_within_bounded_rows() {
        let editor = editor_with(&"word ".repeat(60));
        let rows = editor.layout(20);
        assert!(rows.len() > 1);
        assert!(rows.len() < 40);
        let (row, col) = editor.cursor_visual(&rows);
        assert!(row > 0);
        assert!(col <= 20);
        // Every visual row stays inside the width and the complete buffer is
        // reachable by concatenating rows in order.
        let joined: String = rows
            .iter()
            .map(|row| editor.row_text(row))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, editor.text());
    }

    #[test]
    fn multiline_wrapping_accounts_for_every_line() {
        let mut editor = Composer::new();
        for ch in
            "first line here\nsecond much longer line that wraps around a narrow width".chars()
        {
            editor.insert(ch);
        }
        let (row, _) = editor.cursor_visual(&editor.layout(20));
        assert!(row >= 1, "cursor sits on the second logical line");
    }

    // ---- prompt history ----

    #[test]
    fn history_recalls_without_mutating_and_restores_draft() {
        let mut editor = Composer::new();
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
        let mut editor = Composer::new();
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
    fn ctrl_p_opens_the_palette_and_ctrl_j_inserts_a_newline() {
        let mut app = App::default();
        // Ctrl+P opens the command list even without a leading slash.
        app.on_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.palette.active(&app.input));
        assert!(app.input.is_empty());
        // Typing filters it, and Enter dispatches the completion.
        for ch in "q".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Quit)));
        assert!(!app.palette.active(&app.input));

        // Escape closes an explicit palette without touching the text.
        let mut app = App::default();
        app.on_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.palette.active(&app.input));
        assert!(
            app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE))
                .is_none()
        );
        assert!(!app.palette.active(&app.input), "escape is not undone");

        // Ctrl+J is a literal newline everywhere, unlike Alt+Enter which some
        // terminals reserve for themselves.
        let mut app = App::default();
        app.on_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input.text(), "\n");
        assert!(!app.palette.active(&app.input));
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
        assert_eq!(editor.cursor_visual(&editor.layout(20)), (0, 3));
        editor.left();
        assert_eq!(
            editor.cursor(),
            (0, 1),
            "combining sequence moves as one grapheme"
        );
        assert_eq!(editor.cursor_visual(&editor.layout(20)), (0, 2));
        editor.backspace();
        assert_eq!(editor.text(), "e\u{301}");
    }

    #[test]
    fn paste_multiline_into_empty_editor() {
        let mut editor = Composer::new();
        editor.insert_text("hello\nworld");
        assert_eq!(editor.text(), "hello\nworld");
        assert_eq!(editor.cursor(), (1, 5));
    }

    #[test]
    fn multiline_paste_places_the_cursor_on_the_visible_composer_row() {
        let backend = ratatui::backend::TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.on_paste("one\ntwo");

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        // The cursor is on the second pasted line inside the composer body:
        // top border + border/padding columns + cursor display column.
        let body = app.composer_body;
        assert_eq!(app.input.cursor(), (1, 3));
        assert_eq!(app.last_cursor, Some((body.x + 2 + 3, body.y + 1)));
        terminal
            .backend_mut()
            .assert_cursor_position((body.x + 2 + 3, body.y + 1));
    }

    #[test]
    fn composer_is_a_closed_rounded_frame_with_the_cursor_at_the_text_edge() {
        let backend = ratatui::backend::TestBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        let rows: Vec<&str> = text.lines().collect();
        let area = app.composer_body;
        let top = rows[area.y as usize];
        let bottom = rows[(area.y + area.height - 1) as usize];
        assert!(top.starts_with('╭') && top.ends_with('╮'), "{top:?}");
        assert!(
            bottom.starts_with('╰') && bottom.ends_with('╯'),
            "{bottom:?}"
        );
        for row in &rows[area.y as usize + 1..(area.y + area.height - 1) as usize] {
            assert!(row.starts_with('│') && row.ends_with('│'), "{row:?}");
        }
        // The empty composer leaves the cursor on the first text cell, directly
        // before the placeholder — never one column inside it.
        assert_eq!(app.last_cursor, Some((area.x + 2, area.y + 1)));
    }

    #[test]
    fn paste_splits_text_at_the_cursor_and_keeps_suffix() {
        let mut editor = editor_with("helloworld");
        for _ in 0..5 {
            editor.left();
        }
        editor.insert_text(" brave\nnew ");
        assert_eq!(editor.text(), "hello brave\nnew world");
        assert_eq!(editor.cursor(), (1, 4));
        editor.insert('!');
        assert_eq!(editor.text(), "hello brave\nnew !world");
    }

    #[test]
    fn paste_normalizes_crlf_and_bare_cr() {
        let mut editor = Composer::new();
        editor.insert_text("one\r\ntwo\rthree");
        assert_eq!(editor.text(), "one\ntwo\nthree");
        assert_eq!(editor.cursor(), (2, 5));
    }

    #[test]
    fn paste_preserves_trailing_newline_and_blank_lines() {
        let mut editor = Composer::new();
        editor.insert_text("hello\n\n\n");
        assert_eq!(editor.text(), "hello\n\n\n");
        assert_eq!(editor.lines, vec!["hello", "", "", ""]);
        assert_eq!(editor.cursor(), (3, 0));
    }

    #[test]
    fn paste_preserves_cjk_emoji_and_combining_graphemes() {
        let mut editor = Composer::new();
        editor.insert_text("你好 👨‍👩‍👧‍👦 e\u{301}");
        assert_eq!(editor.text(), "你好 👨‍👩‍👧‍👦 e\u{301}");
        editor.backspace();
        assert_eq!(
            editor.text(),
            "你好 👨‍👩‍👧‍👦 ",
            "combining sequence is one grapheme"
        );
        editor.backspace();
        editor.backspace();
        assert_eq!(
            editor.text(),
            "你好 ",
            "emoji family is removed as one grapheme"
        );
    }

    #[test]
    fn paste_never_submits_and_slash_text_waits_for_enter() {
        let mut app = App::default();
        app.on_paste("/help");
        assert_eq!(app.input.text(), "/help");
        assert!(app.presentation.cells().is_empty());
        assert!(app.items.is_empty());
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit(ref text)) if text == "/help"));
    }

    #[test]
    fn multiline_slash_paste_is_one_prompt_and_one_history_entry() {
        let mut app = App::default();
        let prompt = "/not-a-command\nsecond paragraph\n\nlast";
        app.on_paste(prompt);
        assert!(!app.palette.active(&app.input));
        assert!(
            app.items.is_empty(),
            "paste must not create a transcript item"
        );
        let action = app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Submit(ref text)) if text == prompt));
        assert_eq!(app.input.history, vec![prompt]);
        assert!(
            app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
    }

    // ---- V3.1 responsive sidebar and diff inspector ----

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                out.push_str(buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()));
            }
            out.push('\n');
        }
        out
    }

    fn render_to_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn responsive_sidebar_rules_are_clamped_and_not_a_fixed_third() {
        assert!(!sidebar_visible(80, None));
        assert!(!sidebar_visible(100, None));
        assert!(sidebar_visible(110, None));
        assert!(sidebar_visible(200, None));
        // Explicit override wins at any width.
        assert!(sidebar_visible(80, Some(true)));
        assert!(!sidebar_visible(200, Some(false)));
        assert_eq!(sidebar_width(200, true), 44, "wide screens clamp at 44");
        assert_eq!(sidebar_width(160, true), 44);
        assert_eq!(sidebar_width(159, true), 40);
        assert_eq!(sidebar_width(130, true), 35);
        assert_eq!(sidebar_width(129, true), 30);
        assert_eq!(sidebar_width(110, true), 26);
        assert_eq!(sidebar_width(80, true), 20);
        assert!(sidebar_width(200, true) < 200 / 3);
        assert_eq!(sidebar_width(200, false), 0);
    }

    #[test]
    fn ctrl_b_and_slash_sidebar_toggle_agree() {
        let mut app = App {
            last_width: 200,
            ..App::default()
        };
        assert!(app.sidebar_visible_now());
        app.on_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.sidebar_override, Some(false));
        assert!(!app.sidebar_visible_now());
        app.on_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(app.sidebar_visible_now());
        // The slash command goes through the same toggle and never submits.
        for ch in "/sidebar".chars() {
            app.on_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(
            app.on_key(key(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.sidebar_override, Some(false));
        assert!(app.presentation.cells().is_empty());
    }

    #[test]
    fn draw_never_panics_across_responsive_sizes_and_cjk_goal() {
        let mut app = App {
            model: "a-very-long-model-name-that-should-truncate-gracefully".into(),
            ..App::default()
        };
        app.sidebar.apply_event(&latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload: latch_protocol::EventPayload::ContextMaterialized {
                stats: latch_protocol::ContextStats {
                    instructions_tokens: 3_000,
                    state_tokens: 3_300,
                    recent_tokens: 37_900,
                    recall_tokens: 1_500,
                    tools_tokens: 2_400,
                    total_tokens: 48_100,
                    budget_tokens: 243_808,
                    window_tokens: 256_000,
                    reserve_tokens: 12_192,
                    headroom_tokens: 195_708,
                    durable_events: 503,
                    episodes: 11,
                    estimated: true,
                    status: "bounded".into(),
                    ..latch_protocol::ContextStats::default()
                },
            },
        });
        app.sidebar.apply_event(&latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 2,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload: latch_protocol::EventPayload::TaskStateUpdated {
                state: latch_protocol::TaskState {
                    goal: "实现一个内存 TTL 缓存并验证边界条件 🚀".into(),
                    ..latch_protocol::TaskState::default()
                },
            },
        });
        for width in [
            1, 10, 20, 30, 40, 60, 80, 100, 110, 120, 130, 159, 160, 200, 240,
        ] {
            for height in [1, 2, 4, 6, 10, 24, 60] {
                let _ = render_to_text(&mut app, width, height);
            }
        }
    }

    #[test]
    fn wide_terminal_shows_sidebar_and_narrow_hides_it() {
        let mut wide = App::default();
        wide.sidebar.apply_event(&latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload: latch_protocol::EventPayload::ContextMaterialized {
                stats: latch_protocol::ContextStats {
                    total_tokens: 48_100,
                    budget_tokens: 243_808,
                    window_tokens: 256_000,
                    status: "bounded".into(),
                    ..latch_protocol::ContextStats::default()
                },
            },
        });
        let text = render_to_text(&mut wide, 200, 40);
        assert!(text.contains("CONTEXT"), "{text}");
        assert!(text.contains("Working set"));

        let mut narrow = App::default();
        let text = render_to_text(&mut narrow, 80, 40);
        assert!(!text.contains("Working set"));
        // Resizing across the threshold recomputes visibility without panics.
        let mut resizing = App {
            last_width: 200,
            ..App::default()
        };
        assert!(resizing.sidebar_visible_now());
        let _ = render_to_text(&mut resizing, 80, 24);
        assert!(!resizing.sidebar_visible_now());
        let _ = render_to_text(&mut resizing, 200, 24);
        assert!(resizing.sidebar_visible_now());
    }

    #[test]
    fn diff_overlay_scrolls_toggles_raw_and_closes() {
        let mut app = App {
            last_width: 200,
            ..App::default()
        };
        let mut raw = String::from(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,100 +1,100 @@\n",
        );
        for index in 0..100 {
            raw.push_str(&format!("-old line {index}\n+new line {index}\n"));
        }
        app.open_diff(raw);
        assert!(app.diff_overlay.is_some());
        let _ = render_to_text(&mut app, 120, 20);
        assert!(app.diff_max_scroll > 0 || app.diff_viewport_rows > 0);
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.diff_scroll, 1);
        app.on_key(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.diff_scroll, app.diff_max_scroll);
        app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.diff_raw);
        // While the inspector is open, typing does not reach the composer.
        app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.input.text().is_empty());
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.diff_overlay.is_none());
    }

    fn permission_event(
        request_id: uuid::Uuid,
        resolved: Option<(bool, &str)>,
    ) -> latch_protocol::Event {
        let payload = match resolved {
            None => latch_protocol::EventPayload::PermissionRequested {
                request_id,
                tool: "shell".into(),
                arguments: serde_json::json!({"command":"sudo make install"}),
                reason: "outside-workspace write requires explicit approval".into(),
            },
            Some((approved, source)) => latch_protocol::EventPayload::PermissionResolved {
                request_id,
                approved,
                source: source.into(),
            },
        };
        latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload,
        }
    }

    #[test]
    fn permission_modal_owns_the_keyboard_and_emits_real_decisions() {
        let mut app = App::default();
        let request_id = uuid::Uuid::new_v4();
        app.output(Output::Event(Box::new(permission_event(request_id, None))));
        assert!(app.permission.is_some());
        let text = render_to_text(&mut app, 100, 30);
        assert!(text.contains("Permission required"), "{text}");
        assert!(text.contains("approve"), "{text}");
        let approved = app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            approved,
            Some(Action::Permission {
                approved: true,
                request_id: id
            }) if id == request_id
        ));
        assert!(app.permission.is_none());

        app.output(Output::Event(Box::new(permission_event(request_id, None))));
        let denied = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            denied,
            Some(Action::Permission {
                approved: false,
                request_id: id
            }) if id == request_id
        ));
        // A resolution event clears a prompt that arrived out of band (for
        // example a cancelled turn).
        app.output(Output::Event(Box::new(permission_event(request_id, None))));
        app.output(Output::Event(Box::new(permission_event(
            request_id,
            Some((false, "cancelled")),
        ))));
        assert!(app.permission.is_none());
        assert!(
            app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE))
                .is_none()
                || app.input.text().is_empty()
        );
    }

    #[test]
    fn patch_delta_colors_are_independent() {
        let additions_only = patch_lines(&[PatchFile {
            call_id: "a".into(),
            path: "src/new.rs".into(),
            kind: 'A',
            additions: 18,
            deletions: 0,
            status: CellStatus::Passed,
            diagnostic: String::new(),
            raw: String::new(),
            preview: String::new(),
        }]);
        let spans = &additions_only[0].spans;
        let added = spans
            .iter()
            .find(|span| span.content == "+18")
            .expect("addition span");
        assert_eq!(added.style.fg, Some(Color::Green));
        let removed = spans
            .iter()
            .find(|span| span.content == "−0")
            .expect("deletion span");
        assert_ne!(
            removed.style.fg,
            Some(Color::Red),
            "zero is not colored red"
        );

        let deletions_only = patch_lines(&[PatchFile {
            call_id: "d".into(),
            path: "src/old.rs".into(),
            kind: 'D',
            additions: 0,
            deletions: 7,
            status: CellStatus::Passed,
            diagnostic: String::new(),
            raw: String::new(),
            preview: String::new(),
        }]);
        let spans = &deletions_only[0].spans;
        let added = spans
            .iter()
            .find(|span| span.content == "+0")
            .expect("addition span");
        assert_ne!(added.style.fg, Some(Color::Green));
        let removed = spans
            .iter()
            .find(|span| span.content == "−7")
            .expect("deletion span");
        assert_eq!(removed.style.fg, Some(Color::Red));
    }

    fn find_span_style(lines: &[Line<'static>], needle: &str) -> Style {
        for line in lines {
            for span in &line.spans {
                if span.content.contains(needle) {
                    return span.style;
                }
            }
        }
        panic!("no rendered span contains {needle:?}");
    }

    fn lines_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn inline_preview_colors_real_removed_and_added_source_lines() {
        let preview = "diff --git a/src/calc.rs b/src/calc.rs
--- a/src/calc.rs
+++ b/src/calc.rs
@@ -1,4 +1,4 @@
 pub fn add(a: i32, b: i32) -> i32 {
     let base = 10;
-    base + a + b
+    base + a - b
 }
";
        let lines = patch_lines(&[PatchFile {
            call_id: "p1".into(),
            path: "src/calc.rs".into(),
            kind: 'M',
            additions: 1,
            deletions: 1,
            status: CellStatus::Passed,
            diagnostic: String::new(),
            raw: String::new(),
            preview: preview.into(),
        }]);
        // The actual source lines carry the colors, not the +N/−N summary.
        assert_eq!(
            find_span_style(&lines, "base + a + b").fg,
            Some(Color::Red),
            "deleted source line must be red"
        );
        assert_eq!(
            find_span_style(&lines, "base + a - b").fg,
            Some(Color::Green),
            "added source line must be green"
        );
        assert_eq!(
            find_span_style(&lines, "pub fn add").fg,
            Some(Color::DarkGray),
            "unchanged context is subdued"
        );
        let text = lines_text(&lines);
        assert!(text.contains("-    base + a + b"), "{text}");
        assert!(text.contains("+    base + a - b"), "{text}");
        assert!(text.contains("Edited src/calc.rs  +1 −1"), "{text}");
    }

    #[test]
    fn inline_preview_of_a_new_file_is_all_additions() {
        let preview = "diff --git a/tests/calc.rs b/tests/calc.rs
--- /dev/null
+++ b/tests/calc.rs
@@ -0,0 +1,2 @@
+#[test]
+fn subtracts() {}
";
        let lines = patch_lines(&[PatchFile {
            call_id: "p2".into(),
            path: "tests/calc.rs".into(),
            kind: 'A',
            additions: 2,
            deletions: 0,
            status: CellStatus::Passed,
            diagnostic: String::new(),
            raw: String::new(),
            preview: preview.into(),
        }]);
        assert_eq!(find_span_style(&lines, "#[test]").fg, Some(Color::Green));
        assert!(
            !lines_text(&lines).contains("\n-"),
            "{}",
            lines_text(&lines)
        );
    }

    #[test]
    fn inline_preview_handles_deleted_files_and_unicode_content() {
        let preview = "diff --git a/旧.rs b/旧.rs
--- a/旧.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-旧值 = 计算();
-保留
";
        let lines = patch_lines(&[PatchFile {
            call_id: "p4".into(),
            path: "旧.rs".into(),
            kind: 'D',
            additions: 0,
            deletions: 2,
            status: CellStatus::Passed,
            diagnostic: String::new(),
            raw: String::new(),
            preview: preview.into(),
        }]);
        assert_eq!(
            find_span_style(&lines, "旧值 = 计算();").fg,
            Some(Color::Red)
        );
        let text = lines_text(&lines);
        assert!(text.contains("Edited 旧.rs  +0 −2"), "{text}");
        assert!(!text.contains("\n+"), "{text}");
    }

    #[test]
    fn inline_preview_is_bounded_and_points_at_the_full_diff() {
        let mut preview = String::from(
            "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -1,60 +1,60 @@\n",
        );
        for index in 0..60 {
            preview.push_str(&format!("-old line {index}\n+new line {index}\n"));
        }
        let lines = patch_lines(&[PatchFile {
            call_id: "p3".into(),
            path: "src/big.rs".into(),
            kind: 'M',
            additions: 60,
            deletions: 60,
            status: CellStatus::Passed,
            diagnostic: String::new(),
            raw: String::new(),
            preview,
        }]);
        // Header + blank spacer + at most 14 body lines + the omission note.
        assert!(lines.len() <= 17, "preview is bounded: {}", lines.len());
        let text = lines_text(&lines);
        assert!(text.contains("+new line 0"), "{text}");
        assert!(!text.contains("+new line 14"), "{text}");
        assert!(
            text.contains("diff lines omitted · /diff for the full diff"),
            "{text}"
        );
    }

    #[test]
    fn header_pricing_reaches_the_sidebar() {
        let mut app = App::default();
        app.output(Output::Header {
            model: "deepseek-flash".into(),
            provider: "OpenCode Go".into(),
            workspace: "/tmp/latch".into(),
            branch: "main".into(),
            resumed: false,
            pricing: Some(crate::sidebar::Pricing {
                input_per_million: Some(0.28),
                output_per_million: Some(0.42),
                cache_read_per_million: None,
                cache_write_per_million: None,
                currency: "USD".into(),
            }),
        });
        app.output(Output::Event(Box::new(latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload: latch_protocol::EventPayload::ModelUsage {
                usage: latch_protocol::Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 0,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                },
            },
        })));
        let cost = app.sidebar.estimated_cost().expect("configured pricing");
        assert!((cost.amount - 0.28).abs() < 1e-9);
    }

    // ---- V4 composer and layout redesign ----

    fn assert_snapshot(name: &str, actual: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/snapshots")
            .join(name);
        if std::env::var("LATCH_UPDATE_SNAPSHOTS").is_ok() {
            std::fs::write(&path, actual).unwrap();
            return;
        }
        let expected =
            std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing snapshot {name}"));
        assert_eq!(
            actual.trim_end(),
            expected.trim_end(),
            "snapshot {name} changed"
        );
    }

    fn app_with_header(model: &str, workspace: &str) -> App {
        let mut app = App::default();
        app.output(Output::Header {
            model: model.into(),
            provider: "OpenCode Go".into(),
            workspace: workspace.into(),
            branch: "main".into(),
            resumed: false,
            pricing: None,
        });
        app
    }

    fn presentation_event(payload: latch_protocol::EventPayload) -> latch_protocol::Event {
        latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload,
        }
    }

    /// A deterministic transcript for layout snapshots, fed through the same
    /// durable-event path the live TUI uses.
    fn transcript_fixture(app: &mut App) {
        for payload in [
            latch_protocol::EventPayload::UserMessage {
                text: "Fix the failing test.".into(),
            },
            latch_protocol::EventPayload::ToolRequested {
                call: latch_protocol::ToolCall {
                    id: "t1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                },
            },
            latch_protocol::EventPayload::ToolFailed {
                result: ToolResult {
                    call_id: "t1".into(),
                    name: "shell".into(),
                    output: "exit code 1\nassertion failed".into(),
                    is_error: true,
                    artifact_id: None,
                },
            },
            latch_protocol::EventPayload::AssistantMessageCompleted {
                text: "The addend is wrong; fixing it now.".into(),
                tool_calls: vec![],
                reasoning_content: None,
            },
        ] {
            app.output(Output::Event(Box::new(presentation_event(payload))));
        }
    }

    /// A guarded edit and a newly created file, fed through the same
    /// ToolRequested/FileChanged/ToolResult path the live TUI uses.
    fn patch_preview_fixture(app: &mut App) {
        app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::UserMessage {
                text: "Fix the calculation.".into(),
            },
        ))));
        app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::ToolRequested {
                call: latch_protocol::ToolCall {
                    id: "p1".into(),
                    name: "patch".into(),
                    arguments: serde_json::json!({
                        "path": "src/calc.rs",
                        "old": "base + a + b",
                        "new": "base + a - b",
                    }),
                },
            },
        ))));
        app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::FileChanged {
                before: None,
                after: latch_protocol::FileVersion {
                    path: "src/calc.rs".into(),
                    content_hash: "h1".into(),
                    size: 1,
                },
                owner: latch_protocol::ChangeOwner::Latch,
                undo_artifact: None,
                additions: 1,
                deletions: 1,
                preview: "diff --git a/src/calc.rs b/src/calc.rs\n--- a/src/calc.rs\n+++ b/src/calc.rs\n@@ -1,4 +1,4 @@\n pub fn add(a: i32, b: i32) -> i32 {\n     let base = 10;\n-    base + a + b\n+    base + a - b\n }\n".into(),
                call_id: Some("p1".into()),
            },
        ))));
        app.output(Output::ToolResult(ToolResult {
            call_id: "p1".into(),
            name: "patch".into(),
            output: "updated src/calc.rs @ h1".into(),
            is_error: false,
            artifact_id: None,
        }));
        app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::ToolRequested {
                call: latch_protocol::ToolCall {
                    id: "p2".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({
                        "path": "tests/calc.rs",
                        "content": "#[test]\nfn subtracts() {}",
                    }),
                },
            },
        ))));
        app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::FileChanged {
                before: None,
                after: latch_protocol::FileVersion {
                    path: "tests/calc.rs".into(),
                    content_hash: "h2".into(),
                    size: 1,
                },
                owner: latch_protocol::ChangeOwner::Latch,
                undo_artifact: None,
                additions: 2,
                deletions: 0,
                preview: "diff --git a/tests/calc.rs b/tests/calc.rs\n--- /dev/null\n+++ b/tests/calc.rs\n@@ -0,0 +1,2 @@\n+#[test]\n+fn subtracts() {}\n".into(),
                call_id: Some("p2".into()),
            },
        ))));
        app.output(Output::ToolResult(ToolResult {
            call_id: "p2".into(),
            name: "write".into(),
            output: "updated tests/calc.rs @ h2".into(),
            is_error: false,
            artifact_id: None,
        }));
    }

    #[test]
    fn snapshot_inline_edit_preview() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        patch_preview_fixture(&mut app);
        assert_snapshot(
            "v4_inline_edit_preview.txt",
            &render_to_text(&mut app, 100, 24),
        );
    }

    #[test]
    fn welcome_wordmark_uses_five_distinct_muted_letter_colors() {
        let lines = wordmark_lines();
        assert_eq!(lines.len(), 5);
        let mut colors = Vec::new();
        for line in &lines {
            let letters: Vec<&Span<'static>> = line
                .spans
                .iter()
                .filter(|span| span.content.contains('█'))
                .collect();
            assert_eq!(letters.len(), 5, "each row shows five letter regions");
            for span in letters {
                colors.push(span.style.fg.expect("letter color"));
            }
        }
        let distinct: std::collections::BTreeSet<String> =
            colors.iter().map(|color| format!("{color:?}")).collect();
        assert_eq!(distinct.len(), 5, "all five letters use different colors");
        for color in &colors {
            let Color::Rgb(red, green, blue) = color else {
                panic!("expected rgb wordmark color, got {color:?}");
            };
            let max = *red.max(green).max(blue) as i32;
            let min = *red.min(green).min(blue) as i32;
            assert!(max - min <= 90, "muted tone expected: {color:?}");
        }
    }

    #[test]
    fn idle_composer_body_is_roomier_but_stays_bounded() {
        assert_eq!(ComposerChrome::responsive(30, 1).body, 3);
        assert_eq!(ComposerChrome::responsive(24, 1).body, 3);
        assert_eq!(ComposerChrome::responsive(30, 20).body, 8);
        assert_eq!(ComposerChrome::responsive(12, 1).body, 1);
    }

    fn markdown_table_fixture(app: &mut App, text: &str) {
        app.output(Output::Event(Box::new(presentation_event(
            latch_protocol::EventPayload::AssistantMessageCompleted {
                text: text.into(),
                tool_calls: vec![],
                reasoning_content: None,
            },
        ))));
    }

    #[test]
    fn snapshot_markdown_table_normal() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        markdown_table_fixture(
            &mut app,
            "Here is the plan:\n\n\
             | Step | Owner | Status |\n\
             |:-----|:------|-------:|\n\
             | Inspect the failing tests | Latch | done |\n\
             | Patch the parser | Latch | active |\n\
             | Verify with cargo test | Kernel | pending |",
        );
        assert_snapshot(
            "v4_markdown_table_normal.txt",
            &render_to_text(&mut app, 100, 24),
        );
    }

    #[test]
    fn snapshot_markdown_table_narrow() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        markdown_table_fixture(
            &mut app,
            "| Module | Responsibility | Notes |\n\
             |:-------|:---------------|:------|\n\
             | composer | scrollable multiline editor viewport | keeps the cursor visible while wrapping |\n\
             | sidebar | responsive session state | hidden below 110 columns |",
        );
        assert_snapshot(
            "v4_markdown_table_narrow.txt",
            &render_to_text(&mut app, 56, 24),
        );
    }

    #[test]
    fn snapshot_markdown_table_wide() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        markdown_table_fixture(
            &mut app,
            "| Mode | Mutation | Validation | Notes |\n\
             |:-----|:---------|:-----------|:------|\n\
             | ASK | denied | not run | read-only inspection |\n\
             | PLAN | denied | not run | produces a plan |\n\
             | WORK | policy-approved | required | implements and verifies |",
        );
        assert_snapshot(
            "v4_markdown_table_wide.txt",
            &render_to_text(&mut app, 160, 24),
        );
    }

    #[test]
    fn snapshot_markdown_table_cjk() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        markdown_table_fixture(
            &mut app,
            "| 模块 | 职责 | 状态 |\n\
             |:-----|:-----|-----:|\n\
             | 编辑器 | 可滚动的多行输入视口 | 完成 |\n\
             | 侧边栏 | 响应式会话状态 | 进行中 |",
        );
        assert_snapshot(
            "v4_markdown_table_cjk.txt",
            &render_to_text(&mut app, 100, 24),
        );
    }

    #[test]
    fn snapshot_welcome_state() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        assert_snapshot("v4_welcome.txt", &render_to_text(&mut app, 100, 24));
    }

    #[test]
    fn snapshot_single_line_composer() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text("fix the failing test");
        assert_snapshot("v4_composer_single.txt", &render_to_text(&mut app, 100, 24));
    }

    #[test]
    fn snapshot_multiline_composer() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text(
            "line one\nline two\nline three\nline four\nline five\nline six\nline seven",
        );
        assert_snapshot(
            "v4_composer_multiline.txt",
            &render_to_text(&mut app, 100, 24),
        );
    }

    #[test]
    fn snapshot_large_paste_scrolled_to_top_middle_and_bottom() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        let text = (0..80)
            .map(|index| format!("pasted line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.input.insert_text(&text);
        let _ = render_to_text(&mut app, 100, 30);
        // Bottom (cursor pinned).
        assert_snapshot("v4_paste_bottom.txt", &render_to_text(&mut app, 100, 30));
        // Middle: scroll the viewport without touching the buffer.
        app.input
            .scroll_lines(-30, app.last_input_width, app.last_input_height);
        assert_snapshot("v4_paste_middle.txt", &render_to_text(&mut app, 100, 30));
        // Top.
        app.input
            .scroll_lines(-1000, app.last_input_width, app.last_input_height);
        assert_snapshot("v4_paste_top.txt", &render_to_text(&mut app, 100, 30));
        assert_eq!(app.input.text(), text, "scrolling never edits the buffer");
    }

    #[test]
    fn snapshot_narrow_terminal() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text("a narrow but usable composer");
        assert_snapshot("v4_narrow.txt", &render_to_text(&mut app, 48, 16));
    }

    #[test]
    fn snapshot_wide_terminal_with_sidebar() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.sidebar.apply_event(&latch_protocol::Event {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::nil(),
            sequence: 1,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload: latch_protocol::EventPayload::ContextMaterialized {
                stats: latch_protocol::ContextStats {
                    instructions_tokens: 3_000,
                    state_tokens: 3_300,
                    recent_tokens: 37_900,
                    recall_tokens: 1_500,
                    tools_tokens: 2_400,
                    total_tokens: 48_100,
                    budget_tokens: 243_808,
                    window_tokens: 256_000,
                    reserve_tokens: 12_192,
                    headroom_tokens: 195_708,
                    durable_events: 503,
                    episodes: 11,
                    estimated: true,
                    status: "bounded".into(),
                    ..latch_protocol::ContextStats::default()
                },
            },
        });
        transcript_fixture(&mut app);
        assert_snapshot("v4_wide_sidebar.txt", &render_to_text(&mut app, 200, 40));
    }

    #[test]
    fn snapshot_working_and_interrupted_states() {
        let mut working = app_with_header("deepseek-flash", "/tmp/latch-ui");
        working.busy = true;
        assert_snapshot("v4_working.txt", &render_to_text(&mut working, 100, 20));
        let mut interrupted = app_with_header("deepseek-flash", "/tmp/latch-ui");
        interrupted.interrupted = true;
        assert_snapshot(
            "v4_interrupted.txt",
            &render_to_text(&mut interrupted, 100, 20),
        );
    }

    #[test]
    fn snapshot_long_model_and_workspace_metadata() {
        let mut app = app_with_header(
            "a-very-long-model-name-that-keeps-going-and-going-flash",
            "/home/someone/very/deeply/nested/workspace/path/that/is/long",
        );
        app.input.insert_text("check the long metadata handling");
        assert_snapshot("v4_long_metadata.txt", &render_to_text(&mut app, 160, 20));
    }

    #[test]
    fn snapshot_cjk_prompt() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text("请修复失败的测试并运行验证");
        assert_snapshot("v4_cjk_prompt.txt", &render_to_text(&mut app, 100, 20));
    }

    #[test]
    fn composer_home_end_and_ctrl_variants() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text("one\ntwo\nthree");
        app.on_key(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.input.cursor(), (2, 0), "Home is line start");
        app.on_key(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.input.cursor(), (2, 5), "End is line end");
        app.on_key(key(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor(), (0, 0), "Ctrl+Home is buffer start");
        app.on_key(key(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(app.input.cursor(), (2, 5), "Ctrl+End is buffer end");
    }

    #[test]
    fn page_keys_scroll_the_composer_only_when_it_overflows() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text(
            &(0..60)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let _ = render_to_text(&mut app, 80, 24);
        let before = app.input.viewport();
        app.on_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(
            app.input.viewport() < before,
            "overflowing composer pages upward"
        );
        // A short composer leaves PageUp with its transcript role.
        let mut short = app_with_header("deepseek-flash", "/tmp/latch-ui");
        short.sync_viewport(100, 10);
        short.scroll_up(30);
        let before = short.scroll;
        short.on_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(short.scroll, before - 10, "transcript still pages");
    }

    #[test]
    fn mouse_wheel_routing_targets_the_overflowing_composer() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        app.input.insert_text(
            &(0..40)
                .map(|index| format!("row {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let _ = render_to_text(&mut app, 80, 24);
        assert!(app.composer_body.height > 0);
        let before = app.input.viewport();
        app.composer_scroll_up(WHEEL_ROWS);
        assert!(app.input.viewport() < before);
        app.composer_scroll_down(1);
        assert!(app.input.viewport() <= before);
    }

    #[test]
    fn resize_while_editing_preserves_the_buffer_and_cursor() {
        let mut app = app_with_header("deepseek-flash", "/tmp/latch-ui");
        let text = (0..40)
            .map(|index| format!("resize line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.input.insert_text(&text);
        let _ = render_to_text(&mut app, 120, 30);
        let wide_height = app.composer_body.height;
        let _ = render_to_text(&mut app, 56, 16);
        let narrow_height = app.composer_body.height;
        assert!(narrow_height <= wide_height);
        assert_eq!(app.input.text(), text, "resize never edits the buffer");
        assert!(
            app.last_cursor.is_some(),
            "cursor stays visible after resize"
        );
        let _ = render_to_text(&mut app, 180, 44);
        assert_eq!(app.input.text(), text);
        assert!(app.last_cursor.is_some());
    }

    #[test]
    fn working_and_interrupted_states_are_visible_in_the_composer() {
        let mut working = app_with_header("deepseek-flash", "/tmp/latch-ui");
        working.busy = true;
        assert!(render_to_text(&mut working, 100, 20).contains("working"));
        let mut interrupted = app_with_header("deepseek-flash", "/tmp/latch-ui");
        interrupted.interrupted = true;
        assert!(render_to_text(&mut interrupted, 100, 20).contains("interrupted"));
    }

    #[test]
    fn terminal_screen_commands_toggle_bracketed_paste_symmetrically() {
        let mut entered = Vec::new();
        let mut left = Vec::new();
        enter_screen(&mut entered).unwrap();
        leave_screen(&mut left).unwrap();
        let entered = String::from_utf8(entered).unwrap();
        let left = String::from_utf8(left).unwrap();
        assert_eq!(entered.matches("\u{1b}[?2004h").count(), 1);
        assert_eq!(left.matches("\u{1b}[?2004l").count(), 1);
        assert!(!entered.contains("\u{1b}[?2004l"));
        assert!(!left.contains("\u{1b}[?2004h"));
    }
}
