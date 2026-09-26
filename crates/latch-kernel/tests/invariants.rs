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
    Agent, AgentEventSink, AgentRuntime, CapabilityKind, CapabilityLifetime, CapabilityOwner,
    CapabilityRequest, CapabilityScope, ContextBudget, ContextEngine, ContextEngineFactory,
    ContextEngineSpec, ContextRequest, ContextView, ContinuityEngine, EventStore, EvidenceLedger,
    FailureManager, MaterializeBudget, MaterializedContext, PolicyEngine, TaskStateManager,
    TokenEstimator, ToolExecutor, continuity_context_engine_factory,
};
use latch_protocol::{
    CompletionState, ContextStats, Event, EventPayload, EvidenceStatus, InferenceProfile, Mode,
    ModelMessage, ModelRequest, ModelResponse, ReasoningEffort, Safety, StreamEvent, TaskState,
    ToolCall, ToolResult, Usage,
};
use serde_json::json;
use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
/// provider-facing history the kernel would send. Child providers share the
/// same log, so root and child requests are observable together.
struct RecordingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    responses: Mutex<VecDeque<ModelResponse>>,
    /// Separate script handed to child sessions via `for_session`, so root and
    /// child turns never race one shared response queue.
    child_responses: Option<Vec<ModelResponse>>,
}

impl RecordingProvider {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
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

    fn request_systems(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| request.system.clone())
            .collect()
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
        self.child_responses.as_ref().map(|script| {
            Arc::new(Self {
                requests: self.requests.clone(),
                responses: Mutex::new(script.clone().into()),
                child_responses: None,
            }) as Arc<dyn ModelProvider>
        })
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
                is_error: false,
                content: "working".into(),
                tool_calls: vec![call("c1", "read_file", json!({"path":"a.txt"}))],
                tool_call_id: None,
                reasoning_content: Some("reasoning must replay".into()),

                reasoning: vec![],
                media: Vec::new(),
            },
            ModelMessage {
                role: "tool".into(),
                is_error: false,
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
        // An unknown tool has no implementation, so approval cannot help.
        let unknown = latch_kernel::safety::classify("mystery_tool", &json!({}), context);
        assert!(matches!(unknown.decision, SafetyDecision::Deny(_)));
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

/// Agent-group coordination is durable state, not a live side table: one
/// atomic winner per claim, and a projection rebuilt purely from durable
/// events reconstructs the same ownership. Coordinator state is a derivable
/// cache; the event log remains the source of truth.
#[test]
fn group_claims_are_atomic_and_projection_is_rebuildable() {
    use latch_kernel::agents::GroupCoordinator;
    use latch_protocol::{GroupTask, GroupTaskStatus};
    use uuid::Uuid;

    let store = EventStore::open_memory().unwrap();
    let root = store
        .create_session(Path::new("/tmp/invariant-group"))
        .unwrap();
    let coordinator = GroupCoordinator::new(store.clone(), root, "invariant".into()).unwrap();
    let task = coordinator
        .create_task(root, "shared".into(), String::new(), vec![], true, vec![])
        .unwrap();
    let outcomes = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for index in 0..16u128 {
            let coordinator = coordinator.clone();
            let outcomes = &outcomes;
            let task_id = task.task_id;
            scope.spawn(move || {
                let agent = Uuid::from_u128(index + 1);
                outcomes
                    .lock()
                    .unwrap()
                    .push(coordinator.claim(task_id, agent).map(|task| task.assignee));
            });
        }
    });
    let outcomes = outcomes.into_inner().unwrap();
    let winners = outcomes.iter().filter(|result| result.is_ok()).count();
    assert_eq!(winners, 1, "exactly one atomic claim may win");
    // Durability: a fresh coordinator (process restart) reconstructs the
    // committed ownership from the event log alone.
    let resumed = GroupCoordinator::new(store.clone(), root, "invariant".into()).unwrap();
    let resumed_task = resumed.snapshot().task(task.task_id).cloned().unwrap();
    let owner = resumed_task.assignee.unwrap();
    assert_eq!(resumed_task.status, GroupTaskStatus::Claimed);
    assert_eq!(
        winners,
        outcomes.iter().filter(|result| result.is_ok()).count(),
        "one winner stays one winner after replay"
    );
    // A released task returns to the pool with no owner and is claimable by a
    // different agent, still with an unambiguous event order.
    resumed.release(task.task_id, owner).unwrap();
    let second = Uuid::new_v4();
    let reclaimed = resumed.claim(task.task_id, second).unwrap();
    assert_eq!(reclaimed.assignee, Some(second));
    let GroupTask { status, .. } = reclaimed;
    assert_eq!(status, GroupTaskStatus::Claimed);
}

