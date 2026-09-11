//! The V0.1.1 acceptance fixture: a buggy `calc.py` with an existing unittest
//! must finish as VERIFIED through ASK → PLAN → WORK, with kernel-linked
//! validation evidence and without the model ever seeing or supplying an
//! internal event/call UUID.

use latch_kernel::config::{ContextConfig, PermissionConfig};
use latch_kernel::{
    Agent, AgentRuntime, ContinuityEngine, EventStore, FakeProvider, PolicyEngine, ToolExecutor,
};
use latch_protocol::{
    CompletionState, EventPayload, EvidenceStatus, Mode, ModelResponse, ToolCall,
};
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

#[tokio::test]
async fn calc_fixture_reaches_verified_without_internal_ids() {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("calc-app");
    std::fs::create_dir(&workspace).unwrap();
    let buggy = "def add(a, b):\n    return a - b\n";
    let test_file = "\
import unittest
from calc import add

class TestCalc(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

if __name__ == '__main__':
    unittest.main()
";
    std::fs::write(workspace.join("calc.py"), buggy).unwrap();
    std::fs::write(workspace.join("test_calc.py"), test_file).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&workspace)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "user.email=t@l", "-c", "user.name=t", "add", "."])
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
            "-m",
            "init",
        ])
        .current_dir(&workspace)
        .status()
        .unwrap();

    let buggy_hash = digest(buggy);
    // -B keeps the fixture deterministic across a same-second fix+revalidate
    // (stale __pycache__ mtime granularity would otherwise serve the old
    // bytecode).
    let validate_cmd = "python3 -B -m unittest test_calc -v";
    let scripted = vec![
        // ASK inspects only.
        response(
            "Inspecting.",
            vec![call("ask-read", "read_file", json!({"path":"calc.py"}))],
        ),
        response("add() subtracts instead of adding.", vec![]),
        // PLAN records minimal scope and the validation requirement.
        response(
            "Scoping.",
            vec![call(
                "plan",
                "task_update",
                json!({"add_decisions":["Decision: fix the add implementation only"],"required_validations":["existing unittest passes"]}),
            )],
        ),
        response(
            "Plan ready: fix add, validate with the existing suite.",
            vec![],
        ),
        // WORK: baseline validation fails.
        response(
            "Baseline validation.",
            vec![call(
                "baseline",
                "validate",
                json!({"requirement":"existing unittest passes","command":validate_cmd,"timeout_seconds":120}),
            )],
        ),
        response(
            "Reading before editing.",
            vec![call("work-read", "read_file", json!({"path":"calc.py"}))],
        ),
        response(
            "Applying the fix.",
            vec![call(
                "fix",
                "patch",
                json!({"path":"calc.py","base_hash":buggy_hash,"old":"    return a - b","new":"    return a + b"}),
            )],
        ),
        response(
            "Revalidating.",
            vec![call(
                "revalidate",
                "validate",
                json!({"requirement":"existing unittest passes","command":validate_cmd,"timeout_seconds":120}),
            )],
        ),
        response(
            "Claiming completion.",
            vec![call(
                "done",
                "complete",
                json!({"implementation_done":true}),
            )],
        ),
        response("The suite passes; the fix is verified.", vec![]),
    ];
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(&workspace).unwrap();
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Ask, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let context = ContextConfig {
        max_request_tokens: Some(40_000),
        recent_tokens: 20_000,
        reserve_tokens: 0,
        output_reserve_tokens: 0,
    };
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: Arc::new(FakeProvider::scripted(scripted)),
        tools,
        continuity: ContinuityEngine::new(store.clone(), context.clone()),
        retry_budget: 3,
    });
    agent.set_context_budget(context, latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS);
    let sink = Arc::new(|_| {});

    agent
        .run(
            "Why does add(2, 3) return -1?",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    agent.set_mode(Mode::Plan).unwrap();
    agent
        .run(
            "Plan the minimal fix.",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    agent.set_mode(Mode::Work).unwrap();
    agent
        .run("Fix it and verify.", CancellationToken::new(), sink.clone())
        .await
        .unwrap();

    // The real unittest suite now passes.
    assert_eq!(
        std::fs::read_to_string(workspace.join("calc.py")).unwrap(),
        "def add(a, b):\n    return a + b\n"
    );
    // Kernel-derived completion is VERIFIED.
    assert_eq!(agent.state().completion, CompletionState::Verified);
    let events = store.events(session).unwrap();
    // Both validation attempts are durable; the current evidence is Passed.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::ValidationResult { .. }))
            .count(),
        2
    );
    assert_eq!(
        agent.evidence().status_of("existing unittest passes"),
        Some(EvidenceStatus::Passed)
    );
    // The passing evidence points at the real ValidationResult event.
    let passing = events
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
        .expect("passing evidence");
    assert!(events.iter().any(|e| e.id == passing.source_event
        && matches!(
            &e.payload,
            EventPayload::ValidationResult { passed: true, .. }
        )));
    // The model never supplied or saw an internal event/call id: every tool
    // argument is semantic (requirement names, commands, paths, hashes).
    for event in &events {
        if let EventPayload::ToolRequested { call } = &event.payload {
            let args = serde_json::to_string(&call.arguments).unwrap();
            assert!(
                !args.contains("source_call_id")
                    && !args.contains("source_event")
                    && !args.contains("event_id"),
                "model supplied an internal id: {args}"
            );
            for character in args.split('"') {
                let looks_like_uuid = character.len() == 36
                    && character.chars().filter(|c| *c == '-').count() == 4
                    && character.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
                assert!(!looks_like_uuid, "model handled a UUID argument: {args}");
            }
        }
    }
    // No automatic compaction ever happened.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.payload, EventPayload::ManualCompact { .. }))
    );
}
