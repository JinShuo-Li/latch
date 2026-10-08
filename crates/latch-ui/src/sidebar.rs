//! Responsive observability sidebar state.
//!
//! The sidebar is derived only from authoritative durable events and the
//! kernel's own records: context statistics, canonical task state, evidence,
//! model usage, and change ownership. It never infers status from rendered
//! assistant prose. The same reducer is used live and on resume replay, so the
//! two cannot drift.

use crate::agents::SubagentModel;
use crate::group::GroupModel;
use chrono::{DateTime, Duration, Utc};
use latch_protocol::{
    CompletionState, ContextStats, Event, EventPayload, EvidenceStatus, Mode, TaskState, Usage,
};
use std::collections::{BTreeMap, BTreeSet};

/// Re-export the shared billing-shape type so the CLI can build one.
pub use latch_protocol::ModelPricing as Pricing;

/// Session chrome for the sidebar header.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq)]
pub struct SidebarSession {
    pub model: String,
    pub mode: Mode,
    pub branch: String,
    pub resumed: bool,
    pub pricing: Option<Pricing>,
}

/// One known-or-unknown usage category.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    /// Normalized uncached input (miss). Explicit when the provider reported
    /// it; otherwise derived from input minus a known cache-read count.
    pub cache_miss: Option<u64>,
    /// True when at least one observed request omitted the category, so the
    /// accumulated value is a lower bound rather than a complete total.
    pub input_partial: bool,
    pub output_partial: bool,
    pub cache_read_partial: bool,
    pub cache_write_partial: bool,
    pub cache_miss_partial: bool,
}

impl UsageTotals {
    fn add(&mut self, usage: &Usage) {
        accumulate(
            &mut self.input,
            &mut self.input_partial,
            Some(usage.input_tokens),
        );
        accumulate(
            &mut self.output,
            &mut self.output_partial,
            Some(usage.output_tokens),
        );
        accumulate(
            &mut self.cache_read,
            &mut self.cache_read_partial,
            usage.cache_read_tokens,
        );
        accumulate(
            &mut self.cache_write,
            &mut self.cache_write_partial,
            usage.cache_write_tokens,
        );
        accumulate(
            &mut self.cache_miss,
            &mut self.cache_miss_partial,
            usage.uncached_input_tokens(),
        );
        if usage.cache_miss_tokens.is_none() && usage.cache_read_tokens.is_none() {
            // The provider reported no cache accounting: input is billed as
            // uncached, which is the safe direction, but the estimate is
            // incomplete and must say so.
            self.cache_miss_partial = true;
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.unwrap_or(0) == 0
            && self.output.unwrap_or(0) == 0
            && self.cache_read.is_none()
            && self.cache_write.is_none()
    }

    /// Uncached input used for cost: the accumulated miss when known,
    /// otherwise the full input (which is already correct when the provider
    /// reports no cache categories).
    #[must_use]
    pub fn billed_input(&self) -> (Option<u64>, bool) {
        match (self.cache_miss, self.input) {
            (Some(miss), _) => (Some(miss), self.cache_miss_partial),
            (None, input) => (input, self.input_partial || self.cache_miss_partial),
        }
    }
}

/// Explicit per-run totals. A run is one root user-request execution: it
/// starts at `RunStarted` and ends at `RunCompleted`, so counters reset per
/// task while `UsageTotals` remains the cumulative session view.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTotals {
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
    pub requests: u32,
    pub tool_calls: u32,
    pub files_read: u32,
    pub files_changed: u32,
    pub validations: u32,
    pub usage: UsageTotals,
    /// Cumulative across every provider request in this run.
    pub reasoning_replay_tokens: usize,
    pub tool_argument_tokens: usize,
    pub tool_result_tokens: usize,
    /// Breakdown of only the most recent request (`ContextMaterialized`).
    pub last_reasoning_replay_tokens: usize,
    pub last_tool_argument_tokens: usize,
    pub last_tool_result_tokens: usize,
}

