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
    pending: std::collections::VecDeque<UserInput>,
    closed: bool,
}

/// Live user steering: messages typed while a task is running. The TUI/CLI
/// pushes; the single agent loop drains at safe model boundaries and records
/// each message durably as a normal user turn. Ordering is FIFO and messages
/// are never inserted into an unresolved assistant/tool transaction. Each
/// submission is structured (text plus durable image references), so images
/// are usable mid-run exactly like on the initial prompt.
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

    /// Locks the queue state, recovering from poisoning instead of reporting a
    /// default. Poisoning means a panic happened while the lock was held; the
    /// protected state is a `VecDeque<UserInput>` plus a bool, whose
    /// invariants cannot be left half-written by a panic, so recovering is
    /// strictly safer than silently reporting an empty queue and losing user
    /// input.
    fn lock(&self) -> std::sync::MutexGuard<'_, SteeringState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Submits a steering message. The result is the deterministic
    /// accept/reject outcome; a rejected message is never enqueued.
    pub fn push(&self, input: impl Into<UserInput>) -> SteeringSubmission {
        let mut state = self.lock();
        if state.closed {
            SteeringSubmission::Closed
        } else {
            state.pending.push_back(input.into());
            SteeringSubmission::Accepted
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().pending.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().pending.len()
    }

    pub(super) fn drain(&self) -> Vec<UserInput> {
        self.lock().pending.drain(..).collect()
    }

    /// Atomically closes the queue and takes everything accepted before the
    /// close. When nothing is pending the queue stays closed (the run may
    /// exit). When messages were accepted the queue stays open and returns
    /// them, because the current run must consume them before it may close.
    pub(super) fn close_and_drain(&self) -> Vec<UserInput> {
        let mut state = self.lock();
        if state.pending.is_empty() {
            state.closed = true;
            Vec::new()
        } else {
            state.pending.drain(..).collect()
        }
    }

    /// Marks the run open for acceptance. Called once at the start of `run`.
    pub(super) fn open(&self) {
        self.lock().closed = false;
    }

    /// Closes the run to further acceptance. Any still-pending message was
    /// accepted by a run that is aborting (cancel or error); it is dropped
    /// rather than leaked into a later run. Ctrl+C therefore keeps its
    /// existing semantics: the active run stops and queued steers do not
    /// silently survive it.
    pub(super) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.pending.clear();
    }

    /// Test-only: poison the internal lock to prove recovery never drops
    /// accepted input.
    #[cfg(test)]
    pub(super) fn poison_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.lock();
            panic!("poison the steering lock");
        }));
    }
}
