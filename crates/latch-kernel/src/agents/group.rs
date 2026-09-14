//! Root-scoped agent group: durable coordination overlay for independently
//! persisted child sessions.
//!
//! The group never owns workers and never replaces the supervisor. It owns a
//! shared task DAG, atomic claims, a durable peer mailbox, and replayable
//! progress state. The durable event log is authoritative; every query here is
//! a pure function of the events (or of the SQLite projection rebuilt from
//! them).

use crate::store::{EventStore, GroupClaimOutcome, GroupTaskTransition, GroupTransitionOutcome};
use anyhow::{Result, bail};
use latch_protocol::{
    AgentGroupIdentity, Event, EventPayload, GroupMessage, GroupMessageTarget, GroupTask,
    GroupTaskStatus,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Upper bound for one peer message, enforced by the kernel, so group traffic
/// can never recreate the earlier token explosion.
pub const MAX_GROUP_MESSAGE_BYTES: usize = 4_000;

/// Compact deterministic group progress counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupTaskCounts {
    pub total: usize,
    pub pending: usize,
    pub ready: usize,
    pub claimed: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub blocked: usize,
    pub cancelled: usize,
}

impl GroupTaskCounts {
    #[must_use]
    pub fn active(&self) -> usize {
        self.claimed + self.in_progress
    }
}

/// Advisory workspace-conflict warning between two concurrently active tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupConflict {
    pub task_a: Uuid,
    pub task_b: Uuid,
    pub path: String,
}

/// One compact status snapshot used by the model-facing `group_status` tool and
/// the TUI. It is computed from the reducer, never from ad-hoc event scans.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStatus {
    pub identity: AgentGroupIdentity,
    pub counts: GroupTaskCounts,
    pub ready: Vec<GroupTask>,
    pub active: Vec<GroupTask>,
    pub blocked: Vec<GroupTask>,
    /// Non-completed tasks whose dependencies include a blocked or cancelled
    /// task. Surfaced explicitly instead of silently rewriting the graph.
    pub dependency_failures: Vec<(Uuid, Vec<Uuid>)>,
    pub conflicts: Vec<GroupConflict>,
    pub members: Vec<Uuid>,
}

/// One message as seen by a recipient: delivered marks whether the durable
/// delivery receipt already exists.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupInboxEntry {
    pub message: GroupMessage,
    pub delivered: bool,
}

/// Deterministic in-memory reducer over durable group events. Given the same
/// ordered events, replay yields identical tasks, dependencies, claims,
/// membership, and delivery state. This mirrors `AgentGraph::replay`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupState {
    identity: Option<AgentGroupIdentity>,
    members: BTreeSet<Uuid>,
    tasks: BTreeMap<Uuid, GroupTask>,
    messages: BTreeMap<Uuid, GroupMessage>,
    delivered: BTreeSet<(Uuid, Uuid)>,
}

impl GroupState {
    #[must_use]
    pub fn identity(&self) -> Option<&AgentGroupIdentity> {
        self.identity.as_ref()
    }

    #[must_use]
    pub fn group_id(&self) -> Option<Uuid> {
        self.identity.as_ref().map(|identity| identity.group_id)
    }

    #[must_use]
    pub fn members(&self) -> &BTreeSet<Uuid> {
        &self.members
    }

    #[must_use]
    pub fn tasks(&self) -> &BTreeMap<Uuid, GroupTask> {
        &self.tasks
    }

    #[must_use]
    pub fn task(&self, task_id: Uuid) -> Option<&GroupTask> {
        self.tasks.get(&task_id)
    }

    #[must_use]
    pub fn messages(&self) -> &BTreeMap<Uuid, GroupMessage> {
        &self.messages
    }

