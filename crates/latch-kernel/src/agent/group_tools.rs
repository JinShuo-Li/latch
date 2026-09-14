//! Stable model-facing agent-group tools: `group_task`, `group_message`, and
//! `group_status`. The schemas are fixed and the kernel enforces ownership;
//! the model never gains authority merely by asking.

use super::dispatch::{tool_error, tool_ok};
use super::*;
use crate::agents::{GroupState, GroupStatus};
use anyhow::bail;
use latch_protocol::{GroupMessageTarget, GroupTask, GroupTaskStatus};
use serde_json::Value;
use uuid::Uuid;

pub(super) const GROUP_TOOLS: &[&str] = &["group_task", "group_message", "group_status"];

pub(super) fn is_group_tool(name: &str) -> bool {
    GROUP_TOOLS.contains(&name)
}

const TASK_LIST_LIMIT: usize = 20;
const MESSAGE_LIST_LIMIT: usize = 20;

impl Agent {
    /// Delivers every queued group message addressed to this session at a safe
    /// model boundary. Delivery is durable and exactly-once; it never wakes an
    /// idle agent because it only runs at a boundary of an already-running
    /// turn.
    pub(super) fn deliver_group_messages(&mut self, sink: &AgentEventSink) -> Result<usize> {
        let Some(group) = self.group.clone() else {
            return Ok(0);
        };
        let is_root = self.agent_depth == 0;
        let delivered = group.deliver_pending(self.session_id, is_root)?;
        if delivered.is_empty() {
            return Ok(0);
        }
        // The delivery events were appended directly to this session's durable
        // log; forward them so live consumers see the same order replay does.
        self.forward_appended_events(sink)?;
        Ok(delivered.len())
    }

    /// Concise reason terminal root completion is blocked by unfinished
    /// required group work, if any.
    pub(crate) fn group_completion_blocker(&self) -> Option<String> {
        let group = self.group.as_ref()?;
        group.completion_blocker()
    }

