use super::dispatch::{tool_error, tool_ok};
use super::*;
use crate::agents::{DEFAULT_MAX_AGENT_DEPTH, DelegationContext};
use std::time::Duration;

const CONTROL_TOOLS: &[&str] = &[
    "spawn_agent",
    "send_agent_message",
    "continue_agent",
    "wait_agents",
    "list_agents",
    "interrupt_agent",
    "close_agent",
];

pub(super) fn is_agent_control(name: &str) -> bool {
    CONTROL_TOOLS.contains(&name)
}

impl Agent {
    pub(super) async fn execute_agent_control(
        &mut self,
        call: &ToolCall,
        cancel: &CancellationToken,
        sink: &AgentEventSink,
    ) -> Result<ToolResult> {
        self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        )?;
        let result = if self.agent_depth >= DEFAULT_MAX_AGENT_DEPTH || self.supervisor.is_none() {
            tool_error(
                call,
                "agent control is root-only; maximum spawn depth is 1".into(),
            )
        } else {
            match self.execute_root_agent_control(call, cancel).await {
                Ok(output) => tool_ok(call, output),
                Err(error) => tool_error(call, error.to_string()),
            }
        };
        self.emit(
            if result.is_error {
                EventPayload::ToolFailed {
                    result: result.clone(),
                }
            } else {
                EventPayload::ToolCompleted {
                    result: result.clone(),
                }
            },
            sink,
        )?;
        Ok(result)
    }

    async fn execute_root_agent_control(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let supervisor = self.supervisor.as_ref().expect("checked above");
        match call.name.as_str() {
            "spawn_agent" => {
                let task_name = required_string(call, "task_name")?;
                let message = required_string(call, "message")?;
                let agent_type = call
                    .arguments
                    .get("agent_type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let user_constraints = self
                    .store
                    .memories(self.session_id)?
                    .into_iter()
                    .filter(|memory| {
                        memory.kind == MemoryKind::UserConstraint
                            && memory.validity == Validity::Active
                    })
                    .map(|memory| memory.content)
                    .collect();
                let child = supervisor
                    .spawn_agent(
                        task_name,
                        message,
                        agent_type,
                        DelegationContext {
                            constraints: user_constraints,
                            decisions: self.state.state().decisions.clone(),
                        },
                    )
                    .await?;
                Ok(serde_json::to_string(&child)?)
            }
            "send_agent_message" | "continue_agent" => {
                let agent_id = required_agent_id(call)?;
                let message = required_string(call, "message")?;
                if call.name == "continue_agent" {
                    supervisor.continue_agent(agent_id, message).await?;
                } else {
                    supervisor.send_message(agent_id, message).await?;
                }
                Ok(format!("message queued for agent {agent_id}"))
            }
            "wait_agents" => {
                let ids = optional_agent_ids(call)?;
                let timeout_ms = call
                    .arguments
                    .get("timeout_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(30_000)
                    .min(300_000);
                let waited = tokio::select! {
                    waited = supervisor.wait_agents(&ids, Duration::from_millis(timeout_ms)) => waited?,
                    () = cancel.cancelled() => anyhow::bail!("wait_agents cancelled"),
                };
                // Report bodies travel on exactly one channel: the durable
                // kernel notification this loop delivers at the next safe
                // model boundary — the same request that reads this result.
                // Embedding them here as well would double every report in
                // the provider-visible conversation.
                Ok(serde_json::to_string(&json!({
                    "agents": waited.agents,
                    "reports_pending": waited.reports.len(),
                }))?)
            }
            "list_agents" => Ok(serde_json::to_string(&supervisor.list_agents())?),
            "interrupt_agent" => {
                let agent_id = required_agent_id(call)?;
                supervisor.interrupt_agent(agent_id).await?;
                Ok(format!("interrupt requested for agent {agent_id}"))
            }
            "close_agent" => {
                let agent_id = required_agent_id(call)?;
                supervisor.close_agent(agent_id).await?;
                Ok(format!("agent {agent_id} closed"))
            }
            _ => anyhow::bail!("unknown agent control tool"),
        }
    }

    /// Flushes asynchronous child reports into this session only where a new
    /// model request can safely begin. The durable delivery event is the sole
    /// provider-visible graph event and is also the resume dedupe marker.
    pub(super) fn deliver_agent_notifications(&mut self, sink: &AgentEventSink) -> Result<usize> {
        let Some(supervisor) = self.supervisor.clone() else {
            return Ok(0);
        };
        let reports = supervisor.drain_notifications();
        let count = reports.len();
        let mut reports = reports.into_iter();
        while let Some(report) = reports.next() {
            if let Err(error) = self.emit(
                EventPayload::AgentNotificationDelivered {
                    report: report.clone(),
                },
                sink,
            ) {
                let mut remaining = vec![report];
                remaining.extend(reports);
                supervisor.restore_notifications(remaining);
                return Err(error);
            }
        }
        Ok(count)
    }
}

fn required_string(call: &ToolCall, field: &str) -> Result<String> {
    call.arguments
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{field} must be a non-empty string"))
}

fn required_agent_id(call: &ToolCall) -> Result<Uuid> {
    Uuid::parse_str(&required_string(call, "agent_id")?)
        .map_err(|error| anyhow!("invalid agent_id: {error}"))
}

fn optional_agent_ids(call: &ToolCall) -> Result<Vec<Uuid>> {
    call.arguments
        .get("agent_ids")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .map(|id| {
                    id.as_str()
                        .ok_or_else(|| anyhow!("agent_ids must contain strings"))
                        .and_then(|id| {
                            Uuid::parse_str(id)
                                .map_err(|error| anyhow!("invalid agent_id: {error}"))
                        })
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}
