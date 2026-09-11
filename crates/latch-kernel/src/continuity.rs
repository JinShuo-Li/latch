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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConversationBridge {
    pub current_topic: String,
    pub current_user_intent: String,
    pub unresolved_references: Vec<String>,
    pub recent_decisions: Vec<String>,
    pub ongoing_action: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MaterializedContext {
    pub system: String,
    pub canonical: String,
    pub recalled: String,
    pub recent: Vec<Event>,
    pub bridge: ConversationBridge,
    pub episodes: Vec<Episode>,
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
        let memories = self.store.memories(session_id)?;
        let recalled = if let Some(q) = query {
            self.recall(session_id, q)?
        } else {
            vec![]
        };
        if let Some(query) = query {
            let lower = query.to_ascii_lowercase();
            let memory_ids = memories
                .iter()
                .filter(|memory| memory.content.to_ascii_lowercase().contains(&lower))
                .map(|memory| memory.id)
                .collect();
            self.store.append(
                session_id,
                EventPayload::ContextMemoryRecalled {
                    query: query.into(),
                    memory_ids,
                    event_ids: recalled.iter().map(|event| event.id).collect(),
                },
            )?;
        }
        let events = self.store.events(session_id)?;
        let active_start = events
            .iter()
            .rposition(|event| matches!(event.payload, EventPayload::ManualCompact { .. }))
            .map_or(0, |index| index + 1);
        let recent = select_recent(&events[active_start..], self.config.recent_bytes);
        let bridge = conversation_bridge(state, &events);
        let canonical = format!(
            "{}\nCONVERSATION BRIDGE (navigation only)\n{}",
            render_canonical(state, &memories)?,
            serde_json::to_string_pretty(&bridge)?
        );
        let old_end = events.len().saturating_sub(recent.len());
        let episodes = build_episodes(&events[..old_end]);
        let episode_index = episodes
            .iter()
            .map(|episode| {
                format!(
                    "- events {}-{}: {}",
                    episode.start_sequence, episode.end_sequence, episode.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let recalled_events = recalled
            .iter()
            .map(render_event)
            .collect::<Vec<_>>()
            .join("\n");
        let recalled_text =
            format!("EPISODE INDEX\n{episode_index}\nORIGINAL RECALLED EVENTS\n{recalled_events}");
        let recent_bytes = recent.iter().map(|e| render_event(e).len()).sum();
        let canonical_bytes = canonical.len();
        let recalled_bytes = recalled_text.len();
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
            bridge,
            episodes: episodes.clone(),
            stats: ContextStats {
                recent_bytes,
                recalled_bytes,
                canonical_bytes,
                code_evidence_bytes: 0,
                reserve_bytes: reserve,
                durable_events: events.len(),
                episodes: episodes.len(),
                status: "healthy".into(),
            },
        })
    }
}

fn select_recent(events: &[Event], budget: usize) -> Vec<Event> {
    let units = conversation_units(events);
    let mut size = 0;
    let mut selected = Vec::new();
    for &(start, end) in units.iter().rev() {
        let n = events[start..end]
            .iter()
            .map(|event| render_event(event).len())
            .sum::<usize>();
        if !selected.is_empty() && size + n > budget {
            break;
        }
        size += n;
        selected.push((start, end));
    }
    selected.reverse();
    let mut recent = Vec::new();
    for (start, end) in selected {
        recent.extend_from_slice(&events[start..end]);
    }
    recent
}

/// Groups events into atomic conversation transactions.
///
/// A tool transaction is an assistant turn that proposes tool calls together
/// with every tool result answering those calls. Selecting recent context must
/// never observe only one half of such a transaction: a dangling tool result or
/// an unanswered assistant tool call is invalid for OpenAI-compatible and
/// Anthropic protocols alike.
fn conversation_units(events: &[Event]) -> Vec<(usize, usize)> {
    let mut units = Vec::new();
    let mut index = 0;
    while index < events.len() {
        if let EventPayload::AssistantMessageCompleted { tool_calls, .. } = &events[index].payload
            && !tool_calls.is_empty()
        {
            let expected = tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let mut seen = std::collections::BTreeSet::new();
            let mut end = index + 1;
            let mut cursor = index + 1;
            while cursor < events.len() {
                if matches!(
                    &events[cursor].payload,
                    EventPayload::AssistantMessageCompleted { tool_calls, .. } if !tool_calls.is_empty()
                ) {
                    break;
                }
                if let EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } =
                    &events[cursor].payload
                    && expected.contains(result.call_id.as_str())
                {
                    seen.insert(result.call_id.as_str());
                    end = cursor + 1;
                    if seen.len() == expected.len() {
                        break;
                    }
                }
                cursor += 1;
            }
            units.push((index, end));
            index = end;
        } else {
            units.push((index, index + 1));
            index += 1;
        }
    }
    units
}
fn build_episodes(events: &[Event]) -> Vec<Episode> {
    events
        .chunks(20)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            let topic = chunk
                .iter()
                .find_map(|event| match &event.payload {
                    EventPayload::UserMessage { text } => {
                        Some(text.chars().take(100).collect::<String>())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "session activity".into());
            let mut entities = Vec::new();
            for event in chunk {
                match &event.payload {
                    EventPayload::FileObserved { version } => entities.push(version.path.clone()),
                    EventPayload::FileChanged { after, .. } => entities.push(after.path.clone()),
                    _ => {}
                }
            }
            entities.sort();
            entities.dedup();
            Episode {
                start_sequence: chunk[0].sequence,
                end_sequence: chunk[chunk.len() - 1].sequence,
                topic: topic.clone(),
                entities,
                description: topic,
                event_ids: chunk.iter().map(|event| event.id).collect(),
            }
        })
        .collect()
}