    /// Applies one durable event. Unknown payloads are ignored so a reducer can
    /// consume a mixed event stream without a second filter.
    pub fn apply(&mut self, event: &Event) {
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
                reason,
                findings,
                touched_files,
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
                    if reason.is_some() {
                        task.reason = reason.clone();
                    }
                    if !findings.is_empty() {
                        task.findings = findings.clone();
                    }
                    if !touched_files.is_empty() {
                        task.touched_files = touched_files.clone();
                    }
                }
            }
            EventPayload::GroupTaskReleased { task_id, .. } => {
                if let Some(task) = self.tasks.get_mut(task_id) {
                    task.status = GroupTaskStatus::Pending;
                    task.assignee = None;
                }
            }
            EventPayload::GroupMessageQueued { message }
            | EventPayload::GroupMessageDelivered { message } => {
                self.messages
                    .entry(message.message_id)
                    .or_insert_with(|| message.clone());
                if matches!(event.payload, EventPayload::GroupMessageDelivered { .. }) {
                    self.delivered
                        .insert((message.message_id, event.session_id));
                }
            }
            _ => {}
        }
    }

    /// Replays an ordered event stream. Order is the caller's contract; the
    /// durable log is passed in global commit order.
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    /// Every task in deterministic creation order.
    #[must_use]
    pub fn ordered_tasks(&self) -> Vec<GroupTask> {
        self.tasks_sorted_by(|_| true)
    }

    /// Deterministic task ordering: creation order, then id.
    fn tasks_sorted_by(&self, filter: impl Fn(&GroupTask) -> bool) -> Vec<GroupTask> {
        let mut tasks = self
            .tasks
            .values()
            .filter(|task| filter(task))
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.task_id.cmp(&right.task_id))
        });
        tasks
    }

    #[must_use]
    pub fn counts(&self) -> GroupTaskCounts {
        let mut counts = GroupTaskCounts {
            total: self.tasks.len(),
            ..Default::default()
        };
        for task in self.tasks.values() {
            match task.status {
                GroupTaskStatus::Pending => {
                    counts.pending += 1;
                    if task.is_ready(&self.tasks) {
                        counts.ready += 1;
                    }
                }
                GroupTaskStatus::Claimed => counts.claimed += 1,
                GroupTaskStatus::InProgress => counts.in_progress += 1,
                GroupTaskStatus::Completed => counts.completed += 1,
                GroupTaskStatus::Blocked => counts.blocked += 1,
                GroupTaskStatus::Cancelled => counts.cancelled += 1,
            }
        }
        counts
    }

    #[must_use]
    pub fn ready_tasks(&self) -> Vec<GroupTask> {
        self.tasks_sorted_by(|task| {
            task.status == GroupTaskStatus::Pending && task.is_ready(&self.tasks)
        })
    }

    #[must_use]
    pub fn active_tasks(&self) -> Vec<GroupTask> {
        self.tasks_sorted_by(|task| task.status.is_active())
    }

    #[must_use]
    pub fn blocked_tasks(&self) -> Vec<GroupTask> {
        self.tasks_sorted_by(|task| task.status == GroupTaskStatus::Blocked)
    }

    /// Required tasks that still prevent terminal root completion. Pending,
    /// claimed, in-progress, and blocked all count; completed and cancelled do
    /// not.
    #[must_use]
    pub fn required_unfinished(&self) -> Vec<GroupTask> {
        self.tasks_sorted_by(|task| {
            task.required
                && !matches!(
                    task.status,
                    GroupTaskStatus::Completed | GroupTaskStatus::Cancelled
                )
        })
    }

    /// Non-completed tasks whose dependencies include a blocked or cancelled
    /// task: the graph will never make them ready without an explicit decision.
    #[must_use]
    pub fn dependency_failures(&self) -> Vec<(Uuid, Vec<Uuid>)> {
        let mut failures = Vec::new();
        for task in self.tasks.values() {
            if task.status.is_terminal() {
                continue;
            }
            let failed = task
                .dependencies
                .iter()
                .filter(|dependency| {
                    self.tasks
                        .get(dependency)
                        .is_some_and(|dep| dep.status == GroupTaskStatus::Blocked)
                })
                .copied()
                .collect::<Vec<_>>();
            let mut cancelled = task
                .dependencies
                .iter()
                .filter(|dependency| {
                    self.tasks
                        .get(dependency)
                        .is_some_and(|dep| dep.status == GroupTaskStatus::Cancelled)
                })
                .copied()
                .collect::<Vec<_>>();
            let mut all = failed;
            all.append(&mut cancelled);
            if !all.is_empty() {
                failures.push((task.task_id, all));
            }
        }
        failures.sort();
        failures
    }

    /// Advisory overlap between concurrently active tasks that declare or
    /// report paths. Never blocks an edit.
    #[must_use]
    pub fn conflicts(&self) -> Vec<GroupConflict> {
        let active = self.active_tasks();
        let mut warnings = Vec::new();
        for (index, task) in active.iter().enumerate() {
            let left = task
                .expected_paths
                .iter()
                .chain(task.touched_files.iter())
                .map(|path| normalize_path(path))
                .collect::<HashSet<_>>();
            if left.is_empty() {
                continue;
            }
            for other in &active[index + 1..] {
                let right = other
                    .expected_paths
                    .iter()
                    .chain(other.touched_files.iter())
                    .map(|path| normalize_path(path))
                    .collect::<HashSet<_>>();
                let mut overlapping = left.intersection(&right).cloned().collect::<Vec<String>>();
                overlapping.sort();
                if let Some(path) = overlapping.first() {
                    warnings.push(GroupConflict {
                        task_a: task.task_id,
                        task_b: other.task_id,
                        path: path.clone(),
                    });
                }
            }
        }
        warnings.sort_by(|left, right| {
            left.task_a
                .cmp(&right.task_a)
                .then_with(|| left.task_b.cmp(&right.task_b))
        });
        warnings
    }

    /// Messages addressed to one recipient in FIFO order, with delivery state.
    #[must_use]
    pub fn inbox(&self, recipient: Uuid, is_root: bool) -> Vec<GroupInboxEntry> {
        let mut messages = self
            .messages
            .values()
            .filter(|message| match message.to {
                GroupMessageTarget::Agent(agent) => agent == recipient,
                GroupMessageTarget::Root => is_root,
                GroupMessageTarget::Group => true,
            })
            .cloned()
            .collect::<Vec<_>>();
        messages.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        messages
            .into_iter()
            .map(|message| GroupInboxEntry {
                delivered: self.delivered.contains(&(message.message_id, recipient)),
                message,
            })
            .collect()
    }

    #[must_use]
    pub fn undelivered(&self, recipient: Uuid, is_root: bool) -> Vec<GroupMessage> {
        self.inbox(recipient, is_root)
            .into_iter()
            .filter(|entry| !entry.delivered)
            .map(|entry| entry.message)
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<GroupStatus> {
        let identity = self.identity.clone()?;
        Some(GroupStatus {
            identity,
            counts: self.counts(),
            ready: self.ready_tasks(),
            active: self.active_tasks(),
            blocked: self.blocked_tasks(),
            dependency_failures: self.dependency_failures(),
            conflicts: self.conflicts(),
            members: self.members.iter().copied().collect(),
        })
    }
}

