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
    Event as DurableEvent, MediaRef, Mode, PermissionMode, ReasoningEffort, Safety, ToolResult,
    ToolRunStatus,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::collections::BTreeMap;
use std::io::{self, Stdout, Write};
use tokio::sync::mpsc;

mod agents;
mod composer;
pub mod configuration_center;
mod diff;
mod group;
mod presentation;
mod profile;
mod session_picker;
mod sidebar;
use composer::display_width;
pub use composer::{Composer, VisualRow};
use configuration_center::{CenterAction, ConfigurationCenter, KnownProviderFlow, ProviderSummary};
pub use diff::{DiffDocument, DiffFile, DiffHunk, DiffLine, DiffLineKind, parse_unified_diff};
pub use presentation::{
    AgentOperation, Cell, CellStatus, ExplorationOperation, PatchFile, PresentationModel,
};
pub use profile::{
    CaptureSpec, CatalogModel, CatalogProvider, ChoiceRow, ConfiguredProvider, InferenceCatalog,
    ProfileSelector, SetupCredential, SetupFlow, SetupKind, SetupPlan, SetupStep, SetupStepOutcome,
};
pub use session_picker::{PickerSelection, SessionItem, SessionPreviewLine, run_session_picker};
pub use sidebar::{Pricing, SidebarModel, SidebarSession};

/// Visual rows moved per mouse wheel event.
mod chrome;
mod markdown;
mod runtime;
mod theme;
mod transcript;

pub use runtime::run;
pub use transcript::render_cells_plain;

const WHEEL_ROWS: usize = 3;
/// Maximum slash palette entries shown at once.
const MAX_PALETTE_ROWS: usize = 6;

