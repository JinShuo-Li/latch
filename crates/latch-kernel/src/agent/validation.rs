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
        let requirement = call
            .arguments
            .get("requirement")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let command = call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let (Some(requirement), Some(command)) = (requirement, command) else {
            return Ok(tool_error(
                call,
                "requirement and command are required".into(),
            ));
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
        let timeout = call
            .arguments
            .get("timeout_seconds")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(600);
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
        let detail = format!(
            "{} ({:.1?}): {}",
            output.status_line,
            output.elapsed,
            output.first_line()
        );
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
        let evidence = self
            .evidence
            .build(&requirement, validation_event.id, status, detail);
        // Persist before mutating the live ledger; see kernel_tools.
        self.emit(
            EventPayload::EvidenceCreated {
                evidence: evidence.clone(),
            },
            sink,
        )?;
        self.evidence.push(evidence);
        // 3. Kernel bookkeeping: the validated requirement becomes required
        //    and completion is derived.
        self.state.require_validation(&requirement);
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
            "{verdict} requirement `{requirement}`: {command}\n{}\nCompletion: {completion:?}\n",
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
        if passed {
            self.failures.resolve(&requirement);
        } else {
            let decision = self.failures.record(&requirement, &result.output);
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
}
