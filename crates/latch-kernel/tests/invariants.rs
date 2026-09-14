//! Architectural invariants: the deliberately small, fast, deterministic test
//! tier CI runs.
//!
//! These tests protect what Latch must never stop being: the raw event log as
//! the source of truth, append-only cache epochs that are performance
//! boundaries rather than memory boundaries, canonical state as authoritative
//! truth, kernel-owned evidence, protocol-valid provider requests, and a
//! deterministic provider-facing serialization.
//!
//! They must not need `bwrap`, `rg`, `python3`, a network, timing, or large
//! histories. Detailed correctness and stress coverage stays in the regular
//! local suite; passing this tier alone is not sufficient validation.

use latch_kernel::agent::SteeringSubmission;
use latch_kernel::config::{ContextConfig, OutsidePolicy, PermissionConfig};
use latch_kernel::prompt::PromptCompiler;
use latch_kernel::provider::{
    ModelProvider, ReasoningReplay, StreamSink, anthropic_request, openai_request,
};
use latch_kernel::safety::{Context as SafetyContext, Decision as SafetyDecision};
use latch_kernel::state::StateUpdate;
use latch_kernel::{
    Agent, AgentEventSink, AgentRuntime, ContinuityEngine, EventStore, EvidenceLedger,
    FailureManager, MaterializeBudget, MaterializedContext, PolicyEngine, TaskStateManager,
    ToolExecutor,
};
use latch_protocol::{
    CompletionState, ContextStats, Event, EventPayload, EvidenceStatus, Mode, ModelMessage,
    ModelRequest, ModelResponse, Safety, StreamEvent, TaskState, ToolCall, ToolResult, Usage,
};
use serde_json::json;
use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn response(text: &str, calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        tool_calls: calls,
        stop_reason: "stop".into(),
        usage: None,
        reasoning_content: None,

        reasoning: vec![],
    }
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

/// Provider that records every request so tests can compare the exact
/// provider-facing history the kernel would send.
struct RecordingProvider {
    requests: Mutex<Vec<ModelRequest>>,
    responses: Mutex<VecDeque<ModelResponse>>,
    /// Separate script handed to child sessions via `for_session`, so root and
    /// child turns never race one shared response queue.
    child_responses: Option<Vec<ModelResponse>>,
}

impl RecordingProvider {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
            child_responses: None,
        }
    }

    fn with_children(responses: Vec<ModelResponse>, child_responses: Vec<ModelResponse>) -> Self {
        Self {
            child_responses: Some(child_responses),
            ..Self::new(responses)
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ModelProvider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }
    fn model(&self) -> &str {
        "deepseek-invariants"
    }
    fn for_session(&self, _session_id: uuid::Uuid) -> Option<Arc<dyn ModelProvider>> {
        self.child_responses
            .as_ref()
            .map(|script| Arc::new(Self::new(script.clone())) as Arc<dyn ModelProvider>)
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
        sink: StreamSink,
    ) -> anyhow::Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted response exhausted");
        sink(StreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

fn materialize(
    engine: &ContinuityEngine,
    session: uuid::Uuid,
    state: &TaskState,
    query: Option<&str>,
    budget: &MaterializeBudget,
) -> MaterializedContext {
    engine
        .materialize(
            session,
            state,
            query,
            &EvidenceLedger::default(),
            &FailureManager::new(3),
            "invariant system prompt".into(),
            budget,
        )
        .unwrap()
}

fn rotation_count(store: &EventStore, session: uuid::Uuid) -> usize {
    store
        .events(session)
        .unwrap()
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ContextEpochStarted { .. }))
        .count()
}

fn kernel_bodies(context: &MaterializedContext) -> impl Iterator<Item = &str> {
    context
        .recent
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::KernelContext { content, .. } => Some(content.as_str()),
            _ => None,
        })
}