    /// Human-readable `/group` view. Names come from the root's live agent
    /// graph when available; otherwise short ids are shown.
    pub fn group_overview_text(&self) -> String {
        let Some(group) = self.group.as_ref() else {
            return "no agent group in this session".into();
        };
        let Some(status) = group.status() else {
            return "no agent group has been created yet; create a task with group_task to start one".into();
        };
        let agents = self
            .supervisor
            .as_ref()
            .map(AgentSupervisor::list_agents)
            .unwrap_or_default();
        let name_for = |agent: Uuid| -> String {
            agents
                .iter()
                .find(|snapshot| snapshot.agent_id == agent)
                .map(|snapshot| snapshot.task_name.clone())
                .unwrap_or_else(|| short_id(agent))
        };
        let status_for = |agent: Uuid| -> String {
            agents
                .iter()
                .find(|snapshot| snapshot.agent_id == agent)
                .map(|snapshot| format!("{:?}", snapshot.status).to_ascii_lowercase())
                .unwrap_or_else(|| "?".into())
        };
        let mut lines = vec![format!(
            "Agent group `{}` ({})",
            status.identity.name,
            short_id(status.identity.group_id)
        )];
        lines.push(format!(
            "tasks {} · ready {} · active {} · blocked {} · done {} · cancelled {}",
            status.counts.total,
            status.counts.ready,
            status.counts.active(),
            status.counts.blocked,
            status.counts.completed,
            status.counts.cancelled
        ));
        let mut members = status.members.clone();
        members.sort();
        if members.is_empty() {
            lines.push("members: none".into());
        } else {
            let listed = members
                .iter()
                .map(|member| format!("{} ({})", name_for(*member), status_for(*member)))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("members: {listed}"));
        }
        let state = group.snapshot();
        let tasks = state.ordered_tasks();
        let shown = tasks.len().min(24);
        for task in tasks.iter().take(shown) {
            lines.push(overview_task_line(task, &name_for));
        }
        if tasks.len() > shown {
            lines.push(format!("… and {} more", tasks.len() - shown));
        }
        for (task_id, dependencies) in &status.dependency_failures {
            lines.push(format!(
                "[warning] {} depends on blocked/cancelled {}",
                name_for_task(&status, *task_id),
                dependencies
                    .iter()
                    .map(|dependency| name_for_task(&status, *dependency))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for conflict in &status.conflicts {
            lines.push(format!(
                "[warning] possible workspace conflict: `{}` in {} and {}",
                conflict.path,
                name_for_task(&status, conflict.task_a),
                name_for_task(&status, conflict.task_b)
            ));
        }
        lines.join("\n")
    }

    pub(super) fn execute_group_tool(
        &mut self,
        call: &ToolCall,
        sink: &AgentEventSink,
    ) -> Result<ToolResult> {
        self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        )?;
        let result = self.run_group_tool(call);
        let payload = if result.is_error {
            EventPayload::ToolFailed {
                result: result.clone(),
            }
        } else {
            EventPayload::ToolCompleted {
                result: result.clone(),
            }
        };
        self.emit(payload, sink)?;
        Ok(result)
    }

    fn run_group_tool(&mut self, call: &ToolCall) -> ToolResult {
        let Some(group) = self.group.clone() else {
            return tool_error(call, "agent groups are unavailable in this session".into());
        };
        match call.name.as_str() {
            "group_status" => {
                if let Err(error) = reject_extra_fields(call, &[]) {
                    return tool_error(call, error.to_string());
                }
                let Some(status) = group.status() else {
                    return tool_ok(call, "no agent group has been created yet".into());
                };
                tool_ok(call, status_text(&status))
            }
            "group_task" => self.run_group_task_tool(call, &group),
            "group_message" => self.run_group_message_tool(call, &group),
            _ => tool_error(call, "unknown group tool".into()),
        }
    }

    fn run_group_task_tool(&self, call: &ToolCall, group: &GroupCoordinator) -> ToolResult {
        let Some(op) = call.arguments.get("op").and_then(Value::as_str) else {
            return tool_error(call, "group_task requires `op`".into());
        };
        let is_root = self.agent_depth == 0;
        match op {
            "create" => {
                if let Err(error) = reject_extra_fields(
                    call,
                    &[
                        "op",
                        "title",
                        "description",
                        "dependencies",
                        "required",
                        "expected_paths",
                    ],
                ) {
                    return tool_error(call, error.to_string());
                }
                if !is_root {
                    return tool_error(call, "only the root agent may create group tasks".into());
                }
                let title = match required_str(call, "title") {
                    Ok(value) => value,
                    Err(error) => return tool_error(call, error.to_string()),
                };
                let description = call
                    .arguments
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let dependencies = match uuid_array(call, "dependencies") {
                    Ok(value) => value,
                    Err(error) => return tool_error(call, error.to_string()),
                };
                let required = call
                    .arguments
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let expected_paths = match string_array(call, "expected_paths") {
                    Ok(value) => value,
                    Err(error) => return tool_error(call, error.to_string()),
                };
                match group.create_task(
                    self.session_id,
                    title,
                    description,
                    dependencies,
                    required,
                    expected_paths,
                ) {
                    Ok(task) => tool_ok(
                        call,
                        format!("created group task\n{}", task_line(&task, &short_name)),
                    ),
                    Err(error) => tool_error(call, error.to_string()),
                }
            }
            "list" => {
                if let Err(error) = reject_extra_fields(call, &["op", "status", "limit"]) {
                    return tool_error(call, error.to_string());
                }
                if !group.exists() {
                    return tool_ok(call, "no agent group has been created yet".into());
                }
                let filter = call
                    .arguments
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("open");
                let limit = call
                    .arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|limit| (limit as usize).clamp(1, TASK_LIST_LIMIT))
                    .unwrap_or(TASK_LIST_LIMIT);
                match list_tasks_text(&group.snapshot(), filter, limit) {
                    Ok(text) => tool_ok(call, text),
                    Err(error) => tool_error(call, error.to_string()),
                }
            }
            "claim" => {
                if let Err(error) = reject_extra_fields(call, &["op", "task_id", "agent_id"]) {
                    return tool_error(call, error.to_string());
                }
                let task_id = match required_uuid(call, "task_id") {
                    Ok(value) => value,
                    Err(error) => return tool_error(call, error.to_string()),
                };
                let explicit = match call.arguments.get("agent_id") {
                    Some(_) => match required_uuid(call, "agent_id") {
                        Ok(value) => Some(value),
                        Err(error) => return tool_error(call, error.to_string()),
                    },
                    None => None,
                };
                if let Some(target) = explicit {
                    if !is_root {
                        return tool_error(
                            call,
                            "only the root agent may reassign a task to another agent".into(),
                        );
                    }
                    match group.reassign(task_id, target) {
                        Ok(task) => tool_ok(
                            call,
                            format!("reassigned group task\n{}", task_line(&task, &short_name)),
                        ),
                        Err(error) => tool_error(call, error.to_string()),
                    }
                } else {
                    if let Err(error) = group.join(self.session_id) {
                        return tool_error(call, error.to_string());
                    }
                    match group.claim(task_id, self.session_id) {
                        Ok(task) => tool_ok(
                            call,
                            format!("claimed group task\n{}", task_line(&task, &short_name)),
                        ),
                        Err(error) => tool_error(call, error.to_string()),
                    }
                }
            }
            "start" | "complete" | "block" | "release" | "cancel" => {
                let allowed: &[&str] = match op {
                    "start" | "release" | "cancel" => &["op", "task_id", "reason"],
                    "complete" => &["op", "task_id", "summary", "touched_files", "findings"],
                    "block" => &["op", "task_id", "reason"],
                    _ => unreachable!(),
                };
                if let Err(error) = reject_extra_fields(call, allowed) {
                    return tool_error(call, error.to_string());
                }
                let task_id = match required_uuid(call, "task_id") {
                    Ok(value) => value,
                    Err(error) => return tool_error(call, error.to_string()),
                };
                let outcome = match op {
                    "start" => group.start(task_id, self.session_id),
                    "complete" => {
                        let summary = match required_str(call, "summary") {
                            Ok(value) => value,
                            Err(error) => return tool_error(call, error.to_string()),
                        };
                        let touched_files = match string_array(call, "touched_files") {
                            Ok(value) => value,
                            Err(error) => return tool_error(call, error.to_string()),
                        };
                        let findings = match string_array(call, "findings") {
                            Ok(value) => value,
                            Err(error) => return tool_error(call, error.to_string()),
                        };
                        group.complete(task_id, self.session_id, summary, touched_files, findings)
                    }
                    "block" => {
                        let reason = match required_str(call, "reason") {
                            Ok(value) => value,
                            Err(error) => return tool_error(call, error.to_string()),
                        };
                        group.block(task_id, self.session_id, reason)
                    }
                    "release" => group.release(task_id, self.session_id),
                    "cancel" => {
                        if !is_root {
                            return tool_error(
                                call,
                                "only the root agent may cancel a group task".into(),
                            );
                        }
                        let reason = call
                            .arguments
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        group.cancel(task_id, reason)
                    }
                    _ => unreachable!(),
                };
                match outcome {
                    Ok(task) => tool_ok(
                        call,
                        format!("{op} group task\n{}", task_line(&task, &short_name)),
                    ),
                    Err(error) => tool_error(call, error.to_string()),
                }
            }
            other => tool_error(
                call,
                format!(
                    "unknown group_task op {other:?}; expected create, list, claim, start, complete, block, release, or cancel"
                ),
            ),
        }
    }

