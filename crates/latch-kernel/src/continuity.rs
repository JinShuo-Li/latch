use crate::config::ContextConfig;
use crate::state::{EvidenceLedger, FailureManager};
use crate::store::EventStore;
use crate::tokens::TokenEstimator;
use anyhow::Result;
use latch_protocol::{
    ContextStats, Event, EventPayload, MemoryKind, MemoryRecord, TaskState, Validity,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// Upper bound on episode index entries materialized into context.
const MAX_EPISODE_ENTRIES: usize = 16;
/// Upper bound on durable memories rendered into canonical state.
const MAX_MEMORY_LINES: usize = 64;
/// Per-record content truncation for canonical memory lines.
const MAX_MEMORY_CONTENT: usize = 400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub start_sequence: u64,
    pub end_sequence: u64,
    pub topic: String,
    /// Workspace paths touched or read inside the episode.
    pub entities: Vec<String>,
    /// Tool names executed inside the episode.
    pub tools: Vec<String>,
    /// Structural markers such as `validation-passed`, `validation-failed`,
    /// `reground`, `mutation`, `evidence`, `failure`.
    pub markers: Vec<String>,
    pub summary: String,
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
    /// The bounded, scored subset of episodes selected into the index.
    pub episodes: Vec<Episode>,
    pub stats: ContextStats,
}

/// Token budget for one materialized request view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterializeBudget {
    /// Complete request budget (`context window - reserve`).
    pub request_tokens: usize,
    /// The model's full context window, for display.
    pub window_tokens: usize,
    /// Tokens reserved for the model response plus safety.
    pub reserve_tokens: usize,
    /// Upper bound for the verbatim recent transcript.
    pub recent_tokens: usize,
    /// Tokens already reserved by the caller for tool schemas and extension
    /// context; continuity sizes its own sections inside the remainder.
    pub reserved_tokens: usize,
}