/// The provider-visible history only ever grows within an epoch: the previous
/// request is an exact prefix of the next request, including the system prompt
/// and tool schemas.
fn assert_request_extends(previous: &ModelRequest, next: &ModelRequest) {
    assert_eq!(previous.system, next.system, "system prefix must be stable");
    assert_eq!(previous.tools, next.tools, "tool schemas must be stable");
    assert!(
        next.messages.len() >= previous.messages.len(),
        "provider history shrank: {} -> {}",
        previous.messages.len(),
        next.messages.len()
    );
    assert_eq!(
        &next.messages[..previous.messages.len()],
        &previous.messages[..],
        "provider history must only append"
    );
}

/// Every assistant tool-call turn in the view is answered, and no tool result
/// appears without its call. Rotation must never split a transaction.
fn assert_tool_transactions_atomic(events: &[Event]) {
    let mut expecting: BTreeSet<String> = BTreeSet::new();
    for event in events {
        match &event.payload {
            EventPayload::AssistantMessageCompleted { tool_calls, .. }
                if !tool_calls.is_empty() =>
            {
                assert!(
                    expecting.is_empty(),
                    "previous tool transaction was split before {expecting:?}"
                );
                expecting.extend(tool_calls.iter().map(|call| call.id.clone()));
            }
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                assert!(
                    expecting.remove(&result.call_id),
                    "dangling tool result for {}",
                    result.call_id
                );
            }
            _ => {}
        }
    }
    assert!(
        expecting.is_empty(),
        "unanswered tool calls after rotation: {expecting:?}"
    );
}

/// Append-only provider history, exercised through the real agent run loop.
/// A kernel tool changes canonical state mid-run, so the second request must
/// carry the new state as an appended delta without rewriting what was already
/// sent.
#[tokio::test]
async fn provider_history_is_append_only_within_an_epoch() {
    let dir = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let provider = Arc::new(RecordingProvider::new(vec![
        response(
            "",
            vec![call(
                "state-1",
                "task_update",
                json!({"add_constraints": ["keep wire compatibility"]}),
            )],
        ),
        response("first done", vec![]),
        response("second done", vec![]),
    ]));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let config = ContextConfig {
        max_request_tokens: Some(32_000),
        recent_tokens: 8_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), config.clone()),
        retry_budget: 2,
    });
    agent.set_context_budget(config, latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS);
    let sink: AgentEventSink = Arc::new(|_| {});
    agent
        .run("first task", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    agent
        .run("second task", CancellationToken::new(), sink)
        .await
        .unwrap();

    let requests = provider.requests();
    assert!(
        requests.len() >= 3,
        "expected a tool turn plus a follow-up turn, got {}",
        requests.len()
    );
    for pair in requests.windows(2) {
        assert_request_extends(&pair[0], &pair[1]);
    }
    // The authoritative snapshot sent in the first request is never rewritten:
    // supersession is explicit via appended higher-revision deltas.
    let snapshots: Vec<&str> = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .filter(|message| message.content.contains("KERNEL STATE SNAPSHOT"))
        .map(|message| message.content.as_str())
        .collect();
    assert!(!snapshots.is_empty(), "an epoch starts with a snapshot");
    assert!(
        snapshots.windows(2).all(|pair| pair[0] == pair[1]),
        "earlier kernel snapshots must stay byte-identical"
    );
    // Architecture cacheability is measured on the real requests: every turn
    // after the first shares a large exact prefix.
    let stats: Vec<ContextStats> = store
        .events(session)
        .unwrap()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ContextMaterialized { stats } => Some(stats.clone()),
            _ => None,
        })
        .collect();
    assert!(stats.len() >= requests.len());
    assert_eq!(stats[0].common_prefix_tokens, 0, "no previous request yet");
    let last = stats.last().unwrap();
    assert!(last.request_tokens > 0);
    assert!(last.common_prefix_tokens > 0);
    let cacheability = last.common_prefix_tokens as f64 / last.request_tokens as f64;
    assert!(
        cacheability > 0.5,
        "append-only requests should share a large prefix, got {cacheability}"
    );
}

