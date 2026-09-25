use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use latch_protocol::{
    AgentGroupIdentity, AgentIdentity, CompletionState, Event, EventPayload, GroupMessage,
    GroupMessageTarget, GroupTask, GroupTaskStatus, MemoryRecord, Mode,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

/// Event kinds that make up the durable agent-group domain. The SQLite
/// projection tables below are a pure cache over exactly these events and are
/// rebuilt from them at open; no group truth exists only in the projection.
pub const GROUP_EVENT_KINDS: &[&str] = &[
    "agent_group_created",
    "agent_group_member_joined",
    "group_task_created",
    "group_task_claimed",
    "group_task_status_changed",
    "group_task_released",
    "group_message_queued",
    "group_message_delivered",
];

#[must_use]
pub fn is_group_event(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::AgentGroupCreated { .. }
            | EventPayload::AgentGroupMemberJoined { .. }
            | EventPayload::GroupTaskCreated { .. }
            | EventPayload::GroupTaskClaimed { .. }
            | EventPayload::GroupTaskStatusChanged { .. }
            | EventPayload::GroupTaskReleased { .. }
            | EventPayload::GroupMessageQueued { .. }
            | EventPayload::GroupMessageDelivered { .. }
    )
}

/// Result of one atomic task claim attempt. Only `Claimed` means this caller
/// owns the task.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupClaimOutcome {
    Claimed(Box<Event>),
    Missing,
    NotPending {
        status: GroupTaskStatus,
        assignee: Option<Uuid>,
    },
    DependenciesIncomplete {
        incomplete: Vec<Uuid>,
    },
}

/// Result of one guarded task transition.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupTransitionOutcome {
    Updated(Box<Event>),
    Missing,
    NotOwner { assignee: Option<Uuid> },
    NotAllowed { status: GroupTaskStatus },
}

/// Description of one guarded transition. Kept as a plain struct so the store
/// performs the compare-and-set inside a single immediate transaction.
#[derive(Debug, Clone)]
pub struct GroupTaskTransition {
    pub allowed_from: Vec<GroupTaskStatus>,
    pub to: GroupTaskStatus,
    /// When true the current assignee must equal `actor`.
    pub require_owner: bool,
    /// New owner when the transition changes assignment (reassignment).
    pub assignee: Option<Uuid>,
    pub summary: Option<String>,
    pub reason: Option<String>,
    pub findings: Vec<String>,
    pub touched_files: Vec<String>,
}

#[derive(Clone)]
pub struct EventStore {
    connection: Arc<Mutex<Connection>>,
    /// Test-only fault injection: when set, every append fails so tests can
    /// assert that a persistence failure is not silently downgraded.
    #[cfg(test)]
    fail_appends: Arc<std::sync::atomic::AtomicBool>,
    /// Test-only accounting of how many event rows were deserialized, so
    /// stress tests can prove incremental paths do not rescan history.
    #[cfg(test)]
    scanned: Arc<std::sync::atomic::AtomicUsize>,
}

/// Lightweight row for session discovery. Transcript bodies are intentionally
/// absent; the picker can render its first frame without deserializing events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: Uuid,
    pub workspace: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub mode: Option<Mode>,
    pub model: Option<String>,
    /// Last durable reasoning effort selected for this session, if any.
    pub effort: Option<latch_protocol::ReasoningEffort>,
    pub event_count: u64,
    pub prompt_preview: Option<String>,
    pub completion: Option<CompletionState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPreviewLine {
    pub speaker: &'static str,
    pub text: String,
}

pub struct AgentSessionSpec {
    pub root_session_id: Uuid,
    pub parent_session_id: Uuid,
    pub task_name: String,
    pub agent_type: Option<String>,
    pub depth: u8,
}