#[derive(Debug)]
pub enum Input {
    Submit {
        text: String,
        media: Vec<MediaRef>,
    },
    /// Ask the kernel to validate and ingest one image path as a pending
    /// attachment. The kernel owns artifact ingestion; the TUI never reads
    /// image bytes itself.
    Attach(String),
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
    /// The live inference-profile selector chose a provider/model/effort.
    SetInferenceProfile {
        provider: String,
        model: String,
        effort: ReasoningEffort,
    },
    /// The `/setup` flow completed and should be persisted and applied.
    SetupApply(SetupPlan),
    DiscoverModels {
        provider: String,
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
    /// Effective safety profile after a `/safety` change or resume.
    Safety(Safety),
    /// Effective permission resolver after a `/permissions` change or resume.
    Permissions(PermissionMode),
    Header {
        model: String,
        /// Friendly provider label, for example `OpenCode Go` or `Anthropic`.
        provider: String,
        /// Stable configured provider id, for example `opencode-go`.
        provider_id: String,
        /// Effective reasoning effort of the session.
        effort: ReasoningEffort,
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
    /// One validated image attachment, ingested by the kernel into immutable
    /// artifact storage. The composer shows it as a pending attachment.
    Attachment(MediaRef),
    /// Provider-neutral catalog for the live `/model` selector.
    InferenceCatalog(InferenceCatalog),
    /// Provider kinds and built-in models available to `/setup`.
    SetupCatalog(Vec<SetupKind>),
    SetupProviders(Vec<ProviderSummary>),
    SetupModels {
        provider: String,
        ids: Vec<String>,
    },
    /// The provider cannot run until setup completes; open the guided flow.
    SetupRequired,
    /// The effective profile changed; update model/effort chrome.
    Inference {
        provider_id: String,
        provider_label: String,
        model: String,
        effort: ReasoningEffort,
    },
    /// A symbolic credential reference for a configured provider, used by
    /// `/setup` to prefill without ever exposing a value.
    CredentialLabel {
        provider_id: String,
        label: String,
    },
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
        name: "/attach",
        description: "Attach an image file to the next prompt",
    },
    SlashCommand {
        name: "/attachments",
        description: "List pending image attachments",
    },
    SlashCommand {
        name: "/detach",
        description: "Remove a pending attachment by index, or all",
    },
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
        description: "Select provider, model, and reasoning effort",
    },
    SlashCommand {
        name: "/setup",
        description: "Configure a provider and credential",
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
        name: "/group",
        description: "Show agent-group coordination state",
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
    /// Stable configured provider id of the effective profile.
    provider_id: String,
    /// Effective reasoning effort of the effective profile.
    effort: ReasoningEffort,
    /// Provider-neutral catalog for `/model`, sent by the CLI at startup.
    inference_catalog: InferenceCatalog,
    /// Open live inference-profile selector.
    profile_selector: Option<ProfileSelector>,
    /// Provider kinds available to `/setup`.
    setup_catalog: Vec<SetupKind>,
    setup_providers: Vec<ProviderSummary>,
    setup_discovered: BTreeMap<String, Vec<String>>,
    setup_center: Option<ConfigurationCenter>,
    known_setup: Option<KnownProviderFlow>,
    /// Open guided setup flow.
    setup: Option<SetupFlow>,
    /// A composer text capture opened by the setup flow.
    capture: Option<CaptureState>,
    /// Images validated and ingested by the kernel, waiting to be sent with
    /// the next user turn. Metadata only; bytes stay in artifact storage.
    attachments: Vec<MediaRef>,
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
    /// Full argument payload, retained verbatim so the human can inspect
    /// exactly what is being approved instead of a truncated preview.
    pub arguments: String,
    pub reason: String,
    /// Capability names the operation needs, shown so the human sees the
    /// actual requested boundary rather than only the tool name.
    pub capabilities: Vec<String>,
    /// Highlighted option: 0 approves, 1 denies.
    pub selected: usize,
    /// Full-request inspection mode (`Ctrl+O`).
    pub expanded: bool,
    pub scroll: usize,
}

/// One enabled or disabled choice in a bottom action surface.
#[derive(Debug, Clone)]
pub(crate) struct ActionOption {
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: Action,
}

/// An open composer text capture opened by the setup flow. Masked captures
/// never render their value and are only sent to the CLI on submit.
#[derive(Clone, PartialEq, Eq)]
pub struct CaptureState {
    pub spec: CaptureSpec,
    pub value: String,
}

impl std::fmt::Debug for CaptureState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureState")
            .field("label", &self.spec.label)
            .field(
                "value",
                &if self.spec.masked {
                    "[redacted]"
                } else {
                    &self.value
                },
            )
            .finish()
    }
}

impl CaptureState {
    #[must_use]
    pub fn new(spec: CaptureSpec) -> Self {
        let value = spec.initial.clone();
        Self { spec, value }
    }