/// Kernel deltas append; they never rewrite the history that was already sent.
/// Once canonical state is unchanged, materialization is a pure read.
#[test]
fn kernel_state_deltas_append_without_rewriting_history() {
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(Path::new("/invariants")).unwrap();
    store
        .append(
            session,
            EventPayload::UserMessage {
                text: "fix the failing test".into(),
                media: vec![],
            },
        )
        .unwrap();
    let engine = ContinuityEngine::new(store.clone(), ContextConfig::default());
    let budget = engine.default_budget(64_000, 0);
    let mut state = TaskStateManager::default();
    state.update(StateUpdate {
        goal: Some("fix the failing test".into()),
        ..Default::default()
    });
    let first = materialize(&engine, session, state.state(), None, &budget);
    assert_eq!(first.stats.cache_epoch, 0, "no rotation under budget");

    state.update(StateUpdate {
        add_constraints: vec!["keep the public API stable".into()],
        ..Default::default()
    });
    let second = materialize(&engine, session, state.state(), None, &budget);
    assert_eq!(second.stats.cache_epoch, first.stats.cache_epoch);
    assert_eq!(
        second.recent.len(),
        first.recent.len() + 1,
        "a changed canonical render appends exactly one delta"
    );
    assert_eq!(
        &second.recent[..first.recent.len()],
        &first.recent[..],
        "prior provider-visible events are untouched"
    );
    let snapshot = |context: &MaterializedContext| {
        kernel_bodies(context)
            .find(|body| body.contains("KERNEL STATE SNAPSHOT"))
            .map(str::to_owned)
    };
    assert_eq!(
        snapshot(&first),
        snapshot(&second),
        "the epoch snapshot remains byte-identical"
    );
    assert!(
        kernel_bodies(&second).any(|body| body.contains("KERNEL STATE UPDATE")
            && body.contains("keep the public API stable")),
        "the appended delta carries the complete current truth"
    );

    // Unchanged canonical state does not churn the epoch.
    let third = materialize(&engine, session, state.state(), None, &budget);
    assert_eq!(third.recent, second.recent);
}

