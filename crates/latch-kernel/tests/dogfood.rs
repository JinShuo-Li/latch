use latch_kernel::agent::AgentOutput;
use latch_kernel::config::{ContextConfig, PermissionConfig};
use latch_kernel::provider::{ModelProvider, ReasoningReplay, StreamSink};
use latch_kernel::session::{prompt_history, replay_items, resumed_mode};
use latch_kernel::{
    Agent, AgentEventSink, AgentRuntime, ContinuityEngine, EventStore, FakeProvider, PolicyEngine,
    ToolExecutor,
};
use latch_protocol::{
    CompletionState, DisplayItem, Event, EventPayload, EvidenceStatus, Mode, ModelRequest,
    ModelResponse, StreamEvent, ToolCall, ToolResult, display_items,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
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
    }
}
fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}
fn digest(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Provider that records every request so tests can inspect the exact
/// serialized history the kernel would send to an OpenAI-compatible endpoint.
struct RecordingProvider {
    requests: Mutex<Vec<ModelRequest>>,
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl RecordingProvider {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
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
        "deepseek-test"
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
        for chunk in response.text.as_bytes().chunks(8) {
            sink(StreamEvent::TextDelta(
                String::from_utf8_lossy(chunk).into_owned(),
            ));
        }
        sink(StreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

/// The full V0.2.0 dogfood: ASK inspects, PLAN scopes with a validation
/// requirement, WORK validates (fails), fixes through a guarded edit,
/// revalidates (passes), and completion becomes VERIFIED — without the model
/// ever seeing or supplying an internal event/call id. Resume restores the
/// transcript, mode, state, evidence, failure supervision, and undoable
/// change ownership.
#[tokio::test]
async fn scripted_long_session_dogfood() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&workspace)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@l",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&workspace)
        .status()
        .unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let bug_hash = digest("bug");
    let wrong_hash = digest("wrong");
    let scripted = vec![
        // ASK: inspect only.
        response(
            "I will inspect it.",
            vec![call("ask-read", "read_file", json!({"path":"app.txt"}))],
        ),
        response("The fixture currently contains the bug marker.", vec![]),
        // PLAN: record minimal scope and the validation requirement.
        response(
            "I will record the selected approach.",
            vec![call(
                "plan-state",
                "task_update",
                json!({"add_constraints":["Constraint A: preserve plain-text format"],"add_decisions":["Decision B: replace only the marker"],"add_hypotheses":["Approach C: rewrite the fixture"],"reject_hypotheses":["Approach C: rewrite the fixture"],"required_validations":["fixture exact-content check"],"open_questions":["does the marker need escaping?"],"completion_criteria":["fixture contains good"]}),
            )],
        ),
        response(
            "Plan: make one guarded replacement and validate exact content.",
            vec![],
        ),
        // WORK: baseline validation fails.
        response(
            "Running the baseline validation.",
            vec![call(
                "baseline",
                "validate",
                json!({"requirement":"fixture exact-content check","command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Reading before editing.",
            vec![call("work-read", "read_file", json!({"path":"app.txt"}))],
        ),
        // Repeated failure with unrelated successful inspection in between:
        // supervision must still accumulate.
        response(
            "Trying the replacement.",
            vec![call(
                "bad-patch",
                "patch",
                json!({"path":"app.txt","base_hash":bug_hash,"old":"bug","new":"wrong"}),
            )],
        ),
        response(
            "Revalidating.",
            vec![call(
                "fail-2",
                "validate",
                json!({"requirement":"fixture exact-content check","command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Re-grounding by reading current reality.",
            vec![call("reread", "read_file", json!({"path":"app.txt"}))],
        ),
        response(
            "One more check before changing strategy.",
            vec![call(
                "fail-3",
                "validate",
                json!({"requirement":"fixture exact-content check","command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        // Apply a different correction.
        response(
            "Applying a different correction.",
            vec![call(
                "fix",
                "patch",
                json!({"path":"app.txt","base_hash":wrong_hash,"old":"wrong","new":"good"}),
            )],
        ),
        // Validation reruns and passes; the kernel links the evidence.
        response(
            "Validating the corrected value.",
            vec![call(
                "pass",
                "validate",
                json!({"requirement":"fixture exact-content check","command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Recording scope and closing the open question.",
            vec![call(
                "state-2",
                "task_update",
                json!({"touched_files":["app.txt"],"resolve_questions":["does the marker need escaping?"],"supersede_constraints":["Constraint A: preserve plain-text format"]}),
            )],
        ),
        response(
            "Checking completion.",
            vec![call(
                "complete",
                "complete",
                json!({"implementation_done":true}),
            )],
        ),
        response("Verification complete.", vec![]),
        response("Decision B was to replace only the marker.", vec![]),
    ];
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::scripted(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let context = ContextConfig {
        max_request_tokens: Some(12_000),
        recent_tokens: 2_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let continuity = ContinuityEngine::new(store.clone(), context.clone());
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider,
        tools,
        continuity,
        retry_budget: 3,
    });
    agent.set_context_budget(context, latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS);
    let sink = Arc::new(|_| {});
    agent
        .run(
            "What is in this repository?",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    agent.set_mode(Mode::Plan).unwrap();
    agent
        .run(
            "Plan a minimal correction.",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    let memories = store.memories(session).unwrap();
    assert!(
        memories
            .iter()
            .any(|memory| memory.content.contains("Decision B")
                && memory.kind == latch_protocol::MemoryKind::Decision)
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content.contains("Approach C")
                && format!("{:?}", memory.validity) == "Rejected")
    );
    // Model-authored constraints are TaskConstraints, not UserConstraints.
    assert!(memories.iter().any(|memory| memory.kind
        == latch_protocol::MemoryKind::TaskConstraint
        && memory.content.contains("Constraint A")));
    agent.set_mode(Mode::Work).unwrap();
    agent
        .run(
            "Implement and verify it.",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("app.txt")).unwrap(),
        "good"
    );
    let events = store.events(session).unwrap();
    // Repeated failures with unrelated successful reads in between reached
    // the configured re-ground threshold.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, EventPayload::RegroundRequested { .. }))
    );
    // The kernel linked passing evidence to the real validation event.
    let validation_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::ValidationResult { .. }))
        .collect();
    assert_eq!(validation_events.len(), 4);
    let passing_evidence = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::EvidenceCreated { evidence }
                if evidence.status == EvidenceStatus::Passed =>
            {
                Some(evidence.clone())
            }
            _ => None,
        })
        .next_back()
        .expect("passing evidence exists");
    assert_eq!(passing_evidence.claim, "fixture exact-content check");
    assert!(events.iter().any(|e| e.id == passing_evidence.source_event
        && matches!(
            &e.payload,
            EventPayload::ValidationResult { passed: true, .. }
        )));
    // The model never supplied an internal identifier.
    for event in &events {
        if let EventPayload::ToolRequested { call } = &event.payload {
            let text = serde_json::to_string(&call.arguments).unwrap();
            assert!(
                !text.contains("source_call_id") && !text.contains("source_event"),
                "model supplied an internal id: {text}"
            );
        }
    }
    // Kernel-derived completion is VERIFIED, and the historical failed
    // attempts remain in the raw event log.
    assert_eq!(agent.state().completion, CompletionState::Verified);
    assert_eq!(
        agent.evidence().status_of("fixture exact-content check"),
        Some(EvidenceStatus::Passed)
    );
    assert!(
        agent.failure_lineages().is_empty(),
        "passing validation resolved the lineage"
    );
    // Superseded constraint left the canonical view; the resolved question
    // no longer lingers.
    assert!(
        !agent
            .state()
            .constraints
            .contains(&"Constraint A: preserve plain-text format".to_owned())
    );
    assert!(agent.state().open_questions.is_empty());
    // Turnover: push old material out of the recent window, recall exactly.
    for index in 0..80 {
        store
            .append(
                session,
                EventPayload::UserMessage {
                    text: format!("unrelated turnover {index}"),
                },
            )
            .unwrap();
    }
    let before_compact = store.events(session).unwrap();
    assert!(
        !before_compact
            .iter()
            .any(|e| matches!(e.payload, EventPayload::ManualCompact { .. }))
    );
    let recalled = agent.context(Some("Decision B marker")).unwrap();
    assert!(!recalled.episodes.is_empty());
    assert!(recalled.recalled.contains("Decision B") || recalled.canonical.contains("Decision B"));
    agent
        .run(
            "Continue the second approach: what was Decision B?",
            CancellationToken::new(),
            sink,
        )
        .await
        .unwrap();
    agent.compact().unwrap();
    assert!(store.events(session).unwrap().len() > before_compact.len());

    // The model transitioned WORK → PLAN at the end so resume lands in PLAN.
    agent.set_mode(Mode::Plan).unwrap();
    drop(agent);
    drop(store);

    // ---- Resume ----
    let resumed_store = EventStore::open(&db).unwrap();
    assert_eq!(
        resumed_store.latest_session(Some(&workspace)).unwrap(),
        Some(session)
    );
    let events = resumed_store.events(session).unwrap();
    // Resume replay: user-visible transcript items, no hidden internals, and
    // no duplicate durable events were appended while replaying.
    let items = replay_items(&events);
    assert!(items.iter().any(|item| matches!(
        item,
        DisplayItem::UserMessage { text } if text.contains("What is in this repository?")
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        DisplayItem::KernelNotice { text } if text.contains("re-ground")
    )));
    assert_eq!(resumed_mode(&events, None, Mode::Work), Mode::Plan);
    assert_eq!(
        resumed_mode(&events, Some(Mode::Work), Mode::Work),
        Mode::Work
    );
    assert_eq!(
        prompt_history(&events).first().map(String::as_str),
        Some("What is in this repository?")
    );
    assert!(!items.iter().any(|item| matches!(
        item,
        DisplayItem::AssistantMessage { .. } | DisplayItem::UserMessage { .. }
    ) && items.is_empty()));
    drop(resumed_store);

    // A resumed agent reconstructs state, evidence, failure supervision, and
    // durable change ownership — without re-executing anything.
    let store = EventStore::open(&db).unwrap();
    let resumed_provider = Arc::new(RecordingProvider::new(vec![response("resumed.", vec![])]));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Plan, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    assert_eq!(
        tools.restore_ownership().await.unwrap(),
        2,
        "guarded edits restored"
    );
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Plan,
        store: store.clone(),
        provider: resumed_provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    let state = store
        .events(session)
        .unwrap()
        .iter()
        .rev()
        .find_map(|e| match &e.payload {
            EventPayload::TaskStateUpdated { state } => Some(state.clone()),
            _ => None,
        })
        .unwrap();
    agent.restore_state(state);
    agent.restore_evidence(
        store
            .events(session)
            .unwrap()
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                _ => None,
            })
            .collect(),
    );
    agent.restore_failures().unwrap();
    assert_eq!(agent.state().completion, CompletionState::Verified);
    assert_eq!(
        agent.evidence().status_of("fixture exact-content check"),
        Some(EvidenceStatus::Passed)
    );
    assert!(agent.failure_lineages().is_empty());
    // The recalled decision survives resume.
    let recalled = agent.context(Some("Decision B")).unwrap();
    assert!(recalled.recalled.contains("Decision B") || recalled.canonical.contains("Decision B"));
    // Undo of the Latch-owned fix is still eligible after resume. Undo is a
    // mutation, so it runs under WORK policy like a user switching modes.
    assert_eq!(
        std::fs::read_to_string(workspace.join("app.txt")).unwrap(),
        "good"
    );
    let work_tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    work_tools.restore_ownership().await.unwrap();
    let undo = work_tools
        .execute(
            &ToolCall {
                id: "undo-resume".into(),
                name: "undo".into(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(!undo.is_error, "{}", undo.output);
    assert_eq!(
        std::fs::read_to_string(workspace.join("app.txt")).unwrap(),
        "wrong"
    );
    agent.shutdown_extensions().await.unwrap();
}

/// A stalled validation loop survives resume: the failure streak rebuilt from
/// durable events still crosses the re-ground threshold on the next failure.
#[tokio::test]
async fn resumed_failure_state_survives() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        response(
            "validating",
            vec![call(
                "v1",
                "validate",
                json!({"requirement":"tests pass","command":"false"}),
            )],
        ),
        response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"app.txt"}))],
        ),
        response(
            "validating",
            vec![call(
                "v2",
                "validate",
                json!({"requirement":"tests pass","command":"false"}),
            )],
        ),
        response("done", vec![]),
    ];
    let provider = Arc::new(FakeProvider::scripted(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    agent
        .run("stall", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    assert_eq!(agent.failure_lineages(), vec![("tests pass".to_owned(), 2)]);
    drop(agent);

    // Resume: supervision reconstructs the 2-failure streak from events; the
    // unrelated successful read_file in between did not reset it.
    let store = EventStore::open(&db).unwrap();
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Work,
        store: store.clone(),
        provider: Arc::new(FakeProvider::scripted(vec![])),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    agent.restore_failures().unwrap();
    assert_eq!(agent.failure_lineages(), vec![("tests pass".to_owned(), 2)]);
    // The next failure crosses the threshold.
    let decision = agent
        .run_validation("tests pass", "false", CancellationToken::new())
        .await
        .unwrap();
    assert!(decision.is_error);
    assert!(
        store
            .events(session)
            .unwrap()
            .iter()
            .any(|e| matches!(e.payload, EventPayload::RegroundRequested { .. }))
    );
}

/// A trivial one-file fix follows the short proportional path: read, patch,
/// validate, complete, report. It uses five model turns and four tool calls,
/// with no plan, repository search, subagent, or broader validation sweep. The
/// provider-visible system prompt carries the proportional-effort core and
/// none of the obsolete blanket-persistence wording.
#[tokio::test]
async fn trivial_one_file_fix_takes_the_short_path() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        response(
            "Reading the fixture.",
            vec![call("read", "read_file", json!({"path":"app.txt"}))],
        ),
        response(
            "Replacing the marker.",
            vec![call(
                "fix",
                "patch",
                json!({"path":"app.txt","base_hash":digest("bug"),"old":"bug","new":"good"}),
            )],
        ),
        response(
            "Checking the fixture.",
            vec![call(
                "check",
                "validate",
                json!({"requirement":"fixture exact-content check","command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Done.",
            vec![call(
                "complete",
                "complete",
                json!({"implementation_done":true}),
            )],
        ),
        response("Fixed app.txt and verified the exact content.", vec![]),
    ];
    let provider = Arc::new(RecordingProvider::new(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    agent
        .run(
            "Fix the bug in app.txt.",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(workspace.join("app.txt")).unwrap(),
        "good"
    );
    assert_eq!(agent.state().completion, CompletionState::Verified);

    // One model request per semantic step plus the final report.
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        5,
        "a trivial fix must not take extra model turns"
    );
    let tools_used: Vec<String> = store
        .events(session)
        .unwrap()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolRequested { call } => Some(call.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools_used,
        vec!["read_file", "patch", "validate", "complete"],
        "no plan, search, subagent, or broad sweep for a one-file fix"
    );

    // The provider-visible system prompt is the proportional core.
    let system = &requests[0].system;
    assert!(system.contains("Match effort to the task"));
    assert!(system.contains("Simple, local work"));
    for banned in [
        "Default to action",
        "the first green build",
        "Long tasks may take many tool calls",
        "never stop merely because the session is long",
    ] {
        assert!(
            !system.contains(banned),
            "obsolete persistence wording `{banned}` reached the provider"
        );
    }
}

/// Reasoning and tool history still round-trip across resume byte-for-byte.
#[tokio::test]
async fn reasoning_and_tool_history_round_trip_across_resume() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");

    let scripted = vec![
        ModelResponse {
            text: "I will inspect it.".into(),
            tool_calls: vec![call("read-1", "read_file", json!({"path":"app.txt"}))],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: Some("I should read the file before deciding".into()),
        },
        ModelResponse {
            text: "The file contains the bug marker.".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: Some("final reasoning".into()),
        },
    ];
    let provider = Arc::new(RecordingProvider::new(scripted));
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent
        .run(
            "What is in this repository?",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one tool turn plus one final turn");
    let second = &requests[1];
    let assistant = second
        .messages
        .iter()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("assistant tool call must be replayed structurally");
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("I should read the file before deciding")
    );
    assert_eq!(assistant.tool_calls[0].id, "read-1");
    assert_eq!(assistant.tool_calls[0].name, "read_file");
    let tool = second
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool result must be replayed as role tool");
    assert_eq!(tool.tool_call_id.as_deref(), Some("read-1"));

    let body =
        latch_kernel::provider::openai_request(second, "deepseek-test", ReasoningReplay::Replay);
    let messages = body["messages"].as_array().unwrap();
    let assistant_json = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .expect("assistant tool_calls present in serialized request");
    assert_eq!(
        assistant_json["reasoning_content"],
        "I should read the file before deciding"
    );
    assert_eq!(assistant_json["tool_calls"][0]["id"], "read-1");
    assert_eq!(
        assistant_json["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    let tool_json = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("role tool present in serialized request");
    assert_eq!(tool_json["tool_call_id"], "read-1");
    assert!(
        tool_json["content"].as_str().unwrap().contains("hash:"),
        "tool result carries the observed file content"
    );

    drop(agent);
    drop(store);

    // Resume from disk and verify the durable reasoning and tool linkage still
    // serialize into the next request.
    let store = EventStore::open(&db).unwrap();
    assert_eq!(
        store.latest_session(Some(&workspace)).unwrap(),
        Some(session)
    );
    let provider = Arc::new(RecordingProvider::new(vec![ModelResponse {
        text: "resumed".into(),
        tool_calls: vec![],
        stop_reason: "stop".into(),
        usage: None,
        reasoning_content: None,
    }]));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent
        .run(
            "Continue the inspection.",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let resumed = provider.requests();
    assert_eq!(resumed.len(), 1);
    let assistant = resumed[0]
        .messages
        .iter()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("assistant tool call must survive resume");
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("I should read the file before deciding")
    );
    assert_eq!(assistant.tool_calls[0].id, "read-1");
    let tool = resumed[0]
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool result must survive resume");
    assert_eq!(tool.tool_call_id.as_deref(), Some("read-1"));
    let body = latch_kernel::provider::openai_request(
        &resumed[0],
        "deepseek-test",
        ReasoningReplay::Replay,
    );
    let messages = body["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| {
        m["role"] == "assistant"
            && m["reasoning_content"] == "I should read the file before deciding"
            && m["tool_calls"][0]["id"] == "read-1"
    }));
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "read-1")
    );
}

/// A stale Latch-owned change cannot be undone after an external edit, while
/// an eligible one can.
#[tokio::test]
async fn external_edit_blocks_undo_after_resume() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"app.txt"}))],
        ),
        response(
            "editing",
            vec![call(
                "p1",
                "patch",
                json!({"path":"app.txt","base_hash":digest("bug"),"old":"bug","new":"good"}),
            )],
        ),
        response("done", vec![]),
    ];
    let provider = Arc::new(FakeProvider::scripted(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent
        .run("fix it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    drop(agent);

    // External edit after exit.
    std::fs::write(workspace.join("app.txt"), "externally edited").unwrap();
    let store = EventStore::open(&db).unwrap();
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    tools.restore_ownership().await.unwrap();
    let undo = agent_undo(&tools).await;
    assert!(undo.is_error, "undo must refuse after an external edit");
    assert_eq!(
        std::fs::read_to_string(workspace.join("app.txt")).unwrap(),
        "externally edited"
    );
}

async fn agent_undo(tools: &ToolExecutor) -> ToolResult {
    tools
        .execute(
            &ToolCall {
                id: "undo".into(),
                name: "undo".into(),
                arguments: json!({}),
            },
            CancellationToken::new(),
        )
        .await
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

/// Generic lifecycle invariant over completed tool transactions: every
/// model-issued ToolRequested id has exactly one terminal result id. No
/// zero-result calls, no two-result calls.
fn assert_tool_transaction_invariant(events: &[Event]) {
    let mut terminals: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for event in events {
        match &event.payload {
            EventPayload::ToolRequested { call } => {
                terminals.insert(call.id.as_str(), 0);
            }
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                *terminals.entry(result.call_id.as_str()).or_insert(0) += 1;
            }
            _ => {}
        }
    }
    for (id, count) in &terminals {
        assert_eq!(*count, 1, "call {id} must have exactly one terminal result");
    }
}

/// The exact live regression: one assistant turn with reasoning_content and
/// TWO tool calls, one denied by ASK policy and one succeeding. The durable
/// stream must give every ToolRequested exactly one terminal result, and the
/// next model request must replay the complete provider transaction — both
/// tool_calls, both role="tool" results, and reasoning_content — so the next
/// DeepSeek/OpenCode Go request is protocol-valid.
#[tokio::test]
async fn denied_tool_call_preserves_complete_provider_transaction() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        ModelResponse {
            text: "inspecting and mutating".into(),
            tool_calls: vec![
                call("denied-shell", "shell", json!({"command":"touch injected"})),
                call("read-ok", "read_file", json!({"path":"app.txt"})),
            ],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: Some("I should inspect before deciding".into()),
        },
        ModelResponse {
            text: "The read succeeded; the mutation was refused.".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: Some("final reasoning".into()),
        },
    ];
    let provider = Arc::new(RecordingProvider::new(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent
        .run(
            "inspect this repository",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    // 1-6: durable stream has exactly one terminal result per requested call.
    let events = store.events(session).unwrap();
    assert_tool_transaction_invariant(&events);
    let requested: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ToolRequested { call } => Some(call.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(requested, vec!["denied-shell", "read-ok"]);
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::ToolFailed { result } if result.call_id == "denied-shell"
    )));
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::PermissionDecision { tool, .. } if tool == "shell"
    )));
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::ToolCompleted { result } if result.call_id == "read-ok"
    )));
    // The denied call is one failed tool lifecycle item to the user.
    let visible = replay_items(&events);
    let denied_rows = visible
        .iter()
        .filter(|item| {
            item.call_id() == Some("denied-shell")
                && matches!(
                    item,
                    DisplayItem::ToolActivity {
                        status: latch_protocol::ToolRunStatus::Failed,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(
        denied_rows, 1,
        "denied tool shows as one failed lifecycle row"
    );

    // 7-10: the next model request replays the complete transaction.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one tool turn plus one final turn");
    let second = &requests[1];
    let assistant = second
        .messages
        .iter()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("assistant tool_calls preserved");
    assert_eq!(assistant.tool_calls.len(), 2);
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("I should inspect before deciding")
    );
    let tool_ids: Vec<Option<&str>> = second
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(
        tool_ids.contains(&Some("denied-shell")) && tool_ids.contains(&Some("read-ok")),
        "both role=tool results present: {tool_ids:?}"
    );
    let denied_tool = second
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("denied-shell"))
        .expect("denied result present");
    // In ASK the command runs inside the sandbox with a read-only workspace,
    // so the mutation is refused by the OS boundary rather than by bash-string
    // parsing. Either way it is a structured terminal tool result.
    assert!(
        denied_tool.content.contains("Read-only file system")
            || denied_tool.content.contains("cannot mutate"),
        "mutation comes back as a structured tool result: {}",
        denied_tool.content
    );

    // 11: serialize and assert protocol validity for the thinking wire profile.
    let body =
        latch_kernel::provider::openai_request(second, "deepseek-test", ReasoningReplay::Replay);
    let messages = body["messages"].as_array().unwrap();
    let assistant_json = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
        .expect("assistant tool_calls present in serialized request");
    assert_eq!(assistant_json["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(
        assistant_json["reasoning_content"],
        "I should inspect before deciding"
    );
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "denied-shell")
    );
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "read-ok")
    );

    // ---- Session persistence/resume: the transaction survives intact. ----
    drop(agent);
    drop(store);
    let store = EventStore::open(&db).unwrap();
    let resumed_provider = Arc::new(RecordingProvider::new(vec![ModelResponse {
        text: "resumed".into(),
        tool_calls: vec![],
        stop_reason: "stop".into(),
        usage: None,
        reasoning_content: None,
    }]));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: resumed_provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    let events = store.events(session).unwrap();
    assert_tool_transaction_invariant(&events);
    agent.restore_state(
        events
            .iter()
            .rev()
            .find_map(|e| match &e.payload {
                EventPayload::TaskStateUpdated { state } => Some(state.clone()),
                _ => None,
            })
            .unwrap_or_default(),
    );
    agent.restore_evidence(
        events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                _ => None,
            })
            .collect(),
    );
    agent.restore_failures().unwrap();
    agent
        .run(
            "Continue after the denial.",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let resumed = resumed_provider.requests();
    assert_eq!(resumed.len(), 1);
    let assistant = resumed[0]
        .messages
        .iter()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("assistant tool_calls survive resume");
    assert_eq!(assistant.tool_calls.len(), 2);
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("I should inspect before deciding")
    );
    let tool_ids: Vec<Option<&str>> = resumed[0]
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(
        tool_ids.contains(&Some("denied-shell")) && tool_ids.contains(&Some("read-ok")),
        "both tool results survive resume"
    );
    let body = latch_kernel::provider::openai_request(
        &resumed[0],
        "deepseek-test",
        ReasoningReplay::Replay,
    );
    let messages = body["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| {
        m["role"] == "assistant"
            && m["reasoning_content"] == "I should inspect before deciding"
            && m["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.len() == 2)
    }));
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "denied-shell")
    );
}

