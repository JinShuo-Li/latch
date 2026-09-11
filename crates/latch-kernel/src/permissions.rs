//! Real human-approval semantics for `Ask` policy decisions.
//!
//! The kernel pauses a tool call, emits a durable `PermissionRequested` event,
//! and waits for exactly one resolution. Only a real user decision (through
//! the TUI) can approve; the model never sees or supplies the request id, so it
//! cannot fabricate approval. Non-interactive sessions and resumed sessions
//! with unresolved requests resolve honestly instead of hanging.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct PermissionBroker {
    pending: Arc<Mutex<HashMap<Uuid, oneshot::Sender<bool>>>>,
}

impl PermissionBroker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for a decision on `request_id`. Returns `None` when the waiter is
    /// cancelled or the sender disappears without a decision.
    pub async fn request(&self, request_id: Uuid) -> Option<bool> {
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(request_id, sender);
        receiver.await.ok()
    }

    /// Resolves a pending request. Returns true when a waiter was notified.
    pub async fn resolve(&self, request_id: Uuid, approved: bool) -> bool {
        match self.pending.lock().await.remove(&request_id) {
            Some(sender) => sender.send(approved).is_ok(),
            None => false,
        }
    }

    /// Drops a pending request without a decision (for example when the turn is
    /// cancelled).
    pub async fn cancel(&self, request_id: Uuid) {
        self.pending.lock().await.remove(&request_id);
    }

    #[must_use]
    pub async fn is_pending(&self, request_id: Uuid) -> bool {
        self.pending.lock().await.contains_key(&request_id)
    }

    #[must_use]
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// Request ids currently waiting for a decision.
    #[must_use]
    pub async fn pending_ids(&self) -> Vec<Uuid> {
        self.pending.lock().await.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn request_resolves_exactly_once() {
        let broker = PermissionBroker::new();
        let id = Uuid::new_v4();
        let waiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(id).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(broker.is_pending(id).await);
        assert!(broker.resolve(id, true).await);
        assert_eq!(waiter.await.unwrap(), Some(true));
        // A second resolution has no effect: approval is single-use and cannot
        // be replayed.
        assert!(!broker.resolve(id, true).await);
        assert_eq!(broker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn cancellation_drops_the_waiter() {
        let broker = PermissionBroker::new();
        let id = Uuid::new_v4();
        let waiter = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(id).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        broker.cancel(id).await;
        assert_eq!(waiter.await.unwrap(), None);
        assert!(!broker.resolve(id, true).await);
    }
}