/// Rotation is periodic and hysteretic, never per-turn. Raw history and
/// canonical truth (constraints, decisions, unresolved work) survive it, and
/// recall still reaches material that left the provider-visible epoch.
#[test]
fn rotation_is_occasional_and_preserves_truth_and_recall() {
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(Path::new("/invariants")).unwrap();
    store
        .append(
            session,
            EventPayload::UserMessage {
                text: "early diagnostic: EADDRINUSE on port 4317".into(),
                media: vec![],
            },
        )
        .unwrap();
    let mut state = TaskStateManager::default();
    state.update(StateUpdate {
        goal: Some("finish the long-horizon task".into()),
        add_constraints: vec!["keep the public API stable".into()],
        add_decisions: vec!["rotate cache epochs, never memory".into()],
        add_hypotheses: vec!["rewrite the transport".into()],
        reject_hypotheses: vec!["rewrite the transport".into()],
        open_questions: Some(vec!["does recall survive rotation?".into()]),
        next_actions: Some(vec!["verify resume equivalence".into()]),
        ..Default::default()
    });
    let config = ContextConfig {
        max_request_tokens: Some(20_000),
        recent_tokens: 500,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let engine = ContinuityEngine::new(store.clone(), config.clone());
    let budget = engine.default_budget(64_000, 0);
    let mut latest = materialize(&engine, session, state.state(), None, &budget);
    let mut turns = 0usize;
    while rotation_count(&store, session) < 2 && turns < 40 {
        store
            .append(
                session,
                EventPayload::UserMessage {
                    text: format!("turn {turns} {}", "x".repeat(200)),
                    media: vec![],
                },
            )
            .unwrap();
        store
            .append(
                session,
                EventPayload::AssistantMessageCompleted {
                    text: format!("answer {turns}"),
                    tool_calls: vec![],
                    reasoning_content: None,

                    reasoning: vec![],
                },
            )
            .unwrap();
        latest = materialize(&engine, session, state.state(), None, &budget);
        turns += 1;
    }
    let rotations = rotation_count(&store, session);
    assert!(
        rotations >= 2,
        "working-memory pressure must rotate the epoch"
    );
    assert!(
        rotations * 2 <= turns + 2,
        "rotation is periodic, not per-turn: {rotations} rotations over {turns} turns"
    );
    // Canonical truth is authoritative after rotation.
    assert!(latest.canonical.contains("keep the public API stable"));
    assert!(
        latest
            .canonical
            .contains("rotate cache epochs, never memory")
    );
    assert!(latest.canonical.contains("does recall survive rotation?"));
    assert!(latest.canonical.contains("verify resume equivalence"));
    assert!(latest.stats.cache_epoch >= 1);
    let memory_section = latest
        .canonical
        .split("DURABLE MEMORY")
        .nth(1)
        .unwrap_or_default();
    assert!(
        !memory_section.contains("rewrite the transport"),
        "a rejected hypothesis is never current truth"
    );

    // Resume equivalence: a cold engine reconstructs the same provider-visible
    // epoch from durable state alone.
    let resumed = ContinuityEngine::new(store.clone(), config);
    let resumed_ctx = materialize(&resumed, session, state.state(), None, &budget);
    assert_eq!(resumed_ctx.recent, latest.recent);
    assert_eq!(resumed_ctx.stats.cache_epoch, latest.stats.cache_epoch);
    assert_eq!(
        resumed_ctx.stats.cache_epoch_turns,
        latest.stats.cache_epoch_turns
    );
    assert_eq!(resumed_ctx.episodes, latest.episodes);

    // Rotation is not memory deletion: no hidden compaction, all raw events
    // retained, and FTS recall reaches the evicted diagnostic.
    let raw = store.events(session).unwrap();
    assert!(
        !raw.iter()
            .any(|event| matches!(event.payload, EventPayload::ManualCompact { .. })),
        "rotation must not masquerade as compaction"
    );
    assert_eq!(
        raw.iter()
            .filter(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
            .count(),
        turns + 1,
        "every user turn stays durable"
    );
    let recalled = engine.recall(session, "EADDRINUSE 4317").unwrap();
    assert!(
        recalled
            .iter()
            .any(|event| matches!(&event.payload, EventPayload::UserMessage {  text, .. } if text.contains("EADDRINUSE"))),
        "evicted material stays reachable through exact recall"
    );
    let queried = materialize(
        &engine,
        session,
        state.state(),
        Some("EADDRINUSE 4317"),
        &budget,
    );
    assert!(
        queried.recalled.contains("EADDRINUSE")
            || kernel_bodies(&queried)
                .any(|body| body.contains("KERNEL RECALL") && body.contains("EADDRINUSE")),
        "recall material reaches the provider-visible epoch as a delta"
    );
}

/// Tool transactions are atomic across rotation: an assistant tool-call turn
/// and every result answering it are retained or evicted as one unit, so no
/// provider ever observes a dangling call or result.
#[test]
fn tool_transactions_survive_rotation_atomically() {
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(Path::new("/invariants")).unwrap();
    for index in 0..12 {
        store
            .append(
                session,
                EventPayload::UserMessage {
                    text: format!("request {index}"),
                    media: vec![],
                },
            )
            .unwrap();
        store
            .append(
                session,
                EventPayload::AssistantMessageCompleted {
                    text: format!("calling {index}"),
                    tool_calls: vec![ToolCall {
                        id: format!("call-{index}"),
                        name: "read_file".into(),
                        arguments: json!({"path": format!("src/{index}.rs")}),
                    }],
                    reasoning_content: None,

                    reasoning: vec![],
                },
            )
            .unwrap();
        store
            .append(
                session,
                EventPayload::ToolCompleted {
                    result: ToolResult {
                        call_id: format!("call-{index}"),
                        name: "read_file".into(),
                        output: "y".repeat(600),
                        is_error: false,
                        artifact_id: None,
                        media: Vec::new(),
                    },
                },
            )
            .unwrap();
    }
    let engine = ContinuityEngine::new(
        store.clone(),
        ContextConfig {
            max_request_tokens: Some(20_000),
            recent_tokens: 400,
            reserve_tokens: 0,
            output_reserve_tokens: 0,
        },
    );
    let budget = engine.default_budget(64_000, 0);
    let context = materialize(&engine, session, &TaskState::default(), None, &budget);
    assert_eq!(
        rotation_count(&store, session),
        1,
        "the saturated epoch rotates once"
    );
    assert_tool_transactions_atomic(&context.recent);
    assert!(
        context
            .recent
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ToolCompleted { .. })),
        "a useful working set is retained"
    );
}