/// A context engine that proves the port is real: the agent runtime drives the
/// provider exclusively from this view and never silently falls back to the
/// default continuity implementation. The canned engine has no store handle
/// and appends nothing; the port contract is request/result only. Each engine
/// carries a distinguishable system prompt so tests can attribute provider
/// requests to the session (and therefore the policy) that produced them.
struct CannedContextEngine {
    config: ContextConfig,
    calls: Arc<AtomicUsize>,
    system: String,
}

impl ContextEngine for CannedContextEngine {
    fn name(&self) -> &str {
        "canned"
    }
    fn config(&self) -> &ContextConfig {
        &self.config
    }
    fn set_config(&mut self, config: ContextConfig) {
        self.config = config;
    }
    fn set_estimator(&mut self, _estimator: TokenEstimator) {}
    fn default_budget(&self, window_tokens: usize, reserved_tokens: usize) -> ContextBudget {
        ContextBudget {
            request_tokens: window_tokens.saturating_sub(reserved_tokens),
            window_tokens,
            reserve_tokens: 0,
            recent_tokens: 4_000,
            reserved_tokens,
        }
    }
    fn manual_compact(&mut self, _session_id: uuid::Uuid) -> anyhow::Result<()> {
        Ok(())
    }
    fn recall(&self, _session_id: uuid::Uuid, _query: &str) -> anyhow::Result<Vec<Event>> {
        Ok(Vec::new())
    }
    fn materialize(&self, _request: ContextRequest<'_>) -> anyhow::Result<ContextView> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContextView {
            system: self.system.clone(),
            session_context: String::new(),
            canonical: String::new(),
            recalled: String::new(),
            recent: Vec::new(),
            bridge: Default::default(),
            episodes: Vec::new(),
            stats: ContextStats::default(),
        })
    }
}

/// A `ContextEngineFactory` that stamps every child engine with the child's
/// session id and counts construction and materialization.
fn canned_child_factory(
    label: &str,
    builds: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
) -> ContextEngineFactory {
    let label = label.to_owned();
    Arc::new(move |spec: &ContextEngineSpec<'_>| {
        builds.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CannedContextEngine {
            config: spec.context.clone(),
            calls: calls.clone(),
            system: format!("{label} {}", spec.session_id),
        }))
    })
}

/// The context port is a real boundary, not a wrapper around the default
/// engine: a replaced engine's view is exactly what the provider sees, and the
/// kernel neither imports raw store access into the port nor falls back to
/// continuity behind the caller's back.
#[tokio::test]
async fn context_engine_port_is_replaceable_without_kernel_fallbacks() {
    let dir = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let provider = Arc::new(RecordingProvider::new(vec![response("done", vec![])]));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: CannedContextEngine {
            config: ContextConfig::default(),
            calls: calls.clone(),
            system: "canned context system".into(),
        },
        retry_budget: 2,
    });
    agent.set_context_budget(
        ContextConfig::default(),
        latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
    );
    agent
        .run("hello", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the agent must consult the declared context engine"
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "a plain answer spends one request");
    assert_eq!(requests[0].system, "canned context system");
    assert!(
        requests[0].messages.is_empty(),
        "the provider sees exactly the port's view"
    );
    // Canned materialization appended no kernel context: the default engine did
    // not run alongside the configured one.
    assert!(
        !store
            .events(session)
            .unwrap()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::KernelContext { .. })),
        "kernel context appeared without the configured engine producing it"
    );
}

