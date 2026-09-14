//! Root-visible child-agent state derived from durable events.
//!
//! Child sessions are independent durable transcripts; their tokens, tool
//! calls, and file changes never cross into the root. What does cross is
//! compact and authoritative: the root's own agent-control tool calls
//! (`spawn_agent`, `wait_agents`, ...), their structured results, and the
//! single semantic `AgentNotificationDelivered` report per completed child
//! turn. This reducer turns exactly those events into a small view model the
//! status row and the sidebar can show without inventing progress.

use latch_protocol::{AgentStatus, Event, EventPayload, ToolCall, ToolResult};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

/// The root-only agent coordination tools. Child sessions are denied these,
/// so their presence in a root event stream is authoritative.
pub const AGENT_CONTROL_TOOLS: &[&str] = &[
    "spawn_agent",
    "send_agent_message",
    "continue_agent",
    "wait_agents",
    "list_agents",
    "interrupt_agent",
    "close_agent",
];

#[must_use]
pub fn is_agent_control(name: &str) -> bool {
    AGENT_CONTROL_TOOLS.contains(&name)
}

/// One known child session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentView {
    pub agent_id: Option<Uuid>,
    pub task_name: String,
    pub agent_type: Option<String>,
    pub status: AgentStatus,
    pub summary: String,
}

impl SubagentView {
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self.status, AgentStatus::Starting | AgentStatus::Running)
    }

    /// Short status word for the sidebar and status row.
    #[must_use]
    pub fn label(&self) -> &'static str {
        status_label(&self.status)
    }
}

#[must_use]
pub fn status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "starting",
        AgentStatus::Running => "running",
        AgentStatus::Completed => "completed",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Failed => "failed",
        AgentStatus::Closed => "closed",
    }
}

/// Deterministic child-agent view model. Live and replay event streams produce
/// the same state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentModel {
    agents: Vec<SubagentView>,
    /// Control-tool call id to the child it targets, so a bare result (for
    /// example `close_agent`) can still update the right entry.
    calls: BTreeMap<String, Uuid>,
}

impl SubagentModel {
    #[must_use]
    pub fn agents(&self) -> &[SubagentView] {
        &self.agents
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Children currently starting or running.
    #[must_use]
    pub fn active(&self) -> Vec<&SubagentView> {
        self.agents
            .iter()
            .filter(|agent| agent.is_active())
            .collect()
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.agents.iter().filter(|agent| agent.is_active()).count()
    }

    pub fn apply_event(&mut self, event: &Event) {
        match &event.payload {
            EventPayload::ToolRequested { call } => self.on_call(call),
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                self.on_result(result);
            }
            EventPayload::AgentNotificationDelivered { report } => {
                self.upsert(
                    Some(report.agent_id),
                    &report.task_name,
                    None,
                    report.status,
                    report
                        .summary
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                );
            }
            // Defensive: child-session lifecycle events are not part of the
            // root stream today, but if a future surface forwards them the
            // reducer stays correct.
            EventPayload::AgentStatusChanged { status, .. } => {
                if let Some(agent) = self.agents.last_mut() {
                    agent.status = *status;
                }
            }
            _ => {}
        }
    }