/// Provider serialization is a pure function of the request: repeated builds
/// and repeated serialization produce byte-identical wire bodies, and tool
/// ordering is stable.
#[test]
fn provider_serialization_is_deterministic() {
    let dir = tempdir().unwrap();
    let request = ModelRequest {
        system: "stable system".into(),
        messages: vec![
            ModelMessage::text("user", "inspect"),
            ModelMessage {
                role: "assistant".into(),
                content: "working".into(),
                tool_calls: vec![call("c1", "read_file", json!({"path":"a.txt"}))],
                tool_call_id: None,
                reasoning_content: Some("reasoning must replay".into()),

                reasoning: vec![],
                media: Vec::new(),
            },
            ModelMessage {
                role: "tool".into(),
                content: "contents".into(),
                tool_calls: vec![],
                tool_call_id: Some("c1".into()),
                reasoning_content: None,

                reasoning: vec![],
                media: Vec::new(),
            },
        ],
        tools: ToolExecutor::definitions(),
    };
    let encode = |request: &ModelRequest| {
        (
            serde_json::to_string(
                &openai_request(request, "deepseek-test", ReasoningReplay::Replay).unwrap(),
            )
            .unwrap(),
            serde_json::to_string(&anthropic_request(request, "claude-test").unwrap()).unwrap(),
        )
    };
    assert_eq!(encode(&request), encode(&request));
    assert_eq!(
        ToolExecutor::definitions(),
        ToolExecutor::definitions(),
        "tool schema order is stable"
    );
    assert_eq!(
        PromptCompiler::compile(Mode::Work, dir.path())
            .unwrap()
            .text,
        PromptCompiler::compile(Mode::Work, dir.path())
            .unwrap()
            .text,
        "the compiled system prefix is session-stable"
    );
}

/// The model may describe observations but can never self-certify passing or
/// failing evidence; only the kernel's validate path produces that truth.
#[tokio::test]
async fn only_the_kernel_certifies_validation_evidence() {
    let dir = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let provider = Arc::new(RecordingProvider::new(vec![
        response(
            "",
            vec![call(
                "self-certify",
                "record_evidence",
                json!({"claim":"tests pass","status":"passed","detail":"trust me"}),
            )],
        ),
        response(
            "",
            vec![call(
                "observe",
                "record_evidence",
                json!({"claim":"tests pass","status":"pending","detail":"not yet verified"}),
            )],
        ),
        response("done", vec![]),
    ]));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let config = ContextConfig {
        max_request_tokens: Some(32_000),
        recent_tokens: 8_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), config.clone()),
        retry_budget: 2,
    });
    agent.set_context_budget(config, latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS);
    agent
        .run("verify it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(session).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolFailed { result } if result.call_id == "self-certify"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCompleted { result } if result.call_id == "observe"
    )));
    assert!(
        !events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::EvidenceCreated { evidence }
                if matches!(evidence.status, EvidenceStatus::Passed | EvidenceStatus::Failed)
        )),
        "passed/failed evidence is kernel-owned"
    );
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::EvidenceCreated { evidence } if evidence.status == EvidenceStatus::Pending
        )),
        "pending observations remain available to the model"
    );
    assert_eq!(
        agent.evidence().status_of("tests pass"),
        Some(EvidenceStatus::Pending)
    );
    assert_eq!(agent.state().completion, CompletionState::InProgress);
}