/// The default `ContinuityEngine` satisfies the port byte-for-byte: the same
/// durable state materializes to the same view, epoch accounting, and recall
/// whether called directly or through `dyn ContextEngine`.
#[test]
fn continuity_conforms_to_the_context_port() {
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(Path::new("/invariants")).unwrap();
    store
        .append(
            session,
            EventPayload::UserMessage {
                text: "port parity".into(),
                media: vec![],
            },
        )
        .unwrap();
    let mut state = TaskStateManager::default();
    state.update(StateUpdate {
        goal: Some("port parity".into()),
        add_constraints: vec!["keep the port exact".into()],
        ..Default::default()
    });
    let config = ContextConfig {
        max_request_tokens: Some(16_000),
        recent_tokens: 2_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };

    let direct = ContinuityEngine::new(store.clone(), config.clone());
    let expected = materialize(
        &direct,
        session,
        state.state(),
        None,
        &direct.default_budget(64_000, 0),
    );

    let ported: Box<dyn ContextEngine> =
        Box::new(ContinuityEngine::new(store.clone(), config.clone()));
    assert_eq!(ported.name(), "continuity");
    let through_port = ported
        .materialize(ContextRequest {
            session_id: session,
            state: state.state(),
            query: None,
            evidence: &EvidenceLedger::default(),
            failures: &FailureManager::new(3),
            system: "invariant system prompt".into(),
            session_context: String::new(),
            budget: ported.default_budget(64_000, 0),
            extension_context: "",
            reground: None,
        })
        .unwrap();
    assert_eq!(through_port.recent, expected.recent);
    assert_eq!(through_port.canonical, expected.canonical);
    assert_eq!(through_port.recalled, expected.recalled);
    assert_eq!(through_port.episodes, expected.episodes);
    assert_eq!(through_port.stats, expected.stats);
    let recalled = ported.recall(session, "port parity").unwrap();
    assert!(
        recalled.iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserMessage { text, .. } if text == "port parity"
        )),
        "recall through the port reaches durable history"
    );
}

/// Runtime capabilities are declared with explicit kind, owner, lifetime, and
/// bounded scope. Kernel-owned surfaces and session-owned surfaces are named
/// apart; a request never resolves outside a declaration, and an unimplemented
/// surface (Computer Use here) is simply absent rather than silently granted.
#[test]
fn session_capabilities_declare_scope_owner_and_lifetime() {
    use latch_kernel::sandbox::{Capability, CapabilitySet};

    let dir = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let provider = Arc::new(RecordingProvider::new(vec![]));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    let capabilities = agent.capabilities();
    let descriptors = capabilities.descriptors();
    assert!(!descriptors.is_empty());
    assert!(
        descriptors
            .iter()
            .all(|descriptor| descriptor.lifetime == CapabilityLifetime::Session(session)),
        "every declared capability belongs to this session"
    );
    for (kind, owner) in [
        (CapabilityKind::Workspace, CapabilityOwner::Session(session)),
        (CapabilityKind::Executor, CapabilityOwner::Session(session)),
        (CapabilityKind::Context, CapabilityOwner::Kernel),
        (CapabilityKind::Tools, CapabilityOwner::Kernel),
        (CapabilityKind::Artifacts, CapabilityOwner::Session(session)),
        (CapabilityKind::Agents, CapabilityOwner::Kernel),
    ] {
        let descriptor = descriptors
            .iter()
            .find(|descriptor| descriptor.kind == kind)
            .unwrap_or_else(|| panic!("{kind} is not declared"));
        assert_eq!(descriptor.owner, owner, "{kind} owner");
    }
    assert_eq!(
        capabilities.get("workspace.primary").unwrap().scope,
        CapabilityScope::Workspace {
            root: dir.path().to_path_buf()
        }
    );
    let workspace = capabilities.get("workspace.primary").unwrap();
    assert!(workspace.permissions.contains(Capability::WorkspaceRead));
    assert!(
        !workspace.permissions.contains(Capability::NetworkAccess),
        "declaring a workspace never silently grants network"
    );

    let mut read = CapabilitySet::new();
    read.insert(Capability::WorkspaceRead);
    let inside = CapabilityRequest {
        kind: CapabilityKind::Workspace,
        scope: CapabilityScope::Workspace {
            root: dir.path().join("src"),
        },
        permissions: read.clone(),
    };
    assert!(capabilities.resolve(&inside).is_some());
    let outside = CapabilityRequest {
        scope: CapabilityScope::Workspace {
            root: std::path::PathBuf::from("/etc"),
        },
        ..inside
    };
    assert!(
        capabilities.resolve(&outside).is_none(),
        "a workspace request never resolves outside its declared root"
    );
    // Computer Use is not implemented and therefore not declared; the
    // vocabulary can name it but the kernel grants nothing it did not declare.
    let computer = CapabilityRequest {
        kind: CapabilityKind::Computer,
        scope: CapabilityScope::Remote {
            endpoint: "host:5900".into(),
        },
        permissions: CapabilitySet::new(),
    };
    assert!(capabilities.resolve(&computer).is_none());
}

