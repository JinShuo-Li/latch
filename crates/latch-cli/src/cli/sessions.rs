//! `latch sessions list` / `latch sessions show`: read-only inspection of
//! durable sessions for external agents, CI, and scripts.
//!
//! The commands open the durable store directly, never the TUI, and never
//! expose SQLite rows: every field is a semantic value the kernel already
//! models (mode, profile, completion, prompt preview, transcript lines).

use crate::cli::command::{Args, OutputFormat, SessionsArgs, SessionsCommand};
use crate::cli::output::{EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE};
use crate::cli::session::resolve_workspace;
use latch_kernel::{Config, EventStore, SessionSummary, session};
use latch_protocol::EventPayload;
use serde::Serialize;
use std::process::ExitCode;

/// The session-inspection payloads are unchanged by the run/resume schema
/// version 2 bump, so they keep their own, independent version.
const SESSIONS_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct SessionRecord {
    id: String,
    workspace: String,
    created_at: String,
    updated_at: String,
    mode: String,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    event_count: u64,
    completion: Option<String>,
    prompt_preview: Option<String>,
}

#[derive(Serialize)]
struct SessionsList {
    schema_version: u32,
    sessions: Vec<SessionRecord>,
}

#[derive(Serialize)]
struct PreviewLine {
    speaker: String,
    text: String,
}

#[derive(Serialize)]
struct SessionDetail {
    #[serde(flatten)]
    summary: SessionRecord,
    goal: String,
    recent: Vec<PreviewLine>,
}

#[derive(Serialize)]
struct SessionShow {
    schema_version: u32,
    session: SessionDetail,
}

#[derive(Serialize)]
struct SessionsError {
    schema_version: u32,
    error: String,
}

enum SessionsErrorKind {
    Usage(String),
    Runtime(String),
}

pub async fn execute(args: &Args, sessions: SessionsArgs) -> ExitCode {
    let output = args.output;
    if output == OutputFormat::Jsonl {
        let message = "`sessions` supports --output text or --output json";
        if output.is_machine() {
            print_json(&SessionsError {
                schema_version: SESSIONS_SCHEMA_VERSION,
                error: message.to_owned(),
            });
        }
        eprintln!("error: {message}");
        return ExitCode::from(EXIT_USAGE);
    }
    match run(args, sessions.command, output).await {
        Ok(()) => ExitCode::from(EXIT_SUCCESS),
        Err(SessionsErrorKind::Usage(message)) => {
            report_error(&message, output);
            ExitCode::from(EXIT_USAGE)
        }
        Err(SessionsErrorKind::Runtime(message)) => {
            report_error(&message, output);
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn report_error(message: &str, output: OutputFormat) {
    if output.is_machine() {
        print_json(&SessionsError {
            schema_version: SESSIONS_SCHEMA_VERSION,
            error: message.to_owned(),
        });
    }
    eprintln!("error: {message}");
}

async fn run(
    args: &Args,
    command: SessionsCommand,
    output: OutputFormat,
) -> Result<(), SessionsErrorKind> {
    let config = Config::load(args.config.as_deref())
        .map_err(|error| SessionsErrorKind::Usage(format!("{error:#}")))?;
    let store = EventStore::open(
        &latch_kernel::paths::ResolvedPaths::for_state(&config.state_dir).database_path,
    )
    .map_err(|error| SessionsErrorKind::Runtime(format!("{error:#}")))?;
    match command {
        SessionsCommand::List { workspace } => {
            let filter = workspace
                .as_deref()
                .map(resolve_workspace)
                .transpose()
                .map_err(|error| SessionsErrorKind::Usage(format!("{error:#}")))?;
            let summaries = store
                .list_sessions(filter.as_deref())
                .map_err(|error| SessionsErrorKind::Runtime(format!("{error:#}")))?;
            let records = summaries
                .iter()
                .map(|summary| record(&store, &config, summary))
                .collect::<Result<Vec<_>, _>>()?;
            match output {
                OutputFormat::Text => print_list_text(&records),
                _ => print_json(&SessionsList {
                    schema_version: SESSIONS_SCHEMA_VERSION,
                    sessions: records,
                }),
            }
            Ok(())
        }
        SessionsCommand::Show { selector } => {
            let summary = store
                .resolve_session(&selector)
                .map_err(|error| SessionsErrorKind::Usage(format!("{error:#}")))?;
            let events = store
                .events(summary.id)
                .map_err(|error| SessionsErrorKind::Runtime(format!("{error:#}")))?;
            let profile = session::resumed_inference_profile(&events);
            let goal = events
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    EventPayload::TaskStateUpdated { state } if !state.goal.is_empty() => {
                        Some(state.goal.clone())
                    }
                    _ => None,
                })
                .or_else(|| session::prompt_history(&events).first().cloned())
                .unwrap_or_default();
            let recent = store
                .session_preview(summary.id, 6)
                .map_err(|error| SessionsErrorKind::Runtime(format!("{error:#}")))?
                .into_iter()
                .map(|line| PreviewLine {
                    speaker: line.speaker.to_owned(),
                    text: line.text,
                })
                .collect();
            let mut record = record(&store, &config, &summary)?;
            if let Some(profile) = &profile {
                record.provider = Some(profile.provider.to_string());
                if !profile.model.is_empty() {
                    record.model = Some(profile.model.clone());
                }
                record.effort = Some(crate::cli::output::reasoning_effort_name(profile.effort));
            }
            let detail = SessionDetail {
                summary: record,
                goal,
                recent,
            };
            match output {
                OutputFormat::Text => print_show_text(&detail),
                _ => print_json(&SessionShow {
                    schema_version: SESSIONS_SCHEMA_VERSION,
                    session: detail,
                }),
            }
            Ok(())
        }
    }
}

fn record(
    store: &EventStore,
    config: &Config,
    summary: &SessionSummary,
) -> Result<SessionRecord, SessionsErrorKind> {
    Ok(SessionRecord {
        id: summary.id.to_string(),
        workspace: summary.workspace.clone(),
        created_at: compact_timestamp(&summary.created_at),
        updated_at: compact_timestamp(&summary.updated_at),
        mode: summary.mode.unwrap_or(config.default_mode).to_string(),
        provider: provider_of(store, summary.id),
        model: summary.model.clone(),
        effort: summary
            .effort
            .map(crate::cli::output::reasoning_effort_name),
        event_count: summary.event_count,
        completion: summary
            .completion
            .as_ref()
            .map(crate::cli::output::completion_name),
        prompt_preview: summary.prompt_preview.clone(),
    })
}

/// The most recent provider the session actually resolved: an explicit
/// profile change when one exists, otherwise the last provider that served a
/// model request (which is where `SessionSummary::model` comes from too).
fn provider_of(store: &EventStore, session_id: uuid::Uuid) -> Option<String> {
    let events = store
        .events_of_kinds(
            session_id,
            &["inference_profile_changed", "model_request_started"],
        )
        .ok()?;
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::InferenceProfileChanged { provider, .. } => Some(provider.to_string()),
        EventPayload::ModelRequestStarted { provider, .. } => Some(provider.clone()),
        _ => None,
    })
}

