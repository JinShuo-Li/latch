use super::graph::{AgentGraph, AgentNode};
use super::mailbox::NotificationMailbox;
use super::profile::{DelegationContext, delegation_brief};
use super::worker::{WorkerCommand, run_worker};
use crate::agent::Agent;
use crate::config::ContextConfig;
use crate::continuity::ContinuityEngine;
use crate::provider::ModelProvider;
use crate::store::{AgentSessionSpec, EventStore};
use crate::tools::ToolExecutor;
use anyhow::{Result, anyhow, bail};
use latch_protocol::{AgentIdentity, AgentReport, AgentStatus, EventPayload};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const DEFAULT_MAX_AGENT_DEPTH: u8 = 1;

#[derive(Debug, Clone)]
pub struct WorkerSettings {
    pub context: ContextConfig,
    pub context_window_tokens: usize,
    pub retry_budget: u32,
    pub stagnation_budget: u32,
    pub max_model_turns: Option<u32>,
    /// Effective root inference profile. Children inherit it at spawn so
    /// delegation never silently reverts to a config-default model/effort.
    pub profile: latch_protocol::InferenceProfile,
}

struct WorkerHandle {
    sender: mpsc::Sender<WorkerCommand>,
    lifetime: CancellationToken,
    join: JoinHandle<()>,
}

pub(super) struct SupervisorInner {
    pub root_session_id: Uuid,
    pub workspace: PathBuf,
    pub store: EventStore,
    /// Live root provider. Replaced when the root switches inference profile so
    /// subsequently spawned children inherit the new profile.
    pub provider: RwLock<Arc<dyn ModelProvider>>,
    pub tools: ToolExecutor,
    pub settings: RwLock<WorkerSettings>,
    pub graph: Mutex<AgentGraph>,
    workers: Mutex<HashMap<Uuid, WorkerHandle>>,
    pub notifications: NotificationMailbox,
    activity: Notify,
    shutdown: CancellationToken,
}

impl Drop for SupervisorInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let workers = self.workers.get_mut().unwrap_or_else(|e| e.into_inner());
        for handle in workers.values() {
            handle.lifetime.cancel();
            handle.join.abort();
        }
    }
}