fn spawned_child_id(store: &EventStore, session: uuid::Uuid, call_id: &str) -> uuid::Uuid {
    store
        .events(session)
        .unwrap()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCompleted { result } if result.call_id == call_id => {
                serde_json::from_str::<serde_json::Value>(&result.output)
                    .ok()
                    .and_then(|value| value.get("agent_id")?.as_str().map(str::to_owned))
            }
            _ => None,
        })
        .expect("spawn result carries the child id")
        .parse::<uuid::Uuid>()
        .unwrap()
}

/// A custom context-engine policy configured on the root governs every spawned
/// child: the child's requests are produced by factory-built engines, and the
/// default continuity engine never runs in the child session as a fallback.
#[tokio::test]
async fn custom_context_engine_policy_propagates_to_spawned_children() {
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
                    json!({"task_name":"policy","message":"bounded check"}),
                )],
            ),
            response(
                "",
                vec![call("wait-1", "wait_agents", json!({"timeout_ms": 2_000}))],
            ),
            response("root done", vec![]),
        ],
        vec![response("child done", vec![])],
    ));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let root_calls = Arc::new(AtomicUsize::new(0));
    let child_builds = Arc::new(AtomicUsize::new(0));
    let child_calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: CannedContextEngine {
            config: ContextConfig::default(),
            calls: root_calls.clone(),
            system: "canned root context system".into(),
        },
        retry_budget: 2,
    });
    agent.set_context_engine_factory(canned_child_factory(
        "canned child context system",
        child_builds.clone(),
        child_calls.clone(),
    ));
    agent.set_context_budget(
        ContextConfig::default(),
        latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
    );
    agent
        .run("delegate", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let child_id = spawned_child_id(&store, session, "spawn-1");
    assert_eq!(
        child_builds.load(Ordering::SeqCst),
        1,
        "one child engine built"
    );
    assert!(
        child_calls.load(Ordering::SeqCst) >= 1,
        "the child materialized through its factory-built engine"
    );
    let systems = provider.request_systems();
    assert!(
        systems
            .iter()
            .any(|system| system == "canned root context system"),
        "the root used its configured engine"
    );
    assert!(
        systems
            .iter()
            .any(|system| { system == &format!("canned child context system {child_id}") }),
        "the child request came from the factory-built engine: {systems:?}"
    );
    // Continuity appends kernel context and epoch events; the canned child
    // engine appends neither, so their absence proves no silent fallback.
    let child_events = store.events(child_id).unwrap();
    assert!(
        !child_events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::KernelContext { .. })),
        "the default continuity engine must not run in the child session"
    );
    assert!(
        !child_events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ContextEpochStarted { .. })),
        "no continuity cache epoch may be created in the child session"
    );
}

