use crate::config::ContextConfig;
use crate::context::{
    ContextBudget, ContextEngine, ContextEngineFactory, ContextEngineSpec, ContextRequest,
    ContextView,
};
use crate::state::{EvidenceLedger, FailureManager};
use crate::store::EventStore;
use crate::tokens::TokenEstimator;
use anyhow::{Result, anyhow};
use latch_protocol::{
    ContextStats, Event, EventPayload, KernelContextKind, MemoryKind, MemoryRecord, TaskState,
    Validity,
};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

/// The context-engine vocabulary lives in [`crate::context`]. These names are
/// re-exported under the historical continuity names so existing callers,
/// tests, and durable semantics are untouched by the port extraction.
pub use crate::context::{
    ContextBudget as MaterializeBudget, ContextView as MaterializedContext, ConversationBridge,
    Episode,
};

/// Upper bound on episode index entries materialized into context.
const MAX_EPISODE_ENTRIES: usize = 16;
/// Event count that closes an episode even without a new user intent.
const EPISODE_MAX_EVENTS: usize = 48;
/// Upper bound on durable memories rendered into canonical state.
const MAX_MEMORY_LINES: usize = 64;
/// Per-record content truncation for canonical memory lines.
const MAX_MEMORY_CONTENT: usize = 400;

