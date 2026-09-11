use anyhow::{Context, Result, bail};
use chrono::Utc;
use latch_protocol::{Event, EventPayload, MemoryRecord};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

#[derive(Clone)]
pub struct EventStore {
    connection: Arc<Mutex<Connection>>,
}

impl EventStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let store = Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory()?)),
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
            CREATE TABLE IF NOT EXISTS memory(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, content TEXT NOT NULL, originating_event TEXT NOT NULL, created_at TEXT NOT NULL, validity TEXT NOT NULL, json TEXT NOT NULL);
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

    pub fn latest_session(&self, workspace: Option<&Path>) -> Result<Option<Uuid>> {
        let conn = self.conn()?;
        let value: Option<String> = if let Some(path) = workspace {
            conn.query_row(
                "SELECT id FROM sessions WHERE workspace=?1 ORDER BY updated_at DESC LIMIT 1",
                [path.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .optional()?
        } else {
            conn.query_row(
                "SELECT id FROM sessions ORDER BY updated_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
        };
        value
            .map(|v| Uuid::parse_str(&v).context("invalid session UUID"))
            .transpose()
    }

    pub fn append(&self, session_id: Uuid, payload: EventPayload) -> Result<Event> {
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
        let kind = event_kind(&event.payload);
        let json = serde_json::to_string(&event.payload)?;
        let searchable = searchable_text(&event.payload);
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

    pub fn events(&self, session_id: Uuid) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt=conn.prepare("SELECT sequence,id,parent_id,timestamp,payload FROM events WHERE session_id=?1 ORDER BY sequence")?;
        let rows = stmt.query_map([session_id.to_string()], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|r| {
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
        .collect()
    }

    pub fn search_events(&self, session_id: Uuid, query: &str, limit: usize) -> Result<Vec<Event>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let query = fts_query(query);
        if query.is_empty() {
            return Ok(vec![]);
        }
        let ids: Vec<String> = {
            let conn = self.conn()?;
            let mut stmt=conn.prepare("SELECT event_id FROM event_search WHERE session_id=?1 AND event_search MATCH ?2 LIMIT ?3")?;
            stmt.query_map(params![session_id.to_string(), query, limit], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        let all = self.events(session_id)?;
        Ok(all
            .into_iter()
            .filter(|e| ids.contains(&e.id.to_string()))
            .collect())
    }

    pub fn add_memory(&self, record: &MemoryRecord) -> Result<()> {
        self.conn()?.execute("INSERT INTO memory(id,session_id,kind,content,originating_event,created_at,validity,json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![record.id.to_string(),record.session_id.to_string(),format!("{:?}",record.kind),record.content,record.originating_event.to_string(),record.created_at.to_rfc3339(),format!("{:?}",record.validity),serde_json::to_string(record)?])?;
        Ok(())
    }

    pub fn memories(&self, session_id: Uuid) -> Result<Vec<MemoryRecord>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT json FROM memory WHERE session_id=?1 ORDER BY created_at")?;
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

fn event_kind(p: &EventPayload) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}
fn searchable_text(p: &EventPayload) -> String {
    serde_json::to_string(p).unwrap_or_default()
}
fn fts_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{}\"", s.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
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