/// Hard-deny stays hard: privileged/system-destructive commands are denied in
/// every safety profile, independent of resolver and mode.
#[test]
fn hard_deny_is_independent_of_safety_profile() {
    let workspace = Path::new("/workspace");
    for safety in [Safety::Strict, Safety::Standard, Safety::Autonomous] {
        let context = SafetyContext {
            mode: Mode::Work,
            safety,
            workspace,
            outside: OutsidePolicy::Ask,
            workspace_write: true,
        };
        let classification =
            latch_kernel::safety::classify("shell", &json!({"command": "sudo rm -rf /"}), context);
        assert!(
            matches!(classification.decision, SafetyDecision::Deny(_)),
            "hard deny must not depend on {safety:?}"
        );
        // Unclassified tools remain an explicit Ask in every profile.
        let unknown = latch_kernel::safety::classify("mystery_tool", &json!({}), context);
        assert!(matches!(unknown.decision, SafetyDecision::Ask(_)));
    }
}

/// The steering handshake is atomic at the public boundary: a submission
/// after the run closed is rejected and can never leak into a later request.
#[tokio::test]
async fn a_closed_run_rejects_steering_instead_of_leaking_it() {
    let dir = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let provider = Arc::new(RecordingProvider::new(vec![
        response("first done", vec![]),
        response("second done", vec![]),
    ]));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let config = ContextConfig {
        max_request_tokens: Some(32_000),
        recent_tokens: 8_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), config.clone()),
        retry_budget: 2,
    });
    agent.set_context_budget(config, latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS);
    let sink: AgentEventSink = Arc::new(|_| {});
    agent
        .run("first", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    assert_eq!(
        agent.steering_handle().push("too late"),
        SteeringSubmission::Closed,
        "a closed run rejects instead of queueing for later"
    );
    agent
        .run("second", CancellationToken::new(), sink)
        .await
        .unwrap();
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "the rejected steer created no extra turn"
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|message| !message.content.contains("too late")),
        "a rejected steer must never appear in a later request"
    );
}

/// Cache accounting distinguishes architecture diagnostics from provider
/// measurements, and unknown provider categories stay unknown instead of being
/// fabricated as zero. Context totals are exact component sums.
#[test]
fn cache_accounting_stays_provider_authoritative() {
    let unknown = Usage {
        input_tokens: 1_000,
        output_tokens: 10,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cache_miss_tokens: None,

        reasoning_tokens: None,
    };
    assert_eq!(unknown.uncached_input_tokens(), None);

    let openai_style = Usage {
        input_tokens: 1_000,
        output_tokens: 10,
        cache_read_tokens: Some(600),
        cache_write_tokens: None,
        cache_miss_tokens: None,

        reasoning_tokens: None,
    };
    assert_eq!(openai_style.uncached_input_tokens(), Some(400));

    let explicit = Usage {
        input_tokens: 1_000,
        output_tokens: 10,
        cache_read_tokens: Some(600),
        cache_write_tokens: None,
        cache_miss_tokens: Some(350),

        reasoning_tokens: None,
    };
    assert_eq!(
        explicit.uncached_input_tokens(),
        Some(350),
        "an explicit provider miss is authoritative"
    );

    let mut stats = ContextStats {
        instructions_tokens: 11,
        state_tokens: 22,
        recent_tokens: 33,
        recall_tokens: 44,
        tools_tokens: 55,
        extension_tokens: 66,
        budget_tokens: 10_000,
        request_tokens: 10_000,
        common_prefix_tokens: 7_500,
        ..ContextStats::default()
    };
    stats.recompute();
    assert_eq!(stats.total_tokens, 11 + 22 + 33 + 44 + 55 + 66);
    assert_eq!(stats.headroom_tokens, 10_000 - stats.total_tokens);
    assert_eq!(stats.status, "bounded");
    // The three quantities are separate measures over separate inputs:
    // architecture cacheability, provider prefix utilization, and the
    // provider-reported hit rate. Keeping them named apart is the invariant.
    let architecture_cacheability = stats.common_prefix_tokens * 100 / stats.request_tokens.max(1);
    let provider_prefix_utilization = 600u64 * 100 / stats.common_prefix_tokens as u64;
    let measured_hit_rate = 600u64 * 100 / (600 + 400);
    assert_eq!(architecture_cacheability, 75);
    assert_eq!(provider_prefix_utilization, 8);
    assert_eq!(measured_hit_rate, 60);
    assert_ne!(
        architecture_cacheability, measured_hit_rate as usize,
        "architecture diagnostics and provider measurements are not one number"
    );
}