pub struct ContinuityEngine {
    store: EventStore,
    config: ContextConfig,
    estimator: TokenEstimator,
    generation: u32,
    /// Cached archival episode index. It only ever advances over the immutable
    /// event log and is rebuilt deterministically when the session changes or
    /// history rolls back (a larger working-memory budget).
    episodes: std::sync::Mutex<EpisodeCache>,
}
impl ContinuityEngine {
    #[must_use]
    pub fn config(&self) -> &ContextConfig {
        &self.config
    }
    #[must_use]
    pub fn new(store: EventStore, config: ContextConfig) -> Self {
        Self {
            store,
            config,
            estimator: TokenEstimator::generic(),
            generation: 0,
            episodes: std::sync::Mutex::new(EpisodeCache::default()),
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
            episodes: std::sync::Mutex::new(EpisodeCache::default()),
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

    /// Materializes one bounded request view from durable state. Used by the
    /// `/context` inspector and tests; real requests use
    /// [`Self::materialize_dynamic`] so extension context and re-ground
    /// instructions become part of the durable provider-visible history.
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
        self.materialize_dynamic(
            session_id,
            state,
            query,
            evidence,
            failures,
            system,
            String::new(),
            budget,
            "",
            None,
        )
    }

    /// Materializes one provider-visible request view within a durable cache
    /// epoch.
    ///
    /// Invariant: `instructions + state + recent + recall + tools + extension`
    /// stays within the request budget. Within an epoch the provider-visible
    /// history is append-only: ordinary turns never rewrite or remove material
    /// that was already sent, and kernel-owned context (canonical state,
    /// recalls, archival index, extension sources, re-ground instructions) is
    /// persisted as [`EventPayload::KernelContext`] messages instead of a
    /// synthetic tail that disappears on the next request. When the epoch
    /// approaches the configured working-memory budget it rotates once with
    /// hysteresis, retaining whole semantic units and emitting a complete
    /// authoritative snapshot. Raw events are never deleted or summarized
    /// away; rotation only changes which durable events are inside the current
    /// provider-visible epoch.
    #[allow(clippy::too_many_arguments)]
    pub fn materialize_dynamic(
        &self,
        session_id: Uuid,
        state: &TaskState,
        query: Option<&str>,
        evidence: &EvidenceLedger,
        failures: &FailureManager,
        system: String,
        session_context: String,
        budget: &MaterializeBudget,
        extension_context: &str,
        reground: Option<&str>,
    ) -> Result<MaterializedContext> {
        let estimator = self.estimator;
        let own_budget = budget.request_tokens.saturating_sub(budget.reserved_tokens);
        let memories = self.store.memories(session_id)?;
        // The session context is a provider-visible message, not part of the
        // system prefix, but it still consumes request budget: account for it
        // with the instructions.
        let instructions_tokens = estimator
            .estimate(&system)
            .saturating_add(estimator.estimate(&session_context));
        // A deterministic canonical cap keeps emitted kernel state independent
        // of per-turn extension/tool reservations, so unchanged state does not
        // churn the durable kernel history.
        let canonical_cap = budget.request_tokens.saturating_mul(2) / 5;
        let high_water = budget.recent_tokens.min(own_budget.max(1));

        // Current durable cache epoch. `/compact` forces a later start and a
        // new generation even before the budget is reached.
        let latest_epoch = self
            .store
            .latest_event_of_kinds(session_id, &["context_epoch_started"])?;
        let (mut epoch_start, mut generation, mut rotation_reason, mut rotation_retained) =
            match &latest_epoch {
                Some(event) => match &event.payload {
                    EventPayload::ContextEpochStarted {
                        from_sequence,
                        generation,
                        reason,
                        retained_tokens,
                    } => (
                        *from_sequence,
                        *generation,
                        reason.clone(),
                        *retained_tokens,
                    ),
                    _ => (1, 0, String::new(), 0),
                },
                None => (1, 0, String::new(), 0),
            };
        let compact_after = self
            .store
            .latest_event_of_kinds(session_id, &["manual_compact"])?
            .map(|event| event.sequence + 1)
            .filter(|start| *start > epoch_start);
        if let Some(start) = compact_after {
            epoch_start = start;
            generation = generation.saturating_add(1);
        }
        // A provider/model/effort change alters wire semantics (reasoning
        // replay, tokenizer family, pricing), so the previous epoch's cache
        // accounting must not be reported as reusable across the boundary. The
        // next materialization rotates once, retaining the working set and
        // emitting a fresh snapshot under the new profile.
        let profile_change = self
            .store
            .latest_event_of_kinds(session_id, &["inference_profile_changed"])?;
        let profile_pending = match (&profile_change, &latest_epoch) {
            (Some(profile), Some(epoch)) => profile.sequence > epoch.sequence,
            (Some(_), None) => true,
            _ => false,
        };

        let (mut recent, reached_start) = load_epoch_events(
            &self.store,
            session_id,
            epoch_start,
            generation,
            high_water,
            &estimator,
        )?;
        let mut epoch_tokens = estimated_events_tokens(&recent, &estimator);
        let bridge = conversation_bridge(state, &recent);
        let canonical = render_canonical(
            state,
            &memories,
            evidence,
            failures,
            &bridge,
            canonical_cap,
            &estimator,
        );
        let extension_present =
            !extension_context.trim().is_empty() && extension_context.trim() != "[]";

        let compact_pending = compact_after.is_some();
        // Project the kernel messages this turn would append so the rotation
        // decision is a pure function of durable state plus the current
        // canonical render, not of when a prior turn happened to run.
        let prior_state = last_kernel_body(
            &recent,
            generation,
            &[KernelContextKind::Snapshot, KernelContextKind::StateUpdate],
        );
        let state_changed = prior_state != Some(canonical.as_str());
        let prior_extension =
            last_kernel_body(&recent, generation, &[KernelContextKind::Extension]);
        let extension_changed = extension_present && prior_extension != Some(extension_context);
        // Conversation bytes are what rotation can actually reclaim; an epoch
        // that is over budget only because authoritative kernel state is large
        // must not rotate forever.
        let conversation_tokens: usize = recent
            .iter()
            .filter(|event| !matches!(event.payload, EventPayload::KernelContext { .. }))
            .map(|event| event_tokens(event, &estimator))
            .sum();
        // The high-water mark governs conversation working memory. Kernel
        // messages (current state, recall, archive index) are authoritative
        // and counted in the request budget, but they never force a rotation:
        // current truth must not be crowded out by cache policy.
        let over_budget = !reached_start || conversation_tokens > high_water;
        let rotate = compact_pending || profile_pending || over_budget;
        let mut evicted_tokens = 0usize;

        if rotate {
            let reason = if compact_pending {
                "manual compact".to_owned()
            } else if profile_pending {
                "inference profile changed".to_owned()
            } else {
                "working budget high-water mark reached".to_owned()
            };
            // Retain a large useful working set but leave roughly a quarter of
            // the budget as growth headroom so rotation is occasional rather
            // than per-turn. Retention still respects whole semantic units and
            // reserves room for the snapshot itself.
            // Reserve room for the snapshot, extension sources, and the
            // archival index, plus one eighth of the budget as growth
            // headroom. Post-rotation epochs are therefore at or below the
            // high-water mark, so rotation is occasional, never per-turn.
            // Retain three quarters of the conversation budget so a
            // saturated session rotates once per quarter-budget of growth
            // rather than every turn, while the retained working set stays
            // large. Semantic units are still selected whole.
            let headroom = high_water / 4;
            let retain_budget = if compact_pending {
                0
            } else {
                high_water.saturating_sub(headroom)
            };
            let mut retained = if retain_budget == 0 {
                Vec::new()
            } else {
                bounded_recent(&recent, retain_budget, &estimator).0
            };
            // Prior kernel messages are superseded by the fresh snapshot; the
            // new epoch carries verbatim conversation plus current truth only.
            retained.retain(|event| !matches!(event.payload, EventPayload::KernelContext { .. }));
            let retained_tokens: usize = retained
                .iter()
                .map(|event| event_tokens(event, &estimator))
                .sum();
            let from_sequence = retained
                .first()
                .map(|event| event.sequence)
                .unwrap_or_else(|| self.store.last_sequence(session_id).unwrap_or(0) + 1);
            generation = generation.saturating_add(1);
            self.store.append(
                session_id,
                EventPayload::ContextEpochStarted {
                    from_sequence,
                    reason: reason.clone(),
                    generation,
                    retained_tokens,
                },
            )?;
            rotation_reason = reason;
            rotation_retained = retained_tokens;
            evicted_tokens = epoch_tokens.saturating_sub(retained_tokens);
            epoch_start = from_sequence;
            recent = retained;

            let mut revision = 1u64;
            let content = kernel_content(
                generation,
                revision,
                KernelContextKind::Snapshot,
                &canonical,
            );
            recent.push(self.emit_kernel(
                session_id,
                generation,
                revision,
                KernelContextKind::Snapshot,
                content,
            )?);
            revision += 1;
            if extension_present {
                let content = kernel_content(
                    generation,
                    revision,
                    KernelContextKind::Extension,
                    extension_context,
                );
                recent.push(self.emit_kernel(
                    session_id,
                    generation,
                    revision,
                    KernelContextKind::Extension,
                    content,
                )?);
                revision += 1;
            }
            if let Some(instruction) = reground {
                let content = kernel_content(
                    generation,
                    revision,
                    KernelContextKind::Reground,
                    instruction,
                );
                recent.push(self.emit_kernel(
                    session_id,
                    generation,
                    revision,
                    KernelContextKind::Reground,
                    content,
                )?);
            }
        } else {
            let mut revision = next_kernel_revision(&recent, generation);
            // Current authoritative state: emit a snapshot if this epoch has
            // none yet (legacy sessions), otherwise only when it changed.
            if state_changed {
                let kind = if prior_state.is_none() {
                    KernelContextKind::Snapshot
                } else {
                    KernelContextKind::StateUpdate
                };
                let content = kernel_content(generation, revision, kind, &canonical);
                recent.push(self.emit_kernel(session_id, generation, revision, kind, content)?);
                revision += 1;
            }
            if extension_changed {
                let content = kernel_content(
                    generation,
                    revision,
                    KernelContextKind::Extension,
                    extension_context,
                );
                recent.push(self.emit_kernel(
                    session_id,
                    generation,
                    revision,
                    KernelContextKind::Extension,
                    content,
                )?);
                revision += 1;
            }
            if let Some(instruction) = reground
                && last_kernel_body(&recent, generation, &[KernelContextKind::Reground])
                    != Some(instruction)
            {
                let content = kernel_content(
                    generation,
                    revision,
                    KernelContextKind::Reground,
                    instruction,
                );
                recent.push(self.emit_kernel(
                    session_id,
                    generation,
                    revision,
                    KernelContextKind::Reground,
                    content,
                )?);
            }
        }

        // Archival episode index: episodes cover everything older than the
        // provider-visible epoch, independently of cache layout.
        let archive_end = epoch_start.saturating_sub(1);
        let (selected, episode_count, episode_index) = {
            let mut cache = self
                .episodes
                .lock()
                .map_err(|_| anyhow!("episode index lock poisoned"))?;
            cache.advance(&self.store, session_id, archive_end, &estimator)?;
            let builder = &cache.builder;
            let open = builder.snapshot();
            epoch_tokens = estimated_events_tokens(&recent, &estimator);
            let available = own_budget
                .saturating_sub(instructions_tokens)
                .saturating_sub(epoch_tokens);
            let selected = select_episodes(
                &builder.closed,
                open.as_ref(),
                query,
                state,
                available.min(high_water / 8),
                &estimator,
            );
            let episode_index = selected
                .iter()
                .map(|episode| episode.summary.clone())
                .collect::<Vec<_>>()
                .join("\n");
            let count = builder.closed.len() + usize::from(open.is_some());
            (selected, count, episode_index)
        };
        if !episode_index.is_empty()
            && last_kernel_body(&recent, generation, &[KernelContextKind::EpisodeIndex])
                != Some(episode_index.as_str())
        {
            let revision = next_kernel_revision(&recent, generation);
            let content = kernel_content(
                generation,
                revision,
                KernelContextKind::EpisodeIndex,
                &episode_index,
            );
            recent.push(self.emit_kernel(
                session_id,
                generation,
                revision,
                KernelContextKind::EpisodeIndex,
                content,
            )?);
        }

        // Recall never duplicates the current user turn or anything still in
        // the provider-visible epoch; it reaches into the archival region.
        let mut recalled_text = String::new();
        if let Some(q) = query {
            let current_user_sequence = recent
                .iter()
                .rev()
                .find(|event| event_user_text(&event.payload).is_some())
                .map(|event| event.sequence);
            let recent_ids: HashSet<Uuid> = recent.iter().map(|event| event.id).collect();
            let recalled = self
                .recall(session_id, q)?
                .into_iter()
                .filter(|event| {
                    current_user_sequence.is_none_or(|sequence| event.sequence < sequence)
                })
                .filter(|event| !recent_ids.contains(&event.id))
                .collect::<Vec<_>>();
            self.record_recall(session_id, q, &memories, &recalled)?;
            if !recalled.is_empty() {
                let used = estimated_events_tokens(&recent, &estimator);
                let available = own_budget
                    .saturating_sub(instructions_tokens)
                    .saturating_sub(used);
                let (text, _) = render_recalled(&recalled, available * 3 / 4, &estimator);
                if !text.is_empty()
                    && last_kernel_body(&recent, generation, &[KernelContextKind::Recall])
                        != Some(text.as_str())
                {
                    let revision = next_kernel_revision(&recent, generation);
                    let content =
                        kernel_content(generation, revision, KernelContextKind::Recall, &text);
                    recent.push(self.emit_kernel(
                        session_id,
                        generation,
                        revision,
                        KernelContextKind::Recall,
                        content,
                    )?);
                }
                recalled_text = text;
            }
        }

        // Component accounting partitions the provider-visible epoch exactly,
        // so the total is always the sum of its parts.
        let mut state_used = 0usize;
        let mut recent_used = 0usize;
        let mut recall_used = 0usize;
        let mut episode_used = 0usize;
        let mut extension_used = 0usize;
        let mut conversation_used = 0usize;
        let mut reasoning_replay_used = 0usize;
        let mut tool_arguments_used = 0usize;
        let mut tool_result_used = 0usize;
        let mut image_count = 0usize;
        let mut image_tokens = 0usize;
        for event in &recent {
            let tokens = event_tokens(event, &estimator);
            let media = event_media(event);
            image_count += media.len();
            image_tokens += estimator.estimate_media(media);
            match &event.payload {
                EventPayload::KernelContext {
                    kind: KernelContextKind::Snapshot | KernelContextKind::StateUpdate,
                    ..
                } => state_used += tokens,
                EventPayload::KernelContext {
                    kind: KernelContextKind::Extension,
                    ..
                } => extension_used += tokens,
                EventPayload::KernelContext {
                    kind: KernelContextKind::Recall,
                    ..
                } => recall_used += tokens,
                EventPayload::KernelContext {
                    kind: KernelContextKind::EpisodeIndex,
                    ..
                } => {
                    recall_used += tokens;
                    episode_used += tokens;
                }
                EventPayload::ToolCompleted { .. } | EventPayload::ToolFailed { .. } => {
                    recent_used += tokens;
                    tool_result_used += tokens;
                }
                EventPayload::AssistantMessageCompleted {
                    text,
                    tool_calls,
                    reasoning_content,
                    ..
                } => {
                    recent_used += tokens;
                    let reasoning = reasoning_content
                        .as_deref()
                        .map_or(0, |reasoning| estimator.estimate(reasoning));
                    let arguments: usize = tool_calls
                        .iter()
                        .map(|call| {
                            estimator.estimate(&call.name)
                                + estimator.estimate_json(&call.arguments)
                        })
                        .sum();
                    conversation_used += tokens.saturating_sub(reasoning).saturating_sub(arguments);
                    reasoning_replay_used += reasoning;
                    tool_arguments_used += arguments;
                    let _ = text;
                }
                _ => {
                    recent_used += tokens;
                    conversation_used += tokens;
                }
            }
        }
        let cache_epoch_tokens = state_used + recent_used + recall_used + extension_used;
        let turns = self.store.count_events_after_of_kind(
            session_id,
            "user_message",
            epoch_start.saturating_sub(1),
        )?;
        let durable_events = self.store.last_sequence(session_id)? as usize;
        let mut stats = ContextStats {
            instructions_tokens,
            state_tokens: state_used,
            recent_tokens: recent_used,
            conversation_tokens: conversation_used,
            reasoning_replay_tokens: reasoning_replay_used,
            tool_arguments_tokens: tool_arguments_used,
            tool_result_tokens: tool_result_used,
            image_count,
            image_tokens,
            recall_tokens: recall_used,
            episode_tokens: episode_used,
            recent_start_sequence: epoch_start,
            recent_evicted_tokens: evicted_tokens,
            cache_epoch: generation,
            cache_epoch_tokens,
            cache_epoch_turns: turns,
            cache_rotation_reason: rotation_reason,
            cache_rotation_retained_tokens: rotation_retained,
            tools_tokens: 0,
            extension_tokens: extension_used,
            total_tokens: 0,
            request_tokens: 0,
            common_prefix_tokens: 0,
            budget_tokens: budget.request_tokens,
            window_tokens: budget.window_tokens,
            reserve_tokens: budget.reserve_tokens,
            headroom_tokens: 0,
            durable_events,
            episodes: episode_count,
            selected_episodes: selected.len(),
            estimated: true,
            status: String::new(),
        };
        stats.recompute();
        Ok(MaterializedContext {
            system,
            session_context,
            canonical,
            recalled: recalled_text,
            recent,
            bridge,
            episodes: selected,
            stats,
        })
    }

    /// Appends one durable kernel context message to the provider-visible
    /// history for this epoch.
    fn emit_kernel(
        &self,
        session_id: Uuid,
        generation: u64,
        revision: u64,
        kind: KernelContextKind,
        content: String,
    ) -> Result<Event> {
        self.store.append(
            session_id,
            EventPayload::KernelContext {
                generation,
                revision,
                kind,
                content,
            },
        )
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

/// [`ContinuityEngine`] is the default Latch implementation of the context port.
/// It delegates to the inherent methods unchanged; the port exists so the agent
/// runtime depends on the contract rather than this concrete type.
impl ContextEngine for ContinuityEngine {
    fn name(&self) -> &str {
        "continuity"
    }

    fn config(&self) -> &ContextConfig {
        ContinuityEngine::config(self)
    }

    fn set_config(&mut self, config: ContextConfig) {
        ContinuityEngine::set_config(self, config);
    }

    fn set_estimator(&mut self, estimator: TokenEstimator) {
        ContinuityEngine::set_estimator(self, estimator);
    }

    fn default_budget(&self, window_tokens: usize, reserved_tokens: usize) -> ContextBudget {
        ContinuityEngine::default_budget(self, window_tokens, reserved_tokens)
    }

    fn manual_compact(&mut self, session_id: Uuid) -> Result<()> {
        ContinuityEngine::manual_compact(self, session_id)
    }

    fn recall(&self, session_id: Uuid, query: &str) -> Result<Vec<Event>> {
        ContinuityEngine::recall(self, session_id, query)
    }

    fn materialize(&self, request: ContextRequest<'_>) -> Result<ContextView> {
        self.materialize_dynamic(
            request.session_id,
            request.state,
            request.query,
            request.evidence,
            request.failures,
            request.system,
            request.session_context,
            &request.budget,
            request.extension_context,
            request.reground,
        )
    }
}

/// The default child-session context policy: a fresh [`ContinuityEngine`]
/// priced for the child's effective model over the session's durable store.
/// This is exactly the constructor the kernel used for children before the
/// factory was introduced, so the default remains behaviorally identical.
#[must_use]
pub fn continuity_context_engine_factory(store: EventStore) -> ContextEngineFactory {
    Arc::new(move |spec: &ContextEngineSpec<'_>| {
        Ok(Box::new(ContinuityEngine::for_model(
            store.clone(),
            spec.context.clone(),
            &spec.profile.model,
        )))
    })
}

/// Selects the bounded relevant subset of episodes for the index. Scoring is
/// deterministic: lexical overlap with the query, current file entities,
/// structural markers (validation outcomes, re-grounds), and recency. The
/// newest few episodes always earn a place so the index reflects the live tail.
fn select_episodes(
    closed: &[Episode],
    open: Option<&Episode>,
    query: Option<&str>,
    state: &TaskState,
    budget_tokens: usize,
    estimator: &TokenEstimator,
) -> Vec<Episode> {
    let total = closed.len() + usize::from(open.is_some());
    let newest_start = total.saturating_sub(5);
    let mut scored: Vec<(i64, usize, &Episode)> = closed
        .iter()
        .enumerate()
        .map(|(rank, episode)| {
            let mut score = score_episode(episode, query, state);
            if rank >= newest_start {
                score += 4;
            }
            (score, rank, episode)
        })
        .chain(open.map(|episode| {
            let rank = closed.len();
            let mut score = score_episode(episode, query, state);
            if rank >= newest_start {
                score += 4;
            }
            (score, rank, episode)
        }))
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

/// Events that appear in the provider-facing conversation. Kernel bookkeeping
/// (context materialization, epoch boundaries, compaction, file observations)
/// never reaches the model and must not inflate the recent working set.
fn is_model_visible_event(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::UserMessage { .. }
            | EventPayload::AgentMessageReceived { .. }
            | EventPayload::AssistantMessageCompleted { .. }
            | EventPayload::ToolCompleted { .. }
            | EventPayload::ToolFailed { .. }
            | EventPayload::RegroundRequested { .. }
            | EventPayload::KernelContext { .. }
            | EventPayload::AgentNotificationDelivered { .. }
            | EventPayload::GroupMessageDelivered { .. }
    )
}

/// Chooses the model-visible recent working memory: the largest suffix of
/// whole conversation units whose estimated size fits the budget. The newest
/// unit is always kept even when it alone exceeds the budget, so an oversized
/// tool transaction is never truncated or split. Because units only ever leave
/// from the front as new ones arrive, working memory decays gradually instead
/// of collapsing to half its size at a threshold.
fn bounded_recent(
    events: &[Event],
    budget_tokens: usize,
    estimator: &TokenEstimator,
) -> (Vec<Event>, usize) {
    let conversation: Vec<Event> = events
        .iter()
        .filter(|event| is_model_visible_event(&event.payload))
        .cloned()
        .collect();
    if conversation.is_empty() {
        return (Vec::new(), 0);
    }
    let units = conversation_units(&conversation);
    let unit_tokens: Vec<usize> = units
        .iter()
        .map(|(start, end)| {
            conversation[*start..*end]
                .iter()
                .map(|event| event_tokens(event, estimator))
                .sum::<usize>()
        })
        .collect();
    let mut first = units.len();
    let mut tokens = 0usize;
    for index in (0..units.len()).rev() {
        let unit = unit_tokens[index];
        if first != units.len() && tokens.saturating_add(unit) > budget_tokens {
            break;
        }
        tokens = tokens.saturating_add(unit);
        first = index;
    }
    let start = units[first].0;
    (conversation[start..].to_vec(), tokens)
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
            // Kernel context emitted after a transaction belongs to that
            // transaction for retention purposes, so a rotation cannot split
            // the call/result pair from the state update that explains it.
            end = absorb_kernel_events(events, end);
            units.push((index, end));
            index = end;
        } else if matches!(events[index].payload, EventPayload::KernelContext { .. })
            && !units.is_empty()
        {
            // Kernel context attaches to the preceding unit so it is never
            // separated from the turn it describes.
            units.last_mut().expect("non-empty").1 = index + 1;
            index += 1;
        } else {
            let end = absorb_kernel_events(events, index + 1);
            units.push((index, end));
            index = end;
        }
    }
    units
}