    fn run_group_message_tool(&self, call: &ToolCall, group: &GroupCoordinator) -> ToolResult {
        let Some(op) = call.arguments.get("op").and_then(Value::as_str) else {
            return tool_error(call, "group_message requires `op`".into());
        };
        match op {
            "send" => {
                if let Err(error) = reject_extra_fields(call, &["op", "to", "agent_id", "text"]) {
                    return tool_error(call, error.to_string());
                }
                let text = match required_str(call, "text") {
                    Ok(value) => value,
                    Err(error) => return tool_error(call, error.to_string()),
                };
                let to = match call.arguments.get("to").and_then(Value::as_str) {
                    Some("root") => {
                        if call.arguments.get("agent_id").is_some() {
                            return tool_error(
                                call,
                                "agent_id is only valid when `to` is \"agent\"".into(),
                            );
                        }
                        GroupMessageTarget::Root
                    }
                    Some("group") => {
                        if call.arguments.get("agent_id").is_some() {
                            return tool_error(
                                call,
                                "agent_id is only valid when `to` is \"agent\"".into(),
                            );
                        }
                        GroupMessageTarget::Group
                    }
                    Some("agent") => match required_uuid(call, "agent_id") {
                        Ok(agent) => GroupMessageTarget::Agent(agent),
                        Err(error) => return tool_error(call, error.to_string()),
                    },
                    Some(other) => {
                        return tool_error(
                            call,
                            format!(
                                "unknown message target {other:?}; expected agent, root, or group"
                            ),
                        );
                    }
                    None => {
                        return tool_error(call, "group_message send requires `to`".into());
                    }
                };
                if let Err(error) = group.join(self.session_id) {
                    return tool_error(call, error.to_string());
                }
                match group.send_message(self.session_id, to, text) {
                    Ok(message) => tool_ok(
                        call,
                        format!(
                            "queued group message {} for {}; it is delivered at the recipient's next safe boundary",
                            short_id(message.message_id),
                            target_label(&message.to)
                        ),
                    ),
                    Err(error) => tool_error(call, error.to_string()),
                }
            }
            "list" => {
                if let Err(error) = reject_extra_fields(call, &["op", "limit"]) {
                    return tool_error(call, error.to_string());
                }
                let limit = call
                    .arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|limit| (limit as usize).clamp(1, MESSAGE_LIST_LIMIT))
                    .unwrap_or(10);
                let is_root = self.agent_depth == 0;
                let entries = group.inbox(self.session_id, is_root, limit);
                if entries.is_empty() {
                    return tool_ok(call, "no group messages".into());
                }
                let mut lines = Vec::with_capacity(entries.len() + 1);
                lines.push(format!("{} recent group message(s):", entries.len()));
                for entry in entries {
                    lines.push(format!(
                        "- {} -> {} by {}: {}{}",
                        if entry.delivered { "seen" } else { "unread" },
                        target_label(&entry.message.to),
                        short_id(entry.message.from_agent),
                        compact_text(&entry.message.text, 400),
                        entry.message.created_at.format(" · %Y-%m-%dT%H:%M:%SZ")
                    ));
                }
                tool_ok(call, lines.join("\n"))
            }
            other => tool_error(
                call,
                format!("unknown group_message op {other:?}; expected send or list"),
            ),
        }
    }
}

