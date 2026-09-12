//! Progress and failure supervision: the same event stream feeds live turns and
//! restored sessions so replay and live supervision stay identical.

use super::*;

impl Agent {
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
            self.progress.observe_event(event);
        }
        self.progress_watermark = last;
        Ok(())
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
        let mut calls: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let mut attempts: Vec<(String, bool, String)> = Vec::new();
        for event in &events {
            match &event.payload {
                EventPayload::ToolRequested { call } => {
                    calls.insert(
                        call.id.clone(),
                        (
                            call.name.clone(),
                            failure_subject(&call.name, &call.arguments),
                        ),
                    );
                }
                EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                    if let Some((tool, subject)) = calls.get(&result.call_id)
                        && matches!(tool.as_str(), "shell" | "validate")
                    {
                        let failed = matches!(&event.payload, EventPayload::ToolFailed { .. });
                        attempts.push((subject.clone(), failed, result.output.clone()));
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
