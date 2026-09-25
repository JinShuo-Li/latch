//! Kernel-owned tools: task state, evidence, completion, and the readable summaries
//! the model sees instead of raw JSON.

use super::dispatch::{tool_error, tool_ok};
use super::*;

impl Agent {
    pub(super) fn execute_kernel_tool(
        &mut self,
        call: &ToolCall,
        sink: &AgentEventSink,
    ) -> Result<ToolResult> {
        let started = self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        )?;
        let mut state_updated = call.name == "task_update";
        let result = match call.name.as_str() {
            "task_update" => match serde_json::from_value::<StateUpdate>(call.arguments.clone()) {
                Ok(update) => {
                    if let Err(error) = self.record_state_memories(&update, started.id) {
                        tool_error(call, error.to_string())
                    } else {
                        self.state.update(update);
                        self.refresh_workspace_generation()?;
                        self.state.recompute_completion(&self.evidence);
                        tool_ok(
                            call,
                            format!(
                                "task state updated\n{}",
                                summarize_state(self.state.state())
                            ),
                        )
                    }
                }
                Err(error) => tool_error(call, format!("invalid task update: {error}")),
            },
            "record_evidence" => {
                let claim = call
                    .arguments
                    .get("claim")
                    .and_then(serde_json::Value::as_str);
                let detail = call
                    .arguments
                    .get("detail")
                    .and_then(serde_json::Value::as_str);
                let status = call
                    .arguments
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_observation_status);
                match (claim, detail, status) {
                        (Some(claim), Some(detail), Some(status)) => {
                            // Only kernel-observed validation can produce Passed or
                            // Failed evidence; the model may declare Pending or
                            // Unavailable observations about non-command claims.
                            let evidence =
                                self.evidence
                                    .build(claim, started.id, status, detail);
                            // Persist before mutating the live ledger so a
                            // failed write can never leave phantom evidence
                            // that resume cannot reconstruct.
                            self.emit(
                                EventPayload::EvidenceCreated {
                                    evidence: evidence.clone(),
                                },
                                sink,
                            )?;
                            self.evidence.push(evidence);
                            self.sync_completion(sink)?;
                            tool_ok(
                                call,
                                format!(
                                    "evidence recorded for `{claim}`; completion: {:?}",
                                    self.state.state().completion
                                ),
                            )
                        }
                        (Some(_), _, None) => tool_error(
                            call,
                            "invalid status; record_evidence accepts pending or unavailable. Passed/failed evidence is kernel-owned: run the validate tool".into(),
                        ),
                        _ => tool_error(call, "claim, detail, and status are required".into()),
                    }
            }
            "complete" => {
                let implemented = call
                    .arguments
                    .get("implementation_done")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                // Kernel truth gate: the root may not terminally complete while
                // the active group still holds required unfinished work. The
                // gate never applies to optional or cancelled tasks, and it
                // never applies to a child's own turn completion.
                if implemented
                    && self.agent_depth == 0
                    && let Some(blocker) = self.group_completion_blocker()
                {
                    tool_error(
                        call,
                        format!("terminal completion denied by the agent-group gate: {blocker}"),
                    )
                } else {
                    self.state.set_implementation_done(implemented);
                    self.sync_completion(sink)?;
                    // The model claimed completion and the kernel derived a
                    // terminal, verified-or-implemented state. The loop may exit
                    // in this same turn; `Blocked` still earns a reporting turn.
                    if matches!(
                        self.state.state().completion,
                        CompletionState::Verified | CompletionState::ImplementedNotVerified
                    ) {
                        self.terminal_complete = true;
                    }
                    state_updated = true;
                    tool_ok(
                        call,
                        format!(
                            "implementation claim recorded; kernel-derived completion: {:?}\n{}",
                            self.state.state().completion,
                            summarize_state(self.state.state())
                        ),
                    )
                }
            }
            _ => tool_error(call, "unknown kernel tool".into()),
        };
        if state_updated {
            self.emit(
                EventPayload::TaskStateUpdated {
                    state: self.state.state().clone(),
                },
                sink,
            )?;
        }
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

    /// Emits a `CompletionChanged` event whenever the kernel-derived completion
    /// value actually changes. This is the single announcement point; the model
    /// never writes completion truth.
    pub(super) fn sync_completion(&mut self, sink: &AgentEventSink) -> Result<()> {
        self.refresh_workspace_generation()?;
        self.state.recompute_completion(&self.evidence);
        let derived = self.state.state().completion.clone();
        if self.last_completion.as_ref() != Some(&derived) {
            // Only remember the new completion after the durable announcement
            // commits; a failed write must not mask a later retry.
            self.emit(
                EventPayload::CompletionChanged {
                    completion: derived.clone(),
                },
                sink,
            )?;
            self.last_completion = Some(derived);
        }
        Ok(())
    }

    fn record_state_memories(&self, update: &StateUpdate, source: Uuid) -> Result<()> {
        let existing = self.store.memories(self.session_id)?;
        // Model-authored constraints carry TaskConstraint provenance: they are
        // working assumptions from the model, never user instructions.
        let mut records = update
            .add_constraints
            .iter()
            .filter(|text| !self.state.state().constraints.contains(text))
            .map(|text| (MemoryKind::TaskConstraint, text, Validity::Active))
            .chain(
                update
                    .add_decisions
                    .iter()
                    .filter(|text| !self.state.state().decisions.contains(text))
                    .map(|text| (MemoryKind::Decision, text, Validity::Active)),
            )
            .chain(
                update
                    .add_hypotheses
                    .iter()
                    .map(|text| {
                        let validity = if update.reject_hypotheses.contains(text) {
                            Validity::Rejected
                        } else {
                            Validity::Active
                        };
                        (MemoryKind::Hypothesis, text, validity)
                    })
                    .filter(|(_, text, _)| {
                        !self
                            .state
                            .state()
                            .hypotheses
                            .iter()
                            .any(|hypothesis| hypothesis.text.as_str() == text.as_str())
                    }),
            )
            .collect::<Vec<_>>();
        for (kind, text, validity) in records.drain(..) {
            self.store.add_memory(&MemoryRecord {
                id: Uuid::new_v4(),
                session_id: self.session_id,
                kind,
                content: text.clone(),
                originating_event: source,
                created_at: Utc::now(),
                validity,
                confidence: None,
                dependencies: vec![],
                supersedes: None,
            })?;
        }
        let supersede = |kind: MemoryKind, texts: &[String]| -> Result<()> {
            for text in texts {
                if let Some(previous) = existing
                    .iter()
                    .rev()
                    .find(|memory| memory.kind == kind && memory.content == *text)
                {
                    self.store
                        .set_memory_validity(previous.id, Validity::Superseded)?;
                }
            }
            Ok(())
        };
        supersede(MemoryKind::TaskConstraint, &update.supersede_constraints)?;
        supersede(MemoryKind::Decision, &update.supersede_decisions)?;
        for rejected in update
            .reject_hypotheses
            .iter()
            .filter(|text| !update.add_hypotheses.contains(text))
        {
            if let Some(previous) = existing.iter().rev().find(|memory| {
                memory.kind == MemoryKind::Hypothesis
                    && memory.content == **rejected
                    && memory.validity == Validity::Active
            }) {
                self.store
                    .set_memory_validity(previous.id, Validity::Superseded)?;
                self.store.add_memory(&MemoryRecord {
                    id: Uuid::new_v4(),
                    session_id: self.session_id,
                    kind: MemoryKind::Hypothesis,
                    content: rejected.clone(),
                    originating_event: source,
                    created_at: Utc::now(),
                    validity: Validity::Rejected,
                    confidence: None,
                    dependencies: vec![],
                    supersedes: Some(previous.id),
                })?;
            }
        }
        Ok(())
    }
}

