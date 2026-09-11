//! Deterministic semantic presentation of durable kernel events.
//!
//! The kernel records truth; this reducer decides what that truth means to a
//! person. It deliberately contains no Ratatui types, so live events and a
//! replayed event stream produce the same committed cells.

use latch_protocol::{Event, EventPayload, ToolCall, ToolResult};
use serde_json::Value;

const DEFAULT_OUTPUT_LINES: usize = 8;
const DEFAULT_OUTPUT_CHARS: usize = 1_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStatus {
    Running,
    Passed,
    Failed,
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
                .map(|file| format!("{} {}\n{}", file.kind, file.path, file.raw))
                .collect::<Vec<_>>()
                .join("\n"),
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
            EventPayload::ScopeExpansionRequested { reason, .. } => {
                self.push_notice(format!("Scope needs attention: {reason}"));
            }
            EventPayload::RegroundRequested { .. } => {
                self.push_notice("Repeated failure; re-inspecting relevant context");
            }
            EventPayload::ManualCompact { .. } => {
                self.push_notice("Active context reset; durable history retained");
            }
            EventPayload::ChangeReverted { path, .. } => {
                self.push_notice(format!("Reverted {path}"));
            }
            EventPayload::ShellMutationObserved { paths, .. } if !paths.is_empty() => {
                self.push_notice(format!("Command changed {}", paths.join(", ")));
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

fn exploration_label(call: &ToolCall) -> Option<String> {
    match call.name.as_str() {
        "read_file" => Some(format!("Read {}", string_arg(&call.arguments, "path")?)),
        "search" => {
            let query = string_arg(&call.arguments, "query")?;
            let path = string_arg(&call.arguments, "path").unwrap_or(".");
            Some(format!("Search \"{query}\" in {path}"))
        }
        "git_status" => Some("Inspect git status".into()),
        "git_diff" => Some("Inspect workspace diff".into()),
        "shell" => shell_exploration_label(string_arg(&call.arguments, "command")?),
        _ => None,
    }
}

fn shell_exploration_label(command: &str) -> Option<String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let executable = words.first()?.rsplit('/').next()?;
    match executable {
        "ls" => {
            let targets = words
                .iter()
                .skip(1)
                .filter(|word| !word.starts_with('-'))
                .copied()
                .collect::<Vec<_>>();
            Some(if targets.is_empty() {
                "List workspace".into()
            } else {
                format!("List {}", targets.join(", "))
            })
        }
        "find" => Some(format!(
            "List {}",
            words.get(1).copied().unwrap_or("workspace")
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
        "git" => match words.get(1).copied() {
            Some("status") => Some("Inspect git status".into()),
            Some("diff") => Some("Inspect workspace diff".into()),
            Some("log") => Some("Inspect git history".into()),
            _ => None,
        },
        _ => None,
    }
}

fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
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
    if total > out.lines().count() || text.chars().count() > shown {
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
    use latch_protocol::{FileVersion, Mode};
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
}
