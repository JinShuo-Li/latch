//! Stable machine output for `latch run` / `latch resume` / `latch sessions`.
//!
//! This module owns the versioned JSON schema, the semantic JSONL stream, and
//! the process exit codes. stdout carries only structured/one-shot content;
//! every diagnostic stays on stderr. Secrets never appear here: usage and
//! context values come from durable events, and provider error strings have
//! already passed through the kernel's redaction rules.

use crate::cli::command::OutputFormat;
use latch_protocol::{ContextStats, Event, ToolResult, Usage};
use serde::Serialize;
use std::process::ExitCode;
use uuid::Uuid;

/// Version of the machine-readable schemas emitted by this binary.
pub const SCHEMA_VERSION: u32 = 1;

/// Success or a cleanly terminal run (canonical completion state is detailed
/// in the JSON result, not in the exit code).
pub const EXIT_SUCCESS: u8 = 0;
/// Runtime/model/tool failure.
pub const EXIT_FAILURE: u8 = 1;
/// CLI or configuration error (clap usage errors use the same code).
pub const EXIT_USAGE: u8 = 2;
/// A permission decision could not be resolved non-interactively and was
/// denied; nothing was auto-approved.
pub const EXIT_PERMISSION: u8 = 3;
/// The run was cancelled (for example Ctrl+C).
pub const EXIT_CANCELLED: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The run returned normally.
    Completed,
    /// The run failed (provider, tool, or kernel error).
    Failed,
    /// Setup failed before a run could produce a result (CLI/config).
    ConfigurationError,
    /// The run reached a permission decision that required a human and was
    /// resolved as an explicit denial.
    PermissionDenied,
    /// The run was cancelled.
    Cancelled,
}

impl RunStatus {
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Completed => EXIT_SUCCESS,
            Self::Failed => EXIT_FAILURE,
            Self::ConfigurationError => EXIT_USAGE,
            Self::PermissionDenied => EXIT_PERMISSION,
            Self::Cancelled => EXIT_CANCELLED,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileReport {
    pub provider: String,
    pub model: String,
    pub effort: String,
}

#[derive(Debug, Default, Serialize)]
pub struct UsageReport {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cache_miss_tokens: Option<u64>,
}

/// Accumulates provider-reported usage exactly as durable `ModelUsage` events
/// report it. A category no provider reported stays `null`; it is never
/// fabricated as zero.
#[derive(Debug, Default)]
pub struct UsageAggregate {
    seen: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    cache_miss_tokens: Option<u64>,
}

impl UsageAggregate {
    pub fn observe(&mut self, usage: &Usage) {
        self.seen = true;
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        merge(&mut self.cache_read_tokens, usage.cache_read_tokens);
        merge(&mut self.cache_write_tokens, usage.cache_write_tokens);
        merge(&mut self.cache_miss_tokens, usage.cache_miss_tokens);
    }

