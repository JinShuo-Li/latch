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
use latch_protocol::{
    Event as DurableEvent, Mode, PermissionMode, Safety, ToolResult, ToolRunStatus,
};
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
    /// A selector choice for the durable safety profile.
    SetSafety(Safety),
    /// A selector choice for the durable permission resolver.
    SetPermissions(PermissionMode),
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
    /// Effective safety profile after a `/safety` change or resume.
    Safety(Safety),
    /// Effective permission resolver after a `/permissions` change or resume.
    Permissions(PermissionMode),
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
        name: "/safety",
        description: "Show or switch Strict/Standard/Autonomous",
    },
    SlashCommand {
        name: "/permissions",
        description: "Show or switch the approval resolver",
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
    /// Effective safety profile, restored from durable events on resume.
    safety: Safety,
    /// Effective permission resolver, restored from durable events on resume.
    permissions: PermissionMode,
    /// An open `/safety` or `/permissions` selector above the composer.
    selector: Option<PolicySelector>,
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
    /// Capability names the operation needs, shown so the human sees the
    /// actual requested boundary rather than only the tool name.
    pub capabilities: Vec<String>,
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
            safety: Safety::default(),
            permissions: PermissionMode::default(),
            selector: None,
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
                        capabilities,
                    } => {
                        self.permission = Some(PermissionPrompt {
                            request_id: *request_id,
                            tool: tool.clone(),
                            arguments: crate::sidebar::fit(
                                &serde_json::to_string(arguments).unwrap_or_default(),
                                160,
                            ),
                            reason: reason.clone(),
                            capabilities: capabilities.clone(),
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
            Output::Safety(safety) => self.safety = safety,
            Output::Permissions(mode) => self.permissions = mode,
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
        if let Some(selector) = self.selector {
            let options = selector.kind.options();
            match key.code {
                KeyCode::Up => {
                    let selected = (selector.selected + options.len() - 1) % options.len();
                    self.selector = Some(PolicySelector {
                        selected,
                        ..selector
                    });
                    return None;
                }
                KeyCode::Down => {
                    let selected = (selector.selected + 1) % options.len();
                    self.selector = Some(PolicySelector {
                        selected,
                        ..selector
                    });
                    return None;
                }
                KeyCode::Enter => {
                    self.selector = None;
                    return options
                        .get(selector.selected)
                        .map(|(_, action)| action.clone());
                }
                KeyCode::Esc => {
                    self.selector = None;
                    return None;
                }
                _ => {
                    // Any other key closes the selector and continues normally.
                    self.selector = None;
                }
            }
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

#[derive(Debug, Clone)]
enum Action {
    Submit(String),
    Cancel,
    Resume,
    Quit,
    Permission {
        request_id: uuid::Uuid,
        approved: bool,
    },
    SetSafety(Safety),
    SetPermissions(PermissionMode),
}

/// Which policy selector is open above the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorKind {
    Safety,
    Permissions,
}

#[derive(Debug, Clone, Copy)]
struct PolicySelector {
    kind: SelectorKind,
    selected: usize,
}

impl SelectorKind {
    const fn title(self) -> &'static str {
        match self {
            Self::Safety => "Safety",
            Self::Permissions => "Permissions",
        }
    }

    fn options(self) -> Vec<(&'static str, Action)> {
        match self {
            Self::Safety => vec![
                ("Strict", Action::SetSafety(Safety::Strict)),
                ("Standard", Action::SetSafety(Safety::Standard)),
                ("Autonomous", Action::SetSafety(Safety::Autonomous)),
            ],
            Self::Permissions => vec![
                (
                    "All approved",
                    Action::SetPermissions(PermissionMode::AutoApprove),
                ),
                (
                    "Approved by ask",
                    Action::SetPermissions(PermissionMode::Human),
                ),
                (
                    "Approve for me",
                    Action::SetPermissions(PermissionMode::AiReview),
                ),
            ],
        }
    }

    fn current(self, app: &App) -> usize {
        match self {
            Self::Safety => match app.safety {
                Safety::Strict => 0,
                Safety::Standard => 1,
                Safety::Autonomous => 2,
            },
            Self::Permissions => match app.permissions {
                PermissionMode::AutoApprove => 0,
                PermissionMode::Human => 1,
                PermissionMode::AiReview => 2,
            },
        }
    }
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
        if command == "/safety" {
            self.selector = Some(PolicySelector {
                kind: SelectorKind::Safety,
                selected: SelectorKind::Safety.current(self),
            });
            return None;
        }
        if command == "/permissions" {
            self.selector = Some(PolicySelector {
                kind: SelectorKind::Permissions,
                selected: SelectorKind::Permissions.current(self),
            });
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

/// A restrained selector for `/safety` and `/permissions`, rendered above the
/// palette slot with the same visual language as the command palette.
fn draw_policy_selector(frame: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
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
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let marker = if selected { "› " } else { "  " };
        rows.push(Line::styled(format!("  {marker}{label}"), style));
    }
    frame.render_widget(Paragraph::new(rows), area);
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
/// the same six columns so the rows align without per-letter padding; the
/// extra width keeps the glyphs from looking narrow against terminal cells,
/// which are roughly twice as tall as they are wide.
const WORDMARK: [(&str, [&str; 5]); 5] = [
    ("L", ["███   ", "███   ", "███   ", "███   ", "██████"]),
    ("A", [" ████ ", "██  ██", "██████", "██  ██", "██  ██"]),
    ("T", ["██████", "  ██  ", "  ██  ", "  ██  ", "  ██  "]),
    ("C", [" █████", "██    ", "██    ", "██    ", " █████"]),
    ("H", ["██  ██", "██  ██", "██████", "██  ██", "██  ██"]),
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
                Some(Action::SetSafety(safety)) => {
                    input_tx.send(Input::SetSafety(safety)).await?;
                }
                Some(Action::SetPermissions(mode)) => {
                    input_tx.send(Input::SetPermissions(mode)).await?;
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
#[cfg(test)]
mod tests;