#[derive(Clone)]
pub struct AgentSupervisor {
    inner: Arc<SupervisorInner>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub agent_id: Uuid,
    pub task_name: String,
    pub agent_type: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentWaitResult {
    pub agents: Vec<AgentSnapshot>,
    pub reports: Vec<AgentReport>,
}

impl AgentSupervisor {
    pub(crate) fn new(
        root_session_id: Uuid,
        workspace: PathBuf,
        store: EventStore,
        provider: Arc<dyn ModelProvider>,
        tools: ToolExecutor,
        settings: WorkerSettings,
    ) -> Result<Self> {
        let graph_events = store.agent_events(root_session_id)?;
        let mut graph = AgentGraph::replay(&graph_events);
        // A live turn cannot survive process loss. Reconcile it durably once,
        // but keep the child reusable for an explicit continue.
        for node in graph.snapshots() {
            if matches!(node.status, AgentStatus::Starting | AgentStatus::Running) {
                store.append(
                    node.identity.agent_id,
                    EventPayload::AgentInterrupted {
                        reason: "runtime ended before the child turn completed".into(),
                    },
                )?;
                if let Some(current) = graph.get_mut(node.identity.agent_id) {
                    current.status = AgentStatus::Interrupted;
                }
            }
        }
        let delivered: HashSet<Uuid> = store
            .events_of_kinds(root_session_id, &["agent_notification_delivered"])?
            .into_iter()
            .filter_map(|event| match event.payload {
                EventPayload::AgentNotificationDelivered { report } => Some(report.report_id),
                _ => None,
            })
            .collect();
        let notifications = NotificationMailbox::default();
        for node in graph.snapshots() {
            if let Some(report) = node.report
                && !delivered.contains(&report.report_id)
            {
                notifications.push(report);
            }
        }
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                root_session_id,
                workspace,
                store,
                provider: RwLock::new(provider),
                tools,
                settings: RwLock::new(settings),
                graph: Mutex::new(graph),
                workers: Mutex::new(HashMap::new()),
                notifications,
                activity: Notify::new(),
                shutdown: CancellationToken::new(),
            }),
        })
    }

    pub(crate) fn update_settings(&self, update: impl FnOnce(&mut WorkerSettings)) {
        let mut settings = self
            .inner
            .settings
            .write()
            .unwrap_or_else(|e| e.into_inner());
        update(&mut settings);
    }

    /// Replaces the live root provider used for future child sessions.
    pub(crate) fn set_provider(&self, provider: Arc<dyn ModelProvider>) {
        *self
            .inner
            .provider
            .write()
            .unwrap_or_else(|e| e.into_inner()) = provider;
    }

    pub async fn spawn_agent(
        &self,
        task_name: String,
        message: String,
        agent_type: Option<String>,
        context: DelegationContext,
    ) -> Result<AgentSnapshot> {
        let task_name = task_name.trim().to_owned();
        if task_name.is_empty() || message.trim().is_empty() {
            bail!("task_name and message must be non-empty");
        }
        let brief = delegation_brief(&task_name, &message, &self.inner.workspace, &context);
        let identity = {
            let mut graph = self.inner.graph.lock().unwrap_or_else(|e| e.into_inner());
            if graph.snapshots().iter().any(|node| {
                node.identity.task_name == task_name && node.status != AgentStatus::Closed
            }) {
                bail!("an open agent named `{task_name}` already exists");
            }
            let identity = self.inner.store.create_agent_session(
                &self.inner.workspace,
                AgentSessionSpec {
                    root_session_id: self.inner.root_session_id,
                    parent_session_id: self.inner.root_session_id,
                    task_name,
                    agent_type,
                    depth: 1,
                },
                brief.clone(),
            )?;
            graph.insert(AgentNode {
                identity: identity.clone(),
                status: AgentStatus::Starting,
                report: None,
            });
            identity
        };
        if let Err(error) = self.start_worker(identity.clone(), Some(brief)) {
            self.inner.store.append(
                identity.agent_id,
                EventPayload::AgentStatusChanged {
                    status: AgentStatus::Failed,
                    reason: Some(format!("worker failed to start: {error}")),
                },
            )?;
            self.set_status(identity.agent_id, AgentStatus::Failed, None);
            return Err(error);
        }
        Ok(snapshot(&identity, AgentStatus::Starting))
    }

    pub async fn send_message(&self, agent_id: Uuid, text: String) -> Result<()> {
        self.queue_command(agent_id, text, false).await
    }

    pub async fn continue_agent(&self, agent_id: Uuid, text: String) -> Result<()> {
        self.queue_command(agent_id, text, true).await
    }

    async fn queue_command(&self, agent_id: Uuid, text: String, trigger: bool) -> Result<()> {
        if text.trim().is_empty() {
            bail!("message must be non-empty");
        }
        let status = self.status(agent_id)?;
        if status == AgentStatus::Closed {
            bail!("agent {agent_id} is closed");
        }
        let kind = if trigger {
            latch_protocol::AgentMessageKind::FollowUp
        } else {
            latch_protocol::AgentMessageKind::Information
        };
        let message = latch_protocol::AgentMessage {
            message_id: Uuid::new_v4(),
            kind,
            text,
        };
        self.inner.store.append(
            agent_id,
            EventPayload::AgentMessageQueued {
                message: message.clone(),
            },
        )?;
        let sender = self.ensure_worker(agent_id)?;
        sender
            .send(if trigger {
                WorkerCommand::Continue(message)
            } else {
                WorkerCommand::Information(message)
            })
            .await
            .map_err(|_| anyhow!("agent {agent_id} worker stopped"))
    }

    pub fn list_agents(&self) -> Vec<AgentSnapshot> {
        self.inner
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshots()
            .into_iter()
            .map(|node| snapshot(&node.identity, node.status))
            .collect()
    }

    pub async fn interrupt_agent(&self, agent_id: Uuid) -> Result<()> {
        if self.status(agent_id)? == AgentStatus::Closed {
            bail!("agent {agent_id} is closed");
        }
        self.inner
            .store
            .append(agent_id, EventPayload::AgentInterruptRequested)?;
        if let Some(sender) = self.sender(agent_id) {
            sender
                .send(WorkerCommand::Interrupt)
                .await
                .map_err(|_| anyhow!("agent {agent_id} worker stopped"))?;
        } else {
            self.inner.store.append(
                agent_id,
                EventPayload::AgentInterrupted {
                    reason: "child interrupted while idle".into(),
                },
            )?;
            self.set_status(agent_id, AgentStatus::Interrupted, None);
        }
        Ok(())
    }

    pub async fn close_agent(&self, agent_id: Uuid) -> Result<()> {
        if self.status(agent_id)? == AgentStatus::Closed {
            return Ok(());
        }
        self.inner
            .store
            .append(agent_id, EventPayload::AgentCloseRequested)?;
        let handle = self
            .inner
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&agent_id);
        if let Some(handle) = handle {
            let _ = handle.sender.send(WorkerCommand::Close).await;
            handle.lifetime.cancel();
            let mut join = handle.join;
            if tokio::time::timeout(Duration::from_secs(5), &mut join)
                .await
                .is_err()
            {
                join.abort();
                let _ = join.await;
            }
            if self.status(agent_id)? != AgentStatus::Closed {
                self.inner
                    .store
                    .append(agent_id, EventPayload::AgentClosed)?;
                self.set_status(agent_id, AgentStatus::Closed, None);
            }
        } else {
            self.inner
                .store
                .append(agent_id, EventPayload::AgentClosed)?;
            self.set_status(agent_id, AgentStatus::Closed, None);
        }
        Ok(())
    }

    pub async fn close_all(&self) -> Result<()> {
        let ids = self
            .list_agents()
            .into_iter()
            .filter(|agent| agent.status != AgentStatus::Closed)
            .map(|agent| agent.agent_id)
            .collect::<Vec<_>>();
        for id in ids {
            self.close_agent(id).await?;
        }
        Ok(())
    }

    pub async fn wait_agents(
        &self,
        selected: &[Uuid],
        timeout: Duration,
    ) -> Result<AgentWaitResult> {
        for id in selected {
            self.status(*id)?;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let current = self.wait_result(selected);
            if !current.reports.is_empty()
                || current.agents.iter().any(|agent| terminal(agent.status))
                || timeout.is_zero()
            {
                return Ok(current);
            }
            let notified = self.inner.activity.notified();
            let current = self.wait_result(selected);
            if !current.reports.is_empty()
                || current.agents.iter().any(|agent| terminal(agent.status))
            {
                return Ok(current);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(current);
            }
            if tokio::time::timeout(deadline - now, notified)
                .await
                .is_err()
            {
                return Ok(self.wait_result(selected));
            }
        }
    }

    pub(crate) fn drain_notifications(&self) -> Vec<AgentReport> {
        self.inner.notifications.drain()
    }

    pub(crate) fn restore_notifications(&self, reports: Vec<AgentReport>) {
        self.inner.notifications.restore_front(reports);
    }

    fn status(&self, agent_id: Uuid) -> Result<AgentStatus> {
        self.inner
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .map(|node| node.status)
            .ok_or_else(|| anyhow!("unknown agent {agent_id}"))
    }

    fn wait_result(&self, selected: &[Uuid]) -> AgentWaitResult {
        AgentWaitResult {
            agents: self
                .list_agents()
                .into_iter()
                .filter(|agent| selected.is_empty() || selected.contains(&agent.agent_id))
                .collect(),
            reports: self.inner.notifications.selected(selected),
        }
    }

    fn sender(&self, agent_id: Uuid) -> Option<mpsc::Sender<WorkerCommand>> {
        self.inner
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&agent_id)
            .map(|handle| handle.sender.clone())
    }

    fn ensure_worker(&self, agent_id: Uuid) -> Result<mpsc::Sender<WorkerCommand>> {
        if let Some(sender) = self.sender(agent_id) {
            return Ok(sender);
        }
        let identity = self
            .inner
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .map(|node| node.identity.clone())
            .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
        self.start_worker(identity, None)
    }

    fn start_worker(
        &self,
        identity: AgentIdentity,
        initial: Option<String>,
    ) -> Result<mpsc::Sender<WorkerCommand>> {
        let mut agent = self.build_child(&identity)?;
        let restored_messages = if initial.is_none() {
            undelivered_messages(&self.inner.store, identity.agent_id)?
        } else {
            Vec::new()
        };
        let (sender, receiver) = mpsc::channel(32);
        let lifetime = self.inner.shutdown.child_token();
        let weak = Arc::downgrade(&self.inner);
        let worker_lifetime = lifetime.clone();
        let worker_id = identity.agent_id;
        let join = tokio::spawn(async move {
            run_worker(
                weak,
                identity,
                &mut agent,
                receiver,
                initial,
                restored_messages,
                worker_lifetime,
            )
            .await;
        });
        self.inner
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                worker_id,
                WorkerHandle {
                    sender: sender.clone(),
                    lifetime,
                    join,
                },
            );
        Ok(sender)
    }

    fn build_child(&self, identity: &AgentIdentity) -> Result<Agent> {
        let settings = self
            .inner
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let root_provider = self
            .inner
            .provider
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let provider = root_provider
            .for_session(identity.agent_id)
            .unwrap_or_else(|| root_provider.clone());
        let tools = self.inner.tools.for_child(identity.agent_id)?;
        let continuity = ContinuityEngine::for_model(
            self.inner.store.clone(),
            settings.context.clone(),
            provider.model(),
        );
        let mut agent = Agent::new_child(
            crate::agent::AgentRuntime {
                session_id: identity.agent_id,
                workspace: self.inner.workspace.clone(),
                mode: tools.mode(),
                store: self.inner.store.clone(),
                provider,
                tools,
                continuity,
                retry_budget: settings.retry_budget,
            },
            identity.depth,
        );
        agent.set_stagnation_budget(settings.stagnation_budget);
        agent.set_max_model_turns(settings.max_model_turns);
        agent.set_context_budget(settings.context.clone(), settings.context_window_tokens);
        agent.set_inherited_profile(settings.profile.clone());
        let events = self.inner.store.events(identity.agent_id)?;
        if let Some(state) = events.iter().rev().find_map(|event| match &event.payload {
            EventPayload::TaskStateUpdated { state } => Some(state.clone()),
            _ => None,
        }) {
            agent.restore_state(state);
        }
        agent.restore_evidence(
            events
                .iter()
                .filter_map(|event| match &event.payload {
                    EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                    _ => None,
                })
                .collect(),
        );
        agent.restore_failures()?;
        agent.restore_progress()?;
        Ok(agent)
    }

    pub(super) fn set_status(
        &self,
        agent_id: Uuid,
        status: AgentStatus,
        report: Option<AgentReport>,
    ) {
        if let Some(node) = self
            .inner
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(agent_id)
        {
            node.status = status;
            if report.is_some() {
                node.report = report;
            }
        }
        self.inner.activity.notify_waiters();
    }
}