fn conversation_bridge(state: &TaskState, events: &[Event]) -> ConversationBridge {
    let current_user_intent = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::UserMessage { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let lower = current_user_intent.to_ascii_lowercase();
    let unresolved_references = ["this", "that", "second approach", "continue"]
        .into_iter()
        .filter(|needle| lower.contains(needle))
        .map(str::to_owned)
        .collect();
    ConversationBridge {
        current_topic: current_user_intent.chars().take(160).collect(),
        current_user_intent,
        unresolved_references,
        recent_decisions: state.decisions.iter().rev().take(4).cloned().collect(),
        ongoing_action: state.next_actions.first().cloned(),
    }
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
        EventPayload::AssistantMessageCompleted {
            text, tool_calls, ..
        } => format!(
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
        let context = e
            .materialize(id, &TaskState::default(), None, "system".into())
            .unwrap();
        assert!(context.recent.is_empty());
    }

    fn event(session: Uuid, sequence: u64, payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: session,
            sequence,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    #[test]
    fn recent_selection_keeps_tool_transactions_atomic() {
        use latch_protocol::{ToolCall, ToolResult};
        use serde_json::json;
        let session = Uuid::new_v4();
        let events = vec![
            event(
                session,
                1,
                EventPayload::UserMessage {
                    text: "inspect".into(),
                },
            ),
            event(
                session,
                2,
                EventPayload::AssistantMessageCompleted {
                    text: "calling".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"a"}),
                    }],
                    reasoning_content: Some("reasoning".into()),
                },
            ),
            event(
                session,
                3,
                EventPayload::ToolCompleted {
                    result: ToolResult {
                        call_id: "call-1".into(),
                        name: "read_file".into(),
                        output: "contents".into(),
                        is_error: false,
                        artifact_id: None,
                    },
                },
            ),
            event(
                session,
                4,
                EventPayload::AssistantMessageCompleted {
                    text: "done".into(),
                    tool_calls: vec![],
                    reasoning_content: None,
                },
            ),
        ];
        let transaction = render_event(&events[1]).len() + render_event(&events[2]).len();
        let tail = render_event(&events[3]).len();

        // Budget fits the trailing assistant reply and the tool result but not
        // the assistant tool-call message: the transaction must be dropped
        // whole, never leaving a dangling tool result.
        let split = select_recent(&events, tail + transaction - 1);
        assert_eq!(split.len(), 1);
        assert!(matches!(
            split[0].payload,
            EventPayload::AssistantMessageCompleted { ref tool_calls, .. } if tool_calls.is_empty()
        ));
        assert!(
            !split
                .iter()
                .any(|e| matches!(e.payload, EventPayload::ToolCompleted { .. }))
        );

        // Budget fits the whole transaction: both halves are selected.
        let whole = select_recent(&events, tail + transaction);
        assert!(
            whole
                .iter()
                .any(|e| matches!(e.payload, EventPayload::ToolCompleted { .. }))
        );
        assert!(whole.iter().any(|e| matches!(
            &e.payload,
            EventPayload::AssistantMessageCompleted { tool_calls, .. } if !tool_calls.is_empty()
        )));
    }

    #[test]
    fn recent_selection_never_splits_assistant_from_results() {
        use latch_protocol::{ToolCall, ToolResult};
        use serde_json::json;
        let session = Uuid::new_v4();
        let events = vec![
            event(
                session,
                1,
                EventPayload::AssistantMessageCompleted {
                    text: "calling".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"a"}),
                    }],
                    reasoning_content: None,
                },
            ),
            event(
                session,
                2,
                EventPayload::ToolCompleted {
                    result: ToolResult {
                        call_id: "call-1".into(),
                        name: "read_file".into(),
                        output: "contents".into(),
                        is_error: false,
                        artifact_id: None,
                    },
                },
            ),
        ];
        let units = conversation_units(&events);
        assert_eq!(units, vec![(0, 2)]);
        for budget in [0, 1, 10, 100, 10_000] {
            let recent = select_recent(&events, budget);
            let has_assistant = recent.iter().any(|e| matches!(
                &e.payload,
                EventPayload::AssistantMessageCompleted { tool_calls, .. } if !tool_calls.is_empty()
            ));
            let has_result = recent
                .iter()
                .any(|e| matches!(e.payload, EventPayload::ToolCompleted { .. }));
            assert_eq!(
                has_assistant, has_result,
                "budget {budget} split a transaction"
            );
        }
    }
}
