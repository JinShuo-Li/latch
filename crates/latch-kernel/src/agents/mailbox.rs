use latch_protocol::{AgentMessage, AgentReport};
use std::collections::VecDeque;
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct ChildMailbox {
    messages: std::sync::Arc<Mutex<VecDeque<AgentMessage>>>,
}

impl ChildMailbox {
    /// Idempotent by message id: a message restored from the durable queue on
    /// worker restart and the same message arriving again by channel command
    /// are one delivery, never two.
    pub fn push(&self, message: AgentMessage) {
        let mut messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        if !messages
            .iter()
            .any(|queued| queued.message_id == message.message_id)
        {
            messages.push_back(message);
        }
    }

    pub fn drain(&self) -> Vec<AgentMessage> {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    pub fn has_follow_up(&self) -> bool {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|message| message.kind == latch_protocol::AgentMessageKind::FollowUp)
    }
}

#[derive(Default)]
pub struct NotificationMailbox {
    reports: Mutex<VecDeque<AgentReport>>,
}

impl NotificationMailbox {
    pub fn push(&self, report: AgentReport) {
        let mut reports = self.reports.lock().unwrap_or_else(|e| e.into_inner());
        if !reports
            .iter()
            .any(|queued| queued.report_id == report.report_id)
        {
            reports.push_back(report);
        }
    }

    pub fn drain(&self) -> Vec<AgentReport> {
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    pub fn restore_front(&self, reports: Vec<AgentReport>) {
        let mut queued = self.reports.lock().unwrap_or_else(|e| e.into_inner());
        for report in reports.into_iter().rev() {
            if !queued
                .iter()
                .any(|existing| existing.report_id == report.report_id)
            {
                queued.push_front(report);
            }
        }
    }

    pub fn selected(&self, selected: &[Uuid]) -> Vec<AgentReport> {
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|report| selected.is_empty() || selected.contains(&report.agent_id))
            .cloned()
            .collect()
    }
}