pub(super) fn update_from_worker(
    inner: &Weak<SupervisorInner>,
    agent_id: Uuid,
    status: AgentStatus,
    report: Option<AgentReport>,
) {
    if let Some(inner) = inner.upgrade() {
        if let Some(node) = inner
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(agent_id)
        {
            node.status = status;
            if let Some(report) = report {
                inner.notifications.push(report.clone());
                node.report = Some(report);
            }
        }
        inner.activity.notify_waiters();
    }
}

fn terminal(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed
            | AgentStatus::Interrupted
            | AgentStatus::Failed
            | AgentStatus::Closed
    )
}

fn snapshot(identity: &AgentIdentity, status: AgentStatus) -> AgentSnapshot {
    AgentSnapshot {
        agent_id: identity.agent_id,
        task_name: identity.task_name.clone(),
        agent_type: identity.agent_type.clone(),
        status,
    }
}

pub(super) fn undelivered_messages(
    store: &EventStore,
    agent_id: Uuid,
) -> Result<Vec<latch_protocol::AgentMessage>> {
    let events = store.events_of_kinds(
        agent_id,
        &[
            "agent_spawned",
            "user_message",
            "agent_message_queued",
            "agent_message_received",
        ],
    )?;
    let mut brief = None;
    let mut delivered_turn = false;
    let mut delivered = HashSet::new();
    let mut queued = Vec::new();
    for event in &events {
        match &event.payload {
            EventPayload::AgentSpawned {
                delegation_brief, ..
            } => {
                brief = Some(delegation_brief.clone());
            }
            EventPayload::UserMessage { .. } => delivered_turn = true,
            EventPayload::AgentMessageReceived { message } => {
                delivered_turn = true;
                delivered.insert(message.message_id);
            }
            EventPayload::AgentMessageQueued { message } => queued.push(message.clone()),
            _ => {}
        }
    }
    let mut messages = queued
        .into_iter()
        .filter(|message| !delivered.contains(&message.message_id))
        .collect::<Vec<_>>();
    // A worker restart owes the child its opening turn. When the process ended
    // between spawn and the first model request, the delegation brief exists
    // only inside `AgentSpawned`; redeliver it as the first parent message so
    // an explicit continue still starts from the original task, never from a
    // follow-up alone.
    if !delivered_turn && let Some(brief) = brief {
        messages.insert(
            0,
            latch_protocol::AgentMessage {
                message_id: Uuid::new_v4(),
                kind: latch_protocol::AgentMessageKind::Information,
                text: brief,
            },
        );
    }
    Ok(messages)
}