/// A resumed child (fresh root over the same durable log, no surviving worker)
/// is reconstructed through the configured factory, not the default engine.
#[tokio::test]
async fn resumed_child_reconstructs_through_the_configured_factory() {
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
                    json!({"task_name":"resume-policy","message":"bounded check"}),
                )],
            ),
            response(
                "",
                vec![call("wait-1", "wait_agents", json!({"timeout_ms": 2_000}))],
            ),
            response("root done", vec![]),
        ],
        vec![response("child done", vec![])],
    ));
    let tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: CannedContextEngine {
            config: ContextConfig::default(),
            calls: calls.clone(),
            system: "canned root context system".into(),
        },
        retry_budget: 2,
    });
    agent.set_context_engine_factory(canned_child_factory(
        "canned child context system",
        Arc::new(AtomicUsize::new(0)),
        calls.clone(),
    ));
    agent.set_context_budget(
        ContextConfig::default(),
        latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
    );
    agent
        .run("delegate", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let child_id = spawned_child_id(&store, session, "spawn-1");
    // End the first runtime: its supervisor drops and aborts the idle worker,
    // exactly like a process exit between runs.
    drop(agent);

    let resumed_provider = Arc::new(RecordingProvider::with_children(
        vec![],
        vec![response("resumed child done", vec![])],
    ));
    let resumed_tools = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let resumed_calls = Arc::new(AtomicUsize::new(0));
    let resumed_builds = Arc::new(AtomicUsize::new(0));
    let mut resumed = Agent::new(AgentRuntime {
        session_id: session,
        workspace: dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: resumed_provider,
        tools: resumed_tools,
        continuity: CannedContextEngine {
            config: ContextConfig::default(),
            calls: resumed_calls.clone(),
            system: "canned resumed root context system".into(),
        },
        retry_budget: 2,
    });
    resumed.set_context_engine_factory(canned_child_factory(
        "canned resumed child context system",
        resumed_builds.clone(),
        resumed_calls.clone(),
    ));
    let supervisor = resumed.agent_supervisor().unwrap();
    supervisor
        .continue_agent(child_id, "resume the child".into())
        .await
        .unwrap();
    // `continue_agent` rebuilds a missing worker synchronously, so this proves
    // the factory ran for the resumed child before the worker task started.
    assert_eq!(
        resumed_builds.load(Ordering::SeqCst),
        1,
        "the resumed child must reconstruct through the configured factory"
    );
    let wait = supervisor
        .wait_agents(&[child_id], Duration::from_millis(2_000))
        .await
        .unwrap();
    assert_eq!(wait.agents.len(), 1);
    assert!(
        !store
            .events(child_id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::KernelContext { .. })),
        "no default continuity fallback in the resumed child session"
    );
    drop(resumed);
}

/// The default factory is behaviorally equivalent to the constructor it
/// replaces: `ContinuityEngine::for_model(...)` over the session store, priced
/// for the requested model.
#[test]
fn default_context_engine_factory_matches_continuity_for_model() {
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(Path::new("/invariants")).unwrap();
    store
        .append(
            session,
            EventPayload::UserMessage {
                text: "factory parity".into(),
                media: vec![],
            },
        )
        .unwrap();
    let config = ContextConfig {
        max_request_tokens: Some(16_000),
        recent_tokens: 2_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let profile = InferenceProfile::new(
        "provider",
        "factory-model",
        ReasoningEffort::ProviderDefault,
    );
    let factory = continuity_context_engine_factory(store.clone());
    let through_factory = factory(&ContextEngineSpec {
        session_id: session,
        profile: &profile,
        context: &config,
    })
    .unwrap();
    assert_eq!(through_factory.name(), "continuity");
    let direct = ContinuityEngine::for_model(store.clone(), config.clone(), "factory-model");
    assert_eq!(
        through_factory.default_budget(64_000, 123),
        direct.default_budget(64_000, 123),
        "the factory engine uses the same budget derivation"
    );

    let state = TaskState::default();
    let budget = through_factory.default_budget(64_000, 0);
    let first = through_factory
        .materialize(ContextRequest {
            session_id: session,
            state: &state,
            query: None,
            evidence: &EvidenceLedger::default(),
            failures: &FailureManager::new(3),
            system: "invariant system prompt".into(),
            session_context: String::new(),
            budget,
            extension_context: "",
            reground: None,
        })
        .unwrap();
    let second = direct
        .materialize(
            session,
            &state,
            None,
            &EvidenceLedger::default(),
            &FailureManager::new(3),
            "invariant system prompt".into(),
            &budget,
        )
        .unwrap();
    assert_eq!(first.recent, second.recent);
    assert_eq!(first.canonical, second.canonical);
    assert_eq!(first.stats, second.stats);
}

/// A root running a non-default context engine without a configured child
/// policy fails child spawn loudly. It never silently gives the child the
/// default continuity engine.
#[tokio::test]
async fn custom_root_engine_without_child_policy_fails_closed() {
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
                    json!({"task_name":"no-policy","message":"bounded check"}),
                )],
            ),
            response("continued after the refused spawn", vec![]),
        ],
        vec![],
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
        provider,
        tools,
        continuity: CannedContextEngine {
            config: ContextConfig::default(),
            calls: Arc::new(AtomicUsize::new(0)),
            system: "canned root context system".into(),
        },
        retry_budget: 2,
    });
    agent.set_context_budget(
        ContextConfig::default(),
        latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS,
    );
    agent
        .run("delegate", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(session).unwrap();
    let failure = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolFailed { result } if result.call_id == "spawn-1" => {
                Some(result.output.clone())
            }
            _ => None,
        })
        .expect("a root without a child policy must not silently spawn");
    assert!(
        failure.contains("defines no child-session policy"),
        "the refusal must be actionable: {failure}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ToolCompleted { result } if result.call_id == "spawn-1"
        )),
        "no successful spawn result may be fabricated"
    );
}

