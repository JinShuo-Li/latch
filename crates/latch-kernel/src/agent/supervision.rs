//! Progress and failure supervision: the same event stream feeds live turns and
//! restored sessions so replay and live supervision stay identical.

use super::*;

/// Kernel-owned instruction issued at most once per run when implementation has
/// changed the workspace but the run is about to end with no passing
/// validation. It never asserts the work is wrong and never asks for more than
/// the kernel can justify: it asks for the evidence the model is responsible
/// for producing, and names the honest exit when verification genuinely is not
/// available.
const VERIFICATION_CORRECTION: &str = "Kernel verification check: this run changed the workspace, but its implementation claim or current verification is missing. If implementation is done, run any missing validation (refresh several named requirements with one covering command and requirements), then call complete with implementation_done=true. If verification is unavailable, record_evidence and report why. If no validation is appropriate, explain why and record the implementation claim.";

/// Repeated certification blockers are the same failed attempt even when the
/// model changes the proving command or its successful output. Use the same
/// durable diagnostic in live supervision and replay.
pub(super) fn validation_failure_output(output: &str) -> &str {
    output
        .lines()
        .find(|line| line.starts_with("Certification blocked:"))
        .unwrap_or(output)
}

/// True when a durable event records a workspace mutation Latch itself made
/// during the run: a guarded edit, a mutating shell command, or an extension
/// write. External edits are somebody else's change and pre-existing dirt was
/// not made by this run, so neither counts as the implementation changing the
/// workspace.
fn is_workspace_mutation(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::FileChanged { owner, .. }
            if !matches!(
                owner,
                latch_protocol::ChangeOwner::External | latch_protocol::ChangeOwner::PreExisting
            )
    )
}

/// The global row id of the newest such event is the workspace generation. A
/// write-capable command advances it before execution, so an undetected or
/// interrupted write cannot leave older validation current.
pub(super) fn advances_workspace_generation(payload: &EventPayload) -> bool {
    match payload {
        EventPayload::WorkspaceMutationPossible { .. }
        | EventPayload::ShellMutationObserved { .. }
        | EventPayload::ChangeReverted { .. }
        | EventPayload::ExternalFileChangeDetected { .. } => true,
        EventPayload::FileChanged { owner, .. } => {
            !matches!(owner, latch_protocol::ChangeOwner::PreExisting)
        }
        _ => false,
    }
}

impl Agent {
    /// Incrementally derives the current workspace generation from durable
    /// events. Call before accepting validation evidence or deriving completion.
    pub(super) fn refresh_workspace_generation(&mut self) -> Result<bool> {
        let events = self.store.workspace_mutation_events_after(
            &self.workspace,
            self.workspace_generation_watermark,
        )?;
        let mut generation = self.evidence.workspace_generation();
        for (rowid, event) in &events {
            if advances_workspace_generation(&event.payload) {
                generation = *rowid;
            }
            match &event.payload {
                EventPayload::ProcessStarted {
                    id,
                    may_write_workspace,
                    ..
                } => {
                    if may_write_workspace.unwrap_or(true) {
                        generation = *rowid;
                        self.active_managed_processes
                            .insert(id.clone(), event.session_id);
                    }
                }
                EventPayload::ProcessExited { id, .. }
                    if self.active_managed_processes.remove(id).is_some() =>
                {
                    generation = *rowid;
                }
                _ => {}
            }
            self.workspace_generation_watermark = *rowid;
        }
        let changed = generation != self.evidence.workspace_generation();
        if changed {
            self.evidence.set_workspace_generation(generation);
        }
        self.evidence
            .set_active_writers(!self.active_managed_processes.is_empty());
        Ok(changed)
    }

    /// Feeds every durable event appended since the watermark to the progress
    /// supervisor and advances the watermark. Live supervision and replay
    /// consume the same event stream in the same order.
    pub(super) fn observe_progress_events(&mut self) -> Result<()> {
        let last = self.store.last_sequence(self.session_id)?;
        if self.progress_watermark > last {
            // History only grows, so this can only happen if a fresh store
            // replaced the old one. Skip rather than replaying everything and
            // double-counting supervision.
            self.progress_watermark = last;
            return Ok(());
        }
        let events = self
            .store
            .events_after(self.session_id, self.progress_watermark)?;
        for event in &events {
            if is_workspace_mutation(&event.payload) {
                // Kernel-owned mutation state: the workspace really changed,
                // asserted from the durable log rather than from the model's
                // own `touched_files`.
                self.run_mutated_workspace = true;
            }
            self.progress.observe_event(event);
        }
        self.progress_watermark = last;
        Ok(())
    }

    /// Issues the one-shot verification correction for this run.
    ///
    /// Returns true when the loop should spend one more turn asking for the
    /// evidence a terminal claim is missing. Every condition is kernel-owned:
    /// the completion state is kernel-derived, and the mutation is read from
    /// durable change events. An empty `required_validations` is deliberately
    /// not itself a failure — a read-only, explanatory, or documentation task
    /// may legitimately require none, and such a run mutates nothing, so it
    /// never reaches this path.
    pub(super) fn take_verification_correction(&mut self) -> bool {
        if self.verification_correction_issued
            || !self.run_mutated_workspace
            || !matches!(
                self.state.state().completion,
                CompletionState::InProgress | CompletionState::ImplementedNotVerified
            )
        {
            return false;
        }
        self.verification_correction_issued = true;
        self.verification_correction = Some(VERIFICATION_CORRECTION.to_owned());
        true
    }

