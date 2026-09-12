//! Live steering queue: durable, FIFO messages injected at safe model boundaries.

use super::*;

/// Outcome of submitting a live steering message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum SteeringSubmission {
    /// The running agent accepted the message and will consume it before it
    /// exits. The run cannot close without either consuming it or returning it
    /// to the caller.
    Accepted,
    /// The run already closed. The message was not queued, so it cannot leak
    /// into a later run; the caller owns it and must surface it.
    Closed,
}

#[derive(Debug, Default)]
struct SteeringState {
    pending: std::collections::VecDeque<String>,
    closed: bool,
}

/// Live user steering: messages typed while a task is running. The TUI/CLI
/// pushes; the single agent loop drains at safe model boundaries and records
/// each message durably as a normal user turn. Ordering is FIFO and messages
/// are never inserted into an unresolved assistant/tool transaction.
///
/// The queue is an atomic run-closing handshake. A run [`open`](Self::open)s
/// it while it can still consume, and closing it is atomic with acceptance:
/// any submission that linearizes before the close is returned to the run
/// (which must consume it before exiting), and any submission after the close
/// is rejected instead of waiting for a later run.
#[derive(Debug, Clone, Default)]
pub struct SteeringQueue {
    state: Arc<std::sync::Mutex<SteeringState>>,
}

impl SteeringQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Submits a steering message. The result is the deterministic
    /// accept/reject outcome; a rejected message is never enqueued.
    pub fn push(&self, text: impl Into<String>) -> SteeringSubmission {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            SteeringSubmission::Closed
        } else {
            state.pending.push_back(text.into());
            SteeringSubmission::Accepted
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.pending.is_empty())
            .unwrap_or(true)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.pending.len())
            .unwrap_or(0)
    }

    pub(super) fn drain(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|mut state| state.pending.drain(..).collect())
            .unwrap_or_default()
    }

    /// Atomically closes the queue and takes everything accepted before the
    /// close. When nothing is pending the queue stays closed (the run may
    /// exit). When messages were accepted the queue stays open and returns
    /// them, because the current run must consume them before it may close.
    pub(super) fn close_and_drain(&self) -> Vec<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.is_empty() {
            state.closed = true;
            Vec::new()
        } else {
            state.pending.drain(..).collect()
        }
    }

    /// Marks the run open for acceptance. Called once at the start of `run`.
    pub(super) fn open(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = false;
    }

    /// Closes the run to further acceptance. Any still-pending message was
    /// accepted by a run that is aborting (cancel or error); it is dropped
    /// rather than leaked into a later run. Ctrl+C therefore keeps its
    /// existing semantics: the active run stops and queued steers do not
    /// silently survive it.
    pub(super) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.pending.clear();
    }
}
