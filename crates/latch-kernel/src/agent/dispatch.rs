//! Tool dispatch: lifecycle-accurate execution of one assistant tool batch,
//! including extension guards, steering supersession, and progress suppression.

use super::*;

impl Agent {
    pub(super) async fn execute_batch(
        &mut self,
        calls: Vec<ToolCall>,
        cancel: CancellationToken,
        sink: &AgentEventSink,
    ) -> Result<Vec<ToolResult>> {
        let mut permitted = Vec::new();
        let mut results = Vec::new();
        for call in calls {
            match self
                .extensions
                .guard(
                    "tool.execute",
                    json!({"name":call.name,"arguments":call.arguments}),
                    &cancel,
                )
                .await
            {
                Ok(ExtensionGuardDecision::Allow) => permitted.push(call),
                Ok(ExtensionGuardDecision::Deny(reason)) => {
                    let denied = tool_error(&call, reason.clone());
                    self.emit(
                        EventPayload::PermissionDecision {
                            tool: call.name.clone(),
                            decision: "extension_guard_denied".into(),
                            reason,
                        },
                        sink,
                    )?;
                    self.emit(
                        EventPayload::ToolFailed {
                            result: denied.clone(),
                        },
                        sink,
                    )?;
                    results.push(denied);
                }
                Ok(ExtensionGuardDecision::Ask(reason)) => {
                    let classification = self.tools.classify_call(&call.name, &call.arguments);
                    match self
                        .resolve_ask(&call, &classification, &reason, sink, &cancel)
                        .await
                    {
                        Ok(grant) => {
                            self.tools.grant_call(&call.id, grant);
                            permitted.push(call);
                        }
                        Err(message) => {
                            let denied =
                                self.denied_result(&call, "extension_guard_denied", message, sink)?;
                            results.push(denied);
                        }
                    }
                }
                Err(error) => {
                    let failed = tool_error(&call, format!("extension guard failed: {error}"));
                    self.emit(
                        EventPayload::ToolFailed {
                            result: failed.clone(),
                        },
                        sink,
                    )?;
                    results.push(failed);
                }
            }
        }
        // Policy `Ask` is a real approval request, not a denial. Only an
        // approval keyed to this kernel call id (which the model never
        // supplies) lets the executor proceed.
        let mut policy_allowed = Vec::with_capacity(permitted.len());
        for call in permitted {
            if self.tools.has_grant(&call.id) {
                policy_allowed.push(call);
                continue;
            }
            let classification = if self.extensions.owner_for_tool(&call.name).is_some() {
                crate::safety::extension_classification(self.tools.safety())
            } else {
                self.tools.classify_call(&call.name, &call.arguments)
            };
            match classification.decision.clone() {
                SafetyDecision::Allow => policy_allowed.push(call),
                SafetyDecision::Deny(reason) => {
                    let denied = self.denied_result(&call, "policy_denied", reason, sink)?;
                    results.push(denied);
                }
                SafetyDecision::Ask(reason) => {
                    match self
                        .resolve_ask(&call, &classification, &reason, sink, &cancel)
                        .await
                    {
                        Ok(grant) => {
                            self.tools.grant_call(&call.id, grant);
                            policy_allowed.push(call);
                        }
                        Err(message) => {
                            let denied =
                                self.denied_result(&call, "policy_denied", message, sink)?;
                            results.push(denied);
                        }
                    }
                }
            }
        }
        permitted = policy_allowed;
        // Deterministic suppression: after the model ignored an explicit
        // re-ground, repeated observations of unchanged reality are rejected
        // with a synthetic terminal result instead of spending a tool cycle.
        // The lifecycle invariant still holds: every ToolRequested(call_id)
        // gets exactly one terminal result.
        self.kernel_resolved_calls.clear();
        if self.progress.regrounded() {
            let mut allowed = Vec::with_capacity(permitted.len());
            for call in permitted {
                match self.progress.suppression_reason(&call) {
                    Some(label) => {
                        let suppressed = tool_error(
                            &call,
                            format!(
                                "Kernel suppressed redundant observation `{label}`: its result is unchanged since the last observation in this progress epoch. Use the existing result, act on it, or state the concrete blocker."
                            ),
                        );
                        self.kernel_resolved_calls.insert(call.id.clone());
                        self.emit(
                            EventPayload::ToolFailed {
                                result: suppressed.clone(),
                            },
                            sink,
                        )?;
                        results.push(suppressed);
                    }
                    None => allowed.push(call),
                }
            }
            permitted = allowed;
        }
        let mut executed = if permitted.iter().all(|c| {
            matches!(
                c.name.as_str(),
                "read_file" | "read_image" | "search" | "read_artifact" | "git_status" | "git_diff"
            )
        }) {
            let tasks = permitted
                .into_iter()
                .map(|call| {
                    let tools = self.tools.clone();
                    let c = cancel.clone();
                    tokio::spawn(async move { tools.execute(&call, c).await })
                })
                .collect::<Vec<_>>();
            let mut batch = Vec::new();
            for task in tasks {
                match task.await {
                    Ok(r) => batch.push(r),
                    Err(e) => batch.push(ToolResult {
                        call_id: "join".into(),
                        name: "scheduler".into(),
                        output: e.to_string(),
                        is_error: true,
                        artifact_id: None,
                        media: Vec::new(),
                    }),
                }
            }
            batch
        } else {
            let mut batch = Vec::new();
            for call in permitted {
                // A steer accepted while an earlier call was in flight makes
                // the remaining not-yet-started mutations stale. The in-flight
                // call finishes normally; every later side-effecting call gets
                // a structurally valid synthetic terminal result and the model
                // re-plans under the newer instruction. Read-only calls are
                // harmless and still run.
                if (!self.steering.is_empty() || self.child_mailbox.has_follow_up())
                    && self.call_is_side_effecting(&call)
                {
                    batch.push(self.superseded_result(&call, sink)?);
                    continue;
                }
                if super::agent_controls::is_agent_control(&call.name) {
                    batch.push(self.execute_agent_control(&call, &cancel, sink).await?);
                } else if matches!(
                    call.name.as_str(),
                    "task_update" | "record_evidence" | "complete"
                ) {
                    batch.push(self.execute_kernel_tool(&call, sink)?);
                } else if call.name == "validate" {
                    batch.push(self.execute_validate(&call, cancel.clone(), sink).await?);
                } else if let Some(owner) = self.extensions.owner_for_tool(&call.name) {
                    batch.push(
                        self.execute_extension_tool(&owner, &call, &cancel, sink)
                            .await?,
                    );
                } else {
                    batch.push(self.tools.execute(&call, cancel.clone()).await);
                }
            }
            batch
        };
        results.append(&mut executed);
        Ok(results)
    }