impl RunTotals {
    #[must_use]
    pub fn elapsed_seconds(&self) -> Option<i64> {
        let (Some(started), Some(ended)) = (self.started_at, self.ended_at) else {
            return None;
        };
        Some((ended - started).num_seconds().max(0))
    }
}

fn accumulate(slot: &mut Option<u64>, partial: &mut bool, value: Option<u64>) {
    match value {
        Some(value) => *slot = Some(slot.unwrap_or(0).saturating_add(value)),
        None => *partial = true,
    }
}

/// Estimated cost from configured prices. `partial` marks a lower-bound
/// estimate because some observed requests omitted a usage category.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct CostEstimate {
    pub amount: f64,
    pub currency: String,
    pub partial: bool,
}

/// Per-owner change totals derived from the durable change ledger.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerStats {
    pub files: BTreeSet<String>,
    pub additions: usize,
    pub deletions: usize,
    /// True when file-level ownership is known but line deltas are not (for
    /// example an untrackable shell mutation).
    pub lines_unknown: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
struct FileEntry {
    path: String,
    owner: latch_protocol::ChangeOwner,
    hash: String,
    additions: usize,
    deletions: usize,
}

/// Ownership-preserving change summary. Categories stay distinct: a file that
/// Latch edited and reality later changed is reported as externally modified
/// rather than pretending Latch's record is still clean.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeView {
    entries: Vec<FileEntry>,
    preexisting: BTreeSet<String>,
    external: BTreeSet<String>,
    shell_untrackable: BTreeSet<String>,
    externally_modified: BTreeSet<String>,
}

impl ChangeView {
    fn record(
        &mut self,
        path: String,
        owner: latch_protocol::ChangeOwner,
        hash: String,
        additions: usize,
        deletions: usize,
    ) {
        self.entries
            .retain(|entry| entry.owner != owner || entry.path != path);
        self.entries.push(FileEntry {
            path,
            owner,
            hash,
            additions,
            deletions,
        });
    }

    fn revert(&mut self, path: &str, hash: &str) {
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.path == path && entry.hash == hash)
        {
            self.entries.remove(index);
        }
    }

    fn external_change(&mut self, path: &str) {
        self.external.insert(path.to_owned());
        if self.entries.iter().any(|entry| entry.path == path) {
            self.externally_modified.insert(path.to_owned());
        }
    }

    fn preexisting(&mut self, paths: &[String]) {
        self.preexisting.extend(paths.iter().cloned());
    }

    fn untrackable(&mut self, paths: &[String]) {
        self.shell_untrackable.extend(paths.iter().cloned());
    }

    fn stats_for(&self, owner: Option<&latch_protocol::ChangeOwner>) -> OwnerStats {
        let mut stats = OwnerStats::default();
        for entry in &self.entries {
            let matches = match owner {
                Some(owner) => &entry.owner == owner,
                None => true,
            };
            if matches {
                stats.files.insert(entry.path.clone());
                stats.additions += entry.additions;
                stats.deletions += entry.deletions;
            }
        }
        stats
    }

    #[must_use]
    pub fn latch(&self) -> OwnerStats {
        self.stats_for(Some(&latch_protocol::ChangeOwner::Latch))
    }

    #[must_use]
    pub fn shell(&self) -> OwnerStats {
        self.stats_for(Some(&latch_protocol::ChangeOwner::Shell))
    }

    #[must_use]
    pub fn extension(&self) -> OwnerStats {
        let mut stats = OwnerStats::default();
        for entry in &self.entries {
            if matches!(entry.owner, latch_protocol::ChangeOwner::Extension(_)) {
                stats.files.insert(entry.path.clone());
                stats.additions += entry.additions;
                stats.deletions += entry.deletions;
            }
        }
        stats
    }

    #[must_use]
    pub fn external_files(&self) -> BTreeSet<String> {
        self.external
            .union(&self.preexisting)
            .cloned()
            .collect::<BTreeSet<_>>()
    }

    #[must_use]
    pub fn shell_untrackable_files(&self) -> &BTreeSet<String> {
        &self.shell_untrackable
    }

    #[must_use]
    pub fn externally_modified_files(&self) -> &BTreeSet<String> {
        &self.externally_modified
    }
}