/// Extends a unit boundary over immediately following kernel context messages.
fn absorb_kernel_events(events: &[Event], mut end: usize) -> usize {
    while end < events.len() && matches!(events[end].payload, EventPayload::KernelContext { .. }) {
        end += 1;
    }
    end
}

/// Loads the provider-visible events of the current cache epoch, bounded by the
/// working-memory high-water mark. `reached_start` is false when history is
/// larger than the mark, which is the signal to rotate the epoch.
fn load_epoch_events(
    store: &EventStore,
    session_id: Uuid,
    epoch_start: u64,
    generation: u64,
    high_water: usize,
    estimator: &TokenEstimator,
) -> Result<(Vec<Event>, bool)> {
    let mut limit = 256usize;
    loop {
        let fetched = store.events_tail(session_id, limit)?;
        let exhausted = fetched.len() < limit;
        let visible: Vec<Event> = fetched
            .into_iter()
            .filter(|event| event.sequence >= epoch_start && is_epoch_visible(event, generation))
            .collect();
        let tokens = estimated_events_tokens(&visible, estimator);
        let reached_start = exhausted
            || visible
                .first()
                .is_none_or(|event| event.sequence <= epoch_start);
        if tokens >= high_water || reached_start {
            return Ok((visible, reached_start));
        }
        limit = limit.saturating_mul(2);
    }
}