/// Compact human-readable task state for tool results, replacing raw JSON so
/// the model sees a readable summary without internal identifiers.
pub(super) fn summarize_state(state: &latch_protocol::TaskState) -> String {
    let mut lines = vec![format!("goal: {}", state.goal)];
    if !state.constraints.is_empty() {
        lines.push(format!("constraints: {}", state.constraints.join("; ")));
    }
    if !state.decisions.is_empty() {
        lines.push(format!("decisions: {}", state.decisions.join("; ")));
    }
    let active: Vec<&str> = state
        .hypotheses
        .iter()
        .filter(|h| h.validity == Validity::Active)
        .map(|h| h.text.as_str())
        .collect();
    if !active.is_empty() {
        lines.push(format!("hypotheses: {}", active.join("; ")));
    }
    if !state.rejected_hypotheses.is_empty() {
        lines.push(format!(
            "rejected hypotheses: {}",
            state.rejected_hypotheses.join("; ")
        ));
    }
    if !state.required_validations.is_empty() {
        lines.push(format!(
            "required validations: {}",
            state.required_validations.join("; ")
        ));
    }
    format!("completion: {:?}\n{}", state.completion, lines.join("\n"))
}

pub(super) fn parse_observation_status(value: &str) -> Option<EvidenceStatus> {
    match value {
        "pending" => Some(EvidenceStatus::Pending),
        "unavailable" => Some(EvidenceStatus::Unavailable),
        _ => None,
    }
}