/// Abnormal progress supervision surfaced from the kernel.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagnationView {
    pub redundant_turns: u32,
    pub unchanged: usize,
}

/// Reducer state for the observability sidebar.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct SidebarState {
    #[serde(default)]
    pub activity: crate::activity::ActivityState,
    session: SidebarSession,
    started_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    turns: u32,
    context: Option<ContextStats>,
    task: Option<TaskState>,
    current_request: Option<String>,
    evidence: BTreeMap<String, EvidenceStatus>,
    validation_stale: bool,
    stale_claims: BTreeSet<String>,
    usage: UsageTotals,
    last_usage: Option<Usage>,
    /// Provider requests and tool calls observed in the durable stream.
    tool_calls: u32,
    /// Current or most recent explicit run, reset at each `RunStarted`.
    run: RunTotals,
    changes: ChangeView,
    stall: Option<StagnationView>,
    /// Root-visible child sessions; the child transcripts stay separate.
    subagents: SubagentModel,
    /// Root-scoped coordination overlay (shared tasks, claims, members).
    group: GroupModel,
}

impl SidebarState {
    #[must_use]
    pub fn new(session: SidebarSession) -> Self {
        Self {
            activity: crate::activity::ActivityState::default(),
            session,
            started_at: None,
            updated_at: None,
            turns: 0,
            context: None,
            task: None,
            current_request: None,
            evidence: BTreeMap::new(),
            validation_stale: false,
            stale_claims: BTreeSet::new(),
            usage: UsageTotals::default(),
            last_usage: None,
            tool_calls: 0,
            run: RunTotals::default(),
            changes: ChangeView::default(),
            stall: None,
            subagents: SubagentModel::default(),
            group: GroupModel::default(),
        }
    }

    /// Builds the state by replaying a durable event stream; identical to
    /// applying the same events incrementally.
    #[must_use]
    pub fn from_events(session: SidebarSession, events: &[Event]) -> Self {
        let mut model = Self::new(session);
        for event in events {
            model.apply_event(event);
        }
        model
    }

    pub fn set_session(&mut self, session: SidebarSession) {
        self.session = session;
    }

    #[must_use]
    pub fn session(&self) -> &SidebarSession {
        &self.session
    }

    #[must_use]
    pub const fn turns(&self) -> u32 {
        self.turns
    }

    #[must_use]
    pub fn context(&self) -> Option<&ContextStats> {
        self.context.as_ref()
    }

    #[must_use]
    pub fn task(&self) -> Option<&TaskState> {
        self.task.as_ref()
    }

    #[must_use]
    pub fn current_request(&self) -> Option<&str> {
        self.current_request.as_deref()
    }

    #[must_use]
    pub fn validation_status(&self) -> &'static str {
        let required = self
            .task
            .as_ref()
            .map_or(&[][..], |task| task.required_validations.as_slice());
        let statuses = required
            .iter()
            .filter_map(|claim| self.evidence.get(&claim.trim().to_ascii_lowercase()));
        if statuses
            .clone()
            .any(|status| *status == EvidenceStatus::Failed)
        {
            return "failed";
        }
        if statuses
            .clone()
            .any(|status| *status == EvidenceStatus::Unavailable)
        {
            return "unavailable";
        }
        if self.validation_stale {
            return "stale";
        }
        if !required.is_empty() && self.validations_passed() == required.len() {
            return "passed";
        }
        "pending"
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTotals {
        &self.usage
    }

    #[must_use]
    pub fn run(&self) -> &RunTotals {
        &self.run
    }

    #[must_use]
    pub fn last_usage(&self) -> Option<&Usage> {
        self.last_usage.as_ref()
    }

    #[must_use]
    pub fn changes(&self) -> &ChangeView {
        &self.changes
    }

    #[must_use]
    pub fn subagents(&self) -> &SubagentModel {
        &self.subagents
    }

    /// Compact coordination state for the sidebar GROUP block and tests.
    #[must_use]
    pub fn group(&self) -> &GroupModel {
        &self.group
    }

    #[must_use]
    pub const fn stall(&self) -> Option<StagnationView> {
        self.stall
    }