fn short_name(agent: Uuid) -> String {
    if agent == Uuid::nil() {
        "root".into()
    } else {
        short_id(agent)
    }
}

fn short_id(id: Uuid) -> String {
    id.to_string()[..8].to_owned()
}

fn target_label(target: &GroupMessageTarget) -> String {
    match target {
        GroupMessageTarget::Agent(agent) => format!("agent {}", short_id(*agent)),
        GroupMessageTarget::Root => "root".into(),
        GroupMessageTarget::Group => "group".into(),
    }
}

fn compact_text(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let mut value = normalized
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    value.push('…');
    value
}

fn required_str(call: &ToolCall, field: &str) -> Result<String> {
    call.arguments
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("`{field}` must be a non-empty string"))
}

fn required_uuid(call: &ToolCall, field: &str) -> Result<Uuid> {
    let raw = required_str(call, field)?;
    Uuid::parse_str(&raw).map_err(|error| anyhow!("invalid `{field}`: {error}"))
}

fn uuid_array(call: &ToolCall, field: &str) -> Result<Vec<Uuid>> {
    let Some(value) = call.arguments.get(field) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("`{field}` must be an array of task ids"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| anyhow!("`{field}` must contain task id strings"))
                .and_then(|raw| {
                    Uuid::parse_str(raw).map_err(|error| anyhow!("invalid `{field}` id: {error}"))
                })
        })
        .collect()
}

fn string_array(call: &ToolCall, field: &str) -> Result<Vec<String>> {
    let Some(value) = call.arguments.get(field) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("`{field}` must be an array of strings"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("`{field}` must contain strings"))
        })
        .collect()
}

/// Strict operand validation: an op accepts exactly its own fields, so an
/// ambiguous combination is a local error rather than a silent guess.
fn reject_extra_fields(call: &ToolCall, allowed: &[&str]) -> Result<()> {
    let Some(arguments) = call.arguments.as_object() else {
        bail!("arguments must be a JSON object");
    };
    for key in arguments.keys() {
        if !allowed.contains(&key.as_str()) {
            bail!(
                "field `{key}` is not valid for the selected operation (allowed: {})",
                allowed.join(", ")
            );
        }
    }
    Ok(())
}

fn name_for_task(status: &GroupStatus, task_id: Uuid) -> String {
    status
        .ready
        .iter()
        .chain(status.active.iter())
        .chain(status.blocked.iter())
        .find(|task| task.task_id == task_id)
        .map(|task| task.title.clone())
        .unwrap_or_else(|| short_id(task_id))
}

