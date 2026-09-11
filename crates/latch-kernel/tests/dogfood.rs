use latch_kernel::config::{ContextConfig, PermissionConfig};
use latch_kernel::provider::{ModelProvider, StreamSink};
use latch_kernel::{
    Agent, AgentRuntime, ContinuityEngine, EventStore, FakeProvider, PolicyEngine, ToolExecutor,
};
use latch_protocol::{EventPayload, Mode, ModelRequest, ModelResponse, StreamEvent, ToolCall};
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
    let db = dir.path().join("state.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let bug_hash = digest("bug");
    let wrong_hash = digest("wrong");
    let scripted = vec![
        response(
            "I will inspect it.",
            vec![call("ask-read", "read_file", json!({"path":"app.txt"}))],
        ),
        response("The fixture currently contains the bug marker.", vec![]),
        response(
            "I will record the selected approach.",
            vec![call(
                "plan-state",
                "task_update",
                json!({"add_constraints":["Constraint A: preserve plain-text format"],"add_decisions":["Decision B: replace only the marker"],"add_hypotheses":["Approach C: rewrite the fixture"],"reject_hypotheses":["Approach C: rewrite the fixture"]}),
            )],
        ),
        response(
            "Plan: make one guarded replacement and validate exact content.",
            vec![],
        ),
        response(
            "Reading before editing.",
            vec![call("work-read", "read_file", json!({"path":"app.txt"}))],
        ),
        response(
            "Trying the replacement.",
            vec![call(
                "bad-patch",
                "patch",
                json!({"path":"app.txt","base_hash":bug_hash,"old":"bug","new":"wrong"}),
            )],
        ),
        response(
            "Validating.",
            vec![call(
                "fail-1",
                "shell",
                json!({"command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Retrying validation.",
            vec![call(
                "fail-2",
                "shell",
                json!({"command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "One more check.",
            vec![call(
                "fail-3",
                "shell",
                json!({"command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Re-grounding by reading current reality.",
            vec![call("reread", "read_file", json!({"path":"app.txt"}))],
        ),
        response(
            "Applying a different correction.",
            vec![call(
                "fix",
                "patch",
                json!({"path":"app.txt","base_hash":wrong_hash,"old":"wrong","new":"good"}),
            )],
        ),
        response(
            "Validating the corrected value.",
            vec![call(
                "pass",
                "shell",
                json!({"command":"test \"$(cat app.txt)\" = good"}),
            )],
        ),
        response(
            "Recording validation state.",
            vec![call(
                "validation-state",
                "task_update",
                json!({"required_validations":["fixture exact-content check"],"validation_status":{"fixture exact-content check":true},"touched_files":["app.txt"],"completion_criteria":["fixture contains good"]}),
            )],
        ),
        response(
            "Recording evidence.",
            vec![call(
                "evidence",
                "record_evidence",
                json!({"claim":"fixture exact-content check","status":"passed","detail":"shell exited successfully","source_call_id":"pass"}),
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
    let continuity = ContinuityEngine::new(
        store.clone(),
        ContextConfig {
            active_bytes: 2_000,
            recent_bytes: 700,
            reserve_bytes: 300,
        },
    );
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
    let sink = Arc::new(|_| {});
    agent
        .run(
            "What is in this repository?",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    agent.set_mode(Mode::Plan);
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
            .any(|memory| memory.content.contains("Decision B"))
    );
    assert!(
        memories
            .iter()
            .any(|memory| memory.content.contains("Approach C")
                && format!("{:?}", memory.validity) == "Rejected")
    );
    agent.set_mode(Mode::Work);
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
    assert!(
        store
            .events(session)
            .unwrap()
            .iter()
            .any(|e| matches!(e.payload, EventPayload::RegroundRequested { .. }))
    );
    assert_eq!(format!("{:?}", agent.state().completion), "Verified");
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
    drop(agent);
    drop(store);
    let resumed = EventStore::open(&db).unwrap();
    assert_eq!(
        resumed.latest_session(Some(&workspace)).unwrap(),
        Some(session)
    );
    assert!(
        resumed
            .events(session)
            .unwrap()
            .iter()
            .any(|e| matches!(e.payload, EventPayload::ManualCompact { .. }))
    );
}

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

    let body = latch_kernel::provider::openai_request(second, "deepseek-test");
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
    let body = latch_kernel::provider::openai_request(&resumed[0], "deepseek-test");
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