    #[must_use]
    pub fn session_age(&self) -> Option<Duration> {
        match (self.started_at, self.updated_at) {
            (Some(start), Some(updated)) if updated >= start => Some(updated - start),
            _ => None,
        }
    }

    /// Number of required validations with current passing kernel evidence.
    #[must_use]
    pub fn validations_passed(&self) -> usize {
        let Some(task) = &self.task else {
            return 0;
        };
        task.required_validations
            .iter()
            .filter(|claim| {
                self.evidence
                    .get(&claim.trim().to_ascii_lowercase())
                    .is_some_and(|status| *status == EvidenceStatus::Passed)
            })
            .count()
    }

    /// Estimated cost from the current session usage and configured prices.
    ///
    /// Returns `None` when pricing is unavailable or incomplete for a category
    /// that has known tokens. It never invents a price.
    #[must_use]
    pub fn estimated_cost(&self) -> Option<CostEstimate> {
        let pricing = self.session.pricing.as_ref()?;
        let (billed_input, billed_input_partial) = self.usage.billed_input();
        let categories: [(Option<u64>, Option<f64>, bool); 4] = [
            (
                billed_input,
                pricing.input_per_million,
                billed_input_partial,
            ),
            (
                self.usage.output,
                pricing.output_per_million,
                self.usage.output_partial,
            ),
            (
                self.usage.cache_read,
                pricing.cache_read_per_million,
                self.usage.cache_read_partial,
            ),
            (
                self.usage.cache_write,
                pricing.cache_write_per_million,
                self.usage.cache_write_partial,
            ),
        ];
        let mut amount = 0.0;
        let mut any = false;
        let mut partial = false;
        for (tokens, price, category_partial) in categories {
            let Some(tokens) = tokens else { continue };
            if tokens == 0 {
                continue;
            }
            let Some(price) = price else {
                // A known token count without a configured price makes a
                // faithful total impossible; stay honest rather than guessing.
                return None;
            };
            amount += tokens as f64 / 1_000_000.0 * price;
            any = true;
            partial |= category_partial;
        }
        any.then(|| CostEstimate {
            amount,
            currency: pricing.currency.clone(),
            partial,
        })
    }