/// A single denied tool call still completes its lifecycle with one terminal
/// result and replays reasoning + the denial result to the next request.
#[tokio::test]
async fn single_denied_tool_call_gets_one_terminal_result() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        ModelResponse {
            text: "trying to mutate".into(),
            tool_calls: vec![call(
                "only-denied",
                "shell",
                json!({"command":"touch injected"}),
            )],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: Some("reasoning for the denied call".into()),
        },
        ModelResponse {
            text: "denied.".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ];
    let provider = Arc::new(RecordingProvider::new(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent
        .run("mutate", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    assert_tool_transaction_invariant(&events);
    let second = &provider.requests()[1];
    let assistant = second
        .messages
        .iter()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("assistant tool_calls preserved");
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("reasoning for the denied call")
    );
    assert!(
        second
            .messages
            .iter()
            .any(|m| { m.role == "tool" && m.tool_call_id.as_deref() == Some("only-denied") })
    );
}

/// Multiple denied calls in one turn each complete their lifecycle; the next
/// request replays every denial as a structured tool result.
#[tokio::test]
async fn multiple_denied_calls_in_one_turn_each_get_a_terminal_result() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        ModelResponse {
            text: "two mutations".into(),
            tool_calls: vec![
                call("deny-1", "shell", json!({"command":"touch one"})),
                call("deny-2", "shell", json!({"command":"touch two"})),
            ],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: Some("thinking about both".into()),
        },
        ModelResponse {
            text: "both refused.".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ];
    let provider = Arc::new(RecordingProvider::new(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent
        .run("two mutations", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    assert_tool_transaction_invariant(&events);
    let second = &provider.requests()[1];
    let assistant = second
        .messages
        .iter()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("assistant tool_calls preserved");
    assert_eq!(assistant.tool_calls.len(), 2);
    let tool_ids: Vec<Option<&str>> = second
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(
        tool_ids.contains(&Some("deny-1")) && tool_ids.contains(&Some("deny-2")),
        "both denial results replayed: {tool_ids:?}"
    );
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("thinking about both")
    );
}

/// The live sink and resume replay share one authoritative display path: one
/// normal prompt produces exactly one visible user item, two identical prompts
/// produce two, and the live formatter and replay formatter agree exactly.
#[tokio::test]
async fn live_and_replay_transcripts_converge() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("app.txt"), "bug").unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let scripted = vec![
        response(
            "inspecting",
            vec![call("read-1", "read_file", json!({"path":"app.txt"}))],
        ),
        response("inspected.", vec![]),
        response(
            "inspecting again",
            vec![call("read-2", "read_file", json!({"path":"app.txt"}))],
        ),
        response("inspected again.", vec![]),
    ];
    let provider = Arc::new(RecordingProvider::new(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    let live: Arc<Mutex<Vec<DisplayItem>>> = Arc::new(Mutex::new(Vec::new()));
    let live_sink = live.clone();
    // Replicates the CLI's live display path: durable events go through the
    // shared formatter, and terminal tool results arrive as ToolResult rows.
    let sink: AgentEventSink = Arc::new(move |event| match event {
        AgentOutput::Durable(e) => {
            for item in display_items(&e) {
                live_sink.lock().unwrap().push(item);
            }
        }
        AgentOutput::ToolResult(result) => {
            live_sink.lock().unwrap().push(DisplayItem::ToolActivity {
                call_id: result.call_id,
                verb: result.name,
                target: String::new(),
                detail: first_line(&result.output),
                status: if result.is_error {
                    latch_protocol::ToolRunStatus::Failed
                } else {
                    latch_protocol::ToolRunStatus::Passed
                },
            });
        }
        AgentOutput::Transient(_) => {}
    });
    // Two intentionally identical prompts: both must remain visible.
    agent
        .run("same prompt", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    agent
        .run("same prompt", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    let live_items = live.lock().unwrap().clone();
    let user_live: Vec<&str> = live_items
        .iter()
        .filter_map(|item| match item {
            DisplayItem::UserMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_live,
        vec!["same prompt", "same prompt"],
        "one visible user item per submitted prompt"
    );

    // Resume replay restores exactly one visible item per durable prompt, and
    // the user-prompt portion of live and replay converges through the shared
    // formatter.
    let replay = replay_items(&store.events(session).unwrap());
    let user_replay: Vec<&str> = replay
        .iter()
        .filter_map(|item| match item {
            DisplayItem::UserMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_replay, vec!["same prompt", "same prompt"]);
    assert_eq!(
        user_live, user_replay,
        "live and replay user history converge"
    );
}

/// A legitimate change spanning more than ten files, more than 500 lines, and
/// a dependency manifest must proceed without a scope warning, justification
/// request, or pause.
#[tokio::test]
async fn broad_work_proceeds_without_any_scope_review() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("sample");
    std::fs::create_dir(&workspace).unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();

    let mut calls = Vec::new();
    for index in 0..12 {
        calls.push(call(
            &format!("w{index}"),
            "write",
            json!({
                "path": format!("src/file{index}.rs"),
                "content": format!("pub fn f{index}() -> usize {{ {index} }}\n"),
            }),
        ));
    }
    calls.push(call(
        "manifest",
        "write",
        json!({"path":"Cargo.toml","content":"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"}),
    ));
    let big: String = (0..600).map(|line| format!("line {line}\n")).collect();
    calls.push(call(
        "big",
        "write",
        json!({"path":"src/big.rs","content": big}),
    ));
    let scripted = vec![
        response("Applying the broad change.", calls),
        response("Done.", vec![]),
    ];
    let provider = Arc::new(FakeProvider::scripted(scripted));
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    agent
        .run(
            "Apply the broad change.",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    assert!(workspace.join("src/file11.rs").exists());
    assert!(workspace.join("Cargo.toml").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/big.rs"))
            .unwrap()
            .lines()
            .count(),
        600
    );
    assert!(
        !store
            .events(session)
            .unwrap()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ScopeExpansionRequested { .. })),
        "broad but legitimate work must not pause for scope review"
    );
}