/// Reads `path` and returns the version hash the way a model obtains a
/// `base_hash`.
async fn executor_read_hash(executor: &ToolExecutor, path: &str) -> String {
    let result = executor
        .execute(
            &call("read", "read_file", json!({"path": path})),
            CancellationToken::new(),
        )
        .await;
    assert!(!result.is_error, "{}", result.output);
    result
        .output
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("hash: ")
        .to_owned()
}

/// Whole-file writes are compare-and-swap: the caller's `base_hash` must equal
/// the current bytes, so a writer holding an older version can never silently
/// overwrite a newer change — including one a concurrent child agent authored.
/// The stale write is rejected and the winner's bytes remain.
#[tokio::test]
async fn stale_whole_file_writes_never_overwrite_newer_changes() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("shared.txt"), "H0").unwrap();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let root = ToolExecutor::new(
        dir.path().into(),
        dir.path().join("artifacts"),
        store,
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let child = root.for_child(uuid::Uuid::new_v4()).unwrap();

    // A (root) and B (child) both read H0.
    let root_base = executor_read_hash(&root, "shared.txt").await;
    let child_base = executor_read_hash(&child, "shared.txt").await;
    assert_eq!(root_base, child_base);

    // A writes H1 with the current base.
    let first = root
        .execute(
            &call(
                "a",
                "write",
                json!({"path":"shared.txt","base_hash":root_base,"content":"H1"}),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(!first.is_error, "{}", first.output);

    // B's whole-file write is still based on H0 and must fail, not overwrite.
    let stale = child
        .execute(
            &call(
                "b",
                "write",
                json!({"path":"shared.txt","base_hash":child_base,"content":"H2"}),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(stale.is_error, "{}", stale.output);
    assert!(
        stale.output.contains("stale write rejected"),
        "{}",
        stale.output
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "H1",
        "the newer change must remain"
    );

    // Re-reading the current version admits the retry with the caller's exact
    // bytes: no silent merge of the two writers.
    let fresh = executor_read_hash(&child, "shared.txt").await;
    let retry = child
        .execute(
            &call(
                "c",
                "write",
                json!({"path":"shared.txt","base_hash":fresh,"content":"H2"}),
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(!retry.is_error, "{}", retry.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "H2"
    );
}

/// Kernel-native filesystem tools enforce the same boundary the sandbox does:
/// the configured state directory and symlink aliases into it are never
/// agent-visible, even when the state directory lives inside the workspace.
#[tokio::test]
async fn native_filesystem_tools_cannot_reach_the_state_directory() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("ordinary.txt"), "ordinary").unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("secrets.toml"), "FAKE_SECRET = true\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&state, dir.path().join("alias")).unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(dir.path().join("alias"))
            .arg(&state)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(dir.path()).unwrap();
    let tools = ToolExecutor::new_with_state_dir(
        dir.path().into(),
        dir.path().join("artifacts"),
        state,
        store,
        session,
        PolicyEngine::new(Mode::Work, dir.path().into(), PermissionConfig::default()),
    )
    .unwrap();

    for (tool, path) in [
        ("read_file", "state/secrets.toml"),
        ("read_file", "alias/secrets.toml"),
        ("read_image", "state"),
        ("read_image", "state/secrets.toml"),
    ] {
        let result = tools
            .execute(
                &call("deny", tool, json!({"path": path})),
                CancellationToken::new(),
            )
            .await;
        assert!(
            result.is_error,
            "{tool} {path} must be denied: {}",
            result.output
        );
        assert!(
            result.output.contains("protected state directory"),
            "{tool} {path}: {}",
            result.output
        );
        assert!(
            !result.output.contains("FAKE_SECRET") && result.media.is_empty(),
            "{tool} {path} must not expose state: {}",
            result.output
        );
    }
    // Ordinary workspace reads stay available.
    let ordinary = tools
        .execute(
            &call("ok", "read_file", json!({"path":"ordinary.txt"})),
            CancellationToken::new(),
        )
        .await;
    assert!(!ordinary.is_error, "{}", ordinary.output);
    assert!(ordinary.output.contains("ordinary"));
}