    pub fn apply_event(&mut self, event: &Event) {
        self.activity.apply_event(event);
        self.subagents.apply_event(event);
        self.group.apply_event(event);
        if self.started_at.is_none() {
            self.started_at = Some(event.timestamp);
        }
        if self
            .updated_at
            .is_none_or(|updated| event.timestamp > updated)
        {
            self.updated_at = Some(event.timestamp);
        }
        // Any progress event ends an active stall before the event-specific
        // handling below.
        if clears_stall(&event.payload) {
            self.stall = None;
        }
        if matches!(
            event.payload,
            EventPayload::WorkspaceMutationPossible { .. }
                | EventPayload::FileChanged { .. }
                | EventPayload::ExternalFileChangeDetected { .. }
        ) && (self
            .task
            .as_ref()
            .is_some_and(|task| task.completion == CompletionState::Verified)
            || self
                .evidence
                .values()
                .any(|status| *status == EvidenceStatus::Passed))
        {
            self.validation_stale = true;
            self.stale_claims
                .extend(self.evidence.iter().filter_map(|(claim, status)| {
                    (*status == EvidenceStatus::Passed).then_some(claim.clone())
                }));
        }
        match &event.payload {
            EventPayload::ContextMaterialized { stats } => {
                // One event per provider request: accumulate the run totals and
                // keep the latest request separate. Deterministic under replay
                // because the reducer sees each event exactly once.
                self.run.reasoning_replay_tokens = self
                    .run
                    .reasoning_replay_tokens
                    .saturating_add(stats.reasoning_replay_tokens);
                self.run.tool_argument_tokens = self
                    .run
                    .tool_argument_tokens
                    .saturating_add(stats.tool_arguments_tokens);
                self.run.tool_result_tokens = self
                    .run
                    .tool_result_tokens
                    .saturating_add(stats.tool_result_tokens);
                self.run.last_reasoning_replay_tokens = stats.reasoning_replay_tokens;
                self.run.last_tool_argument_tokens = stats.tool_arguments_tokens;
                self.run.last_tool_result_tokens = stats.tool_result_tokens;
                self.context = Some(stats.clone());
            }
            EventPayload::TaskStateUpdated { state } => self.task = Some(state.clone()),
            EventPayload::CompletionChanged { completion } => {
                let mut task = self.task.clone().unwrap_or_default();
                task.completion = completion.clone();
                self.task = Some(task);
            }
            EventPayload::EvidenceCreated { evidence } => {
                let claim = evidence.claim.trim().to_ascii_lowercase();
                self.evidence.insert(claim.clone(), evidence.status.clone());
                self.stale_claims.remove(&claim);
                if self.stale_claims.is_empty() {
                    self.validation_stale = false;
                }
            }
            EventPayload::ModelUsage { usage } => {
                self.usage.add(usage);
                self.run.usage.add(usage);
                self.last_usage = Some(usage.clone());
            }
            EventPayload::RunStarted { prompt, .. } => {
                self.current_request = (!prompt.trim().is_empty()).then(|| prompt.clone());
                self.run = RunTotals {
                    started_at: Some(event.timestamp),
                    ..RunTotals::default()
                };
            }
            EventPayload::RunCompleted { outcome, .. } => {
                self.run.ended_at = Some(event.timestamp);
                self.run.outcome = Some(outcome.clone());
            }
            EventPayload::UserMessage { text, .. } if !text.trim().is_empty() => {
                self.current_request = Some(text.clone());
            }
            EventPayload::UserMessage { media, .. } if !media.is_empty() => {
                self.current_request = Some("Image request".into());
            }
            EventPayload::ModelRequestStarted { .. } => {
                self.turns += 1;
                self.run.requests += 1;
            }
            EventPayload::ToolRequested { call } => {
                self.tool_calls += 1;
                self.run.tool_calls += 1;
                if call.name == "read_file" {
                    self.run.files_read += 1;
                }
                if call.name == "validate" {
                    self.run.validations += 1;
                }
            }
            EventPayload::GitStateObserved { dirty_paths, .. } => {
                self.changes.preexisting(dirty_paths)
            }
            EventPayload::FileChanged {
                after,
                owner,
                additions,
                deletions,
                ..
            } => {
                self.run.files_changed += 1;
                self.changes.record(
                    after.path.clone(),
                    owner.clone(),
                    after.content_hash.clone(),
                    *additions,
                    *deletions,
                )
            }
            EventPayload::ChangeReverted { path, content_hash } => {
                self.changes.revert(path, content_hash)
            }
            EventPayload::ExternalFileChangeDetected { path, .. } => {
                self.changes.external_change(path)
            }
            EventPayload::ShellMutationObserved { paths, .. } => self.changes.untrackable(paths),
            EventPayload::ProgressStagnation {
                unchanged,
                redundant_turns,
            } => {
                self.stall = Some(StagnationView {
                    redundant_turns: *redundant_turns,
                    unchanged: unchanged.len(),
                });
            }
            EventPayload::ModeChanged { mode } => self.session.mode = *mode,
            EventPayload::SessionResumed => self.session.resumed = true,
            _ => {}
        }
    }
}

/// Events that mean the task actually moved forward; they end a stall.
fn clears_stall(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::UserMessage { .. }
            | EventPayload::ValidationResult { .. }
            | EventPayload::EvidenceCreated { .. }
            | EventPayload::FileChanged { .. }
            | EventPayload::ChangeReverted { .. }
            | EventPayload::TaskStateUpdated { .. }
            | EventPayload::CompletionChanged { .. }
            | EventPayload::ModeChanged { .. }
    )
}

impl SidebarState {
    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        self.started_at
    }
    pub fn updated_at(&self) -> Option<DateTime<Utc>> {
        self.updated_at
    }
    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }
    pub fn validation_stale(&self) -> bool {
        self.validation_stale
    }
}

impl ChangeView {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.shell_untrackable.is_empty()
    }
}
