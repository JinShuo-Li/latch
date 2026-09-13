//! Session-level resume semantics shared by the CLI.
//!
//! Resume must be a real user-level resume: the visible transcript replays
//! from durable events without re-executing anything, the effective mode is
//! restored from the session's own mode history, and the prompt history is
//! reconstructed from existing user events rather than a second history
//! database.

use latch_protocol::{DisplayItem, Event, EventPayload, Mode, PermissionMode, Safety};

/// Resolves the effective startup mode.
///
/// Precedence: explicit CLI `--mode` > the session's own last durable mode
/// change > the configured default. A session that transitioned WORK → PLAN
/// and exited therefore resumes in PLAN unless overridden.
#[must_use]
pub fn resumed_mode(events: &[Event], cli: Option<Mode>, default: Mode) -> Mode {
    cli.or_else(|| {
        events.iter().rev().find_map(|event| match event.payload {
            EventPayload::ModeChanged { mode } => Some(mode),
            _ => None,
        })
    })
    .unwrap_or(default)
}

/// Effective safety profile after resume: the session's last durable change,
/// falling back to the configured default. Resume never silently broadens or
/// narrows the policy beyond what the session actually ended with.
#[must_use]
pub fn resumed_safety(events: &[Event], default: Safety) -> Safety {
    events
        .iter()
        .rev()
        .find_map(|event| match event.payload {
            EventPayload::SafetyChanged { safety } => Some(safety),
            _ => None,
        })
        .unwrap_or(default)
}

/// Effective permission resolver after resume.
#[must_use]
pub fn resumed_permissions(events: &[Event], default: PermissionMode) -> PermissionMode {
    events
        .iter()
        .rev()
        .find_map(|event| match event.payload {
            EventPayload::PermissionsChanged { mode } => Some(mode),
            _ => None,
        })
        .unwrap_or(default)
}

/// The inference profile a session last selected. `None` means the session
/// never changed profile and starts from configuration. Credentials are never
/// part of this record; the CLI resolves them freshly at startup.
#[must_use]
pub fn resumed_inference_profile(events: &[Event]) -> Option<latch_protocol::InferenceProfile> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::InferenceProfileChanged {
            provider,
            model,
            effort,
            ..
        } => Some(latch_protocol::InferenceProfile {
            provider: provider.clone(),
            model: model.clone(),
            effort: *effort,
        }),
        _ => None,
    })
}

/// Rebuilds the user-visible transcript from durable events in chronological
/// order. Hidden internals (reasoning content, context statistics, model usage,
/// raw task state) never appear; this is the exact formatter the live TUI
/// path uses, so replayed history and live rendering always agree.
#[must_use]
pub fn replay_items(events: &[Event]) -> Vec<DisplayItem> {
    events
        .iter()
        .flat_map(latch_protocol::display_items)
        .collect()
}

/// Reconstructs submitted prompt history from existing user events. Slash
/// commands never become durable user messages, so they are naturally absent.
#[must_use]
pub fn prompt_history(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserMessage { text } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn event(payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sequence: 1,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    #[test]
    fn mode_precedence_cli_over_session_over_default() {
        let events = vec![
            event(EventPayload::ModeChanged { mode: Mode::Work }),
            event(EventPayload::ModeChanged { mode: Mode::Plan }),
        ];
        assert_eq!(resumed_mode(&events, None, Mode::Ask), Mode::Plan);
        assert_eq!(
            resumed_mode(&events, Some(Mode::Work), Mode::Ask),
            Mode::Work
        );
        assert_eq!(resumed_mode(&[], None, Mode::Ask), Mode::Ask);
    }

    #[test]
    fn safety_and_permissions_resume_from_the_session_history() {
        let events = vec![
            event(EventPayload::SafetyChanged {
                safety: Safety::Strict,
            }),
            event(EventPayload::PermissionsChanged {
                mode: PermissionMode::AutoApprove,
            }),
            event(EventPayload::SafetyChanged {
                safety: Safety::Autonomous,
            }),
        ];
        assert_eq!(
            resumed_safety(&events, Safety::Standard),
            Safety::Autonomous
        );
        assert_eq!(
            resumed_permissions(&events, PermissionMode::Human),
            PermissionMode::AutoApprove
        );
        assert_eq!(resumed_safety(&[], Safety::Standard), Safety::Standard);
        assert_eq!(
            resumed_permissions(&[], PermissionMode::Human),
            PermissionMode::Human
        );
    }

    #[test]
    fn replay_shows_history_but_never_hidden_internals() {
        let events = vec![
            event(EventPayload::UserMessage {
                text: "fix the bug".into(),
            }),
            event(EventPayload::AssistantMessageCompleted {
                text: "looking".into(),
                tool_calls: vec![],
                reasoning_content: Some("secret reasoning that must not display".into()),
            }),
            event(EventPayload::ToolFailed {
                result: latch_protocol::ToolResult {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    output: "exit code 1\nFAIL".into(),
                    is_error: true,
                    artifact_id: None,
                },
            }),
            event(EventPayload::ToolCompleted {
                result: latch_protocol::ToolResult {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    output: "exit 0".into(),
                    is_error: false,
                    artifact_id: None,
                },
            }),
            event(EventPayload::ContextMaterialized {
                stats: latch_protocol::ContextStats::default(),
            }),
            event(EventPayload::ModelUsage {
                usage: latch_protocol::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cache_miss_tokens: None,
                },
            }),
            event(EventPayload::TaskStateUpdated {
                state: latch_protocol::TaskState::default(),
            }),
        ];
        let items = replay_items(&events);
        let rendered = format!("{items:?}");
        assert!(rendered.contains("fix the bug"));
        assert!(rendered.contains("looking"));
        assert!(!rendered.contains("secret reasoning"));
        assert!(!rendered.contains("ContextMaterialized"));
        assert!(!rendered.contains("TaskStateUpdated"));
        // The failed and completed rows share one call id so the transcript
        // can update a single lifecycle item.
        let tool_rows: Vec<&DisplayItem> = items
            .iter()
            .filter(|item| item.call_id().is_some())
            .collect();
        assert_eq!(tool_rows.len(), 2);
    }

    #[test]
    fn prompt_history_rebuilds_from_user_events_only() {
        let events = vec![
            event(EventPayload::UserMessage {
                text: "first prompt".into(),
            }),
            event(EventPayload::AssistantMessageCompleted {
                text: "ok".into(),
                tool_calls: vec![],
                reasoning_content: None,
            }),
            event(EventPayload::UserMessage {
                text: "second prompt".into(),
            }),
        ];
        assert_eq!(
            prompt_history(&events),
            vec!["first prompt", "second prompt"]
        );
    }
}
