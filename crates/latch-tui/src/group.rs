//! Root-visible agent-group coordination state derived from durable events.
//!
//! The group is a compact coordination overlay: shared tasks, claims, and
//! membership. It never carries child transcripts. Live and replayed event
//! streams produce identical state, so the sidebar and `/group` view agree
//! with what actually committed.

use latch_protocol::{AgentGroupIdentity, Event, EventPayload, GroupTask, GroupTaskStatus};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Compact counts for the sidebar block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupCounts {
    pub total: usize,
    pub ready: usize,
    pub claimed: usize,
    pub in_progress: usize,
    pub blocked: usize,
    pub completed: usize,
    pub cancelled: usize,
}

impl GroupCounts {
    #[must_use]
    pub fn active(&self) -> usize {
        self.claimed + self.in_progress
    }
}

/// Deterministic group view model for the sidebar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupModel {
    identity: Option<AgentGroupIdentity>,
    members: BTreeSet<Uuid>,
    tasks: BTreeMap<Uuid, GroupTask>,
}

impl GroupModel {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.identity.is_none()
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.identity
            .as_ref()
            .map(|identity| identity.name.as_str())
    }

    #[must_use]
    pub fn members(&self) -> &BTreeSet<Uuid> {
        &self.members
    }

    #[must_use]
    pub fn tasks(&self) -> &BTreeMap<Uuid, GroupTask> {
        &self.tasks
    }

    pub fn apply_event(&mut self, event: &Event) {
        match &event.payload {
            EventPayload::AgentGroupCreated { identity } => {
                self.identity = Some(identity.clone());
            }
            EventPayload::AgentGroupMemberJoined { agent_id, .. } => {
                self.members.insert(*agent_id);
            }
            EventPayload::GroupTaskCreated { task } => {
                self.tasks.insert(task.task_id, task.clone());
            }
            EventPayload::GroupTaskClaimed { task_id, agent_id } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = GroupTaskStatus::Claimed;
                    task.assignee = Some(*agent_id);
                }
            }
            EventPayload::GroupTaskStatusChanged {
                task_id,
                status,
                assignee,
                summary,
                ..
            } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = *status;
                    if let Some(assignee) = assignee {
                        task.assignee = Some(*assignee);
                    }
                    if *status == GroupTaskStatus::Pending {
                        task.assignee = None;
                    }
                    if summary.is_some() {
                        task.summary = summary.clone();
                    }
                }
            }
            EventPayload::GroupTaskReleased { task_id, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = GroupTaskStatus::Pending;
                    task.assignee = None;
                }
            }
            _ => {}
        }
    }

    #[must_use]
    pub fn counts(&self) -> GroupCounts {
        let mut counts = GroupCounts {
            total: self.tasks.len(),
            ..Default::default()
        };
        for task in self.tasks.values() {
            match task.status {
                GroupTaskStatus::Pending => {
                    if self.is_ready(task) {
                        counts.ready += 1;
                    }
                }
                GroupTaskStatus::Claimed => counts.claimed += 1,
                GroupTaskStatus::InProgress => counts.in_progress += 1,
                GroupTaskStatus::Blocked => counts.blocked += 1,
                GroupTaskStatus::Completed => counts.completed += 1,
                GroupTaskStatus::Cancelled => counts.cancelled += 1,
            }
        }
        counts
    }

    fn is_ready(&self, task: &GroupTask) -> bool {
        task.status == GroupTaskStatus::Pending
            && task.dependencies.iter().all(|dependency| {
                self.tasks
                    .get(dependency)
                    .is_some_and(|dependency| dependency.status == GroupTaskStatus::Completed)
            })
    }

    /// Active tasks in deterministic creation order.
    #[must_use]
    pub fn active_tasks(&self) -> Vec<&GroupTask> {
        self.sorted(|task| task.status.is_active())
    }

    /// Ready (claimable) tasks in deterministic creation order.
    #[must_use]
    pub fn ready_tasks(&self) -> Vec<&GroupTask> {
        self.sorted(|task| self.is_ready(task))
    }

    fn sorted(&self, keep: impl Fn(&GroupTask) -> bool) -> Vec<&GroupTask> {
        let mut tasks = self
            .tasks
            .values()
            .filter(|task| keep(task))
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.task_id.cmp(&right.task_id))
        });
        tasks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use latch_protocol::GroupTaskStatus;

    fn task(group: Uuid, title: &str, dependencies: Vec<Uuid>) -> GroupTask {
        GroupTask {
            task_id: Uuid::new_v4(),
            group_id: group,
            title: title.into(),
            description: String::new(),
            status: GroupTaskStatus::Pending,
            dependencies,
            assignee: None,
            required: true,
            created_by: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            summary: None,
            findings: vec![],
            expected_paths: vec![],
            touched_files: vec![],
            reason: None,
        }
    }

    fn event(sequence: u64, payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sequence,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    #[test]
    fn reducer_tracks_claims_and_readiness() {
        let group = Uuid::new_v4();
        let root = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let first = task(group, "parser", vec![]);
        let second = task(group, "tests", vec![first.task_id]);
        let mut model = GroupModel::default();
        assert!(model.is_empty());
        model.apply_event(&event(
            1,
            EventPayload::AgentGroupCreated {
                identity: AgentGroupIdentity {
                    group_id: group,
                    root_session_id: root,
                    name: "workspace".into(),
                    created_at: Utc::now(),
                },
            },
        ));
        model.apply_event(&event(
            2,
            EventPayload::AgentGroupMemberJoined {
                group_id: group,
                agent_id: agent,
            },
        ));
        model.apply_event(&event(
            3,
            EventPayload::GroupTaskCreated {
                task: first.clone(),
            },
        ));
        model.apply_event(&event(
            4,
            EventPayload::GroupTaskCreated {
                task: second.clone(),
            },
        ));
        assert_eq!(model.name(), Some("workspace"));
        assert_eq!(model.members().len(), 1);
        let counts = model.counts();
        assert_eq!((counts.total, counts.ready, counts.active()), (2, 1, 0));
        assert_eq!(
            model.ready_tasks()[0].task_id,
            first.task_id,
            "a dependent task is not ready"
        );
        model.apply_event(&event(
            5,
            EventPayload::GroupTaskClaimed {
                task_id: first.task_id,
                agent_id: agent,
            },
        ));
        assert_eq!(model.counts().active(), 1);
        assert!(model.ready_tasks().is_empty());
        model.apply_event(&event(
            6,
            EventPayload::GroupTaskStatusChanged {
                task_id: first.task_id,
                status: GroupTaskStatus::Completed,
                actor: agent,
                assignee: None,
                summary: Some("done".into()),
                reason: None,
                findings: vec![],
                touched_files: vec![],
            },
        ));
        assert_eq!(model.ready_tasks()[0].task_id, second.task_id);
        assert_eq!(
            model.tasks()[&first.task_id].summary.as_deref(),
            Some("done")
        );
    }
}
