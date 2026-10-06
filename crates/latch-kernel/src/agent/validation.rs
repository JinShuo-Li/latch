//! Kernel-owned validation: one command, kernel provenance, evidence recording, and
//! derived completion. The model can never assert a pass itself.

use super::dispatch::tool_error;
use super::*;

impl Agent {
    /// Kernel-owned validation: the model names a requirement and a command,
    /// the kernel executes it, records the ValidationResult, links the evidence
    /// to real provenance, registers the requirement, and derives completion.
    /// The model never supplies or sees an internal event or call id.
    pub(super) async fn execute_validate(
        &mut self,
        call: &ToolCall,
        cancel: CancellationToken,
        sink: &AgentEventSink,
    ) -> Result<ToolResult> {
        self.emit(
            EventPayload::ToolStarted {
                call_id: call.id.clone(),
                tool: call.name.clone(),
            },
            sink,
        )?;
        let requirements = match validation_requirements(&call.arguments) {
            Ok(requirements) => requirements,
            Err(message) => return self.reject_validation(call, message, sink),
        };
        let command = call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let Some(command) = command.filter(|command| !command.trim().is_empty()) else {
            return self.reject_validation(call, "a nonempty command is required".into(), sink);
        };
        // Validation runs commands, so it obeys the same policy as shell. An
        // `Ask` here is a real approval request, not a denial.
        let classification = self.tools.classify_call("validate", &call.arguments);
        match classification.decision.clone() {
            SafetyDecision::Allow => {}
            SafetyDecision::Deny(reason) => {
                return self.denied_result(call, "policy_denied", reason, sink);
            }
            SafetyDecision::Ask(reason) => {
                match self
                    .resolve_ask(call, &classification, &reason, sink, &cancel)
                    .await
                {
                    Ok(grant) => self.tools.grant_call(&call.id, grant),
                    Err(message) => {
                        return self.denied_result(call, "policy_denied", message, sink);
                    }
                }
            }
        }
        let timeout =
            validation_timeout_seconds(&call.arguments, self.tools.shell_timeout_seconds());
        self.refresh_workspace_generation()?;
        let starting_generation = self.evidence.workspace_generation();
        let starting_watermark = self.workspace_generation_watermark;
        let writer_active_at_start = !self.active_managed_processes.is_empty();
        let output = match self
            .tools
            .run_validated_command(call, timeout, cancel)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                let failed = tool_error(call, format!("{error:#}"));
                self.emit(
                    EventPayload::ToolFailed {
                        result: failed.clone(),
                    },
                    sink,
                )?;
                return Ok(failed);
            }
        };
        let passed = output.success;
        self.refresh_workspace_generation()?;
        let conflicting_mutation = writer_active_at_start
            || self
                .store
                .workspace_mutation_events_after(&self.workspace, starting_watermark)?
                .iter()
                .any(|(_, event)| {
                    event.session_id != self.session_id
                        && (super::supervision::advances_workspace_generation(&event.payload)
                            || matches!(
                                &event.payload,
                                EventPayload::ProcessStarted {
                                    may_write_workspace: None | Some(true),
                                    ..
                                }
                            ))
                });
        let mut detail = format!(
            "{} ({:.1?}): {}",
            output.status_line,
            output.elapsed,
            output.first_line()
        );
        if conflicting_mutation {
            detail.push_str("; workspace writer overlapped validation");
        }
        // 1. Durable ValidationResult…
        let validation_event = self.emit(
            EventPayload::ValidationResult {
                command: command.clone(),
                passed,
                detail: detail.clone(),
            },
            sink,
        )?;
        // 2. …linked automatically to evidence for the named requirement.
        let status = if passed {
            EvidenceStatus::Passed
        } else {
            EvidenceStatus::Failed
        };
        for requirement in &requirements {
            let mut evidence = self.evidence.build_validation(
                requirement,
                validation_event.id,
                status.clone(),
                detail.clone(),
            );
            if passed && conflicting_mutation {
                // The entire set certifies no stable workspace if a writer
                // overlapped execution. Preserve every observation for audit.
                evidence.workspace_generation = Some(starting_generation);
            }
            // Persist before mutating the live ledger; see kernel_tools.
            self.emit(
                EventPayload::EvidenceCreated {
                    evidence: evidence.clone(),
                },
                sink,
            )?;
            self.evidence.push(evidence);
            self.state.require_validation(requirement);
        }
        // 3. Kernel bookkeeping: the validated requirement becomes required
        //    and completion is derived.
        self.sync_completion(sink)?;
        self.emit(
            EventPayload::TaskStateUpdated {
                state: self.state.state().clone(),
            },
            sink,
        )?;
        let completion = self.state.state().completion.clone();
        let verdict = if passed { "PASSED" } else { "FAILED" };
        let artifact_note = output
            .artifact_id
            .as_ref()
            .map(|artifact| format!("\n[full output artifact: {artifact}]"))
            .unwrap_or_default();
        // Elapsed time goes last so the head of the body — the failure
        // signature source shared with replayed history — stays deterministic
        // across runs of the same command.
        let mut body = format!(
            "{verdict} requirements `{}`: {command}\n{}\nCompletion: {completion:?}\n",
            requirements.join("`, `"),
            output.status_line,
        );
        let preview: String = output.text.chars().take(2000).collect();
        body.push_str(preview.trim_end());
        body.push_str(&format!("\n(elapsed {:.1?})", output.elapsed));
        body.push_str(&artifact_note);
        let result = ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: body,
            is_error: !passed,
            artifact_id: output.artifact_id,
            media: Vec::new(),
        };
        // 4. The validation's own failure lineage is supervised against the
        //    result body, the exact text a resumed session replays — so live
        //    and restored supervision count identically.
        for requirement in &requirements {
            if passed {
                self.failures.resolve(requirement);
                continue;
            }
            let decision = self.failures.record(requirement, &result.output);
            self.emit(
                EventPayload::FailureAttempt {
                    signature: decision.signature,
                    count: decision.count,
                },
                sink,
            )?;
            if decision.reground {
                self.emit(
                    EventPayload::RegroundRequested {
                        signature: requirement.clone(),
                    },
                    sink,
                )?;
            }
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

    fn reject_validation(
        &mut self,
        call: &ToolCall,
        message: String,
        sink: &AgentEventSink,
    ) -> Result<ToolResult> {
        let result = tool_error(call, message);
        self.emit(
            EventPayload::ToolFailed {
                result: result.clone(),
            },
            sink,
        )?;
        Ok(result)
    }
}

