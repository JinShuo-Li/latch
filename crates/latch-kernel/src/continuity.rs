use crate::config::ContextConfig;
use crate::store::EventStore;
use anyhow::Result;
use latch_protocol::{ContextStats, Event, EventPayload, MemoryRecord, TaskState, Validity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub start_sequence: u64,
    pub end_sequence: u64,
    pub topic: String,
    pub entities: Vec<String>,
    pub description: String,
    pub event_ids: Vec<Uuid>,
}

#[derive(Debug, Clone)]
pub struct MaterializedContext {
    pub system: String,
    pub canonical: String,
    pub recalled: String,
    pub recent: Vec<Event>,
    pub stats: ContextStats,
}

pub struct ContinuityEngine {
    store: EventStore,
    config: ContextConfig,
    generation: u32,
}
impl ContinuityEngine {
    #[must_use]
    pub fn new(store: EventStore, config: ContextConfig) -> Self {
        Self {
            store,
            config,
            generation: 0,
        }
    }
    pub fn manual_compact(&mut self, session_id: Uuid) -> Result<()> {
        self.generation += 1;
        self.store.append(
            session_id,
            EventPayload::ManualCompact {
                generation: self.generation,
            },
        )?;
        Ok(())
    }
    pub fn recall(&self, session_id: Uuid, query: &str) -> Result<Vec<Event>> {
        self.store.search_events(session_id, query, 12)
    }
    pub fn materialize(
        &self,
        session_id: Uuid,
        state: &TaskState,
        query: Option<&str>,
        system: String,
    ) -> Result<MaterializedContext> {
        let events = self.store.events(session_id)?;
        let memories = self.store.memories(session_id)?;
        let recalled = if let Some(q) = query {
            self.recall(session_id, q)?
        } else {
            vec![]
        };
        let active_start = events
            .iter()
            .rposition(|event| matches!(event.payload, EventPayload::ManualCompact { .. }))
            .map_or(0, |index| index + 1);
        let recent = select_recent(&events[active_start..], self.config.recent_bytes);
        let canonical = render_canonical(state, &memories)?;
        let recalled_text = recalled
            .iter()
            .map(render_event)
            .collect::<Vec<_>>()
            .join("\n");
        let recent_bytes = recent.iter().map(|e| render_event(e).len()).sum();
        let canonical_bytes = canonical.len();
        let recalled_bytes = recalled_text.len();
        let episodes = episode_count(events.len(), recent.len());
        let used = recent_bytes + canonical_bytes + recalled_bytes;
        let reserve = self
            .config
            .active_bytes
            .saturating_sub(used)
            .max(self.config.reserve_bytes.min(self.config.active_bytes));
        Ok(MaterializedContext {
            system,
            canonical,
            recalled: recalled_text,
            recent,
            stats: ContextStats {
                recent_bytes,
                recalled_bytes,
                canonical_bytes,
                code_evidence_bytes: 0,
                reserve_bytes: reserve,
                durable_events: events.len(),
                episodes,
                status: "healthy".into(),
            },
        })
    }
}