fn normalize_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// Validates a proposed dependency set against an existing task graph. The
/// graph is never silently repaired: unknown ids, self-dependency, duplicates,
/// and cycles are all hard local errors.
pub fn validate_dag(
    tasks: &BTreeMap<Uuid, GroupTask>,
    task_id: Uuid,
    dependencies: &[Uuid],
) -> Result<()> {
    let mut seen = HashSet::new();
    for dependency in dependencies {
        if *dependency == task_id {
            bail!("task {task_id} cannot depend on itself");
        }
        if !tasks.contains_key(dependency) {
            bail!("unknown dependency {dependency}");
        }
        if !seen.insert(*dependency) {
            bail!("duplicate dependency {dependency}");
        }
    }
    // Adding edges task_id -> dependency must not make any dependency reach
    // task_id again.
    let mut stack = dependencies.to_vec();
    let mut visited = HashSet::new();
    while let Some(current) = stack.pop() {
        if current == task_id {
            bail!("dependency cycle through task {current}");
        }
        if !visited.insert(current) {
            continue;
        }
        if let Some(task) = tasks.get(&current) {
            stack.extend(task.dependencies.iter().copied());
        }
    }
    Ok(())
}

/// Shared group coordinator. One instance per root supervisor; children hold a
/// clone so they can claim tasks and exchange messages without going through
/// the root's worker command channel.
#[derive(Clone)]
pub struct GroupCoordinator {
    inner: Arc<GroupCoordinatorInner>,
}

struct GroupCoordinatorInner {
    store: EventStore,
    root_session_id: Uuid,
    name: String,
    state: Mutex<GroupState>,
}

impl GroupCoordinator {
    /// Builds (or rebuilds) the coordinator for a root session from durable
    /// events. Used by the supervisor at startup and by resume; this is the
    /// only constructor, so live and resumed state always share one code path.
    pub fn new(store: EventStore, root_session_id: Uuid, name: String) -> Result<Self> {
        let coordinator = Self {
            inner: Arc::new(GroupCoordinatorInner {
                store,
                root_session_id,
                name,
                state: Mutex::new(GroupState::default()),
            }),
        };
        coordinator.reload()?;
        Ok(coordinator)
    }

    #[must_use]
    pub fn root_session_id(&self) -> Uuid {
        self.inner.root_session_id
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GroupState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Rebuilds the in-memory reducer from the durable group events belonging
    /// to this root's group. Cross-session delivery events are included in
    /// global commit order, so replay is deterministic.
    fn reload(&self) -> Result<()> {
        let events = self.inner.store.group_events()?;
        let mut state = GroupState::default();
        let mut known_tasks: HashSet<Uuid> = HashSet::new();
        for event in &events {
            let include = match &event.payload {
                EventPayload::AgentGroupCreated { identity } => {
                    identity.root_session_id == self.inner.root_session_id
                }
                EventPayload::AgentGroupMemberJoined { group_id, .. } => {
                    state.group_id() == Some(*group_id)
                }
                EventPayload::GroupTaskCreated { task } => state.group_id() == Some(task.group_id),
                EventPayload::GroupTaskClaimed { task_id, .. }
                | EventPayload::GroupTaskStatusChanged { task_id, .. }
                | EventPayload::GroupTaskReleased { task_id, .. } => known_tasks.contains(task_id),
                EventPayload::GroupMessageQueued { message }
                | EventPayload::GroupMessageDelivered { message } => {
                    state.group_id() == Some(message.group_id)
                }
                _ => false,
            };
            if include {
                state.apply(event);
                if let EventPayload::GroupTaskCreated { task } = &event.payload {
                    known_tasks.insert(task.task_id);
                }
            }
        }
        *self.state() = state;
        Ok(())
    }

    fn apply(&self, event: &Event) {
        self.state().apply(event);
    }

    /// True when this root has created its group.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.state().identity().is_some()
    }