pub struct ContinuityEngine {
    store: EventStore,
    config: ContextConfig,
    estimator: TokenEstimator,
    generation: u32,
}
impl ContinuityEngine {
    #[must_use]
    pub fn new(store: EventStore, config: ContextConfig) -> Self {
        Self {
            store,
            config,
            estimator: TokenEstimator::generic(),
            generation: 0,
        }
    }
    /// Builds an engine whose estimator matches the provider model.
    #[must_use]
    pub fn for_model(store: EventStore, config: ContextConfig, model: &str) -> Self {
        Self {
            store,
            config,
            estimator: TokenEstimator::for_model(model),
            generation: 0,
        }
    }
    #[must_use]
    pub const fn estimator(&self) -> &TokenEstimator {
        &self.estimator
    }
    pub fn set_estimator(&mut self, estimator: TokenEstimator) {
        self.estimator = estimator;
    }
    pub fn set_config(&mut self, config: ContextConfig) {
        self.config = config;
    }
    /// Derives the request budget from this engine's token configuration.
    #[must_use]
    pub fn default_budget(
        &self,
        window_tokens: usize,
        reserved_tokens: usize,
    ) -> MaterializeBudget {
        let window = window_tokens.max(1);
        let reserve = self
            .config
            .output_reserve_tokens
            .saturating_add(self.config.reserve_tokens);
        let request_tokens = self
            .config
            .max_request_tokens
            .unwrap_or_else(|| window.saturating_sub(reserve))
            .min(window);
        MaterializeBudget {
            request_tokens,
            window_tokens: window,
            reserve_tokens: reserve,
            recent_tokens: self.config.recent_tokens,
            reserved_tokens: reserved_tokens.min(request_tokens),
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

    /// Materializes one bounded request view.
    ///
    /// Invariant: `instructions + state + recent + recall` (the sections this
    /// engine owns) stays within the request budget minus any tokens the caller
    /// already reserved for tool schemas and extension context. Components are
    /// allocated in explicit priority order — system prompt, canonical core
    /// (goal/constraints/decisions/evidence/failures), protocol-atomic recent
    /// verbatim transcript, query-recalled original events, then the scored
    /// episode index. Lower-priority material shrinks first; nothing durable is
    /// ever deleted and no automatic compaction exists.
    ///
    /// Every size is a token estimate; the caller adds tools/extension costs
    /// and recomputes the final totals so recalled material is counted exactly
    /// once.
    #[allow(clippy::too_many_arguments)]
    pub fn materialize(
        &self,
        session_id: Uuid,
        state: &TaskState,
        query: Option<&str>,
        evidence: &EvidenceLedger,
        failures: &FailureManager,
        system: String,
        budget: &MaterializeBudget,
    ) -> Result<MaterializedContext> {
        let memories = self.store.memories(session_id)?;
        let active_start = self.active_start(session_id)?;
        let all_events = self.store.events(session_id)?;
        let active_events = &all_events[active_start.min(all_events.len())..];
        // The current user turn is already in the recent transcript; recalling
        // it as "original material" would duplicate it and make the dynamic
        // system block differ between the first and later requests of an epoch.
        let current_user_sequence = active_events
            .iter()
            .rev()
            .find(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
            .map(|event| event.sequence);
        let recalled_events = query
            .map(|q| -> Result<Vec<Event>> {
                let events = self
                    .recall(session_id, q)?
                    .into_iter()
                    .filter(|event| {
                        current_user_sequence.is_none_or(|sequence| event.sequence < sequence)
                    })
                    .collect::<Vec<_>>();
                self.record_recall(session_id, q, &memories, &events)?;
                Ok(events)
            })
            .transpose()?
            .unwrap_or_default();

        let estimator = self.estimator;
        let own_budget = budget.request_tokens.saturating_sub(budget.reserved_tokens);

        // 1. Hard system/kernel instructions come first.
        let instructions_tokens = estimator.estimate(&system);
        let mut used = instructions_tokens;

        // 2. Canonical state, capped by tokens. Individual memory lines drop
        //    lowest-priority first; goal, constraints, decisions, evidence, and
        //    failures survive as long as anything does.
        let canonical_cap = own_budget.saturating_sub(used) * 2 / 5;
        let bridge = conversation_bridge(state, active_events);
        let canonical = render_canonical(
            state,
            &memories,
            evidence,
            failures,
            &bridge,
            canonical_cap,
            &estimator,
        );
        let state_tokens = estimator.estimate(&canonical);
        used = used.saturating_add(state_tokens);

        // 3. Recent verbatim transcript, protocol-atomic, within its own
        //    budget and the global remainder.
        let recent_budget = budget.recent_tokens.min(own_budget.saturating_sub(used));
        let recent = select_recent(active_events, recent_budget, &estimator);
        let recent_tokens = recent
            .iter()
            .map(|event| estimator.estimate(&render_event(event)))
            .sum::<usize>();
        used = used.saturating_add(recent_tokens);

        // 4. Recalled originals, then the episode index, sharing the remaining
        //    recall budget. The combined block is estimated exactly once so
        //    recalled content is never double counted.
        let recall_budget = own_budget.saturating_sub(used);
        let recalled_cap = recall_budget * 3 / 4;
        let (recalled_text, _recalled_selected) =
            render_recalled(&recalled_events, recalled_cap, &estimator);
        let recalled_used = estimator.estimate(&recalled_text);
        let old_end = active_events.len().saturating_sub(recent.len());
        let episodes = build_episodes(&active_events[..old_end]);
        let selected = select_episodes(
            &episodes,
            query,
            state,
            recall_budget.saturating_sub(recalled_used),
            &estimator,
        );
        let episode_index = selected
            .iter()
            .map(|episode| episode.summary.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let recalled_full = format!(
            "EPISODE INDEX\n{}\nORIGINAL RECALLED EVENTS\n{}",
            episode_index, recalled_text
        );
        let recall_tokens = estimator.estimate(&recalled_full);

        let mut stats = ContextStats {
            instructions_tokens,
            state_tokens,
            recent_tokens,
            recall_tokens,
            tools_tokens: 0,
            extension_tokens: 0,
            total_tokens: 0,
            request_tokens: 0,
            common_prefix_tokens: 0,
            budget_tokens: budget.request_tokens,
            window_tokens: budget.window_tokens,
            reserve_tokens: budget.reserve_tokens,
            headroom_tokens: 0,
            durable_events: all_events.len(),
            episodes: episodes.len(),
            selected_episodes: selected.len(),
            estimated: true,
            status: String::new(),
        };
        stats.recompute();
        Ok(MaterializedContext {
            system,
            canonical,
            recalled: recalled_full,
            recent,
            bridge,
            episodes: selected.clone(),
            stats,
        })
    }
    fn active_start(&self, session_id: Uuid) -> Result<usize> {
        let events = self.store.events(session_id)?;
        Ok(events
            .iter()
            .rposition(|event| matches!(event.payload, EventPayload::ManualCompact { .. }))
            .map_or(0, |index| index + 1))
    }
    fn record_recall(
        &self,
        session_id: Uuid,
        query: &str,
        memories: &[MemoryRecord],
        recalled: &[Event],
    ) -> Result<()> {
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
        Ok(())
    }
}

/// Selects the bounded relevant subset of episodes for the index. Scoring is
/// deterministic: lexical overlap with the query, current file entities,
/// structural markers (validation outcomes, re-grounds), and recency. The
/// newest few episodes always earn a place so the index reflects the live tail.
fn select_episodes(
    episodes: &[Episode],
    query: Option<&str>,
    state: &TaskState,
    budget_tokens: usize,
    estimator: &TokenEstimator,
) -> Vec<Episode> {
    let newest_start = episodes.len().saturating_sub(5);
    let mut scored: Vec<(i64, usize, &Episode)> = episodes
        .iter()
        .enumerate()
        .map(|(rank, episode)| {
            let mut score = score_episode(episode, query, state);
            if rank >= newest_start {
                score += 4;
            }
            (score, rank, episode)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut selected = Vec::new();
    let mut used = 0usize;
    for (score, _, episode) in scored {
        if selected.len() >= MAX_EPISODE_ENTRIES {
            break;
        }
        let cost = estimator.estimate(&episode.summary);
        if score <= 0 || used + cost > budget_tokens {
            continue;
        }
        used += cost;
        selected.push(episode.clone());
    }
    selected.sort_by_key(|episode| episode.start_sequence);
    selected
}

fn score_episode(episode: &Episode, query: Option<&str>, state: &TaskState) -> i64 {
    let mut score: i64 = 0;
    let terms = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|term| term.len() > 3)
            .map(|term| term.to_ascii_lowercase())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    };
    let haystack: Vec<String> = terms(&episode.topic)
        .into_iter()
        .chain(
            episode
                .entities
                .iter()
                .map(|entity| entity.to_ascii_lowercase()),
        )
        .collect();
    if let Some(query) = query {
        for term in terms(query) {
            if haystack.iter().any(|candidate| candidate.contains(&term)) {
                score += 3;
            }
        }
    }
    for file in &state.touched_files {
        if episode
            .entities
            .iter()
            .any(|entity| entity.contains(file.as_str()))
        {
            score += 2;
        }
    }
    for marker in &episode.markers {
        score += match marker.as_str() {
            "validation-failed" | "reground" => 3,
            "validation-passed" | "evidence" => 2,
            "failure" | "mutation" => 1,
            _ => 0,
        };
    }
    score
}

fn select_recent(events: &[Event], budget_tokens: usize, estimator: &TokenEstimator) -> Vec<Event> {
    let units = conversation_units(events);
    let mut size = 0;
    let mut selected = Vec::new();
    for &(start, end) in units.iter().rev() {
        let n = events[start..end]
            .iter()
            .map(|event| estimator.estimate(&render_event(event)))
            .sum::<usize>();
        if !selected.is_empty() && size + n > budget_tokens {
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

/// Segments old events into structured episodes. Boundaries align with new
/// user intents (user messages) or volume, so episodes describe coherent spans
/// instead of arbitrary 20-event chunks.
fn build_episodes(events: &[Event]) -> Vec<Episode> {
    let mut episodes = Vec::new();
    let mut current: Vec<&Event> = Vec::new();
    let flush = |current: &mut Vec<&Event>, episodes: &mut Vec<Episode>| {
        if current.is_empty() {
            return;
        }
        let topic = current
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::UserMessage { text } => {
                    Some(text.chars().take(100).collect::<String>())
                }
                _ => None,
            })
            .unwrap_or_else(|| "session activity".into());
        let mut entities = Vec::new();
        let mut tools = Vec::new();
        let mut markers = Vec::new();
        for event in current.iter() {
            match &event.payload {
                EventPayload::FileObserved { version } => entities.push(version.path.clone()),
                EventPayload::FileChanged { after, .. } => {
                    entities.push(after.path.clone());
                    markers.push("mutation".into());
                }
                EventPayload::ToolCompleted { result } => tools.push(result.name.clone()),
                EventPayload::ToolFailed { result } => {
                    tools.push(result.name.clone());
                    markers.push("failure".into());
                }
                EventPayload::ValidationResult { passed, .. } => markers.push(if *passed {
                    "validation-passed".into()
                } else {
                    "validation-failed".into()
                }),
                EventPayload::EvidenceCreated { evidence } => {
                    markers.push(format!("evidence:{:?}", evidence.status));
                }
                EventPayload::FailureAttempt { .. } => markers.push("failure".into()),
                EventPayload::RegroundRequested { .. } => markers.push("reground".into()),
                _ => {}
            }
        }
        for list in [&mut entities, &mut tools] {
            list.sort();
            list.dedup();
        }
        markers.dedup();
        let summary = format!(
            "- #{}-#{}: {}{}{}",
            current[0].sequence,
            current[current.len() - 1].sequence,
            topic,
            if entities.is_empty() {
                String::new()
            } else {
                format!(
                    " [files: {}]",
                    entities
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
            if markers.is_empty() {
                String::new()
            } else {
                format!(" [{}]", markers.join(", "))
            },
        );
        episodes.push(Episode {
            start_sequence: current[0].sequence,
            end_sequence: current[current.len() - 1].sequence,
            topic,
            entities,
            tools,
            markers,
            summary,
        });
        current.clear();
    };
    for event in events {
        let boundary =
            matches!(&event.payload, EventPayload::UserMessage { .. }) || current.len() >= 48;
        if boundary && !current.is_empty() {
            flush(&mut current, &mut episodes);
        }
        current.push(event);
    }
    flush(&mut current, &mut episodes);
    episodes
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

/// Memory kinds in canonical render priority; the tail is dropped first when
/// the canonical budget is tight.
fn memory_priority(kind: &MemoryKind) -> u8 {
    match kind {
        MemoryKind::UserConstraint => 0,
        MemoryKind::UserFact => 1,
        MemoryKind::Decision => 2,
        MemoryKind::TaskConstraint => 3,
        MemoryKind::Hypothesis => 4,
        MemoryKind::ObservedFact => 5,
        MemoryKind::ModelNote => 6,
    }
}

fn render_canonical(
    state: &TaskState,
    memories: &[MemoryRecord],
    evidence: &EvidenceLedger,
    failures: &FailureManager,
    bridge: &ConversationBridge,
    cap_tokens: usize,
    estimator: &TokenEstimator,
) -> String {
    // Compact JSON: canonical state for the model, without prettify padding.
    let state_json = serde_json::to_string(state).unwrap_or_default();
    let evidence_lines = evidence.current_summary();
    let failure_lines = failures
        .active_lineages()
        .into_iter()
        .map(|(subject, count)| format!("- {subject} ×{count}"))
        .collect::<Vec<_>>();
    let mut ordered: Vec<(u8, &MemoryRecord)> = memories
        .iter()
        .filter(|m| {
            !matches!(
                m.validity,
                Validity::Stale | Validity::Superseded | Validity::Rejected
            )
        })
        .map(|m| (memory_priority(&m.kind), m))
        .collect();
    // Canonical priority order: user constraints and decisions first; notes
    // last so budget pressure trims them first.
    ordered.sort_by_key(|(priority, _)| *priority);
    let mut memory_lines: Vec<String> = ordered
        .into_iter()
        .map(|(_, m)| {
            let content: String = m.content.chars().take(MAX_MEMORY_CONTENT).collect();
            format!("- {:?} {}", m.kind, content)
        })
        .collect();
    while memory_lines.len() > MAX_MEMORY_LINES {
        memory_lines.pop();
    }
    let bridge_json = serde_json::to_string(bridge).unwrap_or_default();
    let mut sections = vec![
        format!("CANONICAL TASK STATE\n{state_json}"),
        format!(
            "CURRENT EVIDENCE (kernel-derived; latest observation per claim)\n{}",
            if evidence_lines.is_empty() {
                "- none".into()
            } else {
                evidence_lines.join("\n")
            }
        ),
        format!(
            "ACTIVE FAILURE LINEAGES\n{}",
            if failure_lines.is_empty() {
                "- none".into()
            } else {
                failure_lines.join("\n")
            }
        ),
        format!(
            "DURABLE MEMORY (provenance preserved)\n{}",
            if memory_lines.is_empty() {
                "- none".into()
            } else {
                memory_lines.join("\n")
            }
        ),
        format!("CONVERSATION BRIDGE (navigation only)\n{bridge_json}"),
    ];
    // Drop lowest-priority sections until the canonical view fits its share.
    while sections.len() > 1
        && sections
            .iter()
            .map(|section| estimator.estimate(section))
            .sum::<usize>()
            + 4 * (sections.len() - 1)
            > cap_tokens
    {
        // Priority: bridge first, then memory, then failures, then evidence;
        // task state JSON always stays.
        let drop_index = sections
            .iter()
            .position(|s| s.starts_with("CONVERSATION BRIDGE"))
            .or_else(|| {
                sections
                    .iter()
                    .position(|s| s.starts_with("DURABLE MEMORY"))
            })
            .or_else(|| {
                sections
                    .iter()
                    .position(|s| s.starts_with("ACTIVE FAILURE"))
            })
            .or_else(|| {
                sections
                    .iter()
                    .position(|s| s.starts_with("CURRENT EVIDENCE"))
            })
            .unwrap_or(1);
        sections.remove(drop_index);
    }
    sections.join("\n\n")
}

fn render_recalled(
    recalled: &[Event],
    cap_tokens: usize,
    estimator: &TokenEstimator,
) -> (String, Vec<Event>) {
    let mut text = String::new();
    let mut used = 0usize;
    let mut selected = Vec::new();
    for event in recalled {
        let rendered = render_event(event);
        let cost = estimator.estimate(&rendered);
        if !selected.is_empty() && used + cost > cap_tokens {
            break;
        }
        used += cost;
        text.push_str(&rendered);
        text.push('\n');
        selected.push(event.clone());
    }
    (text, selected)
}

/// Approximates one event's provider-facing text. Assistant turns include the
/// replayed reasoning content and each tool call's full arguments, because the
/// provider bills and caches them; omitting them undercounted real requests.
fn render_event(e: &Event) -> String {
    match &e.payload {
        EventPayload::UserMessage { text } => format!("user: {text}"),
        EventPayload::AssistantMessageCompleted {
            text,
            tool_calls,
            reasoning_content,
        } => {
            let mut rendered = format!("assistant: {text}");
            if let Some(reasoning) = reasoning_content {
                rendered.push_str("\nreasoning: ");
                rendered.push_str(reasoning);
            }
            for call in tool_calls {
                rendered.push_str(&format!(
                    "\ncall {} {} {}",
                    call.id, call.name, call.arguments
                ));
            }
            rendered
        }
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
    use uuid::Uuid;

    fn budget(request_tokens: usize, recent_tokens: usize) -> MaterializeBudget {
        MaterializeBudget {
            request_tokens,
            window_tokens: request_tokens.saturating_add(12_192),
            reserve_tokens: 12_192,
            recent_tokens,
            reserved_tokens: 0,
        }
    }

    fn memory(
        session: Uuid,
        event: Uuid,
        kind: MemoryKind,
        text: &str,
        validity: Validity,
    ) -> MemoryRecord {
        MemoryRecord {
            id: Uuid::new_v4(),
            session_id: session,
            kind,
            content: text.into(),
            originating_event: event,
            created_at: Utc::now(),
            validity,
            confidence: None,
            dependencies: vec![],
            supersedes: None,
        }
    }

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
                EventPayload::UserMessage {
                    text: "Approach C: rewrite the fixture".into(),
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
        store
            .add_memory(&memory(
                sid,
                c.id,
                MemoryKind::UserConstraint,
                "Constraint A: preserve wire compatibility",
                Validity::Active,
            ))
            .unwrap();
        store
            .add_memory(&memory(
                sid,
                d.id,
                MemoryKind::Decision,
                "Decision B: use framed stdio",
                Validity::Active,
            ))
            .unwrap();
        store
            .add_memory(&memory(
                sid,
                rejected.id,
                MemoryKind::Hypothesis,
                "Approach C: rewrite the fixture",
                Validity::Rejected,
            ))
            .unwrap();
        for i in 0..2000 {
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
            add_hypotheses: vec!["Approach C: rewrite the fixture".into()],
            reject_hypotheses: vec!["Approach C: rewrite the fixture".into()],
            ..Default::default()
        });
        let engine = ContinuityEngine::new(
            store.clone(),
            ContextConfig {
                max_request_tokens: Some(24_000),
                recent_tokens: 4_000,
                reserve_tokens: 2_000,
                output_reserve_tokens: 0,
            },
        );
        let ctx = engine
            .materialize(
                sid,
                state.state(),
                Some("EADDRINUSE 4317"),
                &EvidenceLedger::default(),
                &crate::state::FailureManager::new(3),
                "system".into(),
                &budget(24_000, 4_000),
            )
            .unwrap();
        assert!(ctx.canonical.contains("Constraint A"));
        assert!(ctx.canonical.contains("Decision B"));
        // The rejected hypothesis stays rejected: it appears only inside the
        // task-state's rejected list, never as active memory.
        assert!(ctx.canonical.contains("rejected"));
        let memory_section = ctx
            .canonical
            .split("DURABLE MEMORY")
            .nth(1)
            .unwrap_or_default();
        assert!(
            !memory_section.contains("Approach C"),
            "rejected hypothesis must not render as active memory"
        );
        assert!(ctx.recalled.contains("exact diagnostic D"));
        assert!(ctx.recent.iter().any(|e| matches!(
            &e.payload,
            EventPayload::UserMessage { text } if text.contains("unrelated conversation 1999")
        )));
        let all = store.events(sid).unwrap();
        // 4 seed events + 2000 fillers + the recall bookkeeping event.
        assert_eq!(all.len(), 2005);
        assert!(all.iter().any(|e| e.id == diagnostic.id));
        assert!(
            !all.iter()
                .any(|e| matches!(e.payload, EventPayload::ManualCompact { .. }))
        );
        assert!(ctx.recent.len() < all.len());
        // Bounded invariant: the estimated request fits the token budget even
        // with 2000 durable events, and the component sum is exact.
        let materialized = ctx.stats.total_tokens;
        assert!(
            materialized <= 24_000,
            "materialized {materialized} exceeded budget"
        );
        let component_sum = ctx.stats.instructions_tokens
            + ctx.stats.state_tokens
            + ctx.stats.recent_tokens
            + ctx.stats.recall_tokens
            + ctx.stats.tools_tokens
            + ctx.stats.extension_tokens;
        assert_eq!(
            materialized, component_sum,
            "recalled material must be counted exactly once"
        );
        assert_eq!(ctx.stats.headroom_tokens, 24_000 - materialized);
        // Episode index is a bounded selection, not every episode.
        assert!(ctx.episodes.len() <= MAX_EPISODE_ENTRIES);
        assert!(ctx.stats.episodes > ctx.episodes.len() || ctx.stats.episodes < 20);
    }

    #[test]
    fn materialized_context_is_bounded_under_thousands_of_events() {
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/fixture")).unwrap();
        for i in 0..5000 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("filler event {i} {}", "y".repeat(120)),
                    },
                )
                .unwrap();
        }
        let engine = ContinuityEngine::new(
            store.clone(),
            ContextConfig {
                max_request_tokens: Some(16_000),
                recent_tokens: 4_000,
                reserve_tokens: 2_000,
                output_reserve_tokens: 0,
            },
        );
        let ctx = engine
            .materialize(
                sid,
                &TaskState::default(),
                Some("filler event 17"),
                &EvidenceLedger::default(),
                &crate::state::FailureManager::new(3),
                "system prompt".repeat(10),
                &budget(16_000, 4_000),
            )
            .unwrap();
        assert!(ctx.stats.total_tokens <= 16_000);
        assert_eq!(ctx.stats.status, "bounded");
        // 5000 durable fillers plus the one recall bookkeeping event.
        assert_eq!(store.events(sid).unwrap().len(), 5001);
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
                max_request_tokens: Some(100),
                recent_tokens: 50,
                reserve_tokens: 10,
                output_reserve_tokens: 0,
            },
        );
        e.manual_compact(id).unwrap();
        let all = s.events(id).unwrap();
        assert_eq!(all.len(), 2);
        assert!(matches!(all[1].payload, EventPayload::ManualCompact { .. }));
        let context = e
            .materialize(
                id,
                &TaskState::default(),
                None,
                &EvidenceLedger::default(),
                &crate::state::FailureManager::new(3),
                "system".into(),
                &budget(100, 50),
            )
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
    fn render_event_prices_reasoning_and_tool_arguments() {
        let session = Uuid::new_v4();
        let event = event(
            session,
            1,
            EventPayload::AssistantMessageCompleted {
                text: "thinking".into(),
                tool_calls: vec![latch_protocol::ToolCall {
                    id: "c1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({"path":"src/lib.rs","content":"x".repeat(2000)}),
                }],
                reasoning_content: Some("because ".repeat(100)),
            },
        );
        let rendered = render_event(&event);
        assert!(rendered.contains("reasoning:"), "{rendered}");
        assert!(rendered.contains("call c1 write"), "{rendered}");
        assert!(
            rendered.contains(&"x".repeat(100)),
            "tool arguments are included in the recent-context estimate"
        );
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
        let estimator = TokenEstimator::generic();
        let transaction = estimator.estimate(&render_event(&events[1]))
            + estimator.estimate(&render_event(&events[2]));
        let tail = estimator.estimate(&render_event(&events[3]));

        // Budget fits the trailing assistant reply and the tool result but not
        // the assistant tool-call message: the transaction must be dropped
        // whole, never leaving a dangling tool result.
        let split = select_recent(&events, tail + transaction - 1, &estimator);
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
        let whole = select_recent(&events, tail + transaction, &estimator);
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
            let recent = select_recent(&events, budget, &TokenEstimator::generic());
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

    #[test]
    fn episodes_align_with_user_intents_and_carry_structure() {
        let session = Uuid::new_v4();
        let mut events = vec![event(
            session,
            1,
            EventPayload::UserMessage {
                text: "fix the flaky test".into(),
            },
        )];
        events.push(event(
            session,
            2,
            EventPayload::ValidationResult {
                command: "cargo test".into(),
                passed: false,
                detail: "1 failed".into(),
            },
        ));
        events.push(event(
            session,
            3,
            EventPayload::RegroundRequested {
                signature: "cargo test: FAIL".into(),
            },
        ));
        events.push(event(
            session,
            4,
            EventPayload::FileChanged {
                before: None,
                after: latch_protocol::FileVersion {
                    path: "src/lib.rs".into(),
                    content_hash: "abc".into(),
                    size: 3,
                },
                owner: latch_protocol::ChangeOwner::Latch,
                undo_artifact: None,
                additions: 1,
                deletions: 0,
                preview: String::new(),
                call_id: None,
            },
        ));
        events.push(event(
            session,
            5,
            EventPayload::UserMessage {
                text: "now document it".into(),
            },
        ));
        let episodes = build_episodes(&events);
        assert_eq!(episodes.len(), 2, "each user intent starts an episode");
        assert_eq!(episodes[0].topic, "fix the flaky test");
        assert!(
            episodes[0]
                .markers
                .contains(&"validation-failed".to_owned())
        );
        assert!(episodes[0].markers.contains(&"reground".to_owned()));
        assert!(episodes[0].entities.contains(&"src/lib.rs".to_owned()));
        assert_eq!(episodes[1].topic, "now document it");
    }

    #[test]
    fn canonical_budget_drops_notes_before_core() {
        let state = TaskState {
            goal: "ship it".into(),
            constraints: vec!["keep API stable".into()],
            ..TaskState::default()
        };
        let memories: Vec<MemoryRecord> = (0..200)
            .map(|i| {
                memory(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    MemoryKind::ModelNote,
                    &format!("note {i} {}", "n".repeat(200)),
                    Validity::Active,
                )
            })
            .collect();
        let rendered = render_canonical(
            &state,
            &memories,
            &EvidenceLedger::default(),
            &crate::state::FailureManager::new(3),
            &ConversationBridge::default(),
            2_000,
            &TokenEstimator::generic(),
        );
        assert!(rendered.contains("ship it"));
        assert!(rendered.contains("keep API stable"));
        assert!(
            TokenEstimator::generic().estimate(&rendered) < 30_000,
            "canonical must be capped"
        );
    }
}