/// Provider-visible events of one cache epoch. Conversation is visible in the
/// epoch that owns it; kernel context is visible only in the generation that
/// emitted it, so a rotated epoch never resurrects stale authoritative
/// messages even though they stay durable.
fn is_epoch_visible(event: &Event, generation: u64) -> bool {
    match &event.payload {
        EventPayload::KernelContext {
            generation: emitted,
            ..
        } => *emitted == generation,
        payload => is_model_visible_event(payload),
    }
}

fn estimated_events_tokens(events: &[Event], estimator: &TokenEstimator) -> usize {
    events
        .iter()
        .map(|event| event_tokens(event, estimator))
        .sum()
}

/// Images referenced by one durable event. Only user turns and tool results can
/// carry media; everything else contributes none.
fn event_media(event: &Event) -> &[latch_protocol::MediaRef] {
    match &event.payload {
        EventPayload::UserMessage { media, .. } => media,
        EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
            &result.media
        }
        _ => &[],
    }
}

/// Estimated provider cost of one event: rendered text plus conservative
/// image-token accounting. Image bytes are never measured as base64 text.
fn event_tokens(event: &Event, estimator: &TokenEstimator) -> usize {
    estimator.estimate(&render_event(event)) + estimator.estimate_media(event_media(event))
}

/// Stable header for one durable kernel context message. The generation and
/// revision make supersession explicit: a higher revision is current truth,
/// earlier revisions are provenance.
fn kernel_content(generation: u64, revision: u64, kind: KernelContextKind, body: &str) -> String {
    let label = match kind {
        KernelContextKind::Snapshot => "KERNEL STATE SNAPSHOT",
        KernelContextKind::StateUpdate => "KERNEL STATE UPDATE",
        KernelContextKind::Recall => "KERNEL RECALL",
        KernelContextKind::EpisodeIndex => "KERNEL ARCHIVE INDEX",
        KernelContextKind::Reground => "KERNEL RE-GROUND",
        KernelContextKind::Extension => "KERNEL EXTENSION CONTEXT",
    };
    let authority = match kind {
        KernelContextKind::Snapshot | KernelContextKind::StateUpdate => {
            "authoritative current state; supersedes all earlier kernel context"
        }
        KernelContextKind::Recall => {
            "original historical events relevant to the current instruction; not a new request"
        }
        KernelContextKind::EpisodeIndex => {
            "navigational summaries only; raw events remain retrievable"
        }
        KernelContextKind::Reground => "kernel instruction; not user input",
        KernelContextKind::Extension => "extension-provided context sources",
    };
    format!(
        "{label} (cache epoch {generation}.{revision}; {authority})
{body}"
    )
}

/// Next kernel revision within a generation.
fn next_kernel_revision(events: &[Event], generation: u64) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::KernelContext {
                generation: g,
                revision,
                ..
            } if *g == generation => Some(revision + 1),
            _ => None,
        })
        .unwrap_or(1)
}

/// Body of the most recent kernel context message of the given kinds in this
/// generation, without its header.
fn last_kernel_body<'a>(
    events: &'a [Event],
    generation: u64,
    kinds: &[KernelContextKind],
) -> Option<&'a str> {
    events.iter().rev().find_map(|event| match &event.payload {
        EventPayload::KernelContext {
            generation: g,
            kind,
            content,
            ..
        } if *g == generation && kinds.contains(kind) => {
            content.split_once('\n').map(|(_, body)| body)
        }
        _ => None,
    })
}

/// One in-progress episode segment. Fields accumulate exactly like the
/// historical full-scan builder, so incremental extension and a from-scratch
/// rebuild produce the same episodes.
#[derive(Debug, Clone)]
struct OpenEpisode {
    start_sequence: u64,
    end_sequence: u64,
    count: usize,
    topic: Option<String>,
    entities: Vec<String>,
    tools: Vec<String>,
    markers: Vec<String>,
}

