use latch_kernel::config::{ContextConfig, PermissionConfig};
use latch_kernel::provider::ModelProvider;
use latch_kernel::{Agent, ContinuityEngine, EventStore, FakeProvider, PolicyEngine, ToolExecutor};
use latch_protocol::{EventPayload, Mode, ModelResponse, ToolCall};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn response(text: &str, calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        tool_calls: calls,
        stop_reason: "stop".into(),
        usage: None,
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
                json!({"claim":"fixture exact-content check","status":"passed","detail":"shell exited successfully"}),
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
    let mut agent = Agent::new(
        session,
        workspace.clone(),
        Mode::Ask,
        store.clone(),
        provider,
        tools,
        continuity,
        3,
    );
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