    pub(super) async fn execute_extension_tool(
        &mut self,
        owner: &str,
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
        let result = match self
            .extensions
            .execute(owner, &call.name, call.arguments.clone(), cancel)
            .await
        {
            Ok(value) => match serde_json::to_string(&value) {
                Ok(output) => tool_ok(call, output),
                Err(error) => tool_error(
                    call,
                    format!("extension result could not be serialized: {error}"),
                ),
            },
            Err(error) => tool_error(call, error.to_string()),
        };
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

    /// True when executing this call could change durable or external state.
    /// Uses the same safety classification as policy and never a second
    /// argument parser. Only the workspace-reading capability set is
    /// considered safe to run after newer steering arrived; kernel bookkeeping
    /// has no OS capability but still mutates durable session state, so it
    /// counts as side-effecting, and extension tools are conservatively
    /// treated as side-effecting because the kernel does not inspect them.
    fn call_is_side_effecting(&self, call: &ToolCall) -> bool {
        if self.extensions.owner_for_tool(&call.name).is_some() {
            return true;
        }
        let classification = self.tools.classify_call(&call.name, &call.arguments);
        !classification.capabilities.is_read_only()
    }

    /// Terminal result for a mutation the kernel refused to start after newer
    /// steering arrived. Structurally identical to any other tool failure, so
    /// every `ToolRequested` still has exactly one terminal result.
    fn superseded_result(&mut self, call: &ToolCall, sink: &AgentEventSink) -> Result<ToolResult> {
        let superseded = tool_error(call, "superseded by newer user steering".into());
        self.kernel_resolved_calls.insert(call.id.clone());
        self.emit(
            EventPayload::ToolFailed {
                result: superseded.clone(),
            },
            sink,
        )?;
        Ok(superseded)
    }
}

pub(super) fn tool_ok(call: &ToolCall, output: String) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error: false,
        artifact_id: None,
        media: Vec::new(),
    }
}

pub(super) fn tool_error(call: &ToolCall, output: String) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error: true,
        artifact_id: None,
        media: Vec::new(),
    }
}
