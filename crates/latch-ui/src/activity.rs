//! Transient, evidence-based activity. Timers never certify kernel completion.
use chrono::{DateTime, Utc};
use latch_protocol::{Event, EventPayload, StreamActivity};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    #[default]
    Idle,
    Preparing,
    WaitingModel,
    Thinking,
    Writing,
    PreparingTool,
    RunningTool,
    WaitingApproval,
    Cancelling,
    Completed,
    Cancelled,
    Error,
    Interrupted,
}
impl ActivityPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Preparing => "Preparing request",
            Self::WaitingModel => "Waiting for model",
            Self::Thinking => "Thinking",
            Self::Writing => "Writing response",
            Self::PreparingTool => "Preparing tool",
            Self::RunningTool => "Running tool",
            Self::WaitingApproval => "Waiting for approval",
            Self::Cancelling => "Stopping",
            Self::Completed => "Turn finished",
            Self::Cancelled => "Stopped",
            Self::Error => "Request failed",
            Self::Interrupted => "Interrupted",
        }
    }
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::WaitingModel
                | Self::Thinking
                | Self::Writing
                | Self::PreparingTool
                | Self::RunningTool
                | Self::WaitingApproval
                | Self::Cancelling
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityState {
    pub phase: ActivityPhase,
    pub phase_since: Option<DateTime<Utc>>,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub last_provider_activity_at: Option<DateTime<Utc>>,
    pub signal: Option<StreamActivity>,
    pub subject: Option<String>,
    pub model: Option<String>,
    running_tools: BTreeMap<String, String>,
    approvals: BTreeSet<Uuid>,
}
impl ActivityState {
    fn set(&mut self, phase: ActivityPhase, subject: Option<String>, now: DateTime<Utc>) {
        if self.phase != phase || self.subject != subject {
            self.phase_since = Some(now);
        }
        self.phase = phase;
        self.subject = subject;
        self.last_activity_at = Some(now);
    }
    pub fn preparing(&mut self, now: DateTime<Utc>) {
        self.set(ActivityPhase::Preparing, None, now);
    }
    /// Replaying an unfinished turn does not mean its provider or tools are live.
    pub fn interrupted(&mut self, now: DateTime<Utc>) {
        if self.phase.active() {
            self.approvals.clear();
            self.running_tools.clear();
            self.set(ActivityPhase::Interrupted, None, now);
        }
    }
    pub fn failed(&mut self, now: DateTime<Utc>) {
        if self.phase.active() {
            self.approvals.clear();
            self.running_tools.clear();
            self.set(ActivityPhase::Error, None, now);
        }
    }
    pub fn cancelling(&mut self, now: DateTime<Utc>) {
        if self.phase.active() {
            self.set(ActivityPhase::Cancelling, self.subject.clone(), now);
        }
    }
    pub fn text(&mut self, now: DateTime<Utc>) {
        self.last_provider_activity_at = Some(now);
        if matches!(
            self.phase,
            ActivityPhase::WaitingModel
                | ActivityPhase::Thinking
                | ActivityPhase::Writing
                | ActivityPhase::PreparingTool
        ) {
            self.set(ActivityPhase::Writing, None, now);
        }
    }
    pub fn signal(&mut self, signal: StreamActivity, now: DateTime<Utc>) {
        self.signal = Some(signal);
        self.last_provider_activity_at = Some(now);
        if !matches!(
            self.phase,
            ActivityPhase::WaitingModel
                | ActivityPhase::Thinking
                | ActivityPhase::Writing
                | ActivityPhase::PreparingTool
        ) {
            return;
        }
        let next = match signal {
            StreamActivity::Reasoning => Some(ActivityPhase::Thinking),
            StreamActivity::ToolCall => Some(ActivityPhase::PreparingTool),
            StreamActivity::Connected | StreamActivity::Receiving => None,
        };
        if let Some(phase) = next {
            self.set(phase, None, now);
        } else {
            self.last_activity_at = Some(now);
        }
    }
    pub fn apply_event(&mut self, event: &Event) {
        let now = event.timestamp;
        match &event.payload {
            EventPayload::RunStarted { .. } => {
                *self = Self::default();
                self.preparing(now);
            }
            EventPayload::ModelRequestStarted { model, .. } => {
                self.model = Some(model.clone());
                self.signal = None;
                self.last_provider_activity_at = None;
                if self.phase != ActivityPhase::Cancelling {
                    self.set(ActivityPhase::WaitingModel, None, now);
                }
            }
            EventPayload::ModelRequestFinished { .. } => {
                if self.phase != ActivityPhase::Cancelling {
                    self.set(ActivityPhase::Preparing, None, now);
                }
            }
            EventPayload::ToolRequested { call } => {
                if self.approvals.is_empty()
                    && self.running_tools.is_empty()
                    && self.phase != ActivityPhase::Cancelling
                {
                    self.set(ActivityPhase::PreparingTool, Some(call.name.clone()), now);
                }
            }
            EventPayload::ToolStarted { call_id, tool } => {
                self.running_tools.insert(call_id.clone(), tool.clone());
                if self.approvals.is_empty() && self.phase != ActivityPhase::Cancelling {
                    self.set(ActivityPhase::RunningTool, Some(tool.clone()), now);
                }
            }
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                self.running_tools.remove(&result.call_id);
                if self.approvals.is_empty() && self.phase != ActivityPhase::Cancelling {
                    let next = self.running_tools.values().next().cloned();
                    self.set(
                        if next.is_some() {
                            ActivityPhase::RunningTool
                        } else {
                            ActivityPhase::Preparing
                        },
                        next,
                        now,
                    );
                }
            }
            EventPayload::PermissionRequested {
                request_id, tool, ..
            } => {
                self.approvals.insert(*request_id);
                if self.phase != ActivityPhase::Cancelling {
                    self.set(ActivityPhase::WaitingApproval, Some(tool.clone()), now);
                }
            }
            EventPayload::PermissionResolved { request_id, .. } => {
                self.approvals.remove(request_id);
                if self.approvals.is_empty() && self.phase == ActivityPhase::WaitingApproval {
                    self.set(ActivityPhase::PreparingTool, self.subject.clone(), now);
                }
            }
            EventPayload::RunCompleted { outcome, .. } => {
                self.approvals.clear();
                self.running_tools.clear();
                self.set(
                    match outcome.as_str() {
                        "completed" => ActivityPhase::Completed,
                        "cancelled" => ActivityPhase::Cancelled,
                        _ => ActivityPhase::Error,
                    },
                    None,
                    now,
                );
            }
            EventPayload::SessionResumed if self.phase.active() => {
                self.interrupted(now);
            }
            _ => {}
        }
    }
    pub fn elapsed_seconds(&self, now: DateTime<Utc>) -> u64 {
        self.phase_since
            .map_or(0, |since| (now - since).num_seconds().max(0) as u64)
    }
    pub fn quiet_seconds(&self, now: DateTime<Utc>) -> u64 {
        self.last_activity_at
            .map_or(0, |since| (now - since).num_seconds().max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_protocol::ProviderId;
    fn event(payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            parent_id: None,
            sequence: 1,
            session_id: Uuid::nil(),
            timestamp: Utc::now(),
            payload,
        }
    }
    #[test]
    fn silence_never_claims_thinking_or_failure() {
        let mut state = ActivityState::default();
        state.apply_event(&event(EventPayload::ModelRequestStarted {
            provider: ProviderId::from("test").to_string(),
            model: "model".into(),
        }));
        let start = state.phase_since.unwrap();
        assert_eq!(
            state.elapsed_seconds(start + chrono::Duration::seconds(90)),
            90
        );
        assert_eq!(state.phase, ActivityPhase::WaitingModel);
        state.signal(
            StreamActivity::Reasoning,
            start + chrono::Duration::seconds(91),
        );
        assert_eq!(state.phase, ActivityPhase::Thinking);
        state.signal(
            StreamActivity::Receiving,
            start + chrono::Duration::seconds(92),
        );
        assert_eq!(
            state.phase_since,
            Some(start + chrono::Duration::seconds(91))
        );
        state.text(start + chrono::Duration::seconds(93));
        assert_eq!(state.phase, ActivityPhase::Writing);
    }
    #[test]
    fn resume_cannot_resurrect_a_running_phase() {
        let mut state = ActivityState::default();
        state.preparing(Utc::now());
        state.apply_event(&event(EventPayload::SessionResumed));
        assert_eq!(state.phase, ActivityPhase::Interrupted);
    }
    #[test]
    fn cancellation_and_failure_cannot_be_overwritten_by_late_activity() {
        let mut state = ActivityState::default();
        state.preparing(Utc::now());
        state.cancelling(Utc::now());
        state.apply_event(&event(EventPayload::ToolStarted {
            call_id: "late".into(),
            tool: "shell".into(),
        }));
        state.signal(StreamActivity::Reasoning, Utc::now());
        state.text(Utc::now());
        assert_eq!(state.phase, ActivityPhase::Cancelling);
        state.failed(Utc::now());
        state.text(Utc::now());
        state.signal(StreamActivity::Reasoning, Utc::now());
        assert_eq!(state.phase, ActivityPhase::Error);
    }
    #[test]
    fn approval_and_completion_take_precedence_over_late_signals() {
        let mut state = ActivityState::default();
        let id = Uuid::new_v4();
        state.apply_event(&event(EventPayload::PermissionRequested {
            request_id: id,
            tool: "shell".into(),
            arguments: serde_json::json!({}),
            reason: "review".into(),
            capabilities: Vec::new(),
        }));
        state.signal(StreamActivity::Reasoning, Utc::now());
        assert_eq!(state.phase, ActivityPhase::WaitingApproval);
        state.apply_event(&event(EventPayload::RunCompleted {
            run_id: Uuid::new_v4(),
            outcome: "cancelled".into(),
        }));
        state.signal(StreamActivity::Receiving, Utc::now());
        assert_eq!(state.phase, ActivityPhase::Cancelled);
    }
}
