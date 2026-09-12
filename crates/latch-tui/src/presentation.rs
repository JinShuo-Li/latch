//! Deterministic semantic presentation of durable kernel events.
//!
//! The kernel records truth; this reducer decides what that truth means to a
//! person. It deliberately contains no Ratatui types, so live events and a
//! replayed event stream produce the same committed cells.

use crate::agents::is_agent_control;
use crate::diff::{DiffDocument, parse_unified_diff};
use latch_protocol::{AgentStatus, ChangeOwner, Event, EventPayload, ToolCall, ToolResult};
use serde_json::Value;

const DEFAULT_OUTPUT_LINES: usize = 8;
const DEFAULT_OUTPUT_CHARS: usize = 1_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    Running,
    Passed,
    Failed,
}

/// Root-visible child-agent coordination operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOperation {
    Spawn,
    Send,
    Continue,
    Wait,
    List,
    Interrupt,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorationOperation {
    pub call_id: String,
    pub label: String,
    pub status: CellStatus,
    pub diagnostic: String,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFile {
    pub call_id: String,
    pub path: String,
    pub kind: char,
    pub additions: usize,
    pub deletions: usize,
    pub status: CellStatus,
    pub diagnostic: String,
    pub raw: String,
    /// Bounded unified diff computed by the kernel from the real before/after
    /// bytes. Empty while the change has not landed yet.
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Exploration {
        operations: Vec<ExplorationOperation>,
    },
    Command {
        call_id: String,
        command: String,
        status: CellStatus,
        summary: String,
        output: String,
        raw: String,
    },
    Validation {
        call_id: String,
        command: String,
        requirement: String,
        status: CellStatus,
        summary: String,
        output: String,
        raw: String,
    },
    Patch {
        files: Vec<PatchFile>,
    },
    /// A first-class workspace diff (the `git_diff` tool or `/diff`). The
    /// raw document is retained verbatim for the detail view and copy/paste.
    Diff {
        call_id: String,
        status: CellStatus,
        document: DiffDocument,
    },
    /// One root-visible child-agent coordination call. Compact by design: the
    /// child transcript stays in its own session.
    AgentTask {
        call_id: String,
        operation: AgentOperation,
        task_name: String,
        status: CellStatus,
        summary: String,
        diagnostic: String,
        raw: String,
    },
    /// The single semantic report that crosses from a finished child turn.
    AgentReport {
        task_name: String,
        status: AgentStatus,
        summary: String,
    },
    Notice {
        text: String,
    },
    Error {
        text: String,
    },
}

impl Cell {
    #[must_use]
    pub fn raw_text(&self) -> String {
        match self {
            Self::User { text } => format!("user: {text}"),
            Self::Assistant { text } => format!("assistant: {text}"),
            Self::Exploration { operations } => operations
                .iter()
                .map(|op| format!("{}\n{}", op.label, op.raw))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Command { command, raw, .. } => format!("$ {command}\n{raw}"),
            Self::Validation {
                command,
                requirement,
                raw,
                ..
            } => format!("validate {requirement}: {command}\n{raw}"),
            Self::Patch { files } => files
                .iter()
                .map(|file| {
                    if file.preview.is_empty() {
                        format!("{} {}\n{}", file.kind, file.path, file.raw)
                    } else {
                        format!(
                            "{} {}\n{}\n{}",
                            file.kind, file.path, file.raw, file.preview
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Diff { document, .. } => document.raw.clone(),
            Self::AgentTask { raw, .. } => raw.clone(),
            Self::AgentReport {
                task_name,
                status,
                summary,
            } => format!(
                "child agent {task_name} {}: {summary}",
                agent_status_label(status)
            ),
            Self::Notice { text } | Self::Error { text } => text.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresentationModel {
    cells: Vec<Cell>,
}

impl PresentationModel {
    #[must_use]
    pub fn from_events(events: &[Event]) -> Self {
        let mut model = Self::default();
        for event in events {
            model.apply_event(event);
        }
        model
    }

    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.cells.push(Cell::Notice { text: text.into() });
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.cells.push(Cell::Error { text: text.into() });
    }

    pub fn apply_event(&mut self, event: &Event) {
        match &event.payload {
            EventPayload::UserMessage { text } => {
                self.cells.push(Cell::User { text: text.clone() })
            }
            EventPayload::AssistantMessageCompleted { text, .. } if !text.trim().is_empty() => {
                self.cells.push(Cell::Assistant { text: text.clone() });
            }
            EventPayload::ToolRequested { call } => self.begin_tool(call),
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                self.finish_tool(result)
            }
            EventPayload::ValidationResult {
                command,
                passed,
                detail,
            } => self.finish_validation(command, *passed, detail),
            EventPayload::FileChanged {
                after,
                created: false,
                owner: ChangeOwner::Latch,
                additions,
                deletions,
                preview,
                call_id,
                ..
            } => {
                self.update_patch_change(after, *additions, *deletions, preview, call_id.as_deref())
            }
            EventPayload::ExternalFileChangeDetected { path, .. } => self.cells.push(Cell::Error {
                text: format!("{path} changed since it was inspected; re-reading before editing."),
            }),
            EventPayload::ModeChanged { mode } => {
                self.push_notice(format!("Mode switched to {mode}"))
            }
            // Resume is chrome state, not a transcript event; hiding this also
            // keeps the committed transcript invariant across repeated opens.
            EventPayload::SessionResumed => {}
            EventPayload::OperationInterrupted { description, .. } => self.push_error(format!(
                "The previous run ended during {description}; its outcome may be incomplete."
            )),
            // ScopeExpansionRequested is a legacy, replay-only kernel event;
            // the restriction it represented has been removed.
            EventPayload::RegroundRequested { .. } => {
                self.push_notice("Repeated failure; re-inspecting relevant context");
            }
            EventPayload::ProgressStagnation { unchanged, .. } => self.push_notice(format!(
                "Inspection stagnation; re-grounding the model ({} unchanged observation(s))",
                unchanged.len()
            )),
            EventPayload::ManualCompact { .. } => {
                self.push_notice("Active context reset; durable history retained");
            }
            EventPayload::ChangeReverted { path, .. } => {
                self.push_notice(format!("Reverted {path}"));
            }
            EventPayload::ShellMutationObserved { paths, .. } if !paths.is_empty() => {
                self.push_notice(format!("Command changed {}", paths.join(", ")));
            }
            // The only child-agent event that exists in root history: the
            // compact semantic report delivered at a safe model boundary. It
            // renders as one compact cell; the child transcript stays in its
            // own session.
            EventPayload::AgentNotificationDelivered { report } => {
                let summary = report
                    .summary
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                self.cells.push(Cell::AgentReport {
                    task_name: report.task_name.clone(),
                    status: report.status,
                    summary,
                });
            }
            _ => {}
        }
    }

    /// Live tool results use this same terminal transition as replayed
    /// ToolCompleted/ToolFailed events. Applying both is harmless: call ids
    /// update in place.
    pub fn apply_tool_result(&mut self, result: &ToolResult) {
        self.finish_tool(result);
    }

    fn begin_tool(&mut self, call: &ToolCall) {
        if is_hidden_tool(&call.name) {
            return;
        }
        if let Some(operation) = agent_operation(&call.name) {
            let task_name = string_arg(&call.arguments, "task_name")
                .map(str::to_owned)
                .or_else(|| {
                    string_arg(&call.arguments, "agent_id").map(|id| id.chars().take(8).collect())
                })
                .unwrap_or_else(|| "child agent".to_owned());
            self.cells.push(Cell::AgentTask {
                call_id: call.id.clone(),
                operation,
                task_name,
                status: CellStatus::Running,
                summary: String::new(),
                diagnostic: String::new(),
                raw: format!("{} {}", call.name, call.arguments),
            });
            return;
        }
        if call.name == "git_diff" {
            self.cells.push(Cell::Diff {
                call_id: call.id.clone(),
                status: CellStatus::Running,
                document: parse_unified_diff(""),
            });
            return;
        }
        if let Some(label) = exploration_label(call) {
            let operation = ExplorationOperation {
                call_id: call.id.clone(),
                label,
                status: CellStatus::Running,
                diagnostic: String::new(),
                raw: format!("{} {}", call.name, call.arguments),
            };
            if let Some(Cell::Exploration { operations }) = self.cells.last_mut() {
                operations.push(operation);
            } else {
                self.cells.push(Cell::Exploration {
                    operations: vec![operation],
                });
            }
            return;
        }
        if matches!(call.name.as_str(), "patch" | "write") {
            let path = string_arg(&call.arguments, "path")
                .unwrap_or("file")
                .to_owned();
            let (additions, deletions) = patch_delta(call);
            let file = PatchFile {
                call_id: call.id.clone(),
                path,
                kind: if call.name == "write" && call.arguments.get("base_hash").is_none() {
                    'A'
                } else {
                    'M'
                },
                additions,
                deletions,
                status: CellStatus::Running,
                diagnostic: String::new(),
                raw: format!("{} {}", call.name, call.arguments),
                preview: String::new(),
            };
            if let Some(Cell::Patch { files }) = self.cells.last_mut() {
                files.push(file);
            } else {
                self.cells.push(Cell::Patch { files: vec![file] });
            }
            return;
        }
        let command = string_arg(&call.arguments, "command")
            .or_else(|| string_arg(&call.arguments, "requirement"))
            .unwrap_or(&call.name)
            .to_owned();
        if call.name == "validate" {
            self.cells.push(Cell::Validation {
                call_id: call.id.clone(),
                command: string_arg(&call.arguments, "command")
                    .unwrap_or("")
                    .to_owned(),
                requirement: string_arg(&call.arguments, "requirement")
                    .unwrap_or("")
                    .to_owned(),
                status: CellStatus::Running,
                summary: String::new(),
                output: String::new(),
                raw: format!("validate {}", call.arguments),
            });
        } else {
            self.cells.push(Cell::Command {
                call_id: call.id.clone(),
                command,
                status: CellStatus::Running,
                summary: String::new(),
                output: String::new(),
                raw: format!("{} {}", call.name, call.arguments),
            });
        }
    }

    fn finish_tool(&mut self, result: &ToolResult) {
        if is_hidden_tool(&result.name) {
            return;
        }
        let status = if result.is_error {
            CellStatus::Failed
        } else {
            CellStatus::Passed
        };
        let clean = sanitize_output(&result.output);
        let bounded = bounded_output(&clean);
        for cell in self.cells.iter_mut().rev() {
            match cell {
                Cell::AgentTask {
                    call_id,
                    status: cell_status,
                    summary,
                    diagnostic,
                    ..
                } if *call_id == result.call_id => {
                    *cell_status = status;
                    if result.is_error {
                        *diagnostic = useful_error(&bounded);
                    } else {
                        *summary = agent_result_summary(&result.name, &bounded);
                    }
                    return;
                }
                Cell::Exploration { operations } => {
                    if let Some(op) = operations
                        .iter_mut()
                        .find(|op| op.call_id == result.call_id)
                    {
                        op.status = status;
                        op.raw = clean.clone();
                        if result.is_error {
                            op.diagnostic = useful_error(&bounded);
                        }
                        return;
                    }
                }
                Cell::Patch { files } => {
                    if let Some(file) = files.iter_mut().find(|file| file.call_id == result.call_id)
                    {
                        file.status = status;
                        file.raw = clean.clone();
                        if result.is_error {
                            file.diagnostic = humanize_edit_error(&file.path, &bounded);
                        }
                        return;
                    }
                }
                Cell::Diff {
                    call_id,
                    status: cell_status,
                    document,
                } if *call_id == result.call_id => {
                    *cell_status = status;
                    *document = if result.is_error {
                        DiffDocument {
                            files: vec![],
                            raw: clean,
                            parsed: false,
                        }
                    } else {
                        parse_unified_diff(&clean)
                    };
                    return;
                }
                Cell::Command {
                    call_id,
                    status: cell_status,
                    summary,
                    output,
                    raw,
                    command,
                } if *call_id == result.call_id => {
                    *cell_status = status;
                    *raw = clean;
                    *output = if result.is_error {
                        bounded.clone()
                    } else {
                        String::new()
                    };
                    *summary = command_summary(command, &bounded, result.is_error);
                    return;
                }
                Cell::Validation {
                    call_id,
                    status: cell_status,
                    summary,
                    output,
                    raw,
                    ..
                } if *call_id == result.call_id => {
                    let already_final = *cell_status != CellStatus::Running;
                    *cell_status = status;
                    *raw = clean;
                    *output = if result.is_error {
                        bounded.clone()
                    } else {
                        String::new()
                    };
                    if !already_final {
                        *summary = validation_summary(&bounded);
                    }
                    return;
                }
                _ => {}
            }
        }
        // A diff with no preceding model request (`/diff`) still becomes a
        // first-class cell rather than an undifferentiated notice.
        if result.name == "git_diff" {
            self.cells.push(Cell::Diff {
                call_id: result.call_id.clone(),
                status,
                document: if result.is_error {
                    DiffDocument {
                        files: vec![],
                        raw: clean,
                        parsed: false,
                    }
                } else {
                    parse_unified_diff(&clean)
                },
            });
            return;
        }
        // Orphan terminal results remain visible, but never expose correlation
        // identifiers in the normal transcript.
        self.cells.push(if result.is_error {
            Cell::Error {
                text: format!("{} failed\n{}", semantic_tool_name(&result.name), bounded),
            }
        } else {
            Cell::Notice {
                text: format!("{} completed", semantic_tool_name(&result.name)),
            }
        });
    }

    /// Replaces argument-derived edit counts with the kernel ledger's recorded
    /// deltas and attaches the real unified-diff preview, so the transcript
    /// edit summary, diff, and the sidebar ownership totals share one source of
    /// truth. The call id wins over the path when several guarded edits land in
    /// one batch.
    fn update_patch_change(
        &mut self,
        after: &latch_protocol::FileVersion,
        additions: usize,
        deletions: usize,
        preview: &str,
        call_id: Option<&str>,
    ) {
        for cell in self.cells.iter_mut().rev() {
            if let Cell::Patch { files } = cell {
                let file = match call_id {
                    Some(call_id) => files.iter_mut().rev().find(|file| file.call_id == call_id),
                    None => files
                        .iter_mut()
                        .rev()
                        .find(|file| file.path == after.path && file.status == CellStatus::Running),
                };
                if let Some(file) = file {
                    file.additions = additions;
                    file.deletions = deletions;
                    file.preview = preview.to_owned();
                    return;
                }
            }
        }
    }

    fn finish_validation(&mut self, command: &str, passed: bool, detail: &str) {
        if let Some(Cell::Validation {
            status,
            summary,
            output,
            ..
        }) = self
            .cells
            .iter_mut()
            .rev()
            .find(|cell| matches!(cell, Cell::Validation { command: c, .. } if c == command))
        {
            *status = if passed {
                CellStatus::Passed
            } else {
                CellStatus::Failed
            };
            *summary = clean_validation_detail(detail);
            if !passed && output.is_empty() {
                *output = useful_error(detail);
            }
        }
    }
}

fn is_hidden_tool(name: &str) -> bool {
    matches!(name, "record_evidence" | "task_update" | "complete")
}

fn agent_operation(name: &str) -> Option<AgentOperation> {
    if !is_agent_control(name) {
        return None;
    }
    Some(match name {
        "spawn_agent" => AgentOperation::Spawn,
        "send_agent_message" => AgentOperation::Send,
        "continue_agent" => AgentOperation::Continue,
        "wait_agents" => AgentOperation::Wait,
        "list_agents" => AgentOperation::List,
        "interrupt_agent" => AgentOperation::Interrupt,
        _ => AgentOperation::Close,
    })
}

/// Compact result summary for an agent-control cell. Only structured fields
/// that the kernel actually returned are used; anything else is a bounded
/// first line.
fn agent_result_summary(name: &str, output: &str) -> String {
    let value = serde_json::from_str::<Value>(output).ok();
    match name {
        "spawn_agent" => value
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .map_or_else(
                || first_line(output),
                |status| format!("child session {status}"),
            ),
        "wait_agents" => {
            let agents = value
                .as_ref()
                .and_then(|value| value.get("agents"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let running = agents
                .iter()
                .filter(|agent| {
                    agent
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| matches!(status, "starting" | "running"))
                })
                .count();
            let finished = agents.len().saturating_sub(running);
            if agents.is_empty() {
                "no child agents".into()
            } else {
                format!("{running} running · {finished} finished")
            }
        }
        "list_agents" => value.as_ref().and_then(Value::as_array).map_or_else(
            || first_line(output),
            |agents| format!("{} known", agents.len()),
        ),
        "close_agent" => "closed".into(),
        "interrupt_agent" => "interrupted".into(),
        _ => first_line(output),
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

fn exploration_label(call: &ToolCall) -> Option<String> {
    match call.name.as_str() {
        "read_file" => {
            let path = string_arg(&call.arguments, "path")?;
            let mut label = format!("Read {path}");
            if let Some(tail) = number_arg(&call.arguments, "tail") {
                label.push_str(&format!(" (last {tail} lines)"));
            } else if let Some(offset) = number_arg(&call.arguments, "offset") {
                label.push_str(&format!(" from line {offset}"));
            }
            if let Some(limit) = number_arg(&call.arguments, "limit") {
                label.push_str(&format!(" · {limit} lines"));
            }
            Some(label)
        }
        "read_artifact" => {
            let id = string_arg(&call.arguments, "id")?;
            let mut label = format!("Read artifact {id}");
            if let Some(tail) = number_arg(&call.arguments, "tail") {
                label.push_str(&format!(" (last {tail} lines)"));
            } else if let Some(offset) = number_arg(&call.arguments, "offset") {
                label.push_str(&format!(" from line {offset}"));
            }
            Some(label)
        }
        "search" => {
            let query = string_arg(&call.arguments, "query")?;
            let path = string_arg(&call.arguments, "path").unwrap_or(".");
            let mut label = format!("Search \"{query}\" in {path}");
            if let Some(offset) = number_arg(&call.arguments, "offset") {
                label.push_str(&format!(" from match {}", offset + 1));
            }
            Some(label)
        }
        "git_status" => Some("Inspect git status".into()),
        "shell" => shell_exploration_label(string_arg(&call.arguments, "command")?),
        _ => None,
    }
}

/// Semantic label for a shell inspection. Compound commands are split into
/// simple segments first; segments whose meaning is not obvious are never
/// guessed. A command this reducer cannot represent returns `None`, and the
/// caller falls back to the generic `Ran <command>` presentation.
fn shell_exploration_label(command: &str) -> Option<String> {
    let mut labels = Vec::new();
    for segment in split_shell_segments(command)? {
        let label = shell_segment_label(segment)?;
        if labels.last() != Some(&label) {
            labels.push(label);
        }
    }
    let joined = labels.join(" · ");
    (!joined.is_empty() && joined.chars().count() <= 160).then_some(joined)
}

/// Splits a compound command on `&&`, `;`, and `|`. Any other shell feature
/// that could expand, substitute, redirect, background, quote, or nest makes
/// the whole command unrepresentable, so presentation falls back to generic.
fn split_shell_segments(command: &str) -> Option<Vec<&str>> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_and = trimmed.replace("&&", " ");
    if without_and.chars().any(|ch| {
        matches!(
            ch,
            '$' | '`'
                | '<'
                | '>'
                | '&'
                | '('
                | ')'
                | '\\'
                | '\n'
                | '\r'
                | '"'
                | '\''
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '~'
                | '!'
        )
    }) {
        return None;
    }
    let mut segments = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let separator = match bytes[index] {
            b'&' if bytes.get(index + 1) == Some(&b'&') => 2,
            b'|' | b';' => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        let segment = trimmed[start..index].trim();
        if segment.is_empty() {
            return None;
        }
        segments.push(segment);
        index += separator;
        start = index;
    }
    let segment = trimmed[start..].trim();
    if segment.is_empty() {
        return None;
    }
    segments.push(segment);
    (segments.len() <= 6).then_some(segments)
}

fn shell_segment_label(segment: &str) -> Option<String> {
    let words = segment.split_whitespace().collect::<Vec<_>>();
    let executable = words.first()?.rsplit('/').next()?;
    match executable {
        "ls" => {
            let targets = targets(&words[1..]);
            Some(if targets.is_empty() {
                "List workspace".into()
            } else {
                format!("List {targets}")
            })
        }
        "find" => Some(format!(
            "List {}",
            words
                .get(1)
                .filter(|word| !word.starts_with('-'))
                .copied()
                .unwrap_or("workspace")
        )),
        "rg" | "grep" => {
            let query = words
                .iter()
                .skip(1)
                .find(|word| !word.starts_with('-'))
                .copied()
                .unwrap_or("");
            Some(if query.is_empty() {
                "Search workspace".into()
            } else {
                format!("Search {query}")
            })
        }
        "cat" | "head" | "tail" | "sed" => {
            let target = words
                .iter()
                .skip(1)
                .rev()
                .find(|word| !word.starts_with('-'))
                .copied()
                .unwrap_or("input");
            Some(format!("Read {target}"))
        }
        "pwd" => Some("Show working directory".into()),
        "git" => match words.get(1).copied() {
            Some("status") => Some("Inspect git status".into()),
            Some("diff") => Some("Inspect workspace diff".into()),
            Some("log") => Some("Inspect git history".into()),
            _ => None,
        },
        _ => None,
    }
}

fn targets(words: &[&str]) -> String {
    let mut targets = words
        .iter()
        .filter(|word| !word.starts_with('-'))
        .copied()
        .collect::<Vec<_>>();
    if targets.len() > 4 {
        targets.truncate(4);
        targets.push("…");
    }
    targets.join(", ")
}

fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

fn number_arg(arguments: &Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(Value::as_u64)
}

fn patch_delta(call: &ToolCall) -> (usize, usize) {
    if call.name == "patch" {
        let old = string_arg(&call.arguments, "old").unwrap_or("");
        let new = string_arg(&call.arguments, "new").unwrap_or("");
        (new.lines().count(), old.lines().count())
    } else {
        if call.arguments.get("base_hash").is_none() {
            (
                string_arg(&call.arguments, "content").map_or(0, |content| content.lines().count()),
                0,
            )
        } else {
            (0, 0)
        }
    }
}

fn semantic_tool_name(name: &str) -> &str {
    match name {
        "shell" => "Command",
        "patch" | "write" => "Edit",
        "read_file" | "search" | "git_status" | "git_diff" => "Exploration",
        "validate" => "Validation",
        _ => "Action",
    }
}

fn sanitize_output(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        match ch {
            '\r' => {
                while clean.ends_with(|candidate| candidate != '\n') {
                    clean.pop();
                }
            }
            '\n' | '\t' => clean.push(ch),
            candidate if candidate.is_control() => {}
            candidate => clean.push(candidate),
        }
    }
    clean.trim().to_owned()
}

fn bounded_output(text: &str) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    let total = text.lines().count();
    let total_chars: usize = text.lines().map(|line| line.chars().count()).sum();
    for line in text.lines().take(DEFAULT_OUTPUT_LINES) {
        if shown + line.chars().count() > DEFAULT_OUTPUT_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        shown += line.chars().count();
    }
    // Newline separators are not content: compare characters actually shown
    // against characters that exist, or every multiline output would claim to
    // be truncated.
    if total > out.lines().count() || total_chars > shown {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("… output truncated; Ctrl+T shows details");
    }
    out
}

fn useful_error(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty() && line != "exit 0" && !line.starts_with("hash:")
        })
        .take(DEFAULT_OUTPUT_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

fn humanize_edit_error(path: &str, detail: &str) -> String {
    if detail.contains("stale observation") || detail.contains("base_hash") {
        format!("{path} changed since it was inspected; re-reading before editing.")
    } else {
        useful_error(detail)
    }
}

fn command_summary(command: &str, output: &str, failed: bool) -> String {
    let lower = command.to_ascii_lowercase();
    if lower.contains("cargo test")
        && let Some(line) = output
            .lines()
            .rev()
            .find(|line| line.contains("test result:"))
    {
        return line
            .trim()
            .trim_start_matches("test result:")
            .trim()
            .to_owned();
    }
    if lower.contains("pytest")
        && let Some(line) = output
            .lines()
            .rev()
            .find(|line| line.contains("passed") || line.contains("failed"))
    {
        return line.trim_matches('=').trim().to_owned();
    }
    if failed {
        useful_error(output)
            .lines()
            .next()
            .unwrap_or("failed")
            .to_owned()
    } else {
        String::new()
    }
}

fn validation_summary(detail: &str) -> String {
    clean_validation_detail(detail)
}

fn agent_status_label(status: &latch_protocol::AgentStatus) -> &'static str {
    match status {
        latch_protocol::AgentStatus::Starting => "starting",
        latch_protocol::AgentStatus::Running => "running",
        latch_protocol::AgentStatus::Completed => "completed",
        latch_protocol::AgentStatus::Interrupted => "interrupted",
        latch_protocol::AgentStatus::Failed => "failed",
        latch_protocol::AgentStatus::Closed => "closed",
    }
}

fn clean_validation_detail(detail: &str) -> String {
    let first = detail.lines().next().unwrap_or(detail).trim();
    if let Some((status_duration, summary)) = first.split_once(": ")
        && let Some(start) = status_duration.rfind('(')
        && let Some(duration) = status_duration[start + 1..].strip_suffix(')')
        && !summary.is_empty()
    {
        return format!("{summary} · {duration}");
    }
    first
        .replace("exit code 0", "passed")
        .replace("exit code 1", "failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use latch_protocol::{CompletionState, FileVersion, Mode};
    use serde_json::json;
    use uuid::Uuid;

    fn event(payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            sequence: 1,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    fn request(id: &str, name: &str, arguments: Value) -> Event {
        event(EventPayload::ToolRequested {
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            },
        })
    }

    fn result(id: &str, name: &str, output: &str, failed: bool) -> Event {
        event(if failed {
            EventPayload::ToolFailed {
                result: ToolResult {
                    call_id: id.into(),
                    name: name.into(),
                    output: output.into(),
                    is_error: true,
                    artifact_id: None,
                },
            }
        } else {
            EventPayload::ToolCompleted {
                result: ToolResult {
                    call_id: id.into(),
                    name: name.into(),
                    output: output.into(),
                    is_error: false,
                    artifact_id: None,
                },
            }
        })
    }

    #[test]
    fn sequential_reads_and_search_coalesce_and_complete_in_place() {
        let events = vec![
            request(
                "a",
                "read_file",
                json!({"path":"Cargo.toml","base_hash":"secret"}),
            ),
            request("b", "read_file", json!({"path":"src/lib.rs"})),
            request("c", "search", json!({"query":"normalize", "path":"src/"})),
            result("a", "read_file", "hash: deadbeef\ncontents", false),
            result("b", "read_file", "hash: bead\ncontents", false),
            result("c", "search", "src/lib.rs:1:normalize", false),
        ];
        let model = PresentationModel::from_events(&events);
        assert_eq!(model.cells.len(), 1);
        let Cell::Exploration { operations } = &model.cells[0] else {
            panic!()
        };
        assert_eq!(operations.len(), 3);
        assert!(operations.iter().all(|op| op.status == CellStatus::Passed));
        let visible = format!("{operations:?}");
        assert!(visible.contains("Read Cargo.toml"));
        assert!(visible.contains("Search \\\"normalize\\\" in src/"));
        assert!(!operations.iter().any(|op| op.label.contains("deadbeef")));
    }

    #[test]
    fn safe_shell_inspection_uses_exploration_semantics() {
        let model = PresentationModel::from_events(&[
            request("a", "shell", json!({"command":"ls -la src"})),
            request("b", "shell", json!({"command":"rg -n normalize src"})),
            result("a", "shell", "exit 0\nlib.rs", false),
            result("b", "shell", "exit 0\nsrc/lib.rs:1", false),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("List src"));
        assert!(rendered.contains("Search normalize"));
        assert!(!rendered.contains("Running ls"));
    }

    #[test]
    fn ranged_reads_and_artifacts_render_semantic_labels() {
        let model = PresentationModel::from_events(&[
            request(
                "a",
                "read_file",
                json!({"path":"big.rs","offset":2001,"limit":2000}),
            ),
            request("b", "read_artifact", json!({"id":"shell-x.log","tail":50})),
            request("c", "search", json!({"query":"needle","offset":10})),
        ]);
        let visible = format!("{:?}", model.cells());
        assert!(
            visible.contains("Read big.rs from line 2001 · 2000 lines"),
            "{visible}"
        );
        assert!(
            visible.contains("Read artifact shell-x.log (last 50 lines)"),
            "{visible}"
        );
        assert!(visible.contains("Search \\\"needle\\\" in . from match 11"));
    }

    #[test]
    fn compound_shell_inspection_never_renders_raw_separators() {
        let model = PresentationModel::from_events(&[
            request(
                "a",
                "shell",
                json!({"command":"ls -la && cat Cargo.toml && ls src"}),
            ),
            request(
                "b",
                "shell",
                json!({"command":"git status && ls -R src && cat Cargo.toml"}),
            ),
            request(
                "c",
                "shell",
                json!({"command":"git log --oneline -5 | head"}),
            ),
            request("d", "shell", json!({"command":"echo hi && ls"})),
            result("a", "shell", "exit 0\nsrc", false),
            result("b", "shell", "exit 0\nsrc", false),
            result("c", "shell", "exit 0\ndeadbeef init", false),
            result("d", "shell", "exit 0\nhi\nsrc", false),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("List workspace · Read Cargo.toml · List src"));
        assert!(rendered.contains("Inspect git status · List src · Read Cargo.toml"));
        assert!(rendered.contains("Inspect git history · Read input"));
        // Unrecognized compound commands fall back to the generic Ran row.
        assert!(rendered.contains("Ran echo hi && ls"));
        for nonsense in ["List &&", "&&,", "cat, Cargo.toml", "ls, src", "List ,"] {
            assert!(
                !rendered.contains(nonsense),
                "raw command fragments leaked into the transcript: {nonsense}\n{rendered}"
            );
        }
    }

    #[test]
    fn shell_segment_labels_are_conservative() {
        assert_eq!(
            shell_exploration_label("ls -la && cat Cargo.toml && ls src").as_deref(),
            Some("List workspace · Read Cargo.toml · List src")
        );
        assert_eq!(
            shell_exploration_label("ls -la; ls -la").as_deref(),
            Some("List workspace")
        );
        assert_eq!(
            shell_exploration_label("git status && git diff").as_deref(),
            Some("Inspect git status · Inspect workspace diff")
        );
        // Expansion, redirects, quotes, and unrecognized commands fall back.
        assert_eq!(shell_exploration_label("echo $HOME"), None);
        assert_eq!(shell_exploration_label("git diff > out.patch"), None);
        assert_eq!(shell_exploration_label("cargo test"), None);
        assert_eq!(shell_exploration_label("ls && cargo test"), None);
        assert_eq!(shell_exploration_label("cd src && rg foo ."), None);
        assert_eq!(shell_exploration_label("rg foo || true"), None);
    }

    #[test]
    fn command_lifecycle_is_one_cell_and_failure_is_bounded() {
        let mut events = vec![request("a", "shell", json!({"command":"cargo test"}))];
        let huge = (0..100)
            .map(|n| format!("failure line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        events.push(result("a", "shell", &huge, true));
        let model = PresentationModel::from_events(&events);
        assert_eq!(model.cells.len(), 1);
        let Cell::Command { status, output, .. } = &model.cells[0] else {
            panic!()
        };
        assert_eq!(*status, CellStatus::Failed);
        assert!(output.contains("truncated"));
        assert!(output.lines().count() <= DEFAULT_OUTPUT_LINES + 1);
    }

    #[test]
    fn successful_empty_command_stays_compact() {
        let model = PresentationModel::from_events(&[
            request("a", "shell", json!({"command":"cargo check"})),
            result("a", "shell", "exit 0", false),
        ]);
        let Cell::Command { output, status, .. } = &model.cells[0] else {
            panic!()
        };
        assert_eq!(*status, CellStatus::Passed);
        assert!(output.is_empty());
    }

    #[test]
    fn validation_and_patch_are_first_class() {
        let model = PresentationModel::from_events(&[
            request(
                "v",
                "validate",
                json!({"requirement":"tests", "command":"cargo test"}),
            ),
            event(EventPayload::ValidationResult {
                command: "cargo test".into(),
                passed: true,
                detail: "exit code 0 (0.42s): 3 tests passed".into(),
            }),
            request(
                "p",
                "patch",
                json!({"path":"src/lib.rs", "old":"a\nb", "new":"c\nd"}),
            ),
            result("p", "patch", "updated src/lib.rs @ hash", false),
        ]);
        assert!(matches!(
            &model.cells[0],
            Cell::Validation {
                status: CellStatus::Passed,
                ..
            }
        ));
        assert!(
            matches!(&model.cells[1], Cell::Patch { files } if files[0].additions == 2 && files[0].deletions == 2)
        );
    }

    #[test]
    fn failed_validation_and_multi_file_edit_render_semantically() {
        let model = PresentationModel::from_events(&[
            request(
                "v",
                "validate",
                json!({"requirement":"tests", "command":"cargo test"}),
            ),
            event(EventPayload::ValidationResult {
                command: "cargo test".into(),
                passed: false,
                detail: "exit code 1 (0.38s): average_preserves_fraction FAILED".into(),
            }),
            request(
                "a",
                "write",
                json!({"path":"tests/new.rs", "content":"one\ntwo"}),
            ),
            request(
                "b",
                "patch",
                json!({"path":"src/lib.rs", "old":"old", "new":"new"}),
            ),
            result("a", "write", "updated tests/new.rs @ abc", false),
            result("b", "patch", "updated src/lib.rs @ def", false),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("✗ Validation failed"));
        assert!(rendered.contains("cargo test · average_preserves_fraction FAILED · 0.38s"));
        assert!(rendered.contains("Edited 2 files"));
        assert!(rendered.contains("A tests/new.rs"));
        assert!(rendered.contains("M src/lib.rs"));
        assert!(!rendered.contains("abc"));
        assert!(!rendered.contains("call"));
    }

    #[test]
    fn internal_ids_hashes_and_reasoning_do_not_form_cells() {
        let model = PresentationModel::from_events(&[
            event(EventPayload::AssistantMessageCompleted {
                text: "answer".into(),
                tool_calls: vec![],
                reasoning_content: Some("secret".into()),
            }),
            request("call-secret", "record_evidence", json!({"claim":"x"})),
            event(EventPayload::FileObserved {
                version: FileVersion {
                    path: "x".into(),
                    content_hash: "hash-secret".into(),
                    size: 1,
                },
            }),
            event(EventPayload::ModeChanged { mode: Mode::Plan }),
        ]);
        let visible = format!("{:?}", model.cells());
        assert!(!visible.contains("secret"));
        assert!(!visible.contains("record_evidence"));
    }

    #[test]
    fn ansi_and_carriage_return_progress_are_sanitized() {
        assert_eq!(sanitize_output("10%\r20%\r\u{1b}[31mdone\u{1b}[0m"), "done");
    }

    #[test]
    fn live_and_replay_converge() {
        let events = vec![
            event(EventPayload::UserMessage {
                text: "fix it".into(),
            }),
            request("a", "read_file", json!({"path":"src/lib.rs"})),
            result("a", "read_file", "hash: abc\ntext", false),
            event(EventPayload::AssistantMessageCompleted {
                text: "Done.".into(),
                tool_calls: vec![],
                reasoning_content: Some("hidden".into()),
            }),
        ];
        let replay = PresentationModel::from_events(&events);
        let mut live = PresentationModel::default();
        for item in &events {
            live.apply_event(item);
        }
        assert_eq!(live, replay);
    }

    #[test]
    fn semantic_history_snapshot() {
        let cells = vec![
            Cell::Exploration {
                operations: vec![
                    ExplorationOperation {
                        call_id: "a".into(),
                        label: "Read Cargo.toml".into(),
                        status: CellStatus::Passed,
                        diagnostic: String::new(),
                        raw: String::new(),
                    },
                    ExplorationOperation {
                        call_id: "b".into(),
                        label: "Read src/lib.rs".into(),
                        status: CellStatus::Passed,
                        diagnostic: String::new(),
                        raw: String::new(),
                    },
                ],
            },
            Cell::Patch {
                files: vec![PatchFile {
                    call_id: "p".into(),
                    path: "src/lib.rs".into(),
                    kind: 'M',
                    additions: 2,
                    deletions: 2,
                    status: CellStatus::Passed,
                    diagnostic: String::new(),
                    raw: String::new(),
                    preview: String::new(),
                }],
            },
            Cell::Validation {
                call_id: "v".into(),
                command: "cargo test".into(),
                requirement: "tests".into(),
                status: CellStatus::Passed,
                summary: "3 tests passed · 0.42s".into(),
                output: String::new(),
                raw: String::new(),
            },
        ];
        assert_eq!(
            super::super::render_cells_plain(&cells, false).trim_end(),
            include_str!("../tests/snapshots/v3_semantic.txt").trim_end()
        );
    }

    #[test]
    fn edit_summary_uses_kernel_ledger_delta_not_patch_line_count() {
        let model = PresentationModel::from_events(&[
            request(
                "p",
                "patch",
                json!({"path":"src/lib.rs","old":"a\nb\nc","new":"a\nX\nc"}),
            ),
            event(EventPayload::FileChanged {
                before: None,
                after: FileVersion {
                    path: "src/lib.rs".into(),
                    content_hash: "h".into(),
                    size: 5,
                },
                created: false,
                owner: latch_protocol::ChangeOwner::Latch,
                undo_artifact: None,
                additions: 1,
                deletions: 1,
                preview: String::new(),
                call_id: None,
            }),
            result("p", "patch", "updated src/lib.rs @ h", false),
        ]);
        let Cell::Patch { files } = &model.cells[0] else {
            panic!("expected patch cell");
        };
        assert_eq!(files[0].additions, 1, "kernel ledger is authoritative");
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[0].status, CellStatus::Passed);
    }

    #[test]
    fn git_diff_becomes_a_first_class_diff_cell_not_a_notice() {
        let model = PresentationModel::from_events(&[
            request("d", "git_diff", json!({})),
            result(
                "d",
                "git_diff",
                "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
                false,
            ),
        ]);
        assert_eq!(model.cells.len(), 1);
        let Cell::Diff {
            status, document, ..
        } = &model.cells[0]
        else {
            panic!("git_diff must become a Diff cell, got {:?}", model.cells[0]);
        };
        assert_eq!(*status, CellStatus::Passed);
        assert!(document.parsed);
        assert_eq!(document.added_lines(), 1);
        assert_eq!(document.removed_lines(), 1);
        assert!(
            !matches!(&model.cells[0], Cell::Notice { .. }),
            "a diff must never be an undifferentiated notice"
        );
        // Raw remains available for copy/paste and the detail view.
        assert!(model.cells[0].raw_text().contains("diff --git"));
    }

    #[test]
    fn diff_cell_renders_delta_summary_and_bounded_body() {
        let model = PresentationModel::from_events(&[
            request("d", "git_diff", json!({})),
            result(
                "d",
                "git_diff",
                "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
                false,
            ),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("Workspace diff"));
        assert!(rendered.contains("+1"));
        assert!(rendered.contains("−1"));
        assert!(rendered.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn bounded_output_only_marks_real_truncation() {
        assert_eq!(
            bounded_output("exit code 1\nassertion failed"),
            "exit code 1\nassertion failed",
            "short multiline output is not truncated"
        );
        let long = (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let bounded = bounded_output(&long);
        assert!(bounded.contains("output truncated"));
        assert!(bounded.lines().count() <= DEFAULT_OUTPUT_LINES + 1);
        let wide = "x".repeat(4_000);
        assert!(bounded_output(&wide).contains("output truncated"));
    }

    #[test]
    fn narrow_failure_snapshot_content() {
        let cells = vec![
            Cell::User {
                text: "修复 failing test".into(),
            },
            Cell::Command {
                call_id: "c".into(),
                command: "cargo test".into(),
                status: CellStatus::Failed,
                summary: "test failed".into(),
                output: "assertion failed".into(),
                raw: String::new(),
            },
        ];
        assert_eq!(
            super::super::render_cells_plain(&cells, false).trim_end(),
            include_str!("../tests/snapshots/v3_narrow.txt").trim_end()
        );
    }

    #[test]
    fn delivered_child_report_renders_one_compact_cell() {
        let mut model = PresentationModel::default();
        model.apply_event(&event(EventPayload::AgentNotificationDelivered {
            report: latch_protocol::AgentReport {
                report_id: Uuid::new_v4(),
                agent_id: Uuid::new_v4(),
                task_name: "audit-locks".into(),
                status: latch_protocol::AgentStatus::Completed,
                completion: CompletionState::InProgress,
                summary: "Found two unsynchronized locks.\nDetails omitted".into(),
                findings: vec![],
                touched_files: vec!["src/locks.rs".into()],
                evidence: vec![],
                unresolved_questions: vec![],
            },
        }));
        let [
            Cell::AgentReport {
                task_name,
                status,
                summary,
            },
        ] = model.cells()
        else {
            panic!("expected exactly one child-report cell");
        };
        assert_eq!(task_name, "audit-locks");
        assert_eq!(*status, latch_protocol::AgentStatus::Completed);
        assert_eq!(summary, "Found two unsynchronized locks.");
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(
            rendered.contains("✓ Child agent `audit-locks` completed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("└ Found two unsynchronized locks."),
            "{rendered}"
        );
        assert!(
            !rendered.contains("Details omitted"),
            "only one bounded line crosses"
        );
    }

    #[test]
    fn agent_control_calls_stay_compact_in_root_history() {
        let model = PresentationModel::from_events(&[
            request(
                "s1",
                "spawn_agent",
                json!({"task_name":"audit-locks","message":"Review the lock ordering in the cache."}),
            ),
            result(
                "s1",
                "spawn_agent",
                "{\"agent_id\":\"00000000-0000-0000-0000-000000000001\",\"task_name\":\"audit-locks\",\"agent_type\":null,\"status\":\"running\"}",
                false,
            ),
            request("w1", "wait_agents", json!({"agent_ids":[]})),
            result(
                "w1",
                "wait_agents",
                "{\"agents\":[{\"agent_id\":\"00000000-0000-0000-0000-000000000001\",\"task_name\":\"audit-locks\",\"agent_type\":null,\"status\":\"completed\"}],\"reports_pending\":1}",
                false,
            ),
        ]);
        let rendered = super::super::render_cells_plain(model.cells(), false);
        assert!(rendered.contains("• Spawned `audit-locks`"), "{rendered}");
        assert!(rendered.contains("child session running"), "{rendered}");
        assert!(rendered.contains("• Waited for agents"), "{rendered}");
        assert!(rendered.contains("0 running · 1 finished"), "{rendered}");
    }
}