impl EventStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::paths::ResolvedPaths::ensure_private_root(parent)?;
        }
        let connection = Connection::open(path)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            #[cfg(test)]
            fail_appends: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            scanned: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let store = Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory()?)),
            #[cfg(test)]
            fail_appends: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            scanned: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("event store lock poisoned"))
    }

    fn migrate(&self) -> Result<()> {
        self.conn()?.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, workspace TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1);
            CREATE TABLE IF NOT EXISTS events(session_id TEXT NOT NULL, sequence INTEGER NOT NULL, id TEXT NOT NULL UNIQUE, parent_id TEXT, timestamp TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(session_id, sequence));
            CREATE INDEX IF NOT EXISTS events_kind ON events(session_id, kind);
            CREATE INDEX IF NOT EXISTS events_kind_global ON events(kind, session_id, sequence);
            CREATE INDEX IF NOT EXISTS sessions_workspace_updated ON sessions(workspace, updated_at DESC);
            CREATE INDEX IF NOT EXISTS sessions_updated ON sessions(updated_at DESC);
            CREATE TABLE IF NOT EXISTS memory(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, content TEXT NOT NULL, originating_event TEXT NOT NULL, created_at TEXT NOT NULL, validity TEXT NOT NULL, json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS memory_session ON memory(session_id, created_at);
            CREATE VIRTUAL TABLE IF NOT EXISTS event_search USING fts5(session_id UNINDEXED, event_id UNINDEXED, text);
            CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, description TEXT NOT NULL, status TEXT NOT NULL, started_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS agent_groups(group_id TEXT PRIMARY KEY, root_session_id TEXT NOT NULL UNIQUE, json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS group_members(root_session_id TEXT NOT NULL, agent_id TEXT NOT NULL, PRIMARY KEY(root_session_id, agent_id));
            CREATE TABLE IF NOT EXISTS group_tasks(root_session_id TEXT NOT NULL, task_id TEXT PRIMARY KEY, group_id TEXT NOT NULL, status TEXT NOT NULL, assignee TEXT, required INTEGER NOT NULL DEFAULT 1, dependencies TEXT NOT NULL, updated_at TEXT NOT NULL, json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS group_tasks_root_status ON group_tasks(root_session_id, status);
            CREATE TABLE IF NOT EXISTS group_messages(root_session_id TEXT NOT NULL, message_id TEXT PRIMARY KEY, group_id TEXT NOT NULL, target_kind TEXT NOT NULL, target_agent TEXT, created_at TEXT NOT NULL, json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS group_messages_group ON group_messages(group_id, created_at);
            CREATE TABLE IF NOT EXISTS group_message_deliveries(message_id TEXT NOT NULL, agent_id TEXT NOT NULL, delivered_at TEXT NOT NULL, PRIMARY KEY(message_id, agent_id));")?;
        // The projection is derived state; rebuilding it at open guarantees it
        // can never drift from the durable group events.
        self.rebuild_group_projection()?;
        Ok(())
    }

    /// Rebuilds every agent-group projection table from the durable events.
    /// Events are applied in global insertion order, which is the exact order
    /// the durable log committed them.
    pub fn rebuild_group_projection(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM agent_groups", [])?;
        tx.execute("DELETE FROM group_members", [])?;
        tx.execute("DELETE FROM group_tasks", [])?;
        tx.execute("DELETE FROM group_messages", [])?;
        tx.execute("DELETE FROM group_message_deliveries", [])?;
        let placeholders = (0..GROUP_EVENT_KINDS.len())
            .map(|index| format!("?{}", index + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT sequence,id,parent_id,timestamp,session_id,payload FROM events \
             WHERE kind IN ({placeholders}) ORDER BY rowid"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(GROUP_EVENT_KINDS.len());
        for kind in GROUP_EVENT_KINDS {
            params.push(kind);
        }
        let events = {
            let mut statement = tx.prepare(&sql)?;
            statement
                .query_map(params.as_slice(), |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .map(|row| {
                    let (sequence, id, parent, timestamp, session, payload) = row?;
                    Ok(Event {
                        id: Uuid::parse_str(&id)?,
                        session_id: Uuid::parse_str(&session)?,
                        sequence,
                        timestamp: timestamp.parse()?,
                        parent_id: parent.map(|value| Uuid::parse_str(&value)).transpose()?,
                        payload: serde_json::from_str(&payload)?,
                    })
                })
                .collect::<Result<Vec<Event>>>()?
        };
        for event in &events {
            apply_group_event(&tx, event)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn create_session(&self, workspace: &Path) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        self.conn()?.execute(
            "INSERT INTO sessions(id,workspace,created_at,updated_at) VALUES(?1,?2,?3,?3)",
            params![id.to_string(), workspace.to_string_lossy(), now],
        )?;
        Ok(id)
    }

    /// Creates a child session and its first durable topology event in one
    /// transaction. A reported child can therefore never exist without its
    /// parent/root relationship, and a failed edge write cannot orphan a
    /// resumable session.
    pub fn create_agent_session(
        &self,
        workspace: &Path,
        spec: AgentSessionSpec,
        delegation_brief: String,
    ) -> Result<AgentIdentity> {
        let identity = AgentIdentity {
            agent_id: Uuid::new_v4(),
            root_session_id: spec.root_session_id,
            parent_session_id: spec.parent_session_id,
            task_name: spec.task_name,
            agent_type: spec.agent_type,
            depth: spec.depth,
        };
        let now = Utc::now();
        let payload = EventPayload::AgentSpawned {
            identity: identity.clone(),
            delegation_brief,
        };
        let event_id = Uuid::new_v4();
        let kind = event_kind(&payload)?;
        let json = serde_json::to_string(&payload)?;
        let searchable = searchable_text(&payload)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(id,workspace,created_at,updated_at) VALUES(?1,?2,?3,?3)",
            params![
                identity.agent_id.to_string(),
                workspace.to_string_lossy(),
                now.to_rfc3339()
            ],
        )?;
        tx.execute(
            "INSERT INTO events(session_id,sequence,id,parent_id,timestamp,kind,payload) VALUES(?1,1,?2,NULL,?3,?4,?5)",
            params![
                identity.agent_id.to_string(),
                event_id.to_string(),
                now.to_rfc3339(),
                kind,
                json
            ],
        )?;
        tx.execute(
            "INSERT INTO event_search(session_id,event_id,text) VALUES(?1,?2,?3)",
            params![
                identity.agent_id.to_string(),
                event_id.to_string(),
                searchable
            ],
        )?;
        tx.commit()?;
        Ok(identity)
    }

    pub fn latest_session(&self, workspace: Option<&Path>) -> Result<Option<Uuid>> {
        let conn = self.conn()?;
        let value: Option<String> = if let Some(path) = workspace {
            conn.query_row(
                "SELECT s.id FROM sessions s WHERE workspace=?1 AND NOT EXISTS (SELECT 1 FROM events e WHERE e.session_id=s.id AND e.kind='agent_spawned') ORDER BY updated_at DESC LIMIT 1",
                [path.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .optional()?
        } else {
            conn.query_row(
                "SELECT s.id FROM sessions s WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.session_id=s.id AND e.kind='agent_spawned') ORDER BY updated_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
        };
        value
            .map(|v| Uuid::parse_str(&v).context("invalid session UUID"))
            .transpose()
    }

    /// Lists resumable sessions newest-first using scalar indexed lookups only.
    /// It never loads or mutates a transcript.
    pub fn list_sessions(&self, workspace: Option<&Path>) -> Result<Vec<SessionSummary>> {
        let conn = self.conn()?;
        let sql = "SELECT s.id,s.workspace,s.created_at,s.updated_at,
            (SELECT payload FROM events e WHERE e.session_id=s.id AND e.kind='mode_changed' ORDER BY sequence DESC LIMIT 1),
            (SELECT payload FROM events e WHERE e.session_id=s.id AND e.kind='model_request_started' ORDER BY sequence DESC LIMIT 1),
            (SELECT COUNT(*) FROM events e WHERE e.session_id=s.id),
            (SELECT payload FROM events e WHERE e.session_id=s.id AND e.kind='user_message' ORDER BY sequence ASC LIMIT 1),
            (SELECT payload FROM events e WHERE e.session_id=s.id AND e.kind='completion_changed' ORDER BY sequence DESC LIMIT 1),
            (SELECT payload FROM events e WHERE e.session_id=s.id AND e.kind='inference_profile_changed' ORDER BY sequence DESC LIMIT 1)
            FROM sessions s WHERE (?1 IS NULL OR s.workspace=?1)
            AND NOT EXISTS (SELECT 1 FROM events child WHERE child.session_id=s.id AND child.kind='agent_spawned')
            ORDER BY s.updated_at DESC,s.id ASC";
        let workspace = workspace.map(|path| path.to_string_lossy().into_owned());
        let mut statement = conn.prepare(sql)?;
        let rows = statement.query_map([workspace.as_deref()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                workspace,
                created_at,
                updated_at,
                mode,
                model,
                event_count,
                prompt,
                completion,
                effort,
            ) = row?;
            Ok(SessionSummary {
                id: Uuid::parse_str(&id)?,
                workspace,
                created_at: created_at.parse()?,
                updated_at: updated_at.parse()?,
                mode: payload(mode)?.and_then(|payload| match payload {
                    EventPayload::ModeChanged { mode } => Some(mode),
                    _ => None,
                }),
                model: payload(model)?.and_then(|payload| match payload {
                    EventPayload::ModelRequestStarted { model, .. } => Some(model),
                    _ => None,
                }),
                effort: payload(effort)?.and_then(|payload| match payload {
                    EventPayload::InferenceProfileChanged { effort, .. } => Some(effort),
                    _ => None,
                }),
                event_count,
                prompt_preview: payload(prompt)?.and_then(|payload| match payload {
                    EventPayload::UserMessage { text, .. } => Some(compact_preview(&text, 140)),
                    _ => None,
                }),
                completion: payload(completion)?.and_then(|payload| match payload {
                    EventPayload::CompletionChanged { completion } => Some(completion),
                    _ => None,
                }),
            })
        })
        .collect()
    }

    /// Resolves an exact UUID or unambiguous UUID prefix across saved sessions.
    pub fn resolve_session(&self, selector: &str) -> Result<SessionSummary> {
        let selector = selector.trim().to_ascii_lowercase();
        if selector.is_empty() {
            bail!("session selector cannot be empty");
        }
        let matches: Vec<_> = self
            .list_sessions(None)?
            .into_iter()
            .filter(|session| session.id.to_string().starts_with(&selector))
            .collect();
        match matches.as_slice() {
            [] => bail!("no session matches `{selector}`"),
            [session] => Ok(session.clone()),
            many => bail!(
                "session prefix `{selector}` is ambiguous; matches: {}",
                many.iter()
                    .map(|session| session.id.to_string()[..8].to_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Lazily loads a bounded user-visible transcript preview. Reasoning and
    /// internal events are excluded by the SQL predicate and payload match.
    pub fn session_preview(
        &self,
        session_id: Uuid,
        limit: usize,
    ) -> Result<Vec<SessionPreviewLine>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare("SELECT payload FROM events WHERE session_id=?1 AND kind IN ('user_message','assistant_message_completed') ORDER BY sequence DESC LIMIT ?2")?;
        let payloads: Vec<String> = statement
            .query_map(params![session_id.to_string(), limit], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut lines: Vec<_> = payloads
            .into_iter()
            .filter_map(|json| serde_json::from_str::<EventPayload>(&json).ok())
            .filter_map(|payload| match payload {
                EventPayload::UserMessage { text, .. } => Some(SessionPreviewLine {
                    speaker: "You",
                    text: compact_preview(&text, 220),
                }),
                EventPayload::AssistantMessageCompleted { text, .. } if !text.trim().is_empty() => {
                    Some(SessionPreviewLine {
                        speaker: "Latch",
                        text: compact_preview(&text, 220),
                    })
                }
                _ => None,
            })
            .collect();
        lines.reverse();
        Ok(lines)
    }

    #[cfg(test)]
    pub(crate) fn reset_scanned_events(&self) {
        self.scanned.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn scanned_events(&self) -> usize {
        self.scanned.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test-only fault injection: every append fails until disabled.
    #[cfg(test)]
    pub(crate) fn fail_appends(&self, fail: bool) {
        self.fail_appends
            .store(fail, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn append(&self, session_id: Uuid, payload: EventPayload) -> Result<Event> {
        #[cfg(test)]
        if self.fail_appends.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("injected event-store append failure");
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let event = append_in_tx(&tx, session_id, payload)?;
        tx.commit()?;
        Ok(event)
    }

    /// Creates (or returns) the root-scoped agent group. The check and the
    /// `AgentGroupCreated` append commit in one immediate transaction, so
    /// concurrent first uses cannot create two groups for one root.
    pub fn create_agent_group(
        &self,
        root_session_id: Uuid,
        name: &str,
    ) -> Result<AgentGroupIdentity> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT json FROM agent_groups WHERE root_session_id=?1",
                [root_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(json) = existing {
            let identity: AgentGroupIdentity = serde_json::from_str(&json)?;
            tx.commit()?;
            return Ok(identity);
        }
        let identity = AgentGroupIdentity {
            group_id: Uuid::new_v4(),
            root_session_id,
            name: name.to_owned(),
            created_at: Utc::now(),
        };
        append_in_tx(
            &tx,
            root_session_id,
            EventPayload::AgentGroupCreated {
                identity: identity.clone(),
            },
        )?;
        tx.commit()?;
        Ok(identity)
    }

    pub fn agent_group_identity(
        &self,
        root_session_id: Uuid,
    ) -> Result<Option<AgentGroupIdentity>> {
        let json: Option<String> = self
            .conn()?
            .query_row(
                "SELECT json FROM agent_groups WHERE root_session_id=?1",
                [root_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("invalid agent group identity"))
            .transpose()
    }

    /// Durable, idempotent membership join. Returns the join event only when
    /// the agent was not already a member.
    pub fn join_agent_group(
        &self,
        root_session_id: Uuid,
        group_id: Uuid,
        agent_id: Uuid,
    ) -> Result<Option<Event>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group: Option<String> = tx
            .query_row(
                "SELECT group_id FROM agent_groups WHERE root_session_id=?1",
                [root_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match group {
            Some(existing) if existing == group_id.to_string() => {}
            _ => bail!("agent group {group_id} does not belong to root {root_session_id}"),
        }
        let member: Option<String> = tx
            .query_row(
                "SELECT agent_id FROM group_members WHERE root_session_id=?1 AND agent_id=?2",
                params![root_session_id.to_string(), agent_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if member.is_some() {
            tx.commit()?;
            return Ok(None);
        }
        let event = append_in_tx(
            &tx,
            root_session_id,
            EventPayload::AgentGroupMemberJoined { group_id, agent_id },
        )?;
        tx.commit()?;
        Ok(Some(event))
    }

    /// Creates a group task. Dependencies must already exist and form a DAG;
    /// the full-graph validation happens in the kernel group reducer, which
    /// reads the same projection this append updates.
    pub fn create_group_task(
        &self,
        root_session_id: Uuid,
        group_id: Uuid,
        task: GroupTask,
    ) -> Result<GroupTask> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group: Option<String> = tx
            .query_row(
                "SELECT group_id FROM agent_groups WHERE root_session_id=?1",
                [root_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match group {
            Some(existing) if existing == group_id.to_string() => {}
            _ => bail!("agent group {group_id} does not belong to root {root_session_id}"),
        }
        if tx
            .query_row(
                "SELECT task_id FROM group_tasks WHERE task_id=?1",
                [task.task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some()
        {
            bail!("group task {} already exists", task.task_id);
        }
        append_in_tx(
            &tx,
            root_session_id,
            EventPayload::GroupTaskCreated { task: task.clone() },
        )?;
        tx.commit()?;
        Ok(task)
    }

    /// Atomic claim: read the projection and append `GroupTaskClaimed` in one
    /// immediate transaction. Exactly one concurrent claimer can win.
    pub fn claim_group_task(
        &self,
        root_session_id: Uuid,
        group_id: Uuid,
        task_id: Uuid,
        agent_id: Uuid,
    ) -> Result<GroupClaimOutcome> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, Option<String>, String)> = tx
            .query_row(
                "SELECT group_id,status,assignee,dependencies FROM group_tasks \
                 WHERE task_id=?1 AND root_session_id=?2",
                params![task_id.to_string(), root_session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((task_group, status, assignee, dependencies)) = row else {
            return Ok(GroupClaimOutcome::Missing);
        };
        if task_group != group_id.to_string() {
            return Ok(GroupClaimOutcome::Missing);
        }
        let status = parse_task_status(&status)?;
        let assignee = assignee
            .map(|value| Uuid::parse_str(&value))
            .transpose()
            .context("invalid group task assignee")?;
        if status != GroupTaskStatus::Pending || assignee.is_some() {
            return Ok(GroupClaimOutcome::NotPending { status, assignee });
        }
        let dependencies: Vec<Uuid> = serde_json::from_str(&dependencies)?;
        let mut incomplete = Vec::new();
        for dependency in dependencies {
            let dep_status: Option<String> = tx
                .query_row(
                    "SELECT status FROM group_tasks WHERE task_id=?1",
                    [dependency.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if !matches!(dep_status.as_deref(), Some("completed")) {
                incomplete.push(dependency);
            }
        }
        if !incomplete.is_empty() {
            return Ok(GroupClaimOutcome::DependenciesIncomplete { incomplete });
        }
        let event = append_in_tx(
            &tx,
            root_session_id,
            EventPayload::GroupTaskClaimed { task_id, agent_id },
        )?;
        tx.commit()?;
        Ok(GroupClaimOutcome::Claimed(Box::new(event)))
    }

    /// Guarded task transition. Ownership and allowed source states are
    /// compared inside the same immediate transaction that appends the event.
    pub fn transition_group_task(
        &self,
        root_session_id: Uuid,
        task_id: Uuid,
        actor: Uuid,
        transition: GroupTaskTransition,
    ) -> Result<GroupTransitionOutcome> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT status,assignee FROM group_tasks WHERE task_id=?1 AND root_session_id=?2",
                params![task_id.to_string(), root_session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((status, assignee)) = row else {
            return Ok(GroupTransitionOutcome::Missing);
        };
        let status = parse_task_status(&status)?;
        let assignee = assignee
            .map(|value| Uuid::parse_str(&value))
            .transpose()
            .context("invalid group task assignee")?;
        if !transition.allowed_from.contains(&status) {
            return Ok(GroupTransitionOutcome::NotAllowed { status });
        }
        if transition.require_owner && assignee != Some(actor) {
            return Ok(GroupTransitionOutcome::NotOwner { assignee });
        }
        let event = append_in_tx(
            &tx,
            root_session_id,
            EventPayload::GroupTaskStatusChanged {
                task_id,
                status: transition.to,
                actor,
                assignee: transition.assignee,
                summary: transition.summary,
                reason: transition.reason,
                findings: transition.findings,
                touched_files: transition.touched_files,
            },
        )?;
        tx.commit()?;
        Ok(GroupTransitionOutcome::Updated(Box::new(event)))
    }

    /// Explicit release by the current assignee. The transaction that appends
    /// `GroupTaskReleased` also verifies ownership, so a release can never race
    /// a completed task back into the pool.
    pub fn release_group_task(
        &self,
        root_session_id: Uuid,
        task_id: Uuid,
        agent_id: Uuid,
    ) -> Result<GroupTransitionOutcome> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT status,assignee FROM group_tasks WHERE task_id=?1 AND root_session_id=?2",
                params![task_id.to_string(), root_session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((status, assignee)) = row else {
            return Ok(GroupTransitionOutcome::Missing);
        };
        let status = parse_task_status(&status)?;
        let assignee = assignee
            .map(|value| Uuid::parse_str(&value))
            .transpose()
            .context("invalid group task assignee")?;
        if !status.is_active() || assignee != Some(agent_id) {
            return if status.is_active() || status == GroupTaskStatus::Pending {
                Ok(GroupTransitionOutcome::NotOwner { assignee })
            } else {
                Ok(GroupTransitionOutcome::NotAllowed { status })
            };
        }
        let event = append_in_tx(
            &tx,
            root_session_id,
            EventPayload::GroupTaskReleased { task_id, agent_id },
        )?;
        tx.commit()?;
        Ok(GroupTransitionOutcome::Updated(Box::new(event)))
    }

    /// Messages addressed to one recipient that have no delivery marker yet, in
    /// durable FIFO order.
    pub fn group_pending_messages(
        &self,
        group_id: Uuid,
        recipient: Uuid,
        is_root: bool,
    ) -> Result<Vec<GroupMessage>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT m.json FROM group_messages m
             WHERE m.group_id=?1
               AND NOT EXISTS (SELECT 1 FROM group_message_deliveries d
                               WHERE d.message_id=m.message_id AND d.agent_id=?2)
               AND (m.target_kind='group'
                    OR (m.target_kind='agent' AND m.target_agent=?2)
                    OR (m.target_kind='root' AND ?3=1))
             ORDER BY m.rowid",
        )?;
        let rows = statement.query_map(
            params![
                group_id.to_string(),
                recipient.to_string(),
                i64::from(is_root)
            ],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let json = row?;
            serde_json::from_str(&json).context("invalid durable group message")
        })
        .collect()
    }

    /// Atomically delivers one queued message to one recipient by appending
    /// `GroupMessageDelivered` to the recipient's own session and recording the
    /// delivery marker in the same transaction. Returns `None` when the
    /// delivery marker already exists (exactly-once).
    pub fn deliver_group_message(
        &self,
        recipient: Uuid,
        message: GroupMessage,
    ) -> Result<Option<Event>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT agent_id FROM group_message_deliveries WHERE message_id=?1 AND agent_id=?2",
                params![message.message_id.to_string(), recipient.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            tx.commit()?;
            return Ok(None);
        }
        let event = append_in_tx(
            &tx,
            recipient,
            EventPayload::GroupMessageDelivered {
                message: message.clone(),
            },
        )?;
        tx.commit()?;
        Ok(Some(event))
    }

    /// All durable group events, in global commit order. Used by the kernel
    /// group reducer so live and resumed state come from the same stream.
    pub fn group_events(&self) -> Result<Vec<Event>> {
        let placeholders = (0..GROUP_EVENT_KINDS.len())
            .map(|index| format!("?{}", index + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT sequence,id,parent_id,timestamp,session_id,payload FROM events \
             WHERE kind IN ({placeholders}) ORDER BY rowid"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(GROUP_EVENT_KINDS.len());
        for kind in GROUP_EVENT_KINDS {
            params.push(kind);
        }
        let conn = self.conn()?;
        let mut statement = conn.prepare(&sql)?;
        statement
            .query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .map(|row| {
                let (sequence, id, parent, timestamp, session, payload) = row?;
                Ok(Event {
                    id: Uuid::parse_str(&id)?,
                    session_id: Uuid::parse_str(&session)?,
                    sequence,
                    timestamp: timestamp.parse()?,
                    parent_id: parent.map(|value| Uuid::parse_str(&value)).transpose()?,
                    payload: serde_json::from_str(&payload)?,
                })
            })
            .collect()
    }

    /// Number of durable events for a session. Used to avoid materializing the
    /// full event log when nothing new was appended.
    pub fn event_count(&self, session_id: Uuid) -> Result<usize> {
        let count: i64 = self.conn()?.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn events(&self, session_id: Uuid) -> Result<Vec<Event>> {
        self.events_query(
            session_id,
            "SELECT sequence,id,parent_id,timestamp,payload FROM events WHERE session_id=?1 ORDER BY sequence",
            &[&session_id.to_string()],
        )
    }

    /// Reads only topology/lifecycle events for children belonging to a root.
    /// The event log remains authoritative; this is an indexed replay query,
    /// not a second graph database.
    pub fn agent_events(&self, root_session_id: Uuid) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT e.session_id,e.sequence,e.id,e.parent_id,e.timestamp,e.payload
             FROM events e
             WHERE e.kind IN ('agent_spawned','agent_message_queued','agent_message_received',
                'agent_status_changed','agent_report_created','agent_interrupt_requested',
                'agent_interrupted','agent_close_requested','agent_closed')
             ORDER BY e.session_id,e.sequence",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut events = Vec::new();
        let mut children = std::collections::HashSet::new();
        for row in rows {
            let (session, sequence, id, parent, timestamp, payload) = row?;
            let payload: EventPayload = serde_json::from_str(&payload)?;
            if let EventPayload::AgentSpawned { identity, .. } = &payload
                && identity.root_session_id == root_session_id
            {
                children.insert(identity.agent_id);
            }
            let session_id = Uuid::parse_str(&session)?;
            events.push(Event {
                id: Uuid::parse_str(&id)?,
                session_id,
                sequence,
                timestamp: timestamp.parse()?,
                parent_id: parent.map(|value| Uuid::parse_str(&value)).transpose()?,
                payload,
            });
        }
        Ok(events
            .into_iter()
            .filter(|event| children.contains(&event.session_id))
            .collect())
    }

    /// Highest sequence currently stored for the session (0 when empty). This is
    /// the same cursor as a 1-based event count but never deserializes history.
    pub fn last_sequence(&self, session_id: Uuid) -> Result<u64> {
        let last: u64 = self.conn()?.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM events WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(last)
    }

    /// Events strictly after `after_sequence`, in sequence order. Watermarks use
    /// this so per-turn supervision never reloads or re-deserializes history it
    /// has already consumed.
    pub fn events_after(&self, session_id: Uuid, after_sequence: u64) -> Result<Vec<Event>> {
        self.events_query(
            session_id,
            "SELECT sequence,id,parent_id,timestamp,payload FROM events \
             WHERE session_id=?1 AND sequence > ?2 ORDER BY sequence",
            &[&session_id.to_string(), &after_sequence],
        )
    }

    /// Workspace-wide mutation/lifecycle stream in durable insertion order.
    /// Root and child sessions share a workspace, so a per-session sequence
    /// cannot be used as a verification generation.
    pub fn workspace_mutation_events_after(
        &self,
        workspace: &Path,
        after_rowid: u64,
    ) -> Result<Vec<(u64, Event)>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT e.rowid,e.session_id,e.sequence,e.id,e.parent_id,e.timestamp,e.payload
             FROM events e JOIN sessions s ON s.id=e.session_id
             WHERE s.workspace=?1 AND e.rowid>?2
               AND e.kind IN ('workspace_mutation_possible','shell_mutation_observed',
                  'change_reverted','external_file_change_detected','file_changed',
                  'process_started','process_exited')
             ORDER BY e.rowid",
        )?;
        let rows =
            statement.query_map(params![workspace.to_string_lossy(), after_rowid], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?;
        rows.map(|row| {
            let (rowid, session, sequence, id, parent, timestamp, payload) = row?;
            Ok((
                rowid,
                Event {
                    id: Uuid::parse_str(&id)?,
                    session_id: Uuid::parse_str(&session)?,
                    sequence,
                    timestamp: timestamp.parse()?,
                    parent_id: parent.map(|value| Uuid::parse_str(&value)).transpose()?,
                    payload: serde_json::from_str(&payload)?,
                },
            ))
        })
        .collect()
    }

    /// The newest `limit` events in sequence order. Bounds per-turn recent
    /// working-memory loads by the working set instead of history size.
    pub fn events_tail(&self, session_id: Uuid, limit: usize) -> Result<Vec<Event>> {
        self.events_query(
            session_id,
            "SELECT sequence,id,parent_id,timestamp,payload FROM (                 SELECT sequence,id,parent_id,timestamp,payload FROM events                  WHERE session_id=?1 ORDER BY sequence DESC LIMIT ?2             ) ORDER BY sequence",
            &[&session_id.to_string(), &(limit as i64)],
        )
    }

    /// Events strictly after `after_sequence` and strictly before
    /// `before_sequence`, in sequence order. Bounded ranges keep incremental
    /// indexing to the delta.
    pub fn events_between(
        &self,
        session_id: Uuid,
        after_sequence: u64,
        before_sequence: u64,
    ) -> Result<Vec<Event>> {
        self.events_query(
            session_id,
            "SELECT sequence,id,parent_id,timestamp,payload FROM events              WHERE session_id=?1 AND sequence > ?2 AND sequence < ?3 ORDER BY sequence",
            &[
                &session_id.to_string(),
                &after_sequence,
                &before_sequence,
            ],
        )
    }

    /// Events strictly before `before_sequence`, in sequence order. Used to load
    /// the rolled-over history an epoch no longer keeps in its recent tail.
    pub fn events_before(&self, session_id: Uuid, before_sequence: u64) -> Result<Vec<Event>> {
        self.events_query(
            session_id,
            "SELECT sequence,id,parent_id,timestamp,payload FROM events \
             WHERE session_id=?1 AND sequence < ?2 ORDER BY sequence",
            &[&session_id.to_string(), &before_sequence],
        )
    }

    /// Events of the given kinds, in sequence order. Uses the `events_kind`
    /// index instead of deserializing the whole history for a small, targeted
    /// replay (approvals, change ownership, failed tool lineages).
    pub fn events_of_kinds(&self, session_id: Uuid, kinds: &[&str]) -> Result<Vec<Event>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (0..kinds.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT sequence,id,parent_id,timestamp,payload FROM events \
             WHERE session_id=?1 AND kind IN ({placeholders}) ORDER BY sequence"
        );
        let session = session_id.to_string();
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(kinds.len() + 1);
        params.push(&session);
        for kind in kinds {
            params.push(kind);
        }
        self.events_query(session_id, &sql, &params)
    }

    /// Number of events of one kind after a sequence. Derived counters (such
    /// as cache-epoch turns) stay cheap and deterministic on resume.
    pub fn count_events_after_of_kind(
        &self,
        session_id: Uuid,
        kind: &str,
        after_sequence: u64,
    ) -> Result<u64> {
        let count: i64 = self.conn()?.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id=?1 AND kind=?2 AND sequence > ?3",
            rusqlite::params![session_id.to_string(), kind, after_sequence],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Newest event among the given kinds, if any. Boundary lookups (compact and
    /// epoch markers) use this instead of scanning the log.
    pub fn latest_event_of_kinds(&self, session_id: Uuid, kinds: &[&str]) -> Result<Option<Event>> {
        if kinds.is_empty() {
            return Ok(None);
        }
        let placeholders = (0..kinds.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT sequence,id,parent_id,timestamp,payload FROM events \
             WHERE session_id=?1 AND kind IN ({placeholders}) ORDER BY sequence DESC LIMIT 1"
        );
        let session = session_id.to_string();
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(kinds.len() + 1);
        params.push(&session);
        for kind in kinds {
            params.push(kind);
        }
        Ok(self
            .events_query(session_id, &sql, &params)?
            .into_iter()
            .next())
    }

    fn events_query(
        &self,
        session_id: Uuid,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let events: Vec<Event> = rows
            .map(|r| {
                let (sequence, id, parent, timestamp, payload) = r?;
                Ok(Event {
                    id: Uuid::parse_str(&id)?,
                    session_id,
                    sequence,
                    timestamp: timestamp.parse()?,
                    parent_id: parent.map(|v| Uuid::parse_str(&v)).transpose()?,
                    payload: serde_json::from_str(&payload)?,
                })
            })
            .collect::<Result<_>>()?;
        #[cfg(test)]
        self.scanned
            .fetch_add(events.len(), std::sync::atomic::Ordering::Relaxed);
        Ok(events)
    }

    pub fn search_events(&self, session_id: Uuid, query: &str, limit: usize) -> Result<Vec<Event>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let query = fts_query(query);
        if query.is_empty() {
            return Ok(vec![]);
        }
        // Fetch only the matched rows. The FTS selection is ordered by
        // newest insertion first so later corrections remain recallable even
        // when a term appears many times; the outer query presents selected
        // events in chronological order. Bookkeeping kinds are excluded after
        // selection.
        let sql = "SELECT sequence,id,parent_id,timestamp,payload FROM events \
                   WHERE session_id=?1 AND id IN (\
                       SELECT event_id FROM event_search \
                       WHERE session_id=?1 AND event_search MATCH ?2 ORDER BY rowid DESC LIMIT ?3\
                   ) ORDER BY sequence";
        let session = session_id.to_string();
        let events = self.events_query(session_id, sql, &[&session, &query, &(limit as i64)])?;
        Ok(events
            .into_iter()
            .filter(|event| {
                !matches!(
                    &event.payload,
                    EventPayload::ContextMemoryRecalled { .. }
                        | EventPayload::ContextMaterialized { .. }
                        | EventPayload::KernelContext { .. }
                        | EventPayload::AgentSpawned { .. }
                        | EventPayload::AgentMessageQueued { .. }
                        | EventPayload::AgentStatusChanged { .. }
                        | EventPayload::AgentReportCreated { .. }
                        | EventPayload::AgentInterruptRequested
                        | EventPayload::AgentInterrupted { .. }
                        | EventPayload::AgentCloseRequested
                        | EventPayload::AgentClosed
                        | EventPayload::AgentGroupCreated { .. }
                        | EventPayload::AgentGroupMemberJoined { .. }
                        | EventPayload::GroupTaskCreated { .. }
                        | EventPayload::GroupTaskClaimed { .. }
                        | EventPayload::GroupTaskStatusChanged { .. }
                        | EventPayload::GroupTaskReleased { .. }
                        | EventPayload::GroupMessageQueued { .. }
                        | EventPayload::GroupMessageDelivered { .. }
                )
            })
            .collect())
    }

    pub fn add_memory(&self, record: &MemoryRecord) -> Result<()> {
        self.conn()?.execute("INSERT INTO memory(id,session_id,kind,content,originating_event,created_at,validity,json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![record.id.to_string(),record.session_id.to_string(),format!("{:?}",record.kind),record.content,record.originating_event.to_string(),record.created_at.to_rfc3339(),format!("{:?}",record.validity),serde_json::to_string(record)?])?;
        Ok(())
    }

    pub fn memories(&self, session_id: Uuid) -> Result<Vec<MemoryRecord>> {
        let conn = self.conn()?;
        // Deterministic order even for identical timestamps: insertion order is
        // the tie-breaker, so canonical rendering cannot churn between runs.
        let mut stmt =
            conn.prepare("SELECT json FROM memory WHERE session_id=?1 ORDER BY created_at, rowid")?;
        stmt.query_map([session_id.to_string()], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }

    pub fn set_memory_validity(&self, id: Uuid, validity: latch_protocol::Validity) -> Result<()> {
        let conn = self.conn()?;
        let json: String = conn.query_row(
            "SELECT json FROM memory WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )?;
        let mut record: MemoryRecord = serde_json::from_str(&json)?;
        record.validity = validity.clone();
        conn.execute(
            "UPDATE memory SET validity=?2,json=?3 WHERE id=?1",
            params![
                id.to_string(),
                format!("{validity:?}"),
                serde_json::to_string(&record)?
            ],
        )?;
        Ok(())
    }

    pub fn begin_operation(&self, session_id: Uuid, description: &str) -> Result<Uuid> {
        let id = Uuid::new_v4();
        self.conn()?.execute(
            "INSERT INTO operations VALUES(?1,?2,?3,'running',?4)",
            params![
                id.to_string(),
                session_id.to_string(),
                description,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(id)
    }
    pub fn finish_operation(&self, id: Uuid) -> Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE operations SET status='complete' WHERE id=?1",
            [id.to_string()],
        )?;
        if changed == 0 {
            bail!("unknown operation {id}")
        }
        Ok(())
    }
    pub fn mark_operation_reported(&self, id: Uuid) -> Result<()> {
        let changed = self.conn()?.execute(
            "UPDATE operations SET status='interrupted' WHERE id=?1 AND status='running'",
            [id.to_string()],
        )?;
        if changed == 0 {
            bail!("operation {id} was not running")
        }
        Ok(())
    }
    pub fn interrupted_operations(&self, session_id: Uuid) -> Result<Vec<(Uuid, String)>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT id,description FROM operations WHERE session_id=?1 AND status='running'",
        )?;
        statement
            .query_map([session_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (id, description) = row?;
                Ok((Uuid::parse_str(&id)?, description))
            })
            .collect()
    }
}

/// The snake_case tag stored in the `kind` column. A payload without a tag is
/// a durable-state corruption, not a value to paper over with `"unknown"`.
fn event_kind(p: &EventPayload) -> Result<String> {
    serde_json::to_value(p)?
        .get("type")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("event payload has no type tag"))
}

/// Appends one event to a session inside an existing transaction and keeps the
/// group projection exactly in step with the durable log.
fn append_in_tx(tx: &Connection, session_id: Uuid, payload: EventPayload) -> Result<Event> {
    let sequence: u64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE session_id=?1",
        [session_id.to_string()],
        |r| r.get(0),
    )?;
    let parent_id: Option<String> = tx
        .query_row(
            "SELECT id FROM events WHERE session_id=?1 ORDER BY sequence DESC LIMIT 1",
            [session_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let event = Event {
        id: Uuid::new_v4(),
        session_id,
        sequence,
        timestamp: Utc::now(),
        parent_id: parent_id.map(|s| Uuid::parse_str(&s)).transpose()?,
        payload,
    };
    let kind = event_kind(&event.payload)?;
    let json = serde_json::to_string(&event.payload)?;
    let searchable = searchable_text(&event.payload)?;
    tx.execute("INSERT INTO events(session_id,sequence,id,parent_id,timestamp,kind,payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![session_id.to_string(),sequence,event.id.to_string(),event.parent_id.map(|v|v.to_string()),event.timestamp.to_rfc3339(),kind,json])?;
    if is_group_event(&event.payload) {
        apply_group_event(tx, &event)?;
    }
    tx.execute(
        "INSERT INTO event_search(session_id,event_id,text) VALUES(?1,?2,?3)",
        params![session_id.to_string(), event.id.to_string(), searchable],
    )?;
    tx.execute(
        "UPDATE sessions SET updated_at=?2 WHERE id=?1",
        params![session_id.to_string(), event.timestamp.to_rfc3339()],
    )?;
    Ok(event)
}

/// Applies one durable group event to the rebuildable projection tables. Any
/// failure aborts the surrounding transaction, so the projection can never
/// commit ahead of (or behind) the event that explains it.
fn apply_group_event(conn: &Connection, event: &Event) -> Result<()> {
    match &event.payload {
        EventPayload::AgentGroupCreated { identity } => {
            conn.execute(
                "INSERT INTO agent_groups(group_id,root_session_id,json) VALUES(?1,?2,?3)
                 ON CONFLICT(root_session_id) DO UPDATE SET group_id=excluded.group_id, json=excluded.json",
                params![
                    identity.group_id.to_string(),
                    identity.root_session_id.to_string(),
                    serde_json::to_string(identity)?
                ],
            )?;
        }
        EventPayload::AgentGroupMemberJoined { group_id, agent_id } => {
            let root = group_root(conn, *group_id)?;
            conn.execute(
                "INSERT OR IGNORE INTO group_members(root_session_id,agent_id) VALUES(?1,?2)",
                params![root.to_string(), agent_id.to_string()],
            )?;
        }
        EventPayload::GroupTaskCreated { task } => {
            write_group_task(conn, task)?;
        }
        EventPayload::GroupTaskClaimed { task_id, agent_id } => {
            let mut task = load_group_task(conn, *task_id)?
                .ok_or_else(|| anyhow::anyhow!("group task {task_id} missing before claim"))?;
            task.status = GroupTaskStatus::Claimed;
            task.assignee = Some(*agent_id);
            task.updated_at = event.timestamp;
            write_group_task(conn, &task)?;
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
            let mut task = load_group_task(conn, *task_id)?
                .ok_or_else(|| anyhow::anyhow!("group task {task_id} missing before transition"))?;
            task.status = *status;
            if let Some(assignee) = assignee {
                task.assignee = Some(*assignee);
            }
            if status == &GroupTaskStatus::Pending {
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
            task.updated_at = event.timestamp;
            write_group_task(conn, &task)?;
        }
        EventPayload::GroupTaskReleased { task_id, .. } => {
            let mut task = load_group_task(conn, *task_id)?
                .ok_or_else(|| anyhow::anyhow!("group task {task_id} missing before release"))?;
            task.status = GroupTaskStatus::Pending;
            task.assignee = None;
            task.updated_at = event.timestamp;
            write_group_task(conn, &task)?;
        }
        EventPayload::GroupMessageQueued { message } => {
            insert_group_message(conn, message)?;
        }
        EventPayload::GroupMessageDelivered { message } => {
            insert_group_message(conn, message)?;
            conn.execute(
                "INSERT OR IGNORE INTO group_message_deliveries(message_id,agent_id,delivered_at) VALUES(?1,?2,?3)",
                params![
                    message.message_id.to_string(),
                    event.session_id.to_string(),
                    event.timestamp.to_rfc3339()
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn group_root(conn: &Connection, group_id: Uuid) -> Result<Uuid> {
    let root: Option<String> = conn
        .query_row(
            "SELECT root_session_id FROM agent_groups WHERE group_id=?1",
            [group_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let root = root.ok_or_else(|| anyhow::anyhow!("unknown agent group {group_id}"))?;
    Uuid::parse_str(&root).context("invalid agent group root session")
}

fn load_group_task(conn: &Connection, task_id: Uuid) -> Result<Option<GroupTask>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT json FROM group_tasks WHERE task_id=?1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| serde_json::from_str(&json).context("invalid durable group task"))
        .transpose()
}

fn write_group_task(conn: &Connection, task: &GroupTask) -> Result<()> {
    conn.execute(
        "INSERT INTO group_tasks(root_session_id,task_id,group_id,status,assignee,required,dependencies,updated_at,json)
         VALUES((SELECT root_session_id FROM agent_groups WHERE group_id=?1),?2,?1,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(task_id) DO UPDATE SET status=excluded.status, assignee=excluded.assignee,
             required=excluded.required, dependencies=excluded.dependencies,
             updated_at=excluded.updated_at, json=excluded.json",
        params![
            task.group_id.to_string(),
            task.task_id.to_string(),
            task.status.label(),
            task.assignee.map(|value| value.to_string()),
            i64::from(task.required),
            serde_json::to_string(&task.dependencies)?,
            task.updated_at.to_rfc3339(),
            serde_json::to_string(task)?
        ],
    )?;
    Ok(())
}

fn insert_group_message(conn: &Connection, message: &GroupMessage) -> Result<()> {
    let (target_kind, target_agent) = match message.to {
        GroupMessageTarget::Agent(agent) => ("agent", Some(agent.to_string())),
        GroupMessageTarget::Root => ("root", None),
        GroupMessageTarget::Group => ("group", None),
    };
    let root = group_root(conn, message.group_id)?;
    conn.execute(
        "INSERT OR IGNORE INTO group_messages(root_session_id,message_id,group_id,target_kind,target_agent,created_at,json)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            root.to_string(),
            message.message_id.to_string(),
            message.group_id.to_string(),
            target_kind,
            target_agent,
            message.created_at.to_rfc3339(),
            serde_json::to_string(message)?
        ],
    )?;
    Ok(())
}

fn parse_task_status(value: &str) -> Result<GroupTaskStatus> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .with_context(|| format!("invalid durable group task status {value:?}"))
}
fn searchable_text(p: &EventPayload) -> Result<String> {
    Ok(serde_json::to_string(p)?)
}
fn fts_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{}\"", s.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn payload(json: Option<String>) -> Result<Option<EventPayload>> {
    json.map(|value| serde_json::from_str(&value).context("invalid session metadata event"))
        .transpose()
}

fn compact_preview(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let mut value: String = normalized.chars().take(limit.saturating_sub(1)).collect();
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orders_and_persists_events() {
        let s = EventStore::open_memory().unwrap();
        let id = s.create_session(Path::new("/tmp/x")).unwrap();
        let a = s
            .append(
                id,
                EventPayload::UserMessage {
                    text: "a".into(),
                    media: vec![],
                },
            )
            .unwrap();
        let b = s
            .append(
                id,
                EventPayload::UserMessage {
                    text: "b".into(),
                    media: vec![],
                },
            )
            .unwrap();
        assert_eq!((a.sequence, b.sequence), (1, 2));
        let es = s.events(id).unwrap();
        assert_eq!(es[1].parent_id, Some(es[0].id));
    }

    #[test]
    fn session_listing_is_newest_first_and_preview_is_public_only() {
        let store = EventStore::open_memory().unwrap();
        let workspace = Path::new("/tmp/project");
        let first = store.create_session(workspace).unwrap();
        store
            .append(
                first,
                EventPayload::UserMessage {
                    text: "first prompt".into(),
                    media: vec![],
                },
            )
            .unwrap();
        store
            .append(
                first,
                EventPayload::AssistantMessageCompleted {
                    text: "visible answer".into(),
                    tool_calls: vec![],
                    reasoning_content: Some("hidden chain".into()),

                    reasoning: vec![],
                },
            )
            .unwrap();
        let second = store.create_session(workspace).unwrap();
        store
            .append(
                second,
                EventPayload::UserMessage {
                    text: "new prompt".into(),
                    media: vec![],
                },
            )
            .unwrap();
        let listed = store.list_sessions(Some(workspace)).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second);
        assert_eq!(listed[0].prompt_preview.as_deref(), Some("new prompt"));
        let preview = store.session_preview(first, 6).unwrap();
        let shown = format!("{preview:?}");
        assert!(shown.contains("visible answer"));
        assert!(!shown.contains("hidden chain"));
        let unchanged = store.list_sessions(Some(workspace)).unwrap();
        assert_eq!(
            listed, unchanged,
            "listing and previewing must not update sessions"
        );
    }

    #[test]
    fn session_prefix_resolution_reports_ambiguity_and_missing() {
        let store = EventStore::open_memory().unwrap();
        let a = store.create_session(Path::new("/a")).unwrap();
        let exact = store.resolve_session(&a.to_string()).unwrap();
        assert_eq!(exact.id, a);
        assert!(
            store
                .resolve_session("not-a-session")
                .unwrap_err()
                .to_string()
                .contains("no session")
        );
        // Empty prefixes are rejected instead of selecting an arbitrary row.
        assert!(store.resolve_session("").is_err());
        let now = Utc::now().to_rfc3339();
        for id in [
            "aaaaaaaa-0000-0000-0000-000000000001",
            "aaaaaaaa-0000-0000-0000-000000000002",
        ] {
            store.conn().unwrap().execute(
                "INSERT INTO sessions(id,workspace,created_at,updated_at) VALUES(?1,'/x',?2,?2)",
                params![id, now],
            ).unwrap();
        }
        let error = store.resolve_session("aaaaaaaa").unwrap_err().to_string();
        assert!(error.contains("ambiguous"));
        assert!(error.contains("aaaaaaaa"));
    }

    #[test]
    fn incremental_queries_match_full_history() {
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/tmp/incremental")).unwrap();
        for index in 0..6 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("message {index}"),
                        media: vec![],
                    },
                )
                .unwrap();
        }
        store
            .append(sid, EventPayload::ModeChanged { mode: Mode::Plan })
            .unwrap();
        store
            .append(
                sid,
                EventPayload::PermissionRequested {
                    request_id: Uuid::new_v4(),
                    tool: "shell".into(),
                    arguments: serde_json::json!({"command":"ls"}),
                    reason: "test".into(),
                    capabilities: vec![],
                },
            )
            .unwrap();

        let all = store.events(sid).unwrap();
        assert_eq!(store.last_sequence(sid).unwrap(), all.len() as u64);
        assert_eq!(
            store.events_after(sid, 0).unwrap(),
            all,
            "after 0 is the whole history"
        );
        assert_eq!(
            store.events_after(sid, 3).unwrap(),
            all[3..],
            "strictly after the cursor, in order"
        );
        assert_eq!(store.events_before(sid, 4).unwrap(), all[..3]);
        assert!(store.events_before(sid, 1).unwrap().is_empty());

        let modes = store.events_of_kinds(sid, &["mode_changed"]).unwrap();
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].sequence, 7);
        assert_eq!(
            store
                .latest_event_of_kinds(sid, &["mode_changed", "permission_requested"])
                .unwrap()
                .map(|event| event.sequence),
            Some(8),
            "newest of the selected kinds"
        );
        assert!(
            store
                .latest_event_of_kinds(sid, &["safety_changed"])
                .unwrap()
                .is_none()
        );
        assert!(store.events_of_kinds(sid, &[]).unwrap().is_empty());
    }

    #[test]
    fn search_events_matches_the_naive_full_scan() {
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/tmp/search")).unwrap();
        for (index, text) in [
            "alpha needle one",
            "needle two beta",
            "no match here",
            "gamma needle three",
        ]
        .iter()
        .enumerate()
        {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("{text} {index}"),
                        media: vec![],
                    },
                )
                .unwrap();
        }
        // Bookkeeping events also match the FTS text but must never be returned.
        store
            .append(
                sid,
                EventPayload::ContextMaterialized {
                    stats: latch_protocol::ContextStats::default(),
                },
            )
            .unwrap();
        store
            .append(
                sid,
                EventPayload::ContextMemoryRecalled {
                    query: "needle".into(),
                    memory_ids: vec![],
                    event_ids: vec![],
                },
            )
            .unwrap();

        // Recompute the bounded FTS selection, then a full history scan
        // filtered by id and excluded kinds.
        let raw_ids: Vec<String> = {
            let conn = store.conn().unwrap();
            let mut stmt = conn
                .prepare("SELECT event_id FROM event_search WHERE session_id=?1 AND event_search MATCH ?2 ORDER BY rowid DESC LIMIT ?3")
                .unwrap();
            stmt.query_map(params![sid.to_string(), "\"needle\"", 12i64], |row| {
                row.get(0)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };
        let expected: Vec<Event> = store
            .events(sid)
            .unwrap()
            .into_iter()
            .filter(|event| {
                raw_ids.contains(&event.id.to_string())
                    && !matches!(
                        event.payload,
                        EventPayload::ContextMemoryRecalled { .. }
                            | EventPayload::ContextMaterialized { .. }
                    )
            })
            .collect();
        let actual = store.search_events(sid, "needle", 12).unwrap();
        assert_eq!(
            actual, expected,
            "incremental search must match the old scan"
        );
        assert_eq!(actual.len(), 3, "only the matching user turns");
        assert!(
            actual
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence),
            "results stay in sequence order"
        );
    }

    #[test]
    fn session_resume_surfaces_uncertain_operation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions.sqlite3");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let session;
        let operation;
        {
            let store = EventStore::open(&db).unwrap();
            session = store.create_session(&workspace).unwrap();
            store
                .append(
                    session,
                    EventPayload::UserMessage {
                        text: "persist me".into(),
                        media: vec![],
                    },
                )
                .unwrap();
            operation = store.begin_operation(session, "uncertain edit").unwrap();
        }
        let resumed = EventStore::open(&db).unwrap();
        assert_eq!(
            resumed.latest_session(Some(&workspace)).unwrap(),
            Some(session)
        );
        assert_eq!(resumed.events(session).unwrap().len(), 1);
        assert_eq!(
            resumed.interrupted_operations(session).unwrap(),
            vec![(operation, "uncertain edit".into())]
        );
        resumed.mark_operation_reported(operation).unwrap();
        assert!(resumed.interrupted_operations(session).unwrap().is_empty());
    }

    fn group_task_fixture(
        group_id: Uuid,
        root: Uuid,
        title: &str,
        dependencies: Vec<Uuid>,
    ) -> GroupTask {
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
            created_at: Utc::now(),
            updated_at: Utc::now(),
            summary: None,
            findings: vec![],
            expected_paths: vec![],
            touched_files: vec![],
            reason: None,
        }
    }

    #[test]
    fn group_claim_race_has_exactly_one_winner() {
        let store = EventStore::open_memory().unwrap();
        let root = store.create_session(Path::new("/tmp/group-race")).unwrap();
        let identity = store.create_agent_group(root, "race").unwrap();
        let task = store
            .create_group_task(
                root,
                identity.group_id,
                group_task_fixture(identity.group_id, root, "only-one", vec![]),
            )
            .unwrap();
        let outcomes = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for index in 0..32 {
                let store = store.clone();
                let outcomes = &outcomes;
                let group_id = identity.group_id;
                let task_id = task.task_id;
                scope.spawn(move || {
                    let agent = Uuid::from_u128(index as u128 + 1);
                    let outcome = store
                        .claim_group_task(root, group_id, task_id, agent)
                        .unwrap();
                    outcomes.lock().unwrap().push((agent, outcome));
                });
            }
        });
        let outcomes = outcomes.into_inner().unwrap();
        let winners = outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, GroupClaimOutcome::Claimed(_)))
            .collect::<Vec<_>>();
        assert_eq!(
            winners.len(),
            1,
            "exactly one claimer may win: {outcomes:?}"
        );
        for (agent, outcome) in &outcomes {
            match outcome {
                GroupClaimOutcome::Claimed(_) => {}
                GroupClaimOutcome::NotPending { assignee, .. } => {
                    assert_eq!(
                        *assignee,
                        Some(winners[0].0),
                        "losers must observe the single winner, not a second owner"
                    );
                    assert_ne!(*agent, winners[0].0);
                }
                other => panic!("unexpected claim outcome {other:?}"),
            }
        }
        // Durable truth agrees with the in-memory winner, and a rebuild keeps
        // exactly one assignment.
        assert_eq!(
            store
                .events_of_kinds(root, &["group_task_claimed"])
                .unwrap()
                .len(),
            1
        );
        store.rebuild_group_projection().unwrap();
        let resumed = store
            .claim_group_task(root, identity.group_id, task.task_id, Uuid::new_v4())
            .unwrap();
        assert!(matches!(
            resumed,
            GroupClaimOutcome::NotPending {
                assignee: Some(assignee),
                ..
            } if assignee == winners[0].0
        ));
    }

    #[test]
    fn dependent_task_claim_races_dependency_completion_safely() {
        let store = EventStore::open_memory().unwrap();
        let root = store
            .create_session(Path::new("/tmp/group-dep-race"))
            .unwrap();
        let identity = store.create_agent_group(root, "dep").unwrap();
        let foundation = store
            .create_group_task(
                root,
                identity.group_id,
                group_task_fixture(identity.group_id, root, "foundation", vec![]),
            )
            .unwrap();
        let dependent = store
            .create_group_task(
                root,
                identity.group_id,
                group_task_fixture(
                    identity.group_id,
                    root,
                    "dependent",
                    vec![foundation.task_id],
                ),
            )
            .unwrap();
        // Before the dependency completes, the claim cannot win.
        let early = store
            .claim_group_task(root, identity.group_id, dependent.task_id, Uuid::new_v4())
            .unwrap();
        assert!(matches!(
            early,
            GroupClaimOutcome::DependenciesIncomplete { .. }
        ));
        let owner = Uuid::new_v4();
        assert!(matches!(
            store
                .claim_group_task(root, identity.group_id, foundation.task_id, owner)
                .unwrap(),
            GroupClaimOutcome::Claimed(_)
        ));
        assert!(matches!(
            store
                .transition_group_task(
                    root,
                    foundation.task_id,
                    owner,
                    GroupTaskTransition {
                        allowed_from: vec![GroupTaskStatus::Claimed],
                        to: GroupTaskStatus::Completed,
                        require_owner: true,
                        assignee: None,
                        summary: Some("done".into()),
                        reason: None,
                        findings: vec![],
                        touched_files: vec![],
                    },
                )
                .unwrap(),
            GroupTransitionOutcome::Updated(_)
        ));
        // After completion commits, the dependent task is claimable.
        assert!(matches!(
            store
                .claim_group_task(root, identity.group_id, dependent.task_id, Uuid::new_v4())
                .unwrap(),
            GroupClaimOutcome::Claimed(_)
        ));
    }

    #[test]
    fn release_then_reclaim_is_unambiguous() {
        let store = EventStore::open_memory().unwrap();
        let root = store
            .create_session(Path::new("/tmp/group-release"))
            .unwrap();
        let identity = store.create_agent_group(root, "release").unwrap();
        let task = store
            .create_group_task(
                root,
                identity.group_id,
                group_task_fixture(identity.group_id, root, "handoff", vec![]),
            )
            .unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert!(matches!(
            store
                .claim_group_task(root, identity.group_id, task.task_id, first)
                .unwrap(),
            GroupClaimOutcome::Claimed(_)
        ));
        // A sibling cannot release what it does not own.
        assert!(matches!(
            store
                .release_group_task(root, task.task_id, second)
                .unwrap(),
            GroupTransitionOutcome::NotOwner { .. }
        ));
        assert!(matches!(
            store.release_group_task(root, task.task_id, first).unwrap(),
            GroupTransitionOutcome::Updated(_)
        ));
        assert!(matches!(
            store
                .claim_group_task(root, identity.group_id, task.task_id, second)
                .unwrap(),
            GroupClaimOutcome::Claimed(_)
        ));
        // The durable order is exactly claim, release, claim.
        let events = store
            .events_of_kinds(root, &["group_task_claimed", "group_task_released"])
            .unwrap();
        let kinds = events
            .iter()
            .map(|event| match event.payload {
                EventPayload::GroupTaskClaimed { agent_id, .. } => {
                    format!("claim:{agent_id}")
                }
                EventPayload::GroupTaskReleased { agent_id, .. } => {
                    format!("release:{agent_id}")
                }
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                format!("claim:{first}"),
                format!("release:{first}"),
                format!("claim:{second}")
            ]
        );
    }

    #[test]
    fn group_delivery_is_exactly_once_under_concurrency() {
        let store = EventStore::open_memory().unwrap();
        let root = store
            .create_session(Path::new("/tmp/group-delivery-race"))
            .unwrap();
        let recipient = store
            .create_session(Path::new("/tmp/group-delivery-race"))
            .unwrap();
        let identity = store.create_agent_group(root, "delivery").unwrap();
        let message = GroupMessage {
            message_id: Uuid::new_v4(),
            group_id: identity.group_id,
            from_agent: root,
            to: GroupMessageTarget::Agent(recipient),
            text: "once".into(),
            created_at: Utc::now(),
        };
        store
            .append(
                root,
                EventPayload::GroupMessageQueued {
                    message: message.clone(),
                },
            )
            .unwrap();
        let successes = std::sync::Mutex::new(0usize);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let store = store.clone();
                let message = message.clone();
                let successes = &successes;
                scope.spawn(move || {
                    if store
                        .deliver_group_message(recipient, message)
                        .unwrap()
                        .is_some()
                    {
                        *successes.lock().unwrap() += 1;
                    }
                });
            }
        });
        assert_eq!(
            *successes.lock().unwrap(),
            1,
            "exactly one delivery commits"
        );
        assert_eq!(
            store
                .events_of_kinds(recipient, &["group_message_delivered"])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .group_pending_messages(identity.group_id, recipient, false)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn group_projection_is_rebuildable_from_events_after_corruption() {
        let store = EventStore::open_memory().unwrap();
        let root = store
            .create_session(Path::new("/tmp/group-rebuild"))
            .unwrap();
        let agent = Uuid::new_v4();
        let identity = store.create_agent_group(root, "rebuild").unwrap();
        store
            .join_agent_group(root, identity.group_id, agent)
            .unwrap();
        let task = store
            .create_group_task(
                root,
                identity.group_id,
                group_task_fixture(identity.group_id, root, "durable", vec![]),
            )
            .unwrap();
        store
            .claim_group_task(root, identity.group_id, task.task_id, agent)
            .unwrap();
        // Simulate a lost projection: drop every row without touching events.
        {
            let conn = store.conn().unwrap();
            conn.execute("DELETE FROM group_tasks", []).unwrap();
            conn.execute("DELETE FROM group_members", []).unwrap();
            conn.execute("DELETE FROM agent_groups", []).unwrap();
        }
        store.rebuild_group_projection().unwrap();
        assert_eq!(
            store.agent_group_identity(root).unwrap().unwrap().group_id,
            identity.group_id
        );
        let outcome = store
            .claim_group_task(root, identity.group_id, task.task_id, Uuid::new_v4())
            .unwrap();
        assert!(matches!(
            outcome,
            GroupClaimOutcome::NotPending {
                assignee: Some(assignee),
                status: GroupTaskStatus::Claimed
            } if assignee == agent
        ));
    }
}