    #[must_use]
    pub fn display_value(&self) -> String {
        if self.spec.masked {
            "•".repeat(self.value.chars().count().min(64))
        } else {
            self.value.clone()
        }
    }
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
            provider_id: String::new(),
            effort: ReasoningEffort::default(),
            inference_catalog: InferenceCatalog::default(),
            profile_selector: None,
            setup_catalog: Vec::new(),
            setup_providers: Vec::new(),
            setup_discovered: BTreeMap::new(),
            setup_center: None,
            known_setup: None,
            setup: None,
            capture: None,
            attachments: Vec::new(),
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
        if let Some(capture) = self.capture.as_mut() {
            capture.value.push_str(text);
            return;
        }
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
                            arguments: serde_json::to_string_pretty(arguments).unwrap_or_else(
                                |_| serde_json::to_string(arguments).unwrap_or_default(),
                            ),
                            reason: reason.clone(),
                            capabilities: capabilities.clone(),
                            selected: 0,
                            expanded: false,
                            scroll: 0,
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
                provider_id,
                effort,
                workspace,
                branch,
                resumed,
                pricing,
            } => {
                self.model = model.clone();
                self.provider = provider;
                self.provider_id = provider_id;
                self.effort = effort;
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
            Output::Attachment(media) => {
                self.attachments.push(media);
            }
            Output::InferenceCatalog(catalog) => self.inference_catalog = catalog,
            Output::SetupCatalog(kinds) => self.setup_catalog = kinds,
            Output::SetupProviders(mut providers) => {
                for row in &mut providers {
                    if let Some(ids) = self.setup_discovered.get(&row.id) {
                        row.merge_discovered(ids);
                    }
                }
                self.setup_providers = providers;
            }
            Output::SetupModels { provider, ids } => {
                self.setup_discovered.insert(provider.clone(), ids.clone());
                if let Some(row) = self
                    .setup_providers
                    .iter_mut()
                    .find(|row| row.id == provider)
                {
                    row.merge_discovered(&ids);
                }
                if let Some(center) = self.setup_center.as_mut() {
                    center.merge_discovered(&provider, &ids);
                }
            }
            Output::SetupRequired => {
                if !self.setup_catalog.is_empty() {
                    self.setup_center = Some(ConfigurationCenter::new(
                        self.setup_providers.clone(),
                        self.setup_catalog.clone(),
                    ));
                }
            }
            Output::Inference {
                provider_id,
                provider_label,
                model,
                effort,
            } => {
                self.provider_id = provider_id;
                self.provider = provider_label;
                self.model = model.clone();
                self.effort = effort;
                let mut session = self.sidebar.session().clone();
                session.model = model;
                self.sidebar.set_session(session);
            }
            Output::CredentialLabel { .. } => {}
        }
    }

    #[cfg(test)]
    fn apply_item(&mut self, item: DisplayItem) {
        match item {
            DisplayItem::UserMessage { text, .. } => self.items.push(TranscriptItem::User { text }),
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
        // A pending approval owns the keyboard: select, approve, deny, inspect
        // the full request, or cancel the turn.
        if self.permission.is_some() {
            let decision = {
                let prompt = self.permission.as_mut().expect("checked above");
                if prompt.expanded {
                    match key.code {
                        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            prompt.expanded = false;
                            prompt.scroll = 0;
                        }
                        KeyCode::Esc => {
                            prompt.expanded = false;
                            prompt.scroll = 0;
                        }
                        KeyCode::Up => prompt.scroll = prompt.scroll.saturating_sub(1),
                        KeyCode::Down => prompt.scroll = prompt.scroll.saturating_add(1),
                        KeyCode::PageUp => prompt.scroll = prompt.scroll.saturating_sub(10),
                        KeyCode::PageDown => prompt.scroll = prompt.scroll.saturating_add(10),
                        KeyCode::Home => prompt.scroll = 0,
                        KeyCode::End => prompt.scroll = usize::MAX,
                        _ => {}
                    }
                    None
                } else {
                    match key.code {
                        KeyCode::Up => {
                            prompt.selected = 0;
                            None
                        }
                        KeyCode::Down => {
                            prompt.selected = 1;
                            None
                        }
                        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            prompt.expanded = true;
                            prompt.scroll = 0;
                            None
                        }
                        KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
                        KeyCode::Enter => Some(prompt.selected == 0),
                        _ => None,
                    }
                }
            };
            if let Some(approved) = decision {
                let request_id = self
                    .permission
                    .as_ref()
                    .map(|prompt| prompt.request_id)
                    .expect("checked above");
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
        // Setup text capture owns the keyboard until submitted or cancelled.
        if self.capture.is_some() {
            let mut submit = false;
            let mut cancel = false;
            {
                let capture = self.capture.as_mut().expect("checked above");
                match key.code {
                    KeyCode::Esc => cancel = true,
                    KeyCode::Enter => submit = true,
                    KeyCode::Backspace => {
                        capture.value.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        capture.value.clear();
                    }
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        capture.value.push(ch);
                    }
                    _ => {}
                }
            }
            if cancel {
                self.capture = None;
                if let Some(flow) = self.known_setup.as_mut() {
                    flow.back();
                }
                if let Some(flow) = self.setup.as_mut() {
                    flow.back();
                }
            } else if submit {
                let finished = self.capture.take().expect("checked above");
                if let Some(flow) = self.known_setup.as_mut() {
                    flow.submit_capture(finished.value);
                } else if let Some(flow) = self.setup.as_mut() {
                    flow.submit_capture(finished.value);
                } else if let Some(center) = self.setup_center.as_ref() {
                    if let Some((name, credential)) = center.credential_from_capture(finished.value)
                    {
                        self.setup_center = None;
                        return Some(Action::SetupApply(SetupPlan::SetCredential {
                            name,
                            credential,
                        }));
                    }
                }
            }
            return None;
        }
        if let Some(flow) = self.known_setup.as_mut() {
            let outcome = match key.code {
                KeyCode::Esc => {
                    if !flow.back() {
                        self.known_setup = None;
                    }
                    SetupStepOutcome::None
                }
                KeyCode::Backspace => {
                    flow.back();
                    SetupStepOutcome::None
                }
                KeyCode::Up => {
                    flow.up();
                    SetupStepOutcome::None
                }
                KeyCode::Down => {
                    flow.down();
                    SetupStepOutcome::None
                }
                KeyCode::Enter | KeyCode::Char(' ') => flow.confirm(),
                _ => SetupStepOutcome::None,
            };
            match outcome {
                SetupStepOutcome::Capture(spec) => self.capture = Some(CaptureState::new(spec)),
                SetupStepOutcome::Apply(plan) => {
                    self.known_setup = None;
                    self.setup_center = None;
                    return Some(Action::SetupApply(plan));
                }
                SetupStepOutcome::Cancel => self.known_setup = None,
                SetupStepOutcome::None => {}
            }
            return None;
        }
        // The guided `/setup` flow.
        if self.setup.is_some() {
            let outcome = {
                let flow = self.setup.as_mut().expect("checked above");
                match key.code {
                    KeyCode::Esc => {
                        if !flow.back() {
                            Some(SetupStepOutcome::Cancel)
                        } else {
                            None
                        }
                    }
                    KeyCode::Backspace => {
                        flow.back();
                        None
                    }
                    KeyCode::Up => {
                        flow.up();
                        None
                    }
                    KeyCode::Down => {
                        flow.down();
                        None
                    }
                    KeyCode::Enter => Some(flow.confirm()),
                    _ => None,
                }
            };
            if let Some(outcome) = outcome {
                match outcome {
                    SetupStepOutcome::None => {}
                    SetupStepOutcome::Capture(spec) => {
                        self.capture = Some(CaptureState::new(spec));
                    }
                    SetupStepOutcome::Apply(plan) => {
                        self.setup = None;
                        self.setup_center = None;
                        return Some(Action::SetupApply(plan));
                    }
                    SetupStepOutcome::Cancel => self.setup = None,
                }
            }
            return None;
        }
        if let Some(center) = self.setup_center.as_mut() {
            let action = match key.code {
                KeyCode::Esc => {
                    if !center.back() {
                        self.setup_center = None;
                    }
                    None
                }
                KeyCode::Backspace => {
                    center.back();
                    None
                }
                KeyCode::Up => {
                    center.up();
                    None
                }
                KeyCode::Down => {
                    center.down();
                    None
                }
                KeyCode::Enter => center.confirm(),
                _ => None,
            };
            match action {
                Some(CenterAction::StartAdd(kind)) => {
                    if let Some(selected) = self.setup_catalog.iter().find(|item| item.kind == kind)
                    {
                        if selected.requires_base_url {
                            let mut flow = SetupFlow::new(vec![selected.clone()]);
                            if let SetupStepOutcome::Capture(spec) = flow.confirm() {
                                self.capture = Some(CaptureState::new(spec));
                            }
                            self.setup = Some(flow);
                        } else {
                            self.known_setup = Some(KnownProviderFlow::new(selected.clone()));
                        }
                    }
                }
                Some(CenterAction::EditCredential { name, secret }) => {
                    let initial = if secret {
                        String::new()
                    } else {
                        self.setup_providers
                            .iter()
                            .find(|provider| provider.id == name)
                            .and_then(|provider| provider.credential_ref.strip_prefix("env:"))
                            .unwrap_or("")
                            .to_owned()
                    };
                    self.capture = Some(CaptureState::new(CaptureSpec {
                        label: if secret {
                            "API key".to_owned()
                        } else {
                            "environment variable".to_owned()
                        },
                        initial,
                        masked: secret,
                    }));
                }
                Some(CenterAction::Remove(name)) => {
                    self.setup_center = None;
                    return Some(Action::SetupApply(SetupPlan::Remove { name }));
                }
                Some(CenterAction::SetNewSessionDefault(name)) => {
                    self.setup_center = None;
                    return Some(Action::SetupApply(SetupPlan::SetNewSessionDefault { name }));
                }
                Some(CenterAction::SetProviderDefault { name, model }) => {
                    self.setup_center = None;
                    return Some(Action::SetupApply(SetupPlan::SetProviderDefault {
                        name,
                        model,
                    }));
                }
                Some(CenterAction::SetEnabledModels { name, models }) => {
                    self.setup_center = None;
                    return Some(Action::SetupApply(SetupPlan::SetEnabledModels {
                        name,
                        models,
                    }));
                }
                Some(CenterAction::DiscoverModels(provider)) => {
                    return Some(Action::DiscoverModels { provider });
                }
                Some(other) => {
                    self.presentation
                        .push_notice(format!("{other:?} is not available yet"));
                }
                None => {}
            }
            return None;
        }
        // The live inference-profile selector.
        if self.profile_selector.is_some() {
            let outcome = {
                let selector = self.profile_selector.as_mut().expect("checked above");
                match key.code {
                    KeyCode::Esc => Err(()),
                    KeyCode::Backspace => {
                        if selector.back() {
                            Ok(None)
                        } else {
                            Err(())
                        }
                    }
                    KeyCode::Up => {
                        selector.up();
                        Ok(None)
                    }
                    KeyCode::Down => {
                        selector.down();
                        Ok(None)
                    }
                    KeyCode::Enter => Ok(selector.confirm()),
                    _ => Ok(None),
                }
            };
            match outcome {
                Err(()) => {
                    self.profile_selector = None;
                    self.presentation.push_notice("inference profile unchanged");
                }
                Ok(Some((provider, model, effort))) => {
                    self.profile_selector = None;
                    return Some(Action::SetInferenceProfile {
                        provider,
                        model,
                        effort,
                    });
                }
                Ok(None) => {}
            }
            return None;
        }
        if self.diff_overlay.is_some()
            && !(key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
        {
            return self.on_diff_key(key);
        }
        if let Some(selector) = self.selector {
            let options = selector.kind.options();
            let disabled = selector.kind.disabled_reason(self);
            match key.code {
                KeyCode::Up if disabled.is_none() => {
                    let selected = (selector.selected + options.len() - 1) % options.len();
                    self.selector = Some(PolicySelector {
                        selected,
                        ..selector
                    });
                    return None;
                }
                KeyCode::Down if disabled.is_none() => {
                    let selected = (selector.selected + 1) % options.len();
                    self.selector = Some(PolicySelector {
                        selected,
                        ..selector
                    });
                    return None;
                }
                KeyCode::Enter if disabled.is_none() => {
                    self.selector = None;
                    return options
                        .get(selector.selected)
                        .map(|option| option.action.clone());
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
pub(crate) enum Action {
    Submit {
        text: String,
        media: Vec<MediaRef>,
    },
    Attach(String),
    Cancel,
    Resume,
    Quit,
    Permission {
        request_id: uuid::Uuid,
        approved: bool,
    },
    SetSafety(Safety),
    SetPermissions(PermissionMode),
    SetInferenceProfile {
        provider: String,
        model: String,
        effort: ReasoningEffort,
    },
    SetupApply(SetupPlan),
    DiscoverModels {
        provider: String,
    },
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

    fn options(self) -> Vec<ActionOption> {
        match self {
            Self::Safety => vec![
                ActionOption {
                    label: "Strict",
                    description: "Ask before every workspace write.",
                    action: Action::SetSafety(Safety::Strict),
                },
                ActionOption {
                    label: "Standard",
                    description: "Allow ordinary source edits; ask for risky effects.",
                    action: Action::SetSafety(Safety::Standard),
                },
                ActionOption {
                    label: "Autonomous",
                    description: "Pre-grant network access; still classify external effects.",
                    action: Action::SetSafety(Safety::Autonomous),
                },
            ],
            Self::Permissions => vec![
                ActionOption {
                    label: "Ask for approval",
                    description: "Latch asks before operations that require approval.",
                    action: Action::SetPermissions(PermissionMode::Human),
                },
                ActionOption {
                    label: "Approve for me",
                    description: "Latch's reviewer decides eligible requests on your behalf.",
                    action: Action::SetPermissions(PermissionMode::AiReview),
                },
                ActionOption {
                    label: "Auto approve",
                    description: "Eligible operations proceed automatically with recorded provenance.",
                    action: Action::SetPermissions(PermissionMode::AutoApprove),
                },
            ],
        }
    }

    /// All modes are always meaningful; only a live turn makes the choice
    /// unavailable, because the CLI refuses to change policy mid-run.
    fn disabled_reason(self, app: &App) -> Option<&'static str> {
        app.busy.then_some("active turn")
    }

    fn current(self, app: &App) -> usize {
        match self {
            Self::Safety => match app.safety {
                Safety::Strict => 0,
                Safety::Standard => 1,
                Safety::Autonomous => 2,
            },
            Self::Permissions => match app.permissions {
                PermissionMode::Human => 0,
                PermissionMode::AiReview => 1,
                PermissionMode::AutoApprove => 2,
            },
        }
    }
}

impl App {
    /// Whether the currently selected model accepts image input, when the
    /// catalog knows the model. `None` means unknown; the kernel re-checks
    /// every request, so this is only an early, friendlier rejection.
    fn current_model_supports_images(&self) -> Option<bool> {
        let provider = self
            .inference_catalog
            .providers
            .iter()
            .find(|provider| provider.id == self.provider_id)?;
        let model = provider
            .models
            .iter()
            .find(|model| model.id == self.model)?;
        Some(model.supports_image_input())
    }

    /// Compact single-line pending-attachment summary for the composer.
    fn attachment_summary(&self) -> Option<String> {
        if self.attachments.is_empty() {
            return None;
        }
        Some(
            self.attachments
                .iter()
                .map(MediaRef::compact_label)
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// Handles the local `/attach`, `/attachments`, and `/detach` commands.
    /// `/attach` is delegated to the kernel (which validates and ingests);
    /// listing and removal are pure composer state. All three work while a run
    /// is active so an image can be attached to a steering message.
    fn attachment_command(&mut self, command: &str) -> Option<Option<Action>> {
        if command == "/attach" {
            self.presentation
                .push_notice("usage: /attach <path> (PNG, JPEG, or WebP)");
            return Some(None);
        }
        if let Some(path) = command.strip_prefix("/attach ") {
            let path = path.trim().trim_matches(['"', '\'']);
            if path.is_empty() {
                self.presentation
                    .push_notice("usage: /attach <path> (PNG, JPEG, or WebP)");
                return Some(None);
            }
            return Some(Some(Action::Attach(path.to_owned())));
        }
        if command == "/attachments" {
            let message = match self.attachment_summary() {
                Some(summary) => format!("pending attachments: {summary}"),
                None => "no pending attachments".to_owned(),
            };
            self.presentation.push_notice(message);
            return Some(None);
        }
        if command == "/detach" {
            self.presentation.push_notice("usage: /detach <index|all>");
            return Some(None);
        }
        if let Some(argument) = command.strip_prefix("/detach ") {
            let argument = argument.trim();
            if argument.eq_ignore_ascii_case("all") {
                let removed = self.attachments.len();
                self.attachments.clear();
                self.presentation
                    .push_notice(format!("detached {removed} image(s)"));
                return Some(None);
            }
            match argument.parse::<usize>() {
                Ok(index) if index >= 1 && index <= self.attachments.len() => {
                    let removed = self.attachments.remove(index - 1);
                    self.presentation
                        .push_notice(format!("detached {}", removed.compact_label()));
                }
                _ => {
                    let count = self.attachments.len();
                    self.presentation
                        .push_notice(format!("no attachment {argument:?} (1..={count})"));
                }
            }
            return Some(None);
        }
        None
    }

    fn submit_action(&mut self) -> Option<Action> {
        self.palette.forced = false;
        let text = self.input.text();
        let command = text.trim();
        let slash_command = !text.contains(['\n', '\r']) && command.starts_with('/');
        if command.is_empty() && self.attachments.is_empty() {
            self.input.take_for_submit();
            return None;
        }
        if slash_command && let Some(action) = self.attachment_command(command) {
            self.input.take_for_submit();
            return action;
        }
        if matches!(command, "/quit" | "/exit") {
            self.input.take_for_submit();
            return Some(Action::Quit);
        }
        if command == "/resume" && !self.busy {
            self.input.take_for_submit();
            return Some(Action::Resume);
        }
        if command == "/raw" {
            self.input.take_for_submit();
            self.detail = !self.detail;
            return None;
        }
        if command == "/sidebar" {
            self.input.take_for_submit();
            self.toggle_sidebar();
            return None;
        }
        if command == "/context" {
            self.presentation
                .push_notice(self.sidebar.advanced_context_text());
        }
        if command == "/safety" {
            self.input.take_for_submit();
            self.selector = Some(PolicySelector {
                kind: SelectorKind::Safety,
                selected: SelectorKind::Safety.current(self),
            });
            return None;
        }
        if command == "/permissions" {
            self.input.take_for_submit();
            self.selector = Some(PolicySelector {
                kind: SelectorKind::Permissions,
                selected: SelectorKind::Permissions.current(self),
            });
            return None;
        }
        if command == "/model" {
            self.input.take_for_submit();
            if self.busy {
                self.presentation
                    .push_notice("finish or cancel the active turn before switching model");
                return None;
            }
            if self.inference_catalog.providers.is_empty() {
                self.presentation
                    .push_notice("no provider catalog is available; run /setup");
                return None;
            }
            self.profile_selector = Some(ProfileSelector::new(
                self.inference_catalog.clone(),
                &self.provider_id,
                &self.model,
                self.effort,
            ));
            return None;
        }
        if command == "/setup" {
            self.input.take_for_submit();
            if self.busy {
                self.presentation
                    .push_notice("finish or cancel the active turn before setup");
                return None;
            }
            if self.setup_catalog.is_empty() {
                self.presentation
                    .push_notice("provider setup is unavailable in this session");
                return None;
            }
            self.setup_center = Some(ConfigurationCenter::new(
                self.setup_providers.clone(),
                self.setup_catalog.clone(),
            ));
            return None;
        }
        // A pending image must never be silently dropped for a model the
        // catalog knows is text-only; keep the prompt and attachments intact so
        // the user can switch models. The kernel re-checks authoritatively.
        if !slash_command
            && !self.attachments.is_empty()
            && self.current_model_supports_images() == Some(false)
        {
            self.presentation.push_notice(
                "Current model does not accept image input. Choose a vision-capable model with /model.",
            );
            return None;
        }
        let text = self.input.take_for_submit();
        let media = std::mem::take(&mut self.attachments);
        if !slash_command {
            self.busy = true;
        }
        Some(Action::Submit { text, media })
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

#[cfg(test)]
mod tests;