/// Child agents are independent durable sessions. Whatever a child claims or
/// proves stays in its own session: the root receives one compact semantic
/// report at a safe model boundary, never the child's evidence, completion,
/// or transcript, and the provider-visible tool schema stays fixed across the
/// whole agent lifecycle.
#[tokio::test]
async fn child_agent_sessions_never_become_root_truth() {
    let dir = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let provider = Arc::new(RecordingProvider::with_children(
        vec![
            response(
                "",
                vec![call(
                    "spawn-1",
                    "spawn_agent",
                    json!({"task_name":"isolation","message":"delegate a bounded check"}),
                )],
            ),
            response(
                "",
                vec![call("wait-1", "wait_agents", json!({"timeout_ms": 2_000}))],
            ),
            response("root done", vec![]),
        ],
        vec![
            response(
                "",
                vec![
                    call(
                        "child-observe",
                        "record_evidence",
                        json!({"claim":"child check","status":"pending","detail":"child-only"}),
                    ),
                    call(
                        "child-complete",
                        "complete",
                        json!({"implementation_done": true}),
                    ),
                ],
            ),
            response("child done", vec![]),
        ],
    ));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent.set_context_budget(
        ContextConfig::default(),
        latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
    );
    agent
        .run(
            "delegate a check",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    let root_events = store.events(session).unwrap();
    // The notification is the only graph event in root history and it never
    // splits a tool transaction.
    let notifications = root_events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::AgentNotificationDelivered { .. }
            )
        })
        .count();
    assert_eq!(notifications, 1, "exactly one delivered report");
    assert_tool_transactions_atomic(&root_events);
    // Child evidence and completion live only in the child session; the root
    // ledger and completion are untouched by the child's claims.
    assert!(
        agent.evidence().entries().is_empty(),
        "child evidence must never enter the root ledger"
    );
    assert_eq!(agent.state().completion, CompletionState::InProgress);
    assert!(
        !root_events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::EvidenceCreated { .. }))
    );
    let child_id = root_events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCompleted { result } if result.call_id == "spawn-1" => {
                serde_json::from_str::<serde_json::Value>(&result.output)
                    .ok()
                    .and_then(|value| value.get("agent_id")?.as_str().map(str::to_owned))
            }
            _ => None,
        })
        .expect("spawn result carries the child id")
        .parse::<uuid::Uuid>()
        .unwrap();
    let child_events = store.events(child_id).unwrap();
    assert!(child_events
        .iter()
        .any(|event| matches!(&event.payload, EventPayload::EvidenceCreated { evidence } if evidence.claim == "child check")));
    assert!(
        child_events
            .first()
            .is_some_and(|event| matches!(event.payload, EventPayload::AgentSpawned { .. }))
    );
    // The provider-visible schema is identical before and after the whole
    // spawn → report lifecycle.
    let requests = provider.requests();
    assert!(requests.len() >= 2);
    assert_eq!(
        serde_json::to_string(&requests[0].tools).unwrap(),
        serde_json::to_string(&requests[requests.len() - 1].tools).unwrap(),
        "agent lifecycle changes must not alter tool definitions"
    );
    // The durable graph replays to the same topology the supervisor holds.
    let agent_events = store.agent_events(session).unwrap();
    assert!(agent_events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::AgentSpawned { identity, .. } if identity.agent_id == child_id
    )));
    let listed = agent
        .agent_supervisor()
        .expect("root owns a supervisor")
        .list_agents();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].agent_id, child_id);
    assert_eq!(listed[0].status, latch_protocol::AgentStatus::Completed);
}