    /// The kernel instruction attached to the next request: the verification
    /// correction when one is pending, progress re-ground when inspection has
    /// stalled, and both when they apply together.
    pub(super) fn combined_reground_instruction(&self) -> Option<String> {
        let progress = self.progress.reground_instruction();
        match (&self.verification_correction, progress) {
            (Some(correction), Some(progress)) => Some(format!("{correction}\n\n{progress}")),
            (Some(correction), None) => Some(correction.clone()),
            (None, progress) => progress,
        }
    }

    /// Deterministic inspection-loop supervision. Every durable event produced
    /// since the last turn is fed to the supervisor, the turn is settled, and a
    /// crossed stagnation budget injects a kernel-owned re-ground instruction
    /// on the next request.
    pub(super) fn supervise_progress(&mut self, sink: &AgentEventSink) -> Result<()> {
        self.observe_progress_events()?;
        match self.progress.finish_turn() {
            Some(StagnationDecision::Reground {
                unchanged,
                redundant_turns,
            }) => {
                self.emit(
                    EventPayload::ProgressStagnation {
                        unchanged,
                        redundant_turns,
                    },
                    sink,
                )?;
            }
            None => {}
        }
        Ok(())
    }

    /// Failure supervision keyed by validation lineage: failed attempts
    /// escalate their own subject toward re-ground, and only a passing
    /// validation (or a materially changed failure signature, handled inside
    /// the manager) resolves the streak. Successful inspection tools never
    /// touch it.
    pub(super) fn supervise_failures(
        &mut self,
        calls: &[ToolCall],
        results: &[ToolResult],
        sink: &AgentEventSink,
    ) -> Result<()> {
        for result in results {
            let Some(call) = calls.iter().find(|call| call.id == result.call_id) else {
                continue;
            };
            // Validations supervise themselves inside execute_validate so the
            // model path and the kernel path cannot double count. Kernel-
            // suppressed redundant observations are not tool failures.
            if call.name == "validate" || self.kernel_resolved_calls.contains(&result.call_id) {
                continue;
            }
            let subject = failure_subject(&call.name, &call.arguments);
            if result.is_error {
                let decision = self.failures.record(&subject, &result.output);
                self.emit(
                    EventPayload::FailureAttempt {
                        signature: decision.signature,
                        count: decision.count,
                    },
                    sink,
                )?;
                if decision.reground {
                    self.emit(EventPayload::RegroundRequested { signature: subject }, sink)?;
                }
            } else if call.name == "shell" {
                self.failures.resolve(&subject);
            }
        }
        Ok(())
    }

    /// Rebuilds failure supervision from the durable event log so a stalled
    /// validation loop survives `--resume`.
    pub fn restore_failures(&mut self) -> Result<()> {
        let events = self.store.events_of_kinds(
            self.session_id,
            &["tool_requested", "tool_completed", "tool_failed"],
        )?;
        let mut calls: std::collections::HashMap<String, (String, Vec<String>)> =
            std::collections::HashMap::new();
        let mut attempts: Vec<(String, bool, String)> = Vec::new();
        for event in &events {
            match &event.payload {
                EventPayload::ToolRequested { call } => {
                    calls.insert(
                        call.id.clone(),
                        (
                            call.name.clone(),
                            if call.name == "validate" {
                                super::validation::validation_requirements(&call.arguments)
                                    .unwrap_or_else(|_| {
                                        vec![failure_subject(&call.name, &call.arguments)]
                                    })
                            } else {
                                vec![failure_subject(&call.name, &call.arguments)]
                            },
                        ),
                    );
                }
                EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                    if let Some((tool, subjects)) = calls.get(&result.call_id)
                        && matches!(tool.as_str(), "shell" | "validate")
                    {
                        let failed = matches!(&event.payload, EventPayload::ToolFailed { .. });
                        let output = if tool == "validate" {
                            validation_failure_output(&result.output)
                        } else {
                            &result.output
                        };
                        for subject in subjects {
                            attempts.push((subject.clone(), failed, output.to_owned()));
                        }
                    }
                }
                _ => {}
            }
        }
        self.failures.replay(
            attempts
                .iter()
                .map(|(subject, failed, output)| (subject.as_str(), *failed, output.as_str())),
        );
        Ok(())
    }

    /// Rebuilds progress/stagnation supervision from the durable event log so
    /// `--resume` does not immediately forget an active inspection loop. The
    /// watermark advances past everything already consumed.
    pub fn restore_progress(&mut self) -> Result<()> {
        let events = self.store.events(self.session_id)?;
        self.progress.reset();
        self.progress.replay(&events);
        self.progress_watermark = self.store.last_sequence(self.session_id)?;
        Ok(())
    }
}
