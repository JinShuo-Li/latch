//! Permission resolution: policy decisions become grants, human approvals, or a
//! stateless structured-output AI review. Hard deny is never overridable.

use super::dispatch::tool_error;
use super::*;

/// Strict structured-output reviewer used by the `Approve for me` resolver.
const REVIEWER_PROMPT: &str = "You are a security reviewer for a sandboxed coding agent. Classify the risk of exactly one proposed shell command. Reply with strict JSON only, no markdown, no commentary: {\"risk\":\"low|medium|high|critical\",\"reason\":\"one sentence\"}. low means routine, local, reversible inspection or build work. medium, high, or critical mean destructive, privileged, secret-touching, remote side effects, or capability escalation.";

impl Agent {
    /// Expires approval requests that were pending when a session ended.
    /// Resuming cannot continue a tool call that no longer exists, so each
    /// unresolved request is durably marked, not silently forgotten.
    pub fn expire_pending_permissions(store: &EventStore, session_id: Uuid) -> Result<usize> {
        let events = store.events(session_id)?;
        let mut pending = std::collections::BTreeSet::new();
        for event in &events {
            match &event.payload {
                EventPayload::PermissionRequested { request_id, .. } => {
                    pending.insert(*request_id);
                }
                EventPayload::PermissionResolved { request_id, .. } => {
                    pending.remove(request_id);
                }
                _ => {}
            }
        }
        let count = pending.len();
        for request_id in pending {
            store.append(
                session_id,
                EventPayload::PermissionResolved {
                    request_id,
                    approved: false,
                    source: "resume_expired".into(),
                    risk: None,
                },
            )?;
        }
        Ok(count)
    }

    /// Convenience wrapper over [`Self::expire_pending_permissions`] for an
    /// agent that has not finished restoring yet.
    pub fn restore_permissions(&mut self) -> Result<usize> {
        Self::expire_pending_permissions(&self.store, self.session_id)
    }

    /// Resolves an `Ask` according to the configured permission resolver.
    ///
    /// Every path records the normal durable `PermissionRequested` /
    /// `PermissionResolved` provenance and returns a call-scoped capability
    /// grant; none of them can override a hard `Deny`.
    pub(super) async fn resolve_ask(
        &mut self,
        call: &ToolCall,
        classification: &Classification,
        reason: &str,
        sink: &AgentEventSink,
        cancel: &CancellationToken,
    ) -> std::result::Result<CapabilityGrant, String> {
        let request_id = Uuid::new_v4();
        if self
            .emit(
                EventPayload::PermissionRequested {
                    request_id,
                    tool: call.name.clone(),
                    arguments: call.arguments.clone(),
                    reason: reason.to_owned(),
                    capabilities: classification.capabilities.names(),
                },
                sink,
            )
            .is_err()
        {
            return Err("permission request could not be persisted".into());
        }
        match self.tools.permissions() {
            PermissionMode::AutoApprove => {
                // Auto approval records the normal Ask -> Resolved provenance
                // and still grants only the capabilities this call asked for.
                self.record_resolution(request_id, true, "auto", None, sink);
                Ok(grant_for(classification))
            }
            PermissionMode::Human => {
                self.human_resolution(request_id, classification, reason, sink, cancel)
                    .await
            }
            PermissionMode::AiReview => {
                let Some(command) = call
                    .arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                else {
                    // No command to review: use conservative human resolution
                    // rather than fabricating a bash risk judgment.
                    return self
                        .human_resolution(request_id, classification, reason, sink, cancel)
                        .await;
                };
                let (risk, explanation) = self.review_command(command, classification).await;
                if risk == "low" {
                    self.record_resolution(request_id, true, "ai", Some(risk), sink);
                    Ok(grant_for(classification))
                } else {
                    self.record_resolution(request_id, false, "ai", Some(risk.clone()), sink);
                    Err(format!(
                        "Permission denied: {risk} risk — {explanation}. Choose a narrower, safer command and continue."
                    ))
                }
            }
        }
    }