    #[must_use]
    pub fn identity(&self) -> Option<AgentGroupIdentity> {
        self.state().identity().cloned()
    }

    /// Lazily creates the group. Ordinary sessions never pay for group state
    /// because nothing calls this until group functionality is first used.
    pub fn ensure(&self) -> Result<AgentGroupIdentity> {
        if let Some(identity) = self.state().identity().cloned() {
            return Ok(identity);
        }
        let identity = self
            .inner
            .store
            .create_agent_group(self.inner.root_session_id, &self.inner.name)?;
        self.reload()?;
        Ok(identity)
    }

    /// Idempotent durable membership join. The root is implicitly a member.
    pub fn join(&self, agent_id: Uuid) -> Result<()> {
        if agent_id == self.inner.root_session_id {
            return Ok(());
        }
        let identity = self.ensure()?;
        if self.state().members().contains(&agent_id) {
            return Ok(());
        }
        if let Some(event) = self.inner.store.join_agent_group(
            self.inner.root_session_id,
            identity.group_id,
            agent_id,
        )? {
            self.apply(&event);
        }
        Ok(())
    }

    #[must_use]
    pub fn is_member(&self, agent_id: Uuid) -> bool {
        agent_id == self.inner.root_session_id || self.state().members().contains(&agent_id)
    }

    #[must_use]
    pub fn snapshot(&self) -> GroupState {
        self.state().clone()
    }

    #[must_use]
    pub fn counts(&self) -> GroupTaskCounts {
        self.state().counts()
    }

    #[must_use]
    pub fn status(&self) -> Option<GroupStatus> {
        self.state().snapshot()
    }

    /// Creates a task after validating the dependency graph against the
    /// current projection. Creation is root-owned; children never create or
    /// cancel shared work.
    pub fn create_task(
        &self,
        created_by: Uuid,
        title: String,
        description: String,
        dependencies: Vec<Uuid>,
        required: bool,
        expected_paths: Vec<String>,
    ) -> Result<GroupTask> {
        let title = title.trim().to_owned();
        if title.is_empty() {
            bail!("task title must be non-empty");
        }
        let identity = self.ensure()?;
        let tasks = self.state().tasks().clone();
        let task_id = Uuid::new_v4();
        validate_dag(&tasks, task_id, &dependencies)?;
        let now = chrono::Utc::now();
        let task = GroupTask {
            task_id,
            group_id: identity.group_id,
            title,
            description: description.trim().to_owned(),
            status: GroupTaskStatus::Pending,
            dependencies,
            assignee: None,
            required,
            created_by,
            created_at: now,
            updated_at: now,
            summary: None,
            findings: vec![],
            expected_paths: expected_paths
                .into_iter()
                .map(|path| path.trim().to_owned())
                .filter(|path| !path.is_empty())
                .collect(),
            touched_files: vec![],
            reason: None,
        };
        let stored = self.inner.store.create_group_task(
            self.inner.root_session_id,
            identity.group_id,
            task,
        )?;
        self.reload()?;
        Ok(self.state().task(stored.task_id).cloned().unwrap_or(stored))
    }