fn select_recent(events: &[Event], budget: usize) -> Vec<Event> {
    let mut size = 0;
    let mut selected = Vec::new();
    for event in events.iter().rev() {
        let n = render_event(event).len();
        if !selected.is_empty() && size + n > budget {
            break;
        }
        size += n;
        selected.push(event.clone());
    }
    selected.reverse();
    selected
}
fn episode_count(total: usize, recent: usize) -> usize {
    total.saturating_sub(recent).div_ceil(20)
}
fn render_canonical(state: &TaskState, memories: &[MemoryRecord]) -> Result<String> {
    let active = memories
        .iter()
        .filter(|m| !matches!(m.validity, Validity::Stale | Validity::Superseded))
        .map(|m| {
            format!(
                "- {:?} [{:?}] {} (source {})",
                m.kind, m.validity, m.content, m.originating_event
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "CANONICAL TASK STATE\n{}\nDURABLE MEMORY (provenance preserved)\n{}",
        serde_json::to_string_pretty(state)?,
        active
    ))
}
fn render_event(e: &Event) -> String {
    match &e.payload {
        EventPayload::UserMessage { text } => format!("user: {text}"),
        EventPayload::AssistantMessageCompleted { text, tool_calls } => format!(
            "assistant: {text}{}",
            if tool_calls.is_empty() {
                String::new()
            } else {
                format!(
                    " [tool calls: {}]",
                    tool_calls
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ),
        _ => format!(
            "event {} #{}: {}",
            e.id,
            e.sequence,
            serde_json::to_string(&e.payload).unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::state::{StateUpdate, TaskStateManager};
    use chrono::Utc;
    use latch_protocol::{MemoryKind, MemoryRecord};
    use std::path::Path;
    #[test]
    fn continuity_stress_preserves_truth_without_auto_compact() {
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/fixture")).unwrap();
        let c = store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "Constraint A: preserve wire compatibility".into(),
                },
            )
            .unwrap();
        let d = store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "Decision B: use framed stdio".into(),
                },
            )
            .unwrap();
        let rejected = store
            .append(
                sid,
                EventPayload::FailureAttempt {
                    signature: "approach C deadlocked".into(),
                    count: 3,
                },
            )
            .unwrap();
        let diagnostic = store
            .append(
                sid,
                EventPayload::ToolFailed {
                    result: latch_protocol::ToolResult {
                        call_id: "d".into(),
                        name: "shell".into(),
                        output: "exact diagnostic D: EADDRINUSE on 4317".into(),
                        is_error: true,
                        artifact_id: None,
                    },
                },
            )
            .unwrap();
        for (event, kind, text, validity) in [
            (
                c.id,
                MemoryKind::UserConstraint,
                "Constraint A: preserve wire compatibility",
                Validity::Active,
            ),
            (
                d.id,
                MemoryKind::Decision,
                "Decision B: use framed stdio",
                Validity::Active,
            ),
            (
                rejected.id,
                MemoryKind::Hypothesis,
                "approach C deadlocked",
                Validity::Rejected,
            ),
        ] {
            store
                .add_memory(&MemoryRecord {
                    id: Uuid::new_v4(),
                    session_id: sid,
                    kind,
                    content: text.into(),
                    originating_event: event,
                    created_at: Utc::now(),
                    validity,
                    confidence: None,
                    dependencies: vec![],
                    supersedes: None,
                })
                .unwrap();
        }
        for i in 0..100 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("unrelated conversation {i} {}", "x".repeat(80)),
                    },
                )
                .unwrap();
        }
        let mut state = TaskStateManager::default();
        state.update(StateUpdate {
            add_constraints: vec!["Constraint A: preserve wire compatibility".into()],
            add_decisions: vec!["Decision B: use framed stdio".into()],
            add_hypotheses: vec!["approach C deadlocked".into()],
            reject_hypotheses: vec!["approach C deadlocked".into()],
            ..Default::default()
        });
        let engine = ContinuityEngine::new(
            store.clone(),
            ContextConfig {
                active_bytes: 1800,
                recent_bytes: 700,
                reserve_bytes: 200,
            },
        );
        let ctx = engine
            .materialize(sid, state.state(), Some("EADDRINUSE 4317"), "system".into())
            .unwrap();
        assert!(
            ctx.canonical.contains("Constraint A")
                && ctx.canonical.contains("Decision B")
                && ctx.canonical.contains("Rejected")
        );
        assert!(ctx.recalled.contains("exact diagnostic D"));
        assert!(ctx.recent.iter().any(|e|matches!(&e.payload,EventPayload::UserMessage{text} if text.contains("unrelated conversation 99"))));
        let all = store.events(sid).unwrap();
        assert!(all.iter().any(|e| e.id == diagnostic.id));
        assert!(
            !all.iter()
                .any(|e| matches!(e.payload, EventPayload::ManualCompact { .. }))
        );
        assert!(ctx.recent.len() < all.len());
    }
    #[test]
    fn manual_compact_retains_raw_events() {
        let s = EventStore::open_memory().unwrap();
        let id = s.create_session(Path::new("/x")).unwrap();
        s.append(
            id,
            EventPayload::UserMessage {
                text: "keep".into(),
            },
        )
        .unwrap();
        let mut e = ContinuityEngine::new(
            s.clone(),
            ContextConfig {
                active_bytes: 100,
                recent_bytes: 50,
                reserve_bytes: 10,
            },
        );
        e.manual_compact(id).unwrap();
        let all = s.events(id).unwrap();
        assert_eq!(all.len(), 2);
        assert!(matches!(all[1].payload, EventPayload::ManualCompact { .. }));
    }
}