impl OpenEpisode {
    fn into_episode(self) -> Episode {
        let topic = self.topic.unwrap_or_else(|| "session activity".into());
        let mut entities = self.entities;
        let mut tools = self.tools;
        for list in [&mut entities, &mut tools] {
            list.sort();
            list.dedup();
        }
        let mut markers = self.markers;
        markers.dedup();
        let summary = format!(
            "- #{}-#{}: {}{}{}",
            self.start_sequence,
            self.end_sequence,
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
        Episode {
            start_sequence: self.start_sequence,
            end_sequence: self.end_sequence,
            topic,
            entities,
            tools,
            markers,
            summary,
        }
    }

    fn snapshot(&self) -> Episode {
        self.clone().into_episode()
    }
}

/// Streaming episode segmentation. Boundaries align with new user intents or a
/// bounded event count, exactly as the one-shot builder did.
#[derive(Debug, Default, Clone)]
struct EpisodeBuilder {
    closed: Vec<Episode>,
    open: Option<OpenEpisode>,
    /// Highest sequence incorporated so far (0 = none).
    through: u64,
}

impl EpisodeBuilder {
    fn push(&mut self, event: &Event) {
        let is_user = event_user_text(&event.payload).is_some();
        if self
            .open
            .as_ref()
            .is_some_and(|open| is_user || open.count >= EPISODE_MAX_EVENTS)
        {
            self.flush();
        }
        let open = self.open.get_or_insert_with(|| OpenEpisode {
            start_sequence: event.sequence,
            end_sequence: event.sequence,
            count: 0,
            topic: None,
            entities: Vec::new(),
            tools: Vec::new(),
            markers: Vec::new(),
        });
        if open.topic.is_none()
            && let Some(text) = event_user_text(&event.payload)
        {
            open.topic = Some(text.chars().take(100).collect());
        }
        let push_marker = |markers: &mut Vec<String>, label: String| {
            if markers.last() != Some(&label) {
                markers.push(label);
            }
        };
        match &event.payload {
            EventPayload::FileObserved { version } => open.entities.push(version.path.clone()),
            EventPayload::FileChanged { after, .. } => {
                open.entities.push(after.path.clone());
                push_marker(&mut open.markers, "mutation".into());
            }
            EventPayload::ToolCompleted { result } => open.tools.push(result.name.clone()),
            EventPayload::ToolFailed { result } => {
                open.tools.push(result.name.clone());
                push_marker(&mut open.markers, "failure".into());
            }
            EventPayload::ValidationResult { passed, .. } => push_marker(
                &mut open.markers,
                if *passed {
                    "validation-passed".into()
                } else {
                    "validation-failed".into()
                },
            ),
            EventPayload::EvidenceCreated { evidence } => {
                push_marker(&mut open.markers, format!("evidence:{:?}", evidence.status));
            }
            EventPayload::FailureAttempt { .. } => {
                push_marker(&mut open.markers, "failure".into());
            }
            EventPayload::RegroundRequested { .. } => {
                push_marker(&mut open.markers, "reground".into());
            }
            _ => {}
        }
        open.count += 1;
        open.end_sequence = event.sequence;
        self.through = event.sequence;
    }

    fn snapshot(&self) -> Option<Episode> {
        self.open.as_ref().map(OpenEpisode::snapshot)
    }

    fn flush(&mut self) {
        if let Some(open) = self.open.take() {
            self.closed.push(open.into_episode());
        }
    }