    /// Atomsic claim. Only one concurrent claimer wins; the store transaction
    /// is the arbiter, and the in-memory reducer reflects the committed event.
    pub fn claim(&self, task_id: Uuid, agent_id: Uuid) -> Result<GroupTask> {
        let identity = self.ensure()?;
        match self.inner.store.claim_group_task(
            self.inner.root_session_id,
            identity.group_id,
            task_id,
            agent_id,
        )? {
            GroupClaimOutcome::Claimed(event) => {
                self.apply(&event);
                self.state()
                    .task(task_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("claimed task {task_id} missing from reducer"))
            }
            GroupClaimOutcome::Missing => bail!("unknown task {task_id}"),
            GroupClaimOutcome::NotPending { status, assignee } => {
                let owner = assignee
                    .map(|assignee| assignee.to_string())
                    .unwrap_or_else(|| "none".into());
                bail!("task {task_id} is {status} (assignee {owner}), not claimable")
            }
            GroupClaimOutcome::DependenciesIncomplete { incomplete } => bail!(
                "task {task_id} is not ready; incomplete dependencies: {}",
                incomplete
                    .iter()
                    .map(Uuid::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Root-administrative reassignment of a non-terminal task to any member.
    pub fn reassign(&self, task_id: Uuid, target: Uuid) -> Result<GroupTask> {
        self.require_group()?;
        if !self.is_member(target) {
            bail!("agent {target} is not a group member");
        }
        let transition = GroupTaskTransition {
            allowed_from: vec![
                GroupTaskStatus::Pending,
                GroupTaskStatus::Claimed,
                GroupTaskStatus::InProgress,
                GroupTaskStatus::Blocked,
            ],
            to: GroupTaskStatus::Claimed,
            require_owner: false,
            assignee: Some(target),
            summary: None,
            reason: Some("reassigned by root".into()),
            findings: vec![],
            touched_files: vec![],
        };
        self.transition(task_id, self.inner.root_session_id, transition)
    }

    /// `Claimed -> InProgress` by the current assignee.
    pub fn start(&self, task_id: Uuid, actor: Uuid) -> Result<GroupTask> {
        self.transition(
            task_id,
            actor,
            GroupTaskTransition {
                allowed_from: vec![GroupTaskStatus::Claimed],
                to: GroupTaskStatus::InProgress,
                require_owner: true,
                assignee: None,
                summary: None,
                reason: None,
                findings: vec![],
                touched_files: vec![],
            },
        )
    }

    /// Completion records coordination truth plus an optional concise summary.
    /// It never certifies root evidence.
    pub fn complete(
        &self,
        task_id: Uuid,
        actor: Uuid,
        summary: String,
        touched_files: Vec<String>,
        findings: Vec<String>,
    ) -> Result<GroupTask> {
        let summary = summary.trim().to_owned();
        if summary.is_empty() {
            bail!("completing a task requires a concise summary");
        }
        self.transition(
            task_id,
            actor,
            GroupTaskTransition {
                allowed_from: vec![GroupTaskStatus::Claimed, GroupTaskStatus::InProgress],
                to: GroupTaskStatus::Completed,
                require_owner: true,
                assignee: None,
                summary: Some(summary),
                reason: None,
                findings: findings
                    .into_iter()
                    .map(|finding| finding.trim().to_owned())
                    .filter(|finding| !finding.is_empty())
                    .collect(),
                touched_files: touched_files
                    .into_iter()
                    .map(|path| path.trim().to_owned())
                    .filter(|path| !path.is_empty())
                    .collect(),
            },
        )
    }

    /// `Claimed|InProgress -> Blocked` by the current assignee, with a reason.
    pub fn block(&self, task_id: Uuid, actor: Uuid, reason: String) -> Result<GroupTask> {
        let reason = reason.trim().to_owned();
        if reason.is_empty() {
            bail!("blocking a task requires a reason");
        }
        self.transition(
            task_id,
            actor,
            GroupTaskTransition {
                allowed_from: vec![GroupTaskStatus::Claimed, GroupTaskStatus::InProgress],
                to: GroupTaskStatus::Blocked,
                require_owner: true,
                assignee: None,
                summary: None,
                reason: Some(reason),
                findings: vec![],
                touched_files: vec![],
            },
        )
    }

    /// Root-administrative cancel. Cancelling never satisfies a dependency.
    pub fn cancel(&self, task_id: Uuid, reason: Option<String>) -> Result<GroupTask> {
        self.require_group()?;
        self.transition(
            task_id,
            self.inner.root_session_id,
            GroupTaskTransition {
                allowed_from: vec![
                    GroupTaskStatus::Pending,
                    GroupTaskStatus::Claimed,
                    GroupTaskStatus::InProgress,
                    GroupTaskStatus::Blocked,
                ],
                to: GroupTaskStatus::Cancelled,
                require_owner: false,
                assignee: None,
                summary: None,
                reason: reason
                    .map(|reason| reason.trim().to_owned())
                    .filter(|reason| !reason.is_empty()),
                findings: vec![],
                touched_files: vec![],
            },
        )
    }

    /// Release by the current assignee; the task returns to the pending pool
    /// with no owner.
    pub fn release(&self, task_id: Uuid, actor: Uuid) -> Result<GroupTask> {
        self.require_group()?;
        match self
            .inner
            .store
            .release_group_task(self.inner.root_session_id, task_id, actor)?
        {
            GroupTransitionOutcome::Updated(event) => {
                self.apply(&event);
                self.task_or_missing(task_id)
            }
            GroupTransitionOutcome::Missing => bail!("unknown task {task_id}"),
            GroupTransitionOutcome::NotOwner { assignee } => bail!(
                "task {task_id} is assigned to {}; only the assignee can release it",
                assignee
                    .map(|assignee| assignee.to_string())
                    .unwrap_or_else(|| "nobody".into())
            ),
            GroupTransitionOutcome::NotAllowed { status } => {
                bail!("task {task_id} is {status} and cannot be released")
            }
        }
    }

    fn transition(
        &self,
        task_id: Uuid,
        actor: Uuid,
        transition: GroupTaskTransition,
    ) -> Result<GroupTask> {
        match self.inner.store.transition_group_task(
            self.inner.root_session_id,
            task_id,
            actor,
            transition,
        )? {
            GroupTransitionOutcome::Updated(event) => {
                self.apply(&event);
                self.task_or_missing(task_id)
            }
            GroupTransitionOutcome::Missing => bail!("unknown task {task_id}"),
            GroupTransitionOutcome::NotOwner { assignee } => bail!(
                "task {task_id} is assigned to {}; only the assignee can change it",
                assignee
                    .map(|assignee| assignee.to_string())
                    .unwrap_or_else(|| "nobody".into())
            ),
            GroupTransitionOutcome::NotAllowed { status } => {
                bail!("task {task_id} is {status} and cannot make that transition")
            }
        }
    }

    fn task_or_missing(&self, task_id: Uuid) -> Result<GroupTask> {
        self.state()
            .task(task_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("task {task_id} missing from reducer"))
    }

    fn require_group(&self) -> Result<AgentGroupIdentity> {
        self.identity()
            .ok_or_else(|| anyhow::anyhow!("no agent group exists for this root session"))
    }

    /// Queues a durable peer message. Queuing never wakes an idle agent: it
    /// enters a recipient only at that recipient's next safe model boundary.
    pub fn send_message(
        &self,
        from: Uuid,
        to: GroupMessageTarget,
        text: String,
    ) -> Result<GroupMessage> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            bail!("group message text must be non-empty");
        }
        if text.len() > MAX_GROUP_MESSAGE_BYTES {
            bail!(
                "group message is {} bytes; the limit is {MAX_GROUP_MESSAGE_BYTES}. Send a concise message instead of a transcript.",
                text.len()
            );
        }
        let identity = self.ensure()?;
        if let GroupMessageTarget::Agent(agent) = to
            && !self.is_member(agent)
        {
            bail!("agent {agent} is not a group member");
        }
        let message = GroupMessage {
            message_id: Uuid::new_v4(),
            group_id: identity.group_id,
            from_agent: from,
            to,
            text,
            created_at: chrono::Utc::now(),
        };
        let event = self.inner.store.append(
            self.inner.root_session_id,
            EventPayload::GroupMessageQueued {
                message: message.clone(),
            },
        )?;
        self.apply(&event);
        Ok(message)
    }

    /// Delivers every queued message for one recipient at a safe model
    /// boundary. Each message is delivered at most once, across restarts.
    pub fn deliver_pending(&self, recipient: Uuid, is_root: bool) -> Result<Vec<Event>> {
        let Some(identity) = self.state().identity().cloned() else {
            return Ok(Vec::new());
        };
        let pending =
            self.inner
                .store
                .group_pending_messages(identity.group_id, recipient, is_root)?;
        let mut delivered = Vec::new();
        for message in pending {
            if let Some(event) = self.inner.store.deliver_group_message(recipient, message)? {
                self.apply(&event);
                delivered.push(event);
            }
        }
        Ok(delivered)
    }

    /// Recent messages addressed to one recipient, newest last, bounded.
    #[must_use]
    pub fn inbox(&self, recipient: Uuid, is_root: bool, limit: usize) -> Vec<GroupInboxEntry> {
        let mut entries = self.state().inbox(recipient, is_root);
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
        entries
    }

    /// Concise reason when the root may not terminally complete. `None` means
    /// the completion gate is satisfied.
    #[must_use]
    pub fn completion_blocker(&self) -> Option<String> {
        let state = self.state();
        let identity = state.identity()?;
        let unfinished = state.required_unfinished();
        if unfinished.is_empty() {
            return None;
        }
        let listed = unfinished
            .iter()
            .take(6)
            .map(|task| format!("{} [{}]", task.title, task.status))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Agent group `{}` still has {} required task(s) not completed or cancelled: {listed}. \
             Finish them (or have the assignee complete/block them, or cancel as root) before terminal completion.",
            identity.name,
            unfinished.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_protocol::Event;
    use std::path::Path;

    fn event(sequence: u64, payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            sequence,
            timestamp: chrono::Utc::now(),
            parent_id: None,
            payload,
        }
    }

    fn task(group_id: Uuid, root: Uuid, title: &str, dependencies: Vec<Uuid>) -> GroupTask {
        GroupTask {
            task_id: Uuid::new_v4(),
            group_id,
            title: title.into(),
            description: String::new(),
            status: GroupTaskStatus::Pending,
            dependencies,
            assignee: None,
            required: true,
            created_by: root,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            summary: None,
            findings: vec![],
            expected_paths: vec![],
            touched_files: vec![],
            reason: None,
        }
    }

    #[test]
    fn replay_is_deterministic_and_rebuilds_every_field() {
        let group_id = Uuid::new_v4();
        let root = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let first = task(group_id, root, "parser", vec![]);
        let second = task(group_id, root, "tests", vec![first.task_id]);
        let events = vec![
            event(
                1,
                EventPayload::AgentGroupCreated {
                    identity: AgentGroupIdentity {
                        group_id,
                        root_session_id: root,
                        name: "workspace".into(),
                        created_at: chrono::Utc::now(),
                    },
                },
            ),
            event(
                2,
                EventPayload::AgentGroupMemberJoined {
                    group_id,
                    agent_id: agent,
                },
            ),
            event(
                3,
                EventPayload::GroupTaskCreated {
                    task: first.clone(),
                },
            ),
            event(
                4,
                EventPayload::GroupTaskCreated {
                    task: second.clone(),
                },
            ),
            event(
                5,
                EventPayload::GroupTaskClaimed {
                    task_id: first.task_id,
                    agent_id: agent,
                },
            ),
            event(
                6,
                EventPayload::GroupTaskStatusChanged {
                    task_id: first.task_id,
                    status: GroupTaskStatus::Completed,
                    actor: agent,
                    assignee: None,
                    summary: Some("parser done".into()),
                    reason: None,
                    findings: vec!["precedence table".into()],
                    touched_files: vec!["src/parser.rs".into()],
                },
            ),
            event(
                7,
                EventPayload::GroupMessageQueued {
                    message: GroupMessage {
                        message_id: Uuid::new_v4(),
                        group_id,
                        from_agent: agent,
                        to: GroupMessageTarget::Root,
                        text: "parser done".into(),
                        created_at: chrono::Utc::now(),
                    },
                },
            ),
        ];
        let first_replay = GroupState::replay(&events);
        let second_replay = GroupState::replay(&events);
        assert_eq!(first_replay, second_replay, "replay is deterministic");
        assert_eq!(first_replay.members().len(), 1);
        assert_eq!(
            first_replay.task(first.task_id).unwrap().status,
            GroupTaskStatus::Completed
        );
        assert_eq!(
            first_replay.task(first.task_id).unwrap().summary.as_deref(),
            Some("parser done")
        );
        assert!(
            first_replay
                .ready_tasks()
                .iter()
                .any(|t| t.task_id == second.task_id)
        );
        assert_eq!(first_replay.counts().completed, 1);
        assert_eq!(first_replay.inbox(root, true).len(), 1);
        assert!(!first_replay.inbox(root, true)[0].delivered);
    }

    #[test]
    fn dag_validation_rejects_invalid_graphs_and_accepts_valid_ones() {
        let group_id = Uuid::new_v4();
        let root = Uuid::new_v4();
        let a = task(group_id, root, "a", vec![]);
        let b = task(group_id, root, "b", vec![a.task_id]);
        let c = task(group_id, root, "c", vec![b.task_id]);
        let mut tasks = BTreeMap::new();
        for item in [&a, &b, &c] {
            tasks.insert(item.task_id, item.clone());
        }
        validate_dag(&tasks, Uuid::new_v4(), &[a.task_id, c.task_id]).unwrap();
        let unknown = Uuid::new_v4();
        assert!(
            validate_dag(&tasks, unknown, &[unknown])
                .unwrap_err()
                .to_string()
                .contains("itself")
        );
        assert!(
            validate_dag(&tasks, unknown, &[Uuid::new_v4()])
                .unwrap_err()
                .to_string()
                .contains("unknown dependency")
        );
        assert!(
            validate_dag(&tasks, unknown, &[a.task_id, a.task_id])
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        // a -> c would close a cycle a -> b -> c -> a.
        let error = validate_dag(&tasks, a.task_id, &[c.task_id])
            .unwrap_err()
            .to_string();
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn dependency_failures_surface_blocked_and_cancelled_dependencies() {
        let group_id = Uuid::new_v4();
        let root = Uuid::new_v4();
        let dependency = task(group_id, root, "foundation", vec![]);
        let dependent = task(group_id, root, "integration", vec![dependency.task_id]);
        let events = vec![
            event(
                1,
                EventPayload::AgentGroupCreated {
                    identity: AgentGroupIdentity {
                        group_id,
                        root_session_id: root,
                        name: "g".into(),
                        created_at: chrono::Utc::now(),
                    },
                },
            ),
            event(
                2,
                EventPayload::GroupTaskCreated {
                    task: dependency.clone(),
                },
            ),
            event(
                3,
                EventPayload::GroupTaskCreated {
                    task: dependent.clone(),
                },
            ),
            event(
                4,
                EventPayload::GroupTaskStatusChanged {
                    task_id: dependency.task_id,
                    status: GroupTaskStatus::Cancelled,
                    actor: root,
                    assignee: None,
                    summary: None,
                    reason: Some("not needed".into()),
                    findings: vec![],
                    touched_files: vec![],
                },
            ),
        ];
        let state = GroupState::replay(&events);
        assert_eq!(
            state.dependency_failures(),
            vec![(dependent.task_id, vec![dependency.task_id])]
        );
        // Cancelling never makes the dependent ready.
        assert!(state.ready_tasks().is_empty());
        let required = state.required_unfinished();
        assert!(required.iter().any(|t| t.task_id == dependent.task_id));
    }

    #[test]
    fn conflicts_warn_only_between_active_tasks() {
        let group_id = Uuid::new_v4();
        let root = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let mut one = task(group_id, root, "one", vec![]);
        one.expected_paths = vec!["src/lib.rs".into()];
        let mut two = task(group_id, root, "two", vec![]);
        two.expected_paths = vec!["src/lib.rs".into()];
        let mut three = task(group_id, root, "three", vec![]);
        three.expected_paths = vec!["src/other.rs".into()];
        let events = vec![
            event(1, EventPayload::GroupTaskCreated { task: one.clone() }),
            event(2, EventPayload::GroupTaskCreated { task: two.clone() }),
            event(
                3,
                EventPayload::GroupTaskCreated {
                    task: three.clone(),
                },
            ),
            event(
                4,
                EventPayload::GroupTaskClaimed {
                    task_id: one.task_id,
                    agent_id: agent,
                },
            ),
        ];
        let state = GroupState::replay(&events);
        // Only one task is active; no advisory warning yet.
        assert!(state.conflicts().is_empty());
        let mut active_two = state.clone();
        active_two.apply(&event(
            5,
            EventPayload::GroupTaskClaimed {
                task_id: two.task_id,
                agent_id: Uuid::new_v4(),
            },
        ));
        assert_eq!(active_two.conflicts().len(), 1);
    }

    #[test]
    fn coordinator_claim_from_store_is_exactly_one_winner() {
        // The store-level concurrency race is covered in the store tests; this
        // asserts the coordinator surfaces the winner and refuses the loser.
        let store = EventStore::open_memory().unwrap();
        let workspace = Path::new("/tmp/group-coordinator");
        let root = store.create_session(workspace).unwrap();
        let coordinator = GroupCoordinator::new(store.clone(), root, "workspace".into()).unwrap();
        assert!(!coordinator.exists());
        let agent = Uuid::new_v4();
        coordinator.join(agent).unwrap();
        assert!(coordinator.exists());
        let first = coordinator
            .create_task(
                root,
                "parser".into(),
                "implement".into(),
                vec![],
                true,
                vec![],
            )
            .unwrap();
        assert_eq!(coordinator.counts().ready, 1);
        let claimed = coordinator.claim(first.task_id, agent).unwrap();
        assert_eq!(claimed.assignee, Some(agent));
        assert_eq!(claimed.status, GroupTaskStatus::Claimed);
        let error = coordinator
            .claim(first.task_id, Uuid::new_v4())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not claimable"), "{error}");
        // Resume: a fresh coordinator replays the same committed claim.
        let resumed = GroupCoordinator::new(store, root, "workspace".into()).unwrap();
        let recovered = resumed.snapshot();
        assert_eq!(
            recovered.task(first.task_id).unwrap().assignee,
            Some(agent),
            "claims survive a fresh coordinator"
        );
    }

    #[test]
    fn messages_deliver_once_and_survive_a_new_coordinator() {
        let store = EventStore::open_memory().unwrap();
        let workspace = Path::new("/tmp/group-mailbox");
        let root = store.create_session(workspace).unwrap();
        let child = store.create_session(workspace).unwrap();
        let coordinator = GroupCoordinator::new(store.clone(), root, "workspace".into()).unwrap();
        coordinator.join(child).unwrap();
        coordinator
            .send_message(
                child,
                GroupMessageTarget::Root,
                "found the parser bug".into(),
            )
            .unwrap();
        // Before any boundary, the message is pending and nobody has seen it.
        assert_eq!(coordinator.snapshot().undelivered(root, true).len(), 1);
        let delivered = coordinator.deliver_pending(root, true).unwrap();
        assert_eq!(delivered.len(), 1);
        assert!(coordinator.deliver_pending(root, true).unwrap().is_empty());
        // The delivery is durable in the recipient's own session.
        let child_events = store
            .events_of_kinds(root, &["group_message_delivered"])
            .unwrap();
        assert_eq!(child_events.len(), 1);
        assert_eq!(child_events[0].session_id, root);
        let resumed = GroupCoordinator::new(store, root, "workspace".into()).unwrap();
        assert!(resumed.deliver_pending(root, true).unwrap().is_empty());
        assert_eq!(resumed.inbox(root, true, 10).len(), 1);
        assert!(resumed.inbox(root, true, 10)[0].delivered);
    }

    #[test]
    fn completion_gate_requires_required_tasks_resolved() {
        let store = EventStore::open_memory().unwrap();
        let workspace = Path::new("/tmp/group-gate");
        let root = store.create_session(workspace).unwrap();
        let coordinator = GroupCoordinator::new(store, root, "workspace".into()).unwrap();
        assert!(
            coordinator.completion_blocker().is_none(),
            "no group, no gate"
        );
        coordinator.join(Uuid::new_v4()).unwrap();
        assert!(coordinator.completion_blocker().is_none());
        let required = coordinator
            .create_task(root, "parser".into(), String::new(), vec![], true, vec![])
            .unwrap();
        let optional = coordinator
            .create_task(root, "docs".into(), String::new(), vec![], false, vec![])
            .unwrap();
        let blocker = coordinator.completion_blocker().unwrap();
        assert!(blocker.contains("parser"), "{blocker}");
        assert!(!blocker.contains("docs"), "optional tasks never block");
        coordinator.cancel(required.task_id, None).unwrap();
        assert!(
            coordinator.completion_blocker().is_none(),
            "cancelling a required task resolves the gate"
        );
        let agent = coordinator
            .snapshot()
            .members()
            .iter()
            .next()
            .copied()
            .unwrap();
        coordinator.claim(optional.task_id, agent).unwrap();
        assert!(coordinator.completion_blocker().is_none());
    }
}