fn task_line(task: &GroupTask, name_for: &impl Fn(Uuid) -> String) -> String {
    let dependency_text = if task.dependencies.is_empty() {
        String::new()
    } else {
        format!(
            " depends: {}",
            task.dependencies
                .iter()
                .map(|dependency| short_id(*dependency))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let owner = match task.assignee {
        Some(assignee) => format!(" → {}", name_for(assignee)),
        None => String::new(),
    };
    let summary = task
        .summary
        .as_deref()
        .map(|summary| format!(" · {summary}"))
        .unwrap_or_default();
    format!(
        "{} [{}] {}{owner}{dependency_text}{summary}",
        task.status.label(),
        task.task_id,
        task.title
    )
}

/// Human-facing variant for `/group`: short ids keep the view compact.
fn overview_task_line(task: &GroupTask, name_for: &impl Fn(Uuid) -> String) -> String {
    task_line(task, name_for).replacen(&task.task_id.to_string(), &short_id(task.task_id), 1)
}

fn status_text(status: &GroupStatus) -> String {
    let mut lines = vec![format!(
        "group `{}` ({}) · members {} · tasks {} · ready {} · active {} · blocked {} · done {} · cancelled {}",
        status.identity.name,
        short_id(status.identity.group_id),
        status.members.len(),
        status.counts.total,
        status.counts.ready,
        status.counts.active(),
        status.counts.blocked,
        status.counts.completed,
        status.counts.cancelled
    )];
    if status.counts.ready > 0 {
        lines.push(format!(
            "ready: {}",
            status
                .ready
                .iter()
                .take(8)
                .map(|task| format!("{} [{}]", task.title, task.task_id))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    let active = status
        .active
        .iter()
        .take(8)
        .map(|task| {
            format!(
                "{} [{}] → {}{}",
                task.title,
                task.task_id,
                task.assignee.map(short_id).unwrap_or_else(|| "?".into()),
                if task.status == GroupTaskStatus::InProgress {
                    " (in progress)"
                } else {
                    " (claimed)"
                }
            )
        })
        .collect::<Vec<_>>();
    if !active.is_empty() {
        lines.push(format!("active: {}", active.join(" · ")));
    }
    if status.counts.blocked > 0 {
        lines.push(format!(
            "blocked: {}",
            status
                .blocked
                .iter()
                .take(8)
                .map(|task| format!(
                    "{} [{}]{}",
                    task.title,
                    task.task_id,
                    task.reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    for (task_id, dependencies) in status.dependency_failures.iter().take(4) {
        lines.push(format!(
            "dependency failure: {} depends on blocked/cancelled {}",
            task_id,
            dependencies
                .iter()
                .map(Uuid::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for conflict in status.conflicts.iter().take(4) {
        lines.push(format!(
            "possible workspace conflict: `{}` in {} and {}",
            conflict.path, conflict.task_a, conflict.task_b
        ));
    }
    lines.join("\n")
}

fn list_tasks_text(state: &GroupState, filter: &str, limit: usize) -> Result<String> {
    let all = state.ordered_tasks();
    let selected: Vec<&GroupTask> = match filter {
        "open" => all
            .iter()
            .filter(|task| !task.status.is_terminal())
            .collect(),
        "all" => all.iter().collect(),
        "ready" => all
            .iter()
            .filter(|task| task.status == GroupTaskStatus::Pending && task.is_ready(state.tasks()))
            .collect(),
        "active" => all.iter().filter(|task| task.status.is_active()).collect(),
        "blocked" => all
            .iter()
            .filter(|task| task.status == GroupTaskStatus::Blocked)
            .collect(),
        "pending" => all
            .iter()
            .filter(|task| task.status == GroupTaskStatus::Pending)
            .collect(),
        "completed" => all
            .iter()
            .filter(|task| task.status == GroupTaskStatus::Completed)
            .collect(),
        "cancelled" => all
            .iter()
            .filter(|task| task.status == GroupTaskStatus::Cancelled)
            .collect(),
        other => bail!(
            "unknown list status {other:?}; expected open, ready, active, blocked, pending, completed, cancelled, or all"
        ),
    };
    if selected.is_empty() {
        return Ok(match filter {
            "open" => "no open group tasks".to_owned(),
            other => format!("no {other} group tasks"),
        });
    }
    let mut lines = vec![format!(
        "group `{}` · {} {} task(s) · ready {} · active {} · blocked {}",
        state
            .identity()
            .map(|identity| identity.name.clone())
            .unwrap_or_default(),
        selected.len(),
        filter,
        state.counts().ready,
        state.counts().active(),
        state.counts().blocked
    )];
    let shown = selected.len().min(limit);
    for task in selected.iter().take(shown) {
        lines.push(format!("- {}", task_line(task, &short_name)));
    }
    if selected.len() > shown {
        lines.push(format!("… and {} more", selected.len() - shown));
    }
    Ok(lines.join("\n"))
}