    /// Real human approval through the TUI broker. Non-interactive sessions
    /// resolve as an explicit denial instead of hanging.
    pub(super) async fn human_resolution(
        &mut self,
        request_id: Uuid,
        classification: &Classification,
        reason: &str,
        sink: &AgentEventSink,
        cancel: &CancellationToken,
    ) -> std::result::Result<CapabilityGrant, String> {
        if !self.interactive_permissions {
            self.record_resolution(request_id, false, "non_interactive", None, sink);
            return Err(format!("permission denied: {reason}"));
        }
        let approved = tokio::select! {
            decision = self.permissions.request(request_id) => decision.unwrap_or(false),
            () = cancel.cancelled() => {
                self.permissions.cancel(request_id).await;
                false
            }
        };
        let source = if cancel.is_cancelled() {
            "cancelled"
        } else {
            "user"
        };
        self.record_resolution(request_id, approved, source, None, sink);
        if approved {
            Ok(grant_for(classification))
        } else {
            Err(format!("permission denied: {reason}"))
        }
    }

    fn record_resolution(
        &mut self,
        request_id: Uuid,
        approved: bool,
        source: &str,
        risk: Option<String>,
        sink: &AgentEventSink,
    ) {
        let _ = self.emit(
            EventPayload::PermissionResolved {
                request_id,
                approved,
                source: source.into(),
                risk,
            },
            sink,
        );
    }

    /// A separate stateless model call: no coding history, no tools, structured
    /// output only. Malformed or unavailable answers reject conservatively.
    async fn review_command(
        &mut self,
        command: &str,
        classification: &Classification,
    ) -> (String, String) {
        let context = json!({
            "task": self.state.state().goal,
            "workspace": self.workspace.display().to_string(),
            "command": command,
            "capabilities": classification.capabilities.names(),
            "requested_because": classification.reason,
        });
        let request = ModelRequest {
            system: REVIEWER_PROMPT.to_owned(),
            messages: vec![ModelMessage::text("user", context.to_string())],
            tools: vec![],
        };
        let sink: StreamSink = Arc::new(|_| {});
        match self
            .provider
            .stream(request, CancellationToken::new(), sink)
            .await
        {
            Ok(response) => parse_review(&response.text),
            Err(error) => ("critical".into(), format!("reviewer unavailable ({error})")),
        }
    }

    pub(super) fn denied_result(
        &mut self,
        call: &ToolCall,
        decision: &str,
        reason: String,
        sink: &AgentEventSink,
    ) -> ToolResult {
        let denied = tool_error(call, reason.clone());
        let _ = self.emit(
            EventPayload::PermissionDecision {
                tool: call.name.clone(),
                decision: decision.into(),
                reason,
            },
            sink,
        );
        let _ = self.emit(
            EventPayload::ToolFailed {
                result: denied.clone(),
            },
            sink,
        );
        denied
    }
}

pub(super) fn parse_review(text: &str) -> (String, String) {
    let Some(start) = text.find('{') else {
        return ("critical".into(), "unparseable reviewer response".into());
    };
    let Some(end) = text.rfind('}') else {
        return ("critical".into(), "unparseable reviewer response".into());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text[start..=end]) else {
        return ("critical".into(), "unparseable reviewer response".into());
    };
    let risk = value
        .get("risk")
        .and_then(|risk| risk.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let reason = value
        .get("reason")
        .and_then(|reason| reason.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    if matches!(risk.as_str(), "low" | "medium" | "high" | "critical") && !reason.is_empty() {
        (risk, reason)
    } else {
        ("critical".into(), "unparseable reviewer response".into())
    }
}

pub(super) fn grant_for(classification: &Classification) -> CapabilityGrant {
    CapabilityGrant {
        capabilities: classification.capabilities.clone(),
        external_roots: classification.external_roots.clone(),
    }
}