    #[cfg(test)]
    fn finish(&mut self) {
        self.flush();
    }
}

/// Cached archival episode index with a sequence watermark. Newly appended
/// events extend the open segment; already-closed episodes are never rebuilt
/// unless the session changes or the archive rolls back. The raw event log
/// remains the source of truth and is never modified.
#[derive(Debug, Default)]
struct EpisodeCache {
    session_id: Option<Uuid>,
    builder: EpisodeBuilder,
    /// Tokens that left working memory since the previous materialization.
    last_evicted_tokens: usize,
}

impl EpisodeCache {
    fn advance(
        &mut self,
        store: &EventStore,
        session_id: Uuid,
        archive_end: u64,
        estimator: &TokenEstimator,
    ) -> Result<()> {
        // A different session, or an archive that moved backwards (a larger
        // working-memory budget), invalidates the derived index. Rebuilding is
        // deterministic and never touches the raw log.
        let rebuild = self.session_id != Some(session_id) || self.builder.through > archive_end;
        if rebuild {
            self.session_id = Some(session_id);
            self.builder = EpisodeBuilder::default();
            self.last_evicted_tokens = 0;
        }
        if self.builder.through < archive_end {
            let delta = store.events_between(
                session_id,
                self.builder.through,
                archive_end.saturating_add(1),
            )?;
            if !rebuild {
                self.last_evicted_tokens = delta
                    .iter()
                    .map(|event| event_tokens(event, estimator))
                    .sum();
            }
            for event in &delta {
                self.builder.push(event);
            }
        } else {
            self.last_evicted_tokens = 0;
        }
        Ok(())
    }
}

/// One-shot segmentation used by tests and equivalence checks. It is exactly
/// the streaming builder run to completion.
#[cfg(test)]
fn build_episodes(events: &[Event]) -> Vec<Episode> {
    let mut builder = EpisodeBuilder::default();
    for event in events {
        builder.push(event);
    }
    builder.finish();
    builder.closed
}

fn conversation_bridge(state: &TaskState, events: &[Event]) -> ConversationBridge {
    let current_user_intent = events
        .iter()
        .rev()
        .find_map(|event| event_user_text(&event.payload).map(str::to_owned))
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

fn event_user_text(payload: &EventPayload) -> Option<&str> {
    match payload {
        EventPayload::UserMessage { text, .. } => Some(text),
        EventPayload::AgentMessageReceived { message } => Some(&message.text),
        _ => None,
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
        let cost = estimator.estimate(&rendered) + estimator.estimate_media(event_media(event));
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
        EventPayload::UserMessage { text, media } => {
            let mut rendered = format!("user: {text}");
            for media_ref in media {
                rendered.push('\n');
                rendered.push_str(&media_ref.compact_label());
            }
            rendered
        }
        EventPayload::AgentMessageReceived { message } => format!("user: {}", message.text),
        EventPayload::AssistantMessageCompleted {
            text,
            tool_calls,
            reasoning_content,
            ..
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
        EventPayload::AgentNotificationDelivered { report } => format!(
            "kernel child report {} {:?}: {}",
            report.task_name, report.status, report.summary
        ),
        EventPayload::GroupMessageDelivered { message } => format!(
            "kernel group message from {}: {}",
            message.from_agent, message.text
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
                    media: vec![],
                },
            )
            .unwrap();
        let d = store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "Decision B: use framed stdio".into(),
                    media: vec![],
                },
            )
            .unwrap();
        let rejected = store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "Approach C: rewrite the fixture".into(),
                    media: vec![],
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
                        media: Vec::new(),
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
                        media: vec![],
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
            EventPayload::UserMessage {  text, .. } if text.contains("unrelated conversation 1999")
        )));
        let all = store.events(sid).unwrap();
        let epoch_events = all
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::ContextEpochStarted { .. }))
            .count();
        // 4 seed events + 2000 fillers + the recall bookkeeping event, plus
        // cache-epoch and kernel-context events. Raw history keeps every user
        // turn and the epoch rotation is durable.
        assert!(
            all.iter()
                .filter(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
                .count()
                >= 2000
        );
        assert!(epoch_events >= 1, "the epoch rotates once under pressure");
        assert!(ctx.stats.recent_start_sequence > 1);
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
                        media: vec![],
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
        // Working memory moved into the archival region without any durable
        // cliff event: the raw log still holds every filler plus the recall
        // bookkeeping event.
        assert!(ctx.stats.recent_start_sequence > 1);
        assert!(
            ctx.stats.episode_tokens <= ctx.stats.recall_tokens,
            "episode metadata is a subset of the recall section"
        );
        assert!(ctx.stats.episodes > 0, "an archival index exists");
        let events = store.events(sid).unwrap();
        let epoch_events = events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ContextEpochStarted { .. }))
            .count();
        assert!(epoch_events >= 1, "pressure rotates the cache epoch");
        assert!(events.len() >= 5001, "raw history is retained in full");
    }

    #[test]
    fn manual_compact_retains_raw_events() {
        let s = EventStore::open_memory().unwrap();
        let id = s.create_session(Path::new("/x")).unwrap();
        s.append(
            id,
            EventPayload::UserMessage {
                text: "keep".into(),
                media: vec![],
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
        // Working memory resets to an authoritative snapshot; the original
        // user turn stays durable and retrievable.
        assert!(
            context
                .recent
                .iter()
                .all(|event| matches!(event.payload, EventPayload::KernelContext { .. })),
            "manual compact leaves no old conversation in working memory"
        );
        assert!(context.recent.iter().any(|event| matches!(
            &event.payload,
            EventPayload::KernelContext {
                kind: KernelContextKind::Snapshot,
                ..
            }
        )));
        assert!(context.stats.cache_epoch >= 1);
        assert_eq!(context.stats.cache_rotation_reason, "manual compact");
        assert!(s.events(id).unwrap().iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserMessage {  text, .. } if text == "keep"
        )));
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

                reasoning: vec![],
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
                    media: vec![],
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

                    reasoning: vec![],
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
                        media: Vec::new(),
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

                    reasoning: vec![],
                },
            ),
        ];
        let estimator = TokenEstimator::generic();
        let transaction = estimator.estimate(&render_event(&events[1]))
            + estimator.estimate(&render_event(&events[2]));
        let tail = estimator.estimate(&render_event(&events[3]));

        // Budget fits the trailing assistant reply but not the tool
        // transaction: the transaction is dropped whole, never leaving a
        // dangling tool result.
        let (split, _) = bounded_recent(&events, tail + transaction - 1, &estimator);
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

        // Growing the budget by a little includes the whole transaction; it is
        // never partially included.
        let (pair, _) = bounded_recent(&events, tail + transaction, &estimator);
        assert_eq!(pair.len(), 3, "the transaction joins as a whole unit");
        assert!(
            pair.iter()
                .any(|e| matches!(e.payload, EventPayload::ToolCompleted { .. }))
        );

        // A budget that covers the whole conversation keeps every unit intact.
        let total: usize = events
            .iter()
            .map(|event| event_tokens(event, &estimator))
            .sum();
        let (whole, _) = bounded_recent(&events, total, &estimator);
        assert_eq!(whole.len(), 4);
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

                    reasoning: vec![],
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
                        media: Vec::new(),
                    },
                },
            ),
        ];
        let units = conversation_units(&events);
        assert_eq!(units, vec![(0, 2)]);
        for budget in [0, 1, 10, 100, 10_000] {
            let (recent, _) = bounded_recent(&events, budget, &TokenEstimator::generic());
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
                media: vec![],
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
                created: true,
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
                media: vec![],
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

    #[tokio::test]
    async fn cache_epochs_append_between_rotations_and_rotate_with_hysteresis() {
        use crate::state::EvidenceLedger;
        use crate::state::FailureManager;
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/tmp")).unwrap();
        for index in 0..20 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("request {index} {}", "x".repeat(200)),
                        media: vec![],
                    },
                )
                .unwrap();
            store
                .append(
                    sid,
                    EventPayload::AssistantMessageCompleted {
                        text: format!("answer {index}"),
                        tool_calls: vec![],
                        reasoning_content: None,

                        reasoning: vec![],
                    },
                )
                .unwrap();
        }
        let engine = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let state = TaskStateManager::default();
        let budget = MaterializeBudget {
            request_tokens: 100_000,
            window_tokens: 128_000,
            reserve_tokens: 0,
            recent_tokens: 600,
            reserved_tokens: 0,
        };
        let materialize = || {
            engine
                .materialize(
                    sid,
                    state.state(),
                    None,
                    &EvidenceLedger::default(),
                    &FailureManager::new(3),
                    "stable system".into(),
                    &budget,
                )
                .unwrap()
        };
        let first = materialize();
        assert!(!first.recent.is_empty());
        assert!(
            first.stats.recent_tokens <= 600,
            "working memory respects its budget"
        );
        assert!(
            first.recent.len() < 40,
            "only a bounded working set is kept"
        );
        assert!(first.stats.cache_epoch >= 1, "pressure rotated once");
        let rotations = |store: &EventStore| {
            store
                .events(sid)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::ContextEpochStarted { .. }))
                .count()
        };
        assert_eq!(rotations(&store), 1);
        assert!(
            first.stats.recent_evicted_tokens > 0,
            "rotation is explained"
        );
        assert!(first.recent.iter().any(|event| matches!(
            &event.payload,
            EventPayload::KernelContext {
                kind: KernelContextKind::Snapshot,
                ..
            }
        )));

        // Deterministic: materializing again without new events changes nothing
        // because the post-rotation epoch is at or below the high-water mark.
        let again = materialize();
        assert_eq!(again.recent, first.recent);
        assert_eq!(
            again.stats.recent_start_sequence,
            first.stats.recent_start_sequence
        );
        assert_eq!(
            rotations(&store),
            1,
            "hysteresis prevents per-turn rotation"
        );

        // Ordinary turns append: the previous provider-visible history stays an
        // exact prefix while the epoch has headroom.
        store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "one more request".into(),
                    media: vec![],
                },
            )
            .unwrap();
        store
            .append(
                sid,
                EventPayload::AssistantMessageCompleted {
                    text: "one more answer".into(),
                    tool_calls: vec![],
                    reasoning_content: None,

                    reasoning: vec![],
                },
            )
            .unwrap();
        let third = materialize();
        assert_eq!(rotations(&store), 1, "no rotation while headroom remains");
        let prefix: Vec<Uuid> = first.recent.iter().map(|event| event.id).collect();
        assert_eq!(
            third.recent[..prefix.len()]
                .iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            prefix,
            "ordinary turns only append to the provider-visible epoch"
        );
        assert!(third.recent.len() > first.recent.len());
        assert!(third.stats.cache_epoch_turns > first.stats.cache_epoch_turns);

        // Keep appending until the epoch fills; rotation happens occasionally,
        // not every turn.
        let mut turns = 3u64;
        let mut previous = third;
        while rotations(&store) < 2 && turns < 200 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("filler {turns} {}", "y".repeat(200)),
                        media: vec![],
                    },
                )
                .unwrap();
            store
                .append(
                    sid,
                    EventPayload::AssistantMessageCompleted {
                        text: format!("reply {turns}"),
                        tool_calls: vec![],
                        reasoning_content: None,

                        reasoning: vec![],
                    },
                )
                .unwrap();
            previous = materialize();
            turns += 1;
        }
        assert!(rotations(&store) >= 2, "the epoch rotates again when full");
        assert!(
            turns > 3 && (rotations(&store) as u64) < turns,
            "rotation is periodic, not per-turn (took {turns} turns)"
        );
        assert!(
            previous.stats.recent_start_sequence > 0,
            "working set remains useful after rotation"
        );
        assert!(previous.stats.recent_tokens > 0);

        // Evicted originals stay durable and reachable through recall.
        let recalled = engine.recall(sid, "request 0").unwrap();
        assert!(
            recalled.iter().any(|event| matches!(
                &event.payload,
                EventPayload::UserMessage {  text, .. } if text.contains("request 0")
            )),
            "evicted events remain retrievable"
        );
        assert!(store.events(sid).unwrap().iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserMessage {  text, .. } if text.contains("request 0")
        )));

        // Resume equivalence: a fresh engine reconstructs the same working set
        // and the same logical episodes from the raw log.
        let resumed = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let resumed_ctx = resumed
            .materialize(
                sid,
                state.state(),
                None,
                &EvidenceLedger::default(),
                &FailureManager::new(3),
                "stable system".into(),
                &budget,
            )
            .unwrap();
        assert_eq!(resumed_ctx.recent, previous.recent);
        assert_eq!(
            resumed_ctx.stats.recent_start_sequence,
            previous.stats.recent_start_sequence
        );
        let summaries: Vec<_> = previous
            .episodes
            .iter()
            .map(|e| e.summary.clone())
            .collect();
        let resumed_summaries: Vec<_> = resumed_ctx
            .episodes
            .iter()
            .map(|e| e.summary.clone())
            .collect();
        assert_eq!(
            resumed_summaries, summaries,
            "episode index is deterministic on resume"
        );
    }

    #[test]
    fn episode_index_is_incrementally_equivalent_to_a_full_rebuild() {
        use latch_protocol::{ToolCall, ToolResult};
        use serde_json::json;
        let session = Uuid::new_v4();
        let mut events = Vec::new();
        let mut sequence = 0u64;
        let mut push = |payload: EventPayload| {
            sequence += 1;
            events.push(event(session, sequence, payload));
        };
        for index in 0..60 {
            push(EventPayload::UserMessage {
                text: format!("task {index}"),
                media: vec![],
            });
            if index % 3 == 0 {
                push(EventPayload::AssistantMessageCompleted {
                    text: "calling".into(),
                    tool_calls: vec![ToolCall {
                        id: format!("call-{index}"),
                        name: "read_file".into(),
                        arguments: json!({"path": format!("src/{index}.rs")}),
                    }],
                    reasoning_content: None,

                    reasoning: vec![],
                });
                push(EventPayload::ToolCompleted {
                    result: ToolResult {
                        call_id: format!("call-{index}"),
                        name: "read_file".into(),
                        output: "contents".into(),
                        is_error: false,
                        artifact_id: None,
                        media: Vec::new(),
                    },
                });
            }
            if index % 7 == 0 {
                push(EventPayload::ValidationResult {
                    command: "cargo test".into(),
                    passed: index % 14 == 0,
                    detail: "checked".into(),
                });
                push(EventPayload::EvidenceCreated {
                    evidence: crate::state::EvidenceLedger::default().build(
                        format!("claim {index}"),
                        Uuid::new_v4(),
                        latch_protocol::EvidenceStatus::Passed,
                        "kernel observed",
                    ),
                });
            }
            push(EventPayload::AssistantMessageCompleted {
                text: format!("answer {index}"),
                tool_calls: vec![],
                reasoning_content: None,

                reasoning: vec![],
            });
        }

        // A one-shot rebuild is the reference.
        let reference = build_episodes(&events);

        // Extending the index in chunks must match a fresh rebuild of the same
        // prefix at every step, including the open (unsealed) trailing segment.
        let mut builder = EpisodeBuilder::default();
        let mut consumed = 0usize;
        for chunk in events.chunks(7) {
            for event in chunk {
                builder.push(event);
            }
            consumed += chunk.len();
            let mut incremental = builder.closed.clone();
            if let Some(open) = builder.snapshot() {
                incremental.push(open);
            }
            assert_eq!(
                incremental,
                build_episodes(&events[..consumed]),
                "incremental index diverged after {consumed} events"
            );
        }
        builder.finish();
        assert_eq!(
            builder.closed, reference,
            "final index equals a full rebuild"
        );
        assert!(reference.len() > 10, "the fixture produces real episodes");

        // The engine's cached index must agree with a cold rebuild over the
        // same durable log, including the selected episode summaries.
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/episodes")).unwrap();
        let config = ContextConfig {
            max_request_tokens: Some(8_000),
            recent_tokens: 200,
            reserve_tokens: 0,
            output_reserve_tokens: 0,
        };
        let incremental = ContinuityEngine::new(store.clone(), config.clone());
        let state = TaskState::default();
        let budget = incremental.default_budget(64_000, 0);
        for chunk in events.chunks(23) {
            for event in chunk {
                store.append(sid, event.payload.clone()).unwrap();
            }
            incremental
                .materialize(
                    sid,
                    &state,
                    None,
                    &EvidenceLedger::default(),
                    &crate::state::FailureManager::new(3),
                    "system".into(),
                    &budget,
                )
                .unwrap();
        }
        let warm = incremental
            .materialize(
                sid,
                &state,
                None,
                &EvidenceLedger::default(),
                &crate::state::FailureManager::new(3),
                "system".into(),
                &budget,
            )
            .unwrap();
        let cold = ContinuityEngine::new(store.clone(), config)
            .materialize(
                sid,
                &state,
                None,
                &EvidenceLedger::default(),
                &crate::state::FailureManager::new(3),
                "system".into(),
                &budget,
            )
            .unwrap();
        assert_eq!(warm.recent, cold.recent, "working set is deterministic");
        assert_eq!(
            warm.stats.recent_start_sequence,
            cold.stats.recent_start_sequence
        );
        assert_eq!(warm.stats.episodes, cold.stats.episodes);
        assert_eq!(
            warm.episodes, cold.episodes,
            "incremental and cold indexes select the same episodes"
        );
    }

    #[test]
    fn long_history_turns_index_only_the_delta() {
        use crate::state::EvidenceLedger;
        use crate::state::FailureManager;
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/long")).unwrap();
        for index in 0..5_000 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("turn {index}"),
                        media: vec![],
                    },
                )
                .unwrap();
            store
                .append(
                    sid,
                    EventPayload::AssistantMessageCompleted {
                        text: format!("reply {index}"),
                        tool_calls: vec![],
                        reasoning_content: None,

                        reasoning: vec![],
                    },
                )
                .unwrap();
        }
        let config = ContextConfig {
            max_request_tokens: Some(8_000),
            recent_tokens: 400,
            reserve_tokens: 0,
            output_reserve_tokens: 0,
        };
        let engine = ContinuityEngine::new(store.clone(), config.clone());
        let budget = engine.default_budget(128_000, 0);
        let mut state = TaskStateManager::default();
        state.update(crate::state::StateUpdate {
            add_constraints: vec!["keep the API stable".into()],
            add_decisions: vec!["index incrementally".into()],
            ..Default::default()
        });
        let materialize = |engine: &ContinuityEngine| {
            engine
                .materialize(
                    sid,
                    state.state(),
                    None,
                    &EvidenceLedger::default(),
                    &FailureManager::new(3),
                    "system".into(),
                    &budget,
                )
                .unwrap()
        };
        // Cold start builds the archival index once, from the full log.
        let cold = materialize(&engine);
        let cold_scanned = store.scanned_events();
        assert!(cold_scanned >= 10_000, "cold index scanned {cold_scanned}");
        assert!(cold.canonical.contains("keep the API stable"));
        assert!(cold.canonical.contains("index incrementally"));

        // A new turn must read only the bounded recent tail and the newly
        // archived delta, not the 10k-event history.
        store.reset_scanned_events();
        store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "one more turn".into(),
                    media: vec![],
                },
            )
            .unwrap();
        let warm = materialize(&engine);
        let scanned = store.scanned_events();
        assert!(
            scanned < 2_000,
            "a new turn scanned {scanned} of {} durable events",
            store.last_sequence(sid).unwrap()
        );
        assert!(warm.stats.recent_tokens > 0);
        assert!(warm.canonical.contains("keep the API stable"));
        assert!(warm.canonical.contains("index incrementally"));

        // Resume equivalence: a fresh engine over the same durable log
        // reconstructs the same working set and archival selection.
        let resumed = ContinuityEngine::new(store.clone(), config);
        let resumed_ctx = materialize(&resumed);
        assert_eq!(resumed_ctx.recent, warm.recent);
        assert_eq!(
            resumed_ctx.stats.recent_start_sequence,
            warm.stats.recent_start_sequence
        );
        assert_eq!(resumed_ctx.episodes, warm.episodes);
    }

    #[test]
    fn compact_resets_working_memory_and_ignores_legacy_epoch_boundaries() {
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/boundary")).unwrap();
        for i in 0..4 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("before compact {i}"),
                        media: vec![],
                    },
                )
                .unwrap();
        }
        store
            .append(sid, EventPayload::ManualCompact { generation: 1 })
            .unwrap();
        let first_after = store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "after compact".into(),
                    media: vec![],
                },
            )
            .unwrap();
        let tail = store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "tail".into(),
                    media: vec![],
                },
            )
            .unwrap();

        let engine = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let context = engine
            .materialize(
                sid,
                &TaskState::default(),
                None,
                &EvidenceLedger::default(),
                &crate::state::FailureManager::new(3),
                "system".into(),
                &budget(100_000, 60_000),
            )
            .unwrap();
        // `/compact` starts a fresh cache epoch: the provider-visible history
        // begins at a new authoritative snapshot, and nothing behind the
        // compact is sent as verbatim history.
        assert!(
            context
                .recent
                .iter()
                .all(|event| event.sequence > tail.sequence),
            "working memory never reaches behind an explicit compact"
        );
        assert_eq!(context.stats.cache_rotation_reason, "manual compact");
        assert!(context.stats.cache_epoch >= 1);
        assert!(context.recent.iter().any(|event| matches!(
            &event.payload,
            EventPayload::KernelContext {
                kind: KernelContextKind::Snapshot,
                ..
            }
        )));
        // The raw log keeps every event, including the ones the compact moved
        // out of the provider view.
        let all = store.events(sid).unwrap();
        assert!(all.iter().any(|event| event.id == first_after.id));
        assert!(all.iter().any(|event| event.id == tail.id));
        assert!(all.iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserMessage {  text, .. } if text.starts_with("before compact")
        )));
    }

    #[test]
    fn recall_prefers_later_revision_across_topics_after_compact_and_resume() {
        let store = EventStore::open_memory().unwrap();
        let sid = store
            .create_session(Path::new("/recall-regression"))
            .unwrap();
        for index in 0..16 {
            store
                .append(
                    sid,
                    EventPayload::UserMessage {
                        text: format!("protocol endpoint originally alpha, repetition {index}"),
                        media: vec![],
                    },
                )
                .unwrap();
        }
        store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "database uses SQLite WAL".into(),
                    media: vec![],
                },
            )
            .unwrap();
        store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "protocol endpoint revised to beta".into(),
                    media: vec![],
                },
            )
            .unwrap();
        store
            .append(
                sid,
                EventPayload::UserMessage {
                    text: "配置密钥 保持隔离".into(),
                    media: vec![],
                },
            )
            .unwrap();
        let mut engine = ContinuityEngine::new(store.clone(), ContextConfig::default());
        engine.manual_compact(sid).unwrap();
        let resumed = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let protocol = resumed.recall(sid, "protocol endpoint").unwrap();
        assert!(protocol.iter().any(|event| matches!(&event.payload,
            EventPayload::UserMessage { text, .. } if text.contains("revised to beta"))));
        let database = resumed.recall(sid, "SQLite WAL").unwrap();
        assert!(database.iter().any(|event| matches!(&event.payload,
            EventPayload::UserMessage { text, .. } if text.contains("database"))));
        let cjk = resumed.recall(sid, "配置密钥").unwrap();
        assert!(cjk.iter().any(|event| matches!(&event.payload,
            EventPayload::UserMessage { text, .. } if text.contains("保持隔离"))));
        assert_eq!(
            store
                .events(sid)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
                .count(),
            19
        );
    }
    #[test]
    fn cache_epochs_beat_per_turn_sliding_suffix_on_prefix_reuse() {
        use crate::state::EvidenceLedger;
        use crate::state::FailureManager;
        fn common_prefix(a: &str, b: &str) -> usize {
            a.bytes()
                .zip(b.bytes())
                .take_while(|(left, right)| left == right)
                .count()
        }
        fn ratio(previous: &str, next: &str) -> f64 {
            common_prefix(previous, next) as f64 / previous.len().max(1) as f64
        }

        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(Path::new("/measure")).unwrap();
        let engine = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let mut state = TaskStateManager::default();
        state.update(crate::state::StateUpdate {
            add_constraints: vec!["preserve current truth".into()],
            add_decisions: vec!["epoch-based cache".into()],
            ..Default::default()
        });
        let high = 1_000usize;
        let budget = MaterializeBudget {
            request_tokens: 100_000,
            window_tokens: 128_000,
            reserve_tokens: 0,
            recent_tokens: high,
            reserved_tokens: 0,
        };
        let mut new_signatures: Vec<String> = Vec::new();
        let mut old_signatures: Vec<String> = Vec::new();
        let mut visible: Vec<Event> = Vec::new();
        let turns = 48usize;
        for turn in 0..turns {
            visible.push(
                store
                    .append(
                        sid,
                        EventPayload::UserMessage {
                            text: format!("turn {turn} {}", "x".repeat(200)),
                            media: vec![],
                        },
                    )
                    .unwrap(),
            );
            visible.push(
                store
                    .append(
                        sid,
                        EventPayload::AssistantMessageCompleted {
                            text: format!("answer {turn}"),
                            tool_calls: vec![],
                            reasoning_content: None,

                            reasoning: vec![],
                        },
                    )
                    .unwrap(),
            );
            let ctx = engine
                .materialize(
                    sid,
                    state.state(),
                    None,
                    &EvidenceLedger::default(),
                    &FailureManager::new(3),
                    "system".into(),
                    &budget,
                )
                .unwrap();
            new_signatures.push(
                ctx.recent
                    .iter()
                    .map(render_event)
                    .collect::<Vec<_>>()
                    .join("\u{1e}"),
            );
            // The old architecture kept the largest suffix and regenerated a
            // synthetic kernel tail every request; neither was append-only.
            let (old_recent, _) = bounded_recent(&visible, high, &engine.estimator);
            let old = old_recent
                .iter()
                .map(render_event)
                .collect::<Vec<_>>()
                .join("\u{1e}");
            old_signatures.push(format!("{old}\u{1f}KERNEL TAIL {turn}"));
        }

        let ratios = |signatures: &[String]| -> Vec<f64> {
            signatures
                .windows(2)
                .map(|pair| ratio(&pair[0], &pair[1]))
                .collect()
        };
        let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len().max(1) as f64;
        let median = |values: &[f64]| {
            let mut sorted = values.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            sorted[sorted.len() / 2]
        };
        let new_ratios = ratios(&new_signatures);
        let old_ratios = ratios(&old_signatures);
        let high_reuse = |values: &[f64]| {
            values.iter().filter(|value| **value >= 0.9).count() as f64 / values.len().max(1) as f64
        };
        let rotations = store
            .events(sid)
            .unwrap()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ContextEpochStarted { .. }))
            .count();
        let worst_rotation = new_ratios.iter().cloned().fold(1.0f64, f64::min);
        eprintln!(
            "cache measurement: turns={turns} rotations={rotations} \
             new_mean={:.2} old_mean={:.2} new_median={:.2} old_median={:.2} \
             new_high_reuse={:.0}% old_high_reuse={:.0}% worst={:.2}",
            mean(&new_ratios),
            mean(&old_ratios),
            median(&new_ratios),
            median(&old_ratios),
            high_reuse(&new_ratios) * 100.0,
            high_reuse(&old_ratios) * 100.0,
            worst_rotation
        );

        // Cache resets are occasional and never a large cliff.
        assert!(
            rotations <= turns / 4,
            "too many rotations: {rotations} over {turns} turns"
        );
        assert!(
            new_ratios.len() == turns - 1 && old_ratios.len() == turns - 1,
            "every ordinary turn is measured"
        );
        // The epoch strategy keeps most turns highly reusable, while the old
        // per-turn suffix reset the provider prefix almost every turn.
        assert!(
            mean(&new_ratios) > mean(&old_ratios) + 0.3,
            "new {:.2} vs old {:.2}",
            mean(&new_ratios),
            mean(&old_ratios)
        );
        assert!(high_reuse(&new_ratios) >= 0.7);
        assert!(
            high_reuse(&new_ratios) > high_reuse(&old_ratios) + 0.4,
            "new {:.0}% vs old {:.0}%",
            high_reuse(&new_ratios) * 100.0,
            high_reuse(&old_ratios) * 100.0
        );
        assert!(rotations >= 2, "the epoch rotates under pressure");
        // Cache breaks are occasional and controlled: at most one per rotation.
        let breaks = new_ratios.iter().filter(|ratio| **ratio < 0.5).count();
        assert!(
            breaks <= rotations,
            "{breaks} prefix breaks for {rotations} rotations"
        );
        let _ = worst_rotation;

        // Current truth survives every rotation.
        let last = engine
            .materialize(
                sid,
                state.state(),
                None,
                &EvidenceLedger::default(),
                &FailureManager::new(3),
                "system".into(),
                &budget,
            )
            .unwrap();
        assert!(last.canonical.contains("preserve current truth"));
        assert!(last.canonical.contains("epoch-based cache"));
        assert!(last.stats.cache_epoch > 0);

        // Resume equivalence: a cold engine reconstructs the same epoch view.
        let resumed = ContinuityEngine::new(store.clone(), ContextConfig::default());
        let resumed_ctx = resumed
            .materialize(
                sid,
                state.state(),
                None,
                &EvidenceLedger::default(),
                &FailureManager::new(3),
                "system".into(),
                &budget,
            )
            .unwrap();
        assert_eq!(resumed_ctx.recent, last.recent);
        assert_eq!(
            resumed_ctx.stats.cache_epoch, last.stats.cache_epoch,
            "durable epoch boundaries replay deterministically"
        );
    }
}