fn validation_requirements(
    arguments: &serde_json::Value,
) -> std::result::Result<Vec<String>, String> {
    let primary = arguments
        .get("requirement")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "a nonempty requirement is required".to_owned())?;
    let mut requirements = vec![primary.trim().to_owned()];
    if let Some(additional) = arguments.get("requirements") {
        let names = additional
            .as_array()
            .ok_or_else(|| "requirements must be an array of nonempty names".to_owned())?;
        for name in names {
            let name = name
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| "requirements must contain only nonempty names".to_owned())?;
            if !requirements
                .iter()
                .any(|existing| crate::state::same_text(existing, name))
            {
                requirements.push(name.trim().to_owned());
            }
        }
    }
    Ok(requirements)
}

fn validation_timeout_seconds(arguments: &serde_json::Value, default: u64) -> u64 {
    arguments
        .get("timeout_seconds")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{validation_requirements, validation_timeout_seconds};
    use serde_json::json;

    #[test]
    fn validation_set_rejects_invalid_names_and_deduplicates_exact_requirements() {
        assert_eq!(
            validation_requirements(&json!({"requirement":" A ","requirements":["a", "B"]}))
                .unwrap(),
            ["A", "B"]
        );
        for arguments in [
            json!({"requirement":""}),
            json!({"requirement":"A","requirements":"B"}),
            json!({"requirement":"A","requirements":[""]}),
            json!({"requirement":"A","requirements":[false]}),
        ] {
            assert!(validation_requirements(&arguments).is_err(), "{arguments}");
        }
    }

    #[test]
    fn validation_timeout_uses_shell_default_for_missing_or_invalid_values() {
        assert_eq!(validation_timeout_seconds(&json!({}), 120), 120);
        assert_eq!(
            validation_timeout_seconds(&json!({"timeout_seconds": -1}), 120),
            120
        );
        assert_eq!(
            validation_timeout_seconds(&json!({"timeout_seconds": "30"}), 120),
            120
        );
    }

    #[test]
    fn validation_timeout_preserves_explicit_unsigned_values_including_zero() {
        assert_eq!(
            validation_timeout_seconds(&json!({"timeout_seconds": 30}), 120),
            30
        );
        assert_eq!(
            validation_timeout_seconds(&json!({"timeout_seconds": 0}), 120),
            0
        );
    }
}