    fn on_call(&mut self, call: &ToolCall) {
        if !is_agent_control(&call.name) {
            return;
        }
        let agent_id = call
            .arguments
            .get("agent_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok());
        if let Some(agent_id) = agent_id {
            self.calls.insert(call.id.clone(), agent_id);
        }
        match call.name.as_str() {
            "spawn_agent" => {
                let task_name = call
                    .arguments
                    .get("task_name")
                    .and_then(Value::as_str)
                    .unwrap_or("child agent")
                    .to_owned();
                let agent_type = call
                    .arguments
                    .get("agent_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.upsert(
                    None,
                    &task_name,
                    agent_type,
                    AgentStatus::Starting,
                    "starting".into(),
                );
            }
            "send_agent_message" | "continue_agent" => {
                if let Some(agent_id) = agent_id {
                    self.set_status(agent_id, AgentStatus::Running, "message queued");
                }
            }
            "interrupt_agent" => {
                if let Some(agent_id) = agent_id {
                    self.set_status(agent_id, AgentStatus::Interrupted, "interrupt requested");
                }
            }
            "close_agent" => {
                if let Some(agent_id) = agent_id {
                    self.set_status(agent_id, AgentStatus::Closed, "closed");
                }
            }
            _ => {}
        }
    }

    fn on_result(&mut self, result: &ToolResult) {
        if !is_agent_control(&result.name) {
            return;
        }
        let failed = result.is_error;
        match result.name.as_str() {
            "spawn_agent" => {
                if let Ok(snapshot) = serde_json::from_str::<AgentSnapshot>(&result.output) {
                    self.remove_pending(&snapshot.task_name);
                    self.upsert(
                        Some(snapshot.agent_id),
                        &snapshot.task_name,
                        snapshot.agent_type,
                        if failed {
                            AgentStatus::Failed
                        } else {
                            snapshot.status
                        },
                        if failed {
                            "spawn failed".into()
                        } else {
                            "started".into()
                        },
                    );
                } else if failed {
                    self.mark_call_failed(&result.call_id, "spawn failed");
                }
            }
            "wait_agents" => {
                if let Ok(value) = serde_json::from_str::<Value>(&result.output) {
                    let snapshot_values = value
                        .get("agents")
                        .and_then(Value::as_array)
                        .cloned()
                        .or_else(|| value.as_array().cloned())
                        .unwrap_or_default();
                    let mut running = 0usize;
                    let mut done = 0usize;
                    for value in snapshot_values {
                        if let Ok(snapshot) = serde_json::from_value::<AgentSnapshot>(value) {
                            if matches!(
                                snapshot.status,
                                AgentStatus::Starting | AgentStatus::Running
                            ) {
                                running += 1;
                            } else {
                                done += 1;
                            }
                            self.upsert(
                                Some(snapshot.agent_id),
                                &snapshot.task_name,
                                snapshot.agent_type,
                                snapshot.status,
                                String::new(),
                            );
                        }
                    }
                    let summary = if failed {
                        "wait failed".to_owned()
                    } else if running == 0 && done == 0 {
                        "no child agents".to_owned()
                    } else {
                        format!("{running} running · {done} finished")
                    };
                    self.set_call_summary(&result.call_id, summary);
                } else {
                    self.set_call_summary(&result.call_id, first_line(&result.output));
                }
            }
            "list_agents" => {
                if let Ok(snapshots) = serde_json::from_str::<Vec<AgentSnapshot>>(&result.output) {
                    let count = snapshots.len();
                    for snapshot in snapshots {
                        self.upsert(
                            Some(snapshot.agent_id),
                            &snapshot.task_name,
                            snapshot.agent_type,
                            snapshot.status,
                            String::new(),
                        );
                    }
                    self.set_call_summary(&result.call_id, format!("{count} known"));
                }
            }
            "close_agent" => self.resolve_call(&result.call_id, AgentStatus::Closed, "closed"),
            "interrupt_agent" => {
                self.resolve_call(&result.call_id, AgentStatus::Interrupted, "interrupted")
            }
            _ => {}
        }
    }

    fn upsert(
        &mut self,
        agent_id: Option<Uuid>,
        task_name: &str,
        agent_type: Option<String>,
        status: AgentStatus,
        summary: String,
    ) {
        let existing = match agent_id {
            Some(agent_id) => self
                .agents
                .iter_mut()
                .find(|agent| agent.agent_id == Some(agent_id)),
            None => self
                .agents
                .iter_mut()
                .find(|agent| agent.agent_id.is_none() && agent.task_name == task_name),
        };
        if let Some(agent) = existing {
            agent.status = status;
            if !summary.is_empty() {
                agent.summary = summary;
            }
            if agent_type.is_some() {
                agent.agent_type = agent_type;
            }
            return;
        }
        self.agents.push(SubagentView {
            agent_id,
            task_name: task_name.to_owned(),
            agent_type,
            status,
            summary,
        });
    }

    fn set_status(&mut self, agent_id: Uuid, status: AgentStatus, summary: &str) {
        if let Some(agent) = self
            .agents
            .iter_mut()
            .find(|agent| agent.agent_id == Some(agent_id))
        {
            agent.status = status;
            agent.summary = summary.to_owned();
        }
    }

    fn resolve_call(&mut self, call_id: &str, status: AgentStatus, summary: &str) {
        if let Some(agent_id) = self.calls.get(call_id).copied() {
            self.set_status(agent_id, status, summary);
        }
    }

    fn mark_call_failed(&mut self, call_id: &str, summary: &str) {
        if let Some(agent_id) = self.calls.get(call_id).copied() {
            self.set_status(agent_id, AgentStatus::Failed, summary);
        }
    }

    fn set_call_summary(&mut self, call_id: &str, summary: String) {
        if let Some(agent_id) = self.calls.get(call_id).copied()
            && let Some(agent) = self
                .agents
                .iter_mut()
                .find(|agent| agent.agent_id == Some(agent_id))
        {
            agent.summary = summary;
        }
    }

    fn remove_pending(&mut self, task_name: &str) {
        self.agents
            .retain(|agent| !(agent.agent_id.is_none() && agent.task_name == task_name));
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// Shape of the root-visible `AgentSnapshot` returned by `spawn_agent` and
/// `list_agents`. Mirrors the kernel's serialization without importing kernel
/// types.
#[derive(Debug, serde::Deserialize)]
struct AgentSnapshot {
    agent_id: Uuid,
    task_name: String,
    #[serde(default)]
    agent_type: Option<String>,
    status: AgentStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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

    fn call(id: &str, name: &str, arguments: Value) -> Event {
        event(EventPayload::ToolRequested {
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            },
        })
    }

    fn result(id: &str, name: &str, output: &str) -> Event {
        event(EventPayload::ToolCompleted {
            result: ToolResult {
                call_id: id.into(),
                name: name.into(),
                output: output.into(),
                is_error: false,
                artifact_id: None,
                media: Vec::new(),
            },
        })
    }

    #[test]
    fn spawn_result_places_the_child_and_tracks_its_status() {
        let mut model = SubagentModel::default();
        model.apply_event(&call(
            "s1",
            "spawn_agent",
            serde_json::json!({"task_name": "audit-locks", "message": "Review locking", "agent_type": "explorer"}),
        ));
        assert_eq!(model.agents().len(), 1);
        assert_eq!(model.agents()[0].status, AgentStatus::Starting);
        model.apply_event(&result(
            "s1",
            "spawn_agent",
            &format!(
                "{{\"agent_id\":\"{}\",\"task_name\":\"audit-locks\",\"agent_type\":\"explorer\",\"status\":\"running\"}}",
                Uuid::new_v4()
            ),
        ));
        assert_eq!(model.agents().len(), 1);
        assert_eq!(model.agents()[0].status, AgentStatus::Running);
        assert!(model.agents()[0].agent_id.is_some());
        assert_eq!(model.active_count(), 1);
    }

    #[test]
    fn wait_and_list_results_update_known_children() {
        let mut model = SubagentModel::default();
        let id = Uuid::new_v4();
        model.apply_event(&result(
            "w1",
            "wait_agents",
            &format!(
                "{{\"agents\":[{{\"agent_id\":\"{id}\",\"task_name\":\"perf\",\"agent_type\":null,\"status\":\"completed\"}}],\"reports_pending\":1}}"
            ),
        ));
        assert_eq!(model.agents()[0].task_name, "perf");
        assert_eq!(model.agents()[0].status, AgentStatus::Completed);
        assert_eq!(model.active_count(), 0);
    }

    #[test]
    fn delivered_report_is_authoritative_for_status() {
        let mut model = SubagentModel::default();
        let agent_id = Uuid::new_v4();
        model.apply_event(&call(
            "s1",
            "spawn_agent",
            serde_json::json!({"task_name": "audit-locks", "message": "Review locking"}),
        ));
        model.apply_event(&result(
            "s1",
            "spawn_agent",
            &format!(
                "{{\"agent_id\":\"{agent_id}\",\"task_name\":\"audit-locks\",\"agent_type\":null,\"status\":\"running\"}}"
            ),
        ));
        model.apply_event(&event(EventPayload::AgentNotificationDelivered {
            report: latch_protocol::AgentReport {
                report_id: Uuid::new_v4(),
                agent_id,
                task_name: "audit-locks".into(),
                status: AgentStatus::Failed,
                completion: latch_protocol::CompletionState::InProgress,
                summary: "Two locks are unsynchronized.\nMore detail".into(),
                findings: vec![],
                touched_files: vec![],
                evidence: vec![],
                unresolved_questions: vec![],
            },
        }));
        assert_eq!(model.agents().len(), 1);
        assert_eq!(model.agents()[0].status, AgentStatus::Failed);
        assert_eq!(model.agents()[0].summary, "Two locks are unsynchronized.");
    }

    #[test]
    fn close_result_uses_the_recorded_call_target() {
        let mut model = SubagentModel::default();
        let id = Uuid::new_v4();
        model.apply_event(&result(
            "l1",
            "list_agents",
            &format!(
                "[{{\"agent_id\":\"{id}\",\"task_name\":\"audit-locks\",\"agent_type\":null,\"status\":\"running\"}}]"
            ),
        ));
        model.apply_event(&call(
            "c1",
            "close_agent",
            serde_json::json!({"agent_id": id.to_string()}),
        ));
        model.apply_event(&result("c1", "close_agent", "agent closed"));
        assert_eq!(model.agents()[0].status, AgentStatus::Closed);
    }
}
