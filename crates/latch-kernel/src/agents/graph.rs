use latch_protocol::{AgentIdentity, AgentReport, AgentStatus, Event, EventPayload};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNode {
    pub identity: AgentIdentity,
    pub status: AgentStatus,
    pub report: Option<AgentReport>,
}

#[derive(Debug, Default)]
pub struct AgentGraph {
    nodes: HashMap<Uuid, AgentNode>,
}

impl AgentGraph {
    #[must_use]
    pub fn replay(events: &[Event]) -> Self {
        let mut graph = Self::default();
        for event in events {
            match &event.payload {
                EventPayload::AgentSpawned { identity, .. } => {
                    graph.nodes.insert(
                        identity.agent_id,
                        AgentNode {
                            identity: identity.clone(),
                            status: AgentStatus::Starting,
                            report: None,
                        },
                    );
                }
                EventPayload::AgentStatusChanged { status, .. } => {
                    if let Some(node) = graph.nodes.get_mut(&event.session_id) {
                        node.status = *status;
                    }
                }
                EventPayload::AgentReportCreated { report } => {
                    if let Some(node) = graph.nodes.get_mut(&event.session_id) {
                        node.status = report.status;
                        node.report = Some(report.clone());
                    }
                }
                EventPayload::AgentInterrupted { .. } => {
                    if let Some(node) = graph.nodes.get_mut(&event.session_id) {
                        node.status = AgentStatus::Interrupted;
                    }
                }
                EventPayload::AgentClosed => {
                    if let Some(node) = graph.nodes.get_mut(&event.session_id) {
                        node.status = AgentStatus::Closed;
                    }
                }
                _ => {}
            }
        }
        graph
    }

    pub fn insert(&mut self, node: AgentNode) {
        self.nodes.insert(node.identity.agent_id, node);
    }

    pub fn get(&self, id: Uuid) -> Option<&AgentNode> {
        self.nodes.get(&id)
    }

    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut AgentNode> {
        self.nodes.get_mut(&id)
    }

    pub fn snapshots(&self) -> Vec<AgentNode> {
        let mut nodes = self.nodes.values().cloned().collect::<Vec<_>>();
        nodes.sort_by(|left, right| {
            left.identity
                .task_name
                .cmp(&right.identity.task_name)
                .then_with(|| left.identity.agent_id.cmp(&right.identity.agent_id))
        });
        nodes
    }
}