/// Second-precision RFC 3339, stable for both text columns and machine
/// consumers. Sub-second precision is not needed to identify a session.
fn compact_timestamp(timestamp: &chrono::DateTime<chrono::Utc>) -> String {
    let text = timestamp.to_rfc3339();
    text.split('.').next().unwrap_or(&text).to_owned()
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(line) => println!("{line}"),
        Err(error) => eprintln!("error: could not serialize output: {error}"),
    }
}

fn print_list_text(records: &[SessionRecord]) {
    if records.is_empty() {
        println!("no sessions");
        return;
    }
    println!(
        "{:<36}  {:<25}  {:<4}  {:<28}  {:>6}  PROMPT",
        "ID", "UPDATED", "MODE", "MODEL", "EVENTS"
    );
    for record in records {
        println!(
            "{:<36}  {:<25}  {:<4}  {:<28}  {:>6}  {}",
            record.id,
            record.updated_at,
            record.mode,
            record.model.as_deref().unwrap_or("-"),
            record.event_count,
            record.prompt_preview.as_deref().unwrap_or("")
        );
    }
}

fn print_show_text(detail: &SessionDetail) {
    let summary = &detail.summary;
    println!("session      {}", summary.id);
    println!("workspace    {}", summary.workspace);
    println!("mode         {}", summary.mode);
    println!(
        "profile      {} · {} ({})",
        summary.provider.as_deref().unwrap_or("-"),
        summary.model.as_deref().unwrap_or("-"),
        summary.effort.as_deref().unwrap_or("-")
    );
    println!("created      {}", summary.created_at);
    println!("updated      {}", summary.updated_at);
    println!("events       {}", summary.event_count);
    if let Some(completion) = &summary.completion {
        println!("completion   {completion}");
    }
    if !detail.goal.is_empty() {
        println!("goal         {}", detail.goal);
    }
    if !detail.recent.is_empty() {
        println!();
        for line in &detail.recent {
            println!("{:<9}  {}", line.speaker, line.text);
        }
    }
}