    #[must_use]
    pub fn report(&self) -> UsageReport {
        if !self.seen {
            return UsageReport::default();
        }
        UsageReport {
            input_tokens: Some(self.input_tokens),
            output_tokens: Some(self.output_tokens),
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            cache_miss_tokens: self.cache_miss_tokens,
        }
    }
}

fn merge(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ContextReport {
    pub request_tokens: Option<u64>,
    pub common_prefix_tokens: Option<u64>,
    pub cache_epoch: Option<u64>,
}

impl ContextReport {
    #[must_use]
    pub fn from_stats(stats: &ContextStats) -> Self {
        Self {
            request_tokens: Some(stats.request_tokens as u64),
            common_prefix_tokens: Some(stats.common_prefix_tokens as u64),
            cache_epoch: Some(stats.cache_epoch),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct EventsReport {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub count: u64,
}

impl EventsReport {
    #[must_use]
    pub fn range(pre: u64, post: u64) -> Self {
        if post <= pre {
            return Self::default();
        }
        Self {
            first_sequence: pre + 1,
            last_sequence: post,
            count: post - pre,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ResultReport {
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorReport {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct MachineResult {
    pub schema_version: u32,
    pub command: String,
    pub session_id: Option<Uuid>,
    pub workspace: String,
    pub status: RunStatus,
    pub profile: Option<ProfileReport>,
    pub result: ResultReport,
    pub usage: UsageReport,
    pub context: ContextReport,
    pub events: EventsReport,
    pub error: Option<ErrorReport>,
}

impl MachineResult {
    #[must_use]
    pub fn new(command: &str, workspace: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: command.to_owned(),
            session_id: None,
            workspace,
            status: RunStatus::ConfigurationError,
            profile: None,
            result: ResultReport::default(),
            usage: UsageReport::default(),
            context: ContextReport::default(),
            events: EventsReport::default(),
            error: None,
        }
    }

    pub fn fail(&mut self, status: RunStatus, message: impl Into<String>) {
        self.status = status;
        self.error = Some(ErrorReport {
            message: message.into(),
        });
    }
}

/// Streams semantic records in `jsonl` mode and the single final object in
/// `json` mode. In `text` mode it behaves like the historical one-shot path.
#[derive(Debug, Clone, Copy)]
pub struct Reporter {
    output: OutputFormat,
}

impl Reporter {
    #[must_use]
    pub const fn new(output: OutputFormat) -> Self {
        Self { output }
    }

    pub fn text_delta(&self, text: &str) {
        match self.output {
            OutputFormat::Text => print!("{text}"),
            OutputFormat::Json => {}
            OutputFormat::Jsonl => emit(&JsonlRecord::TextDelta { text }),
        }
    }

    pub fn tool_result(&self, result: &ToolResult) {
        if self.output != OutputFormat::Jsonl {
            return;
        }
        emit(&JsonlRecord::ToolResult {
            call_id: &result.call_id,
            name: &result.name,
            status: if result.is_error { "error" } else { "ok" },
        });
    }

    pub fn durable_event(&self, event: &Event) {
        if self.output != OutputFormat::Jsonl {
            return;
        }
        emit(&JsonlRecord::DurableEvent {
            sequence: event.sequence,
            event_type: event_type(event),
        });
    }

    pub fn finish(&self, result: &MachineResult) {
        match self.output {
            OutputFormat::Text => {}
            OutputFormat::Json => print_json(result),
            OutputFormat::Jsonl => emit(&JsonlRecord::Final { result }),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonlRecord<'a> {
    TextDelta {
        text: &'a str,
    },
    ToolResult {
        call_id: &'a str,
        name: &'a str,
        status: &'a str,
    },
    DurableEvent {
        sequence: u64,
        event_type: String,
    },
    Final {
        result: &'a MachineResult,
    },
}

fn emit(record: &JsonlRecord<'_>) {
    match serde_json::to_string(record) {
        Ok(line) => println!("{line}"),
        Err(error) => eprintln!("error: could not serialize output record: {error}"),
    }
}

fn print_json(result: &MachineResult) {
    match serde_json::to_string(result) {
        Ok(line) => println!("{line}"),
        Err(error) => eprintln!("error: could not serialize result: {error}"),
    }
}

/// The wire (snake_case) name of a reasoning effort, as config and JSON
/// consumers expect it. Never a debug string.
#[must_use]
pub fn reasoning_effort_name(effort: latch_protocol::ReasoningEffort) -> String {
    serde_json::to_value(effort)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| effort.label().to_owned())
}

/// The snake_case serde tag stored for a durable event payload, derived from
/// the protocol's own serialization (never a Rust debug string).
#[must_use]
pub fn event_type(event: &Event) -> String {
    serde_json::to_value(&event.payload)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|tag| tag.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Converts a status into the process exit code, keeping the schema and the
/// process contract in one place.
#[must_use]
pub fn exit_code(status: RunStatus) -> ExitCode {
    ExitCode::from(status.exit_code())
}
