use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use latch_protocol::{AgentIdentity, CompletionState, Event, EventPayload, MemoryRecord, Mode};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

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
            std::fs::create_dir_all(parent)?;
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
            CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, description TEXT NOT NULL, status TEXT NOT NULL, started_at TEXT NOT NULL);")?;
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
                    EventPayload::UserMessage { text } => Some(compact_preview(&text, 140)),
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
                EventPayload::UserMessage { text } => Some(SessionPreviewLine {
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
        tx.execute(
            "INSERT INTO event_search(session_id,event_id,text) VALUES(?1,?2,?3)",
            params![session_id.to_string(), event.id.to_string(), searchable],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            params![session_id.to_string(), event.timestamp.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(event)
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
        // insertion (rowid) so the same durable log always yields the same
        // bounded recall set; the outer query then presents it in event
        // order. The bookkeeping kinds are excluded after selection so the
        // limit keeps its original meaning.
        let sql = "SELECT sequence,id,parent_id,timestamp,payload FROM events \
                   WHERE session_id=?1 AND id IN (\
                       SELECT event_id FROM event_search \
                       WHERE session_id=?1 AND event_search MATCH ?2 ORDER BY rowid LIMIT ?3\
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
fn searchable_text(p: &EventPayload) -> Result<String> {
    Ok(serde_json::to_string(p)?)
}
fn fts_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{}\"", s.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
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
            .append(id, EventPayload::UserMessage { text: "a".into() })
            .unwrap();
        let b = s
            .append(id, EventPayload::UserMessage { text: "b".into() })
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

        // Recompute the historical implementation: FTS ids first (insertion
        // order), then a full history scan filtered by id and excluded kinds.
        let raw_ids: Vec<String> = {
            let conn = store.conn().unwrap();
            let mut stmt = conn
                .prepare("SELECT event_id FROM event_search WHERE session_id=?1 AND event_search MATCH ?2 ORDER BY rowid LIMIT ?3")
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
}
