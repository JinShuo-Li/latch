use super::permissions::parse_review;
use super::request::{common_prefix_bytes, context_messages, sanitize_tool_history};
use super::*;
use crate::{
    config::{ContextConfig, PermissionConfig},
    provider::FakeProvider,
    tools::PolicyEngine,
};
use latch_protocol::{ModelResponse, ToolCall};
use serde_json::json;
use tempfile::tempdir;
#[tokio::test]
async fn loop_executes_multiple_read_tools() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("a"), "hello").unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    let responses = vec![
        ModelResponse {
            text: "checking".into(),
            tool_calls: vec![
                ToolCall {
                    id: "1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"a"}),
                },
                ToolCall {
                    id: "2".into(),
                    name: "search".into(),
                    arguments: json!({"query":"hello"}),
                },
            ],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "done".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ];
    let p = Arc::new(FakeProvider::scripted(responses));
    let policy = PolicyEngine::new(Mode::Ask, d.path().into(), PermissionConfig::default());
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        policy,
    )
    .unwrap();
    let continuity = ContinuityEngine::new(store.clone(), ContextConfig::default());
    let mut a = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: p,
        tools,
        continuity,
        retry_budget: 2,
    });
    let out = a
        .run("inspect", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    assert_eq!(out, "checkingdone");
    let events = store.events(sid).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::ToolCompleted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn loop_executes_registered_extension_tool() {
    let d = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    let provider = Arc::new(FakeProvider::scripted(vec![
        ModelResponse {
            text: "calling extension".into(),
            tool_calls: vec![ToolCall {
                id: "ext-1".into(),
                name: "fixture.echo".into(),
                arguments: json!({"value":"through-agent"}),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "extension complete".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    let fixture = format!("{}/tests/fixtures/extension.py", env!("CARGO_MANIFEST_DIR"));
    agent
        .load_extension("fixture".into(), "python3", &[fixture])
        .await
        .unwrap();
    agent
        .run(
            "use the extension",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    assert!(store.events(sid).unwrap().iter().any(|event| matches!(&event.payload, EventPayload::ToolCompleted { result } if result.name == "fixture.echo" && result.output.contains("through-agent"))));
    agent.shutdown_extensions().await.unwrap();
}

#[tokio::test]
async fn validate_links_kernel_provenance_without_model_ids() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("x.txt"), "good").unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    let provider = Arc::new(FakeProvider::scripted(vec![
        ModelResponse {
            text: "validating".into(),
            tool_calls: vec![ToolCall {
                id: "model-call-1".into(),
                name: "validate".into(),
                arguments: json!({
                    "requirement": "content is good",
                    "command": "test \"$(cat x.txt)\" = good"
                }),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "validated".into(),
            tool_calls: vec![ToolCall {
                id: "model-call-2".into(),
                name: "complete".into(),
                arguments: json!({"implementation_done": true}),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "done".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    agent
        .run("verify it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(sid).unwrap();
    // Kernel recorded a ValidationResult and evidence with real provenance.
    let validation = events
        .iter()
        .find_map(|e| match &e.payload {
            EventPayload::ValidationResult {
                command, passed, ..
            } => Some((command.clone(), *passed)),
            _ => None,
        })
        .expect("validation result recorded");
    assert_eq!(validation, ("test \"$(cat x.txt)\" = good".into(), true));
    let evidence = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
            _ => None,
        })
        .next_back()
        .expect("evidence created");
    assert_eq!(evidence.status, EvidenceStatus::Passed);
    assert_eq!(evidence.claim, "content is good");
    // The evidence source points at a real durable event (the validation
    // result), not at a model-supplied id.
    assert!(events.iter().any(|e| e.id == evidence.source_event
        && matches!(&e.payload, EventPayload::ValidationResult { .. })));
    // The requirement was kernel-registered and completion derived.
    assert!(
        agent
            .state()
            .required_validations
            .iter()
            .any(|r| r == "content is good")
    );
    assert_eq!(agent.state().completion, CompletionState::Verified);
}

#[tokio::test]
async fn record_evidence_rejects_kernel_owned_statuses() {
    let d = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    let provider = Arc::new(FakeProvider::scripted(vec![
        ModelResponse {
            text: "claiming".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "record_evidence".into(),
                arguments: json!({"claim":"tests pass","status":"passed","detail":"self-asserted"}),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "done".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ]));
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    agent
        .run("try it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    // The self-passed evidence was refused: no Passed evidence exists.
    assert!(
            !store
                .events(sid)
                .unwrap()
                .iter()
                .any(|e| matches!(&e.payload, EventPayload::EvidenceCreated { evidence } if evidence.status == EvidenceStatus::Passed))
        );
}

#[tokio::test]
async fn fail_then_pass_validation_supersedes_completion() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("x.txt"), "bad").unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Work, d.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: Arc::new(FakeProvider::scripted(vec![])),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    // Baseline validation fails; the implementation claim is not yet made.
    let failed = agent
        .run_validation(
            "content good",
            "test \"$(cat x.txt)\" = good",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(failed.is_error);
    agent.state.set_implementation_done(true);
    agent
        .run_validation(
            "content good",
            "test \"$(cat x.txt)\" = good",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        agent.state().completion,
        CompletionState::ImplementedNotVerified
    );
    // Fix, then revalidate: the PASS supersedes the earlier FAIL.
    std::fs::write(d.path().join("x.txt"), "good").unwrap();
    let passed = agent
        .run_validation(
            "content good",
            "test \"$(cat x.txt)\" = good",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!passed.is_error);
    assert_eq!(agent.state().completion, CompletionState::Verified);
    // All raw attempts remain in history, and the failing ones stay Failed
    // while current evidence for the claim is Passed.
    let events = store.events(sid).unwrap();
    let validation_results = events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::ValidationResult { .. }))
        .count();
    assert_eq!(validation_results, 3);
    assert_eq!(
        agent.evidence.status_of("content good"),
        Some(EvidenceStatus::Passed)
    );
    let failed_entries = agent
        .evidence
        .entries()
        .iter()
        .filter(|e| e.status == EvidenceStatus::Failed)
        .count();
    assert_eq!(failed_entries, 2, "history keeps the failed attempts");
    // A passing validation resolved its failure lineage.
    assert!(agent.failure_lineages().is_empty());
}

#[test]
fn sanitizer_keeps_reasoning_on_corrupt_history_and_whole_transactions() {
    // A genuinely corrupt old session: assistant proposes two calls but
    // only one result exists. The sanitizer must still keep reasoning so a
    // thinking provider never loses required state, even though the
    // dangling tool call is stripped.
    let corrupt = vec![
        ModelMessage::text("user", "inspect"),
        ModelMessage {
            role: "assistant".into(),
            content: "thinking".into(),
            tool_calls: vec![
                ToolCall {
                    id: "a".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a"}),
                },
                ToolCall {
                    id: "b".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "b"}),
                },
            ],
            tool_call_id: None,
            reasoning_content: Some("reasoned".into()),
        },
        ModelMessage {
            role: "tool".into(),
            content: "b result".into(),
            tool_calls: vec![],
            tool_call_id: Some("b".into()),
            reasoning_content: None,
        },
    ];
    let sanitized = sanitize_tool_history(corrupt);
    let assistant = sanitized
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant kept");
    assert!(assistant.tool_calls.is_empty(), "dangling calls stripped");
    assert_eq!(
        assistant.reasoning_content.as_deref(),
        Some("reasoned"),
        "reasoning survives the defensive transform"
    );
    // With the lifecycle invariant, complete transactions (denied call
    // included) are kept whole: reasoning + both tool_calls + all results.
    let complete = vec![
        ModelMessage::text("user", "inspect"),
        ModelMessage {
            role: "assistant".into(),
            content: "thinking".into(),
            tool_calls: vec![
                ToolCall {
                    id: "a".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a"}),
                },
                ToolCall {
                    id: "b".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "b"}),
                },
            ],
            tool_call_id: None,
            reasoning_content: Some("reasoned".into()),
        },
        ModelMessage {
            role: "tool".into(),
            content: "a denied".into(),
            tool_calls: vec![],
            tool_call_id: Some("a".into()),
            reasoning_content: None,
        },
        ModelMessage {
            role: "tool".into(),
            content: "b result".into(),
            tool_calls: vec![],
            tool_call_id: Some("b".into()),
            reasoning_content: None,
        },
    ];
    let kept = sanitize_tool_history(complete);
    let assistant = kept
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant kept");
    assert_eq!(assistant.tool_calls.len(), 2);
    assert_eq!(assistant.reasoning_content.as_deref(), Some("reasoned"));
    assert_eq!(
        kept.iter().filter(|m| m.role == "tool").count(),
        2,
        "both results kept"
    );
}

#[test]
fn context_messages_anchor_mid_task_windows_instead_of_dropping_them() {
    fn event(sequence: u64, payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            sequence,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }
    let assistant = event(
        2,
        EventPayload::AssistantMessageCompleted {
            text: "checking".into(),
            tool_calls: vec![ToolCall {
                id: "a".into(),
                name: "read_file".into(),
                arguments: json!({"path":"a"}),
            }],
            reasoning_content: None,
        },
    );
    let tool = event(
        3,
        EventPayload::ToolCompleted {
            result: ToolResult {
                call_id: "a".into(),
                name: "read_file".into(),
                output: "hash: x\ncontents".into(),
                is_error: false,
                artifact_id: None,
            },
        },
    );
    let base = crate::continuity::MaterializedContext {
        system: "system".into(),
        canonical: String::new(),
        recalled: String::new(),
        recent: vec![assistant.clone(), tool.clone()],
        bridge: crate::continuity::ConversationBridge::default(),
        episodes: vec![],
        stats: latch_protocol::ContextStats::default(),
    };
    // The user prompt already scrolled out of the window: the transcript is
    // preserved behind a deterministic kernel continuation anchor.
    let messages = context_messages(&base);
    assert_eq!(messages.first().map(|m| m.role.as_str()), Some("user"));
    assert!(
        messages[0]
            .content
            .contains("scrolled out of the active recent window")
    );
    assert!(
        messages
            .iter()
            .any(|m| m.role == "assistant" && !m.tool_calls.is_empty())
    );
    assert!(
        messages
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("a"))
    );
    // A window that still holds the user prompt is replayed verbatim.
    let with_user = crate::continuity::MaterializedContext {
        recent: vec![
            event(
                1,
                EventPayload::UserMessage {
                    text: "do it".into(),
                },
            ),
            assistant,
            tool,
        ],
        ..base
    };
    let messages = context_messages(&with_user);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "do it");
}

#[tokio::test]
async fn context_stats_cover_the_complete_request_in_tokens() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("a"), "hello world").unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    let responses = vec![
        ModelResponse {
            text: "checking".into(),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: json!({"path":"a"}),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "done".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ];
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Ask, d.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Ask,
        store: store.clone(),
        provider: Arc::new(FakeProvider::scripted(responses)),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    agent.set_context_budget(ContextConfig::default(), 128_000);
    agent
        .run("inspect", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let stats = store
        .events(sid)
        .unwrap()
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::ContextMaterialized { stats } => Some(stats.clone()),
            _ => None,
        })
        .expect("context stats");
    assert!(stats.estimated);
    assert_eq!(stats.window_tokens, 128_000);
    assert!(
        stats.tools_tokens > 0,
        "tool schemas are part of the real request"
    );
    assert!(
        stats.instructions_tokens > 0,
        "compiled system prompt is accounted"
    );
    let sum = stats.instructions_tokens
        + stats.state_tokens
        + stats.recent_tokens
        + stats.recall_tokens
        + stats.tools_tokens
        + stats.extension_tokens;
    assert_eq!(stats.total_tokens, sum, "no double counting");
    assert_eq!(
        stats.headroom_tokens,
        stats.budget_tokens.saturating_sub(sum)
    );
}

#[tokio::test]
async fn approved_outside_write_executes_and_denied_write_does_not() {
    use crate::config::OutsidePolicy;
    let workspace_dir = tempdir().unwrap();
    let outside_dir = tempdir().unwrap();
    let outside_path = outside_dir.path().join("outside.txt");

    for approve in [true, false] {
        let store = EventStore::open_memory().unwrap();
        let sid = store.create_session(workspace_dir.path()).unwrap();
        let responses = vec![
            ModelResponse {
                text: "writing outside".into(),
                tool_calls: vec![ToolCall {
                    id: "write-1".into(),
                    name: "write".into(),
                    arguments: json!({"path": outside_path.to_string_lossy(), "content": "approved", "base_hash": null}),
                }],
                stop_reason: "tool_calls".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ];
        let tools = ToolExecutor::new(
            workspace_dir.path().into(),
            workspace_dir.path().join("art"),
            store.clone(),
            sid,
            PolicyEngine::new(
                Mode::Work,
                workspace_dir.path().into(),
                PermissionConfig {
                    outside_workspace: OutsidePolicy::Ask,
                    ..PermissionConfig::default()
                },
            ),
        )
        .unwrap();
        let mut agent = Agent::new(AgentRuntime {
            session_id: sid,
            workspace: workspace_dir.path().into(),
            mode: Mode::Work,
            store: store.clone(),
            provider: Arc::new(FakeProvider::scripted(responses)),
            tools,
            continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
            retry_budget: 2,
        });
        agent.enable_interactive_permissions();
        let broker = agent.permission_broker();
        let run = agent.run("write outside", CancellationToken::new(), Arc::new(|_| {}));
        let approver = async {
            for _ in 0..400 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                if let Some(request_id) = broker.pending_ids().await.first().copied() {
                    broker.resolve(request_id, approve).await;
                    return;
                }
            }
            panic!("agent never requested approval");
        };
        let (result, ()) = tokio::join!(run, approver);
        result.unwrap();
        let events = store.events(sid).unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.payload, EventPayload::PermissionRequested { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::PermissionResolved {
                approved: decision,
                source,
                ..
            } if *decision == approve && source == "user"
        )));
        if approve {
            assert_eq!(std::fs::read_to_string(&outside_path).unwrap(), "approved");
            std::fs::remove_file(&outside_path).unwrap();
        } else {
            assert!(
                !outside_path.exists(),
                "denied outside write must not execute"
            );
            assert!(events.iter().any(|event| matches!(
                &event.payload,
                EventPayload::ToolFailed { result } if result.output.contains("permission denied")
            )));
        }
    }
}

#[tokio::test]
async fn non_interactive_ask_is_denied_with_a_durable_record() {
    let workspace_dir = tempdir().unwrap();
    let outside_dir = tempdir().unwrap();
    let outside_path = outside_dir.path().join("outside.txt");
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(workspace_dir.path()).unwrap();
    let responses = vec![
        ModelResponse {
            text: "writing outside".into(),
            tool_calls: vec![ToolCall {
                id: "write-1".into(),
                name: "write".into(),
                arguments: json!({"path": outside_path.to_string_lossy(), "content": "x", "base_hash": null}),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "done".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ];
    let tools = ToolExecutor::new(
        workspace_dir.path().into(),
        workspace_dir.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(
            Mode::Work,
            workspace_dir.path().into(),
            PermissionConfig {
                outside_workspace: crate::config::OutsidePolicy::Ask,
                ..PermissionConfig::default()
            },
        ),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: workspace_dir.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: Arc::new(FakeProvider::scripted(responses)),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 2,
    });
    // Interactive approval is intentionally not enabled.
    agent
        .run("write outside", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    assert!(!outside_path.exists());
    let events = store.events(sid).unwrap();
    assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::PermissionResolved { approved: false, source, .. } if source == "non_interactive"
        )));
}

#[test]
fn resume_expires_unresolved_permission_requests() {
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(std::path::Path::new("/tmp")).unwrap();
    let request_id = Uuid::new_v4();
    store
        .append(
            sid,
            EventPayload::PermissionRequested {
                request_id,
                tool: "shell".into(),
                arguments: json!({"command":"rm -rf /"}),
                reason: "test".into(),
                capabilities: vec!["privileged_operation".into()],
            },
        )
        .unwrap();
    let expired = Agent::expire_pending_permissions(&store, sid).unwrap();
    assert_eq!(expired, 1);
    let events = store.events(sid).unwrap();
    assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::PermissionResolved { request_id: id, approved: false, source, .. } if *id == request_id && source == "resume_expired"
        )));
    // Expiring again is a no-op: the request is durably resolved.
    assert_eq!(Agent::expire_pending_permissions(&store, sid).unwrap(), 0);
}

#[tokio::test]
async fn configured_turn_breaker_stops_abnormal_loops() {
    let d = tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(d.path()).unwrap();
    // Three identical empty responses would never be scripted in practice;
    // this only proves the opt-in breaker fires when configured.
    let responses = (0..4)
        .map(|index| ModelResponse {
            text: format!("turn {index}"),
            tool_calls: vec![ToolCall {
                id: format!("call-{index}"),
                name: "search".into(),
                arguments: json!({"query": format!("q{index}")}),
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        })
        .collect();
    let tools = ToolExecutor::new(
        d.path().into(),
        d.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Ask, d.path().into(), PermissionConfig::default()),
    )
    .unwrap();
    let continuity = ContinuityEngine::new(store.clone(), ContextConfig::default());
    let mut agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: d.path().into(),
        mode: Mode::Ask,
        store,
        provider: Arc::new(FakeProvider::scripted(responses)),
        tools,
        continuity,
        retry_budget: 2,
    });
    agent.set_max_model_turns(Some(2));
    let error = agent
        .run("loop", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .expect_err("breaker must fire");
    assert!(error.to_string().contains("circuit breaker"));
}

fn policy_agent(
    dir: &tempfile::TempDir,
    config: PermissionConfig,
    responses: Vec<ModelResponse>,
) -> (EventStore, Uuid, Agent) {
    let workspace = dir.path();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(workspace).unwrap();
    let tools = ToolExecutor::new(
        workspace.into(),
        dir.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Work, workspace.into(), config),
    )
    .unwrap();
    let agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: workspace.into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: Arc::new(FakeProvider::scripted(responses)),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    (store, sid, agent)
}

fn tool_then_final(id: &str, name: &str, arguments: serde_json::Value) -> Vec<ModelResponse> {
    vec![
        ModelResponse {
            text: "acting".into(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            }],
            stop_reason: "tool_calls".into(),
            usage: None,
            reasoning_content: None,
        },
        ModelResponse {
            text: "done".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    ]
}

fn review_response(risk: &str, reason: &str) -> ModelResponse {
    ModelResponse {
        text: json!({"risk": risk, "reason": reason}).to_string(),
        tool_calls: vec![],
        stop_reason: "stop".into(),
        usage: None,
        reasoning_content: None,
    }
}

type PermissionRecord = (Vec<String>, Option<bool>, Option<String>, Option<String>);

fn permission_events(events: &[Event]) -> Vec<PermissionRecord> {
    let mut requests: Vec<PermissionRecord> = Vec::new();
    for event in events {
        match &event.payload {
            EventPayload::PermissionRequested { capabilities, .. } => {
                requests.push((capabilities.clone(), None, None, None));
            }
            EventPayload::PermissionResolved {
                approved,
                source,
                risk,
                ..
            } => {
                if let Some(last) = requests.last_mut() {
                    last.1 = Some(*approved);
                    last.2 = Some(source.clone());
                    last.3 = risk.clone();
                }
            }
            _ => {}
        }
    }
    requests
}

#[tokio::test]
async fn external_effects_are_asked_even_under_autonomous_auto_approve() {
    use crate::config::OutsidePolicy;
    let d = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let target = outside.path().join("note.txt");
    let responses = tool_then_final(
        "w1",
        "write",
        json!({"path": target.to_string_lossy(), "content": "hi", "base_hash": null}),
    );
    let (store, sid, mut agent) = policy_agent(
        &d,
        PermissionConfig {
            outside_workspace: OutsidePolicy::Ask,
            mode: PermissionMode::AutoApprove,
            ..PermissionConfig::default()
        },
        responses,
    );
    agent.set_safety(Safety::Autonomous).unwrap();
    agent
        .run("write outside", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(sid).unwrap();
    let requests = permission_events(&events);
    assert_eq!(
        requests.len(),
        1,
        "external effect must become an Ask first"
    );
    assert!(
        requests[0]
            .0
            .iter()
            .any(|cap| cap == "external_filesystem_write"),
        "{:?}",
        requests[0].0
    );
    assert_eq!(requests[0].1, Some(true));
    assert_eq!(requests[0].2.as_deref(), Some("auto"));
    assert!(target.exists());
}

#[tokio::test]
async fn auto_approve_never_overrides_hard_deny() {
    use crate::config::OutsidePolicy;
    let d = tempdir().unwrap();
    let responses = tool_then_final(
        "w1",
        "write",
        json!({"path": "/etc/sudoers", "content": "x", "base_hash": null}),
    );
    let (store, sid, mut agent) = policy_agent(
        &d,
        PermissionConfig {
            outside_workspace: OutsidePolicy::Ask,
            mode: PermissionMode::AutoApprove,
            ..PermissionConfig::default()
        },
        responses,
    );
    agent.set_safety(Safety::Autonomous).unwrap();
    agent
        .run(
            "write system file",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let events = store.events(sid).unwrap();
    assert!(
        permission_events(&events).is_empty(),
        "hard deny must not even ask"
    );
    let failed = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolFailed { result } => Some(result.output.clone()),
            _ => None,
        })
        .expect("hard deny result");
    assert!(failed.contains("denied by policy"), "{failed}");
}

#[tokio::test]
async fn ai_review_low_approves_and_records_provenance() {
    let d = tempdir().unwrap();
    let mut responses = tool_then_final("p1", "shell", json!({"command": "git push origin main"}));
    responses.insert(1, review_response("low", "routine branch push"));
    let (store, sid, mut agent) = policy_agent(
        &d,
        PermissionConfig {
            mode: PermissionMode::AiReview,
            ..PermissionConfig::default()
        },
        responses,
    );
    agent
        .run("push", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(sid).unwrap();
    let requests = permission_events(&events);
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].0.iter().any(|cap| cap == "remote_side_effect"),
        "{:?}",
        requests[0].0
    );
    assert_eq!(requests[0].1, Some(true));
    assert_eq!(requests[0].2.as_deref(), Some("ai"));
    assert_eq!(requests[0].3.as_deref(), Some("low"));
}

#[tokio::test]
async fn ai_review_rejects_medium_risk_with_its_reason() {
    let d = tempdir().unwrap();
    let mut responses = tool_then_final("p1", "shell", json!({"command": "git push origin main"}));
    responses.insert(1, review_response("medium", "pushes to a shared remote"));
    let (store, sid, mut agent) = policy_agent(
        &d,
        PermissionConfig {
            mode: PermissionMode::AiReview,
            ..PermissionConfig::default()
        },
        responses,
    );
    agent
        .run("push", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(sid).unwrap();
    let requests = permission_events(&events);
    assert_eq!(requests[0].1, Some(false));
    assert_eq!(requests[0].2.as_deref(), Some("ai"));
    assert_eq!(requests[0].3.as_deref(), Some("medium"));
    let failed = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolFailed { result } if result.call_id == "p1" => {
                Some(result.output.clone())
            }
            _ => None,
        })
        .expect("denied result");
    assert!(
        failed.contains("Permission denied: medium risk — pushes to a shared remote"),
        "{failed}"
    );
}

#[tokio::test]
async fn ai_review_unparseable_output_rejects_conservatively() {
    let d = tempdir().unwrap();
    let mut responses = tool_then_final("p1", "shell", json!({"command": "git push origin main"}));
    responses.insert(
        1,
        ModelResponse {
            text: "I think this is fine, go ahead".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        },
    );
    let (store, sid, mut agent) = policy_agent(
        &d,
        PermissionConfig {
            mode: PermissionMode::AiReview,
            ..PermissionConfig::default()
        },
        responses,
    );
    agent
        .run("push", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(sid).unwrap();
    let requests = permission_events(&events);
    assert_eq!(requests[0].1, Some(false));
    assert_eq!(requests[0].3.as_deref(), Some("critical"));
}

#[test]
fn reviewer_output_parsing_is_strict() {
    assert_eq!(
        parse_review("{\"risk\":\"low\",\"reason\":\"routine\"}"),
        ("low".into(), "routine".into())
    );
    assert_eq!(parse_review("no json here").0, "critical");
    assert_eq!(
        parse_review("{\"risk\":\"maybe\",\"reason\":\"x\"}").0,
        "critical"
    );
    assert_eq!(parse_review("{\"risk\":\"low\"}").0, "critical");
}

struct SteeringProvider {
    requests: std::sync::Mutex<Vec<ModelRequest>>,
    responses: std::sync::Mutex<std::collections::VecDeque<ModelResponse>>,
    /// Set from `Agent::steering_handle()` after construction; the provider
    /// simulates a user typing into the live queue.
    steering: std::sync::RwLock<SteeringQueue>,
    inject_on_request: usize,
    injections: Vec<String>,
}

#[async_trait::async_trait]
impl ModelProvider for SteeringProvider {
    fn name(&self) -> &str {
        "steering"
    }
    fn model(&self) -> &str {
        "steering-test"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
        sink: StreamSink,
    ) -> Result<ModelResponse> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        if index == self.inject_on_request {
            let steering = self.steering.read().unwrap().clone();
            for text in &self.injections {
                let _ = steering.push(text.clone());
            }
        }
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted steering response");
        for chunk in response.text.as_bytes().chunks(8) {
            sink(StreamEvent::TextDelta(
                String::from_utf8_lossy(chunk).into_owned(),
            ));
        }
        sink(StreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

fn tool_response(text: &str, id: &str, name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }],
        stop_reason: "tool_calls".into(),
        usage: None,
        reasoning_content: None,
    }
}

fn multi_tool_response(text: &str, calls: Vec<(&str, &str, serde_json::Value)>) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        tool_calls: calls
            .into_iter()
            .map(|(id, name, arguments)| ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            })
            .collect(),
        stop_reason: "tool_calls".into(),
        usage: None,
        reasoning_content: None,
    }
}

fn steering_agent(
    dir: &tempfile::TempDir,
    responses: Vec<ModelResponse>,
    injections: Vec<String>,
    inject_on_request: usize,
) -> (EventStore, Uuid, Agent, Arc<SteeringProvider>) {
    let workspace = dir.path();
    std::fs::write(dir.path().join("a"), "alpha").unwrap();
    std::fs::write(dir.path().join("b"), "beta").unwrap();
    let store = EventStore::open_memory().unwrap();
    let sid = store.create_session(workspace).unwrap();
    let tools = ToolExecutor::new(
        workspace.into(),
        dir.path().join("art"),
        store.clone(),
        sid,
        PolicyEngine::new(Mode::Work, workspace.into(), PermissionConfig::default()),
    )
    .unwrap();
    let provider = Arc::new(SteeringProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        responses: std::sync::Mutex::new(responses.into()),
        steering: std::sync::RwLock::new(SteeringQueue::new()),
        inject_on_request,
        injections,
    });
    let agent = Agent::new(AgentRuntime {
        session_id: sid,
        workspace: workspace.into(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity: ContinuityEngine::new(store.clone(), ContextConfig::default()),
        retry_budget: 3,
    });
    *provider.steering.write().unwrap() = agent.steering_handle();
    (store, sid, agent, provider)
}

fn user_turns(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn steering_is_injected_after_tool_results_at_the_next_boundary() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            tool_response("checking", "c1", "read_file", json!({"path":"a"})),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["also inspect b".into()],
        1,
    );
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "steering kept the loop going");
    let messages = &requests[1].messages;
    let assistant = messages
        .iter()
        .position(|message| message.role == "assistant" && !message.tool_calls.is_empty())
        .expect("assistant tool call");
    let tool_result = messages
        .iter()
        .position(|message| message.role == "tool")
        .expect("tool result");
    let steer = messages
        .iter()
        .rposition(|message| message.role == "user" && message.content.contains("also inspect b"))
        .expect("steering message injected");
    assert!(assistant < tool_result, "assistant precedes its result");
    assert!(
        tool_result < steer,
        "steering lands after the resolved tool transaction: {messages:#?}"
    );
    assert!(
        !messages[assistant + 1..tool_result]
            .iter()
            .any(|message| message.role == "user"),
        "no user message inside an unresolved transaction"
    );
    // Durable history keeps the turns distinct and ordered.
    let events = store.events(sid).unwrap();
    assert_eq!(user_turns(&events), vec!["start", "also inspect b"]);
}

#[tokio::test]
async fn multiple_steering_messages_preserve_order_and_stay_distinct() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            tool_response("checking", "c1", "read_file", json!({"path":"a"})),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["first steer".into(), "second steer".into()],
        1,
    );
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(sid).unwrap();
    assert_eq!(
        user_turns(&events),
        vec!["start", "first steer", "second steer"]
    );
    let ids: Vec<Uuid> = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
        .map(|event| event.id)
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(
        ids.iter().collect::<std::collections::HashSet<_>>().len() == 3,
        "each turn is a distinct durable message"
    );

    let requests = provider.requests.lock().unwrap().clone();
    let messages = &requests[1].messages;
    let steer_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == "user"
                && (message.content.contains("first steer")
                    || message.content.contains("second steer"))
        })
        .map(|(index, _)| index)
        .collect();
    assert!(!steer_positions.is_empty());
    let rendered = messages
        .iter()
        .filter(|message| message.role == "user")
        .map(|message| message.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.find("first steer").unwrap() < rendered.find("second steer").unwrap(),
        "{rendered}"
    );
}

#[tokio::test]
async fn steering_during_a_running_tool_waits_for_its_result() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, _provider) = steering_agent(
        &d,
        vec![
            tool_response(
                "running",
                "t1",
                "shell",
                json!({"command":"sleep 0.3 && echo done"}),
            ),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec![],
        0,
    );
    let steering = agent.steering_handle();
    let inject = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let _ = steering.push("stop after this tool".to_owned());
    });
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    inject.await.unwrap();

    let events = store.events(sid).unwrap();
    let tool_index = events
            .iter()
            .position(|event| matches!(&event.payload, EventPayload::ToolCompleted { result } if !result.is_error))
            .expect("tool completed normally");
    let steer_index = events
            .iter()
            .position(|event| {
                matches!(&event.payload, EventPayload::UserMessage { text } if text == "stop after this tool")
            })
            .expect("steering recorded");
    assert!(
        tool_index < steer_index,
        "the in-flight tool finished before the message was injected"
    );
}

#[tokio::test]
async fn steering_after_a_plain_answer_gets_another_turn() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            ModelResponse {
                text: "I am done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["not done yet".into()],
        1,
    );
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "a pending steer prevents early stop");
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.role == "user" && message.content.contains("not done yet")),
        "second request carries the steer"
    );
    let events = store.events(sid).unwrap();
    assert_eq!(user_turns(&events), vec!["start", "not done yet"]);
    // A steer accepted while the run was closing is consumed exactly once
    // and cannot remain in the queue after the run returns.
    assert!(
        agent.steering_handle().is_empty(),
        "an accepted steer is consumed before exit"
    );
    let recorded = events
            .iter()
            .filter(|event| {
                matches!(&event.payload, EventPayload::UserMessage { text } if text == "not done yet")
            })
            .count();
    assert_eq!(recorded, 1, "the accepted steer is recorded exactly once");
}

#[tokio::test]
async fn injected_constraints_override_stale_decisions_end_to_end() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            tool_response("checking", "c1", "read_file", json!({"path":"a"})),
            tool_response(
                "updating",
                "u1",
                "task_update",
                json!({
                    "supersede_decisions": ["use plan A"],
                    "add_decisions": ["use plan B"]
                }),
            ),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["do not use plan A; use plan B".into()],
        1,
    );
    agent.state.update(crate::state::StateUpdate {
        add_decisions: vec!["use plan A".into()],
        ..Default::default()
    });
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    // The model received the steer, issued the superseding task_update, and
    // the stale canonical decision is actually gone.
    assert_eq!(agent.state.state().decisions, vec!["use plan B".to_owned()]);
    let events = store.events(sid).unwrap();
    let latest_state = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::TaskStateUpdated { state } => Some(state.clone()),
            _ => None,
        })
        .expect("task state update");
    assert!(!latest_state.decisions.contains(&"use plan A".to_owned()));
    assert!(latest_state.decisions.contains(&"use plan B".to_owned()));

    let requests = provider.requests.lock().unwrap().clone();
    assert!(requests[1].system.contains("overrides earlier decisions"));
    assert_eq!(requests.len(), 3, "the steer earned a re-plan turn");
    // The next request's volatile kernel context carries the new decision
    // and no longer the superseded one; the stable prefix is untouched.
    // Durable memory and the bridge still quote the user's own wording.
    let kernel_context = requests[2].messages.last().expect("kernel context");
    let canonical_json = kernel_context
        .content
        .split("CANONICAL TASK STATE")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("canonical state json");
    let canonical: serde_json::Value = serde_json::from_str(canonical_json.trim()).unwrap();
    let decisions = canonical["decisions"].as_array().unwrap();
    assert!(
        decisions.iter().any(|decision| decision == "use plan B"),
        "{canonical}"
    );
    assert!(
        !decisions.iter().any(|decision| decision == "use plan A"),
        "{canonical}"
    );
    assert_eq!(requests[0].system, requests[2].system);
}

#[tokio::test]
async fn live_and_resumed_steering_state_remain_identical() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, _provider) = steering_agent(
        &d,
        vec![
            tool_response("checking", "c1", "read_file", json!({"path":"a"})),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["persist this steer".into()],
        1,
    );
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(sid).unwrap();
    assert_eq!(user_turns(&events), vec!["start", "persist this steer"]);
    assert_eq!(
        crate::session::prompt_history(&events),
        vec!["start", "persist this steer"]
    );
    // A resumed continuity materialization sees the same user turns in the
    // volatile recent window.
    let continuity = ContinuityEngine::new(store.clone(), ContextConfig::default());
    let budget = continuity.default_budget(128_000, 0);
    let ctx = continuity
        .materialize(
            sid,
            &Default::default(),
            None,
            &crate::state::EvidenceLedger::default(),
            &FailureManager::new(3),
            "system".into(),
            &budget,
        )
        .unwrap();
    let recent_users: Vec<String> = ctx
        .recent
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(recent_users, vec!["start", "persist this steer"]);
}

#[tokio::test]
async fn request_prefix_is_append_only_and_cacheable_within_an_epoch() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            tool_response("checking", "c1", "read_file", json!({"path":"a"})),
            ModelResponse {
                text: "done".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec![],
        0,
    );
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let stats: Vec<latch_protocol::ContextStats> = store
        .events(sid)
        .unwrap()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ContextMaterialized { stats } => Some(stats.clone()),
            _ => None,
        })
        .collect();
    assert!(stats.len() >= 2, "one request per turn");
    assert!(stats[1].request_tokens > 0);
    assert!(stats[1].common_prefix_tokens > 0);
    let requests = provider.requests.lock().unwrap().clone();
    let cacheability = stats[1].common_prefix_tokens as f64 / stats[1].request_tokens as f64;
    assert!(
        cacheability > 0.5,
        "append-only requests share a large prefix: {cacheability}"
    );

    // The compiled stable prefix is byte-identical across ordinary turns,
    // and it is the head of every request's system block.
    let compiled = PromptCompiler::compile(Mode::Work, d.path()).unwrap().text;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].system, requests[1].system,
        "no canonical change between the two turns"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.system.starts_with(&compiled))
    );
    // A normal continuation turn does not trigger retrieval: only the
    // first user turn supplies a query.
    let events = store.events(sid).unwrap();
    let recalls = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ContextMemoryRecalled { .. }))
        .count();
    assert_eq!(recalls, 1, "continuation turns must not recall");
}

#[test]
fn steering_acceptance_and_closing_are_atomic() {
    let queue = SteeringQueue::new();
    assert_eq!(queue.push("first"), SteeringSubmission::Accepted);
    // Closing with an accepted message keeps the run open and hands the
    // message back for consumption.
    assert_eq!(queue.close_and_drain(), vec!["first".to_owned()]);
    assert_eq!(queue.push("second"), SteeringSubmission::Accepted);
    assert_eq!(queue.close_and_drain(), vec!["second".to_owned()]);
    // Closing with nothing pending latches the queue closed.
    assert!(queue.close_and_drain().is_empty());
    assert_eq!(queue.push("late"), SteeringSubmission::Closed);
    assert!(queue.is_empty(), "a rejected steer is never enqueued");
    // A later run opens the queue and does not see the rejected message.
    queue.open();
    assert_eq!(queue.push("next"), SteeringSubmission::Accepted);
    assert_eq!(queue.drain(), vec!["next".to_owned()]);
    assert!(queue.is_empty());
}

#[test]
fn aborted_runs_drop_accepted_steers_instead_of_leaking_them() {
    let queue = SteeringQueue::new();
    assert_eq!(queue.push("in flight"), SteeringSubmission::Accepted);
    queue.close();
    assert!(queue.is_empty());
    queue.open();
    assert!(
        queue.is_empty(),
        "an accepted steer cannot survive an aborted run"
    );
}

#[test]
fn concurrent_push_and_close_have_one_deterministic_outcome() {
    use std::sync::Barrier;
    for _ in 0..200 {
        let queue = SteeringQueue::new();
        let pusher = queue.clone();
        let barrier = Arc::new(Barrier::new(2));
        let peer = barrier.clone();
        let handle = std::thread::spawn(move || {
            peer.wait();
            pusher.push("race")
        });
        barrier.wait();
        let drained = queue.close_and_drain();
        match handle.join().unwrap() {
            SteeringSubmission::Accepted => {
                // The run saw the submission and must consume it.
                assert_eq!(drained, vec!["race".to_owned()]);
            }
            SteeringSubmission::Closed => {
                // The close linearized first; nothing is left behind.
                assert!(drained.is_empty());
                assert!(queue.is_empty());
            }
        }
    }
}

#[test]
fn common_prefix_bytes_stays_on_utf8_char_boundaries() {
    assert_eq!(common_prefix_bytes("abc", "abd"), 2);
    assert_eq!(common_prefix_bytes("identical", "identical"), 9);
    // 你 and 何 share their first two bytes, so the raw byte prefix lands
    // inside a three-byte CJK character; the measurement must clamp to the
    // previous char boundary instead of producing an invalid slice offset.
    assert_eq!(common_prefix_bytes("你", "何"), 0);
    assert_eq!(common_prefix_bytes("a你", "a何"), 1);
    let left = "goal: 修复缓存策略";
    let right = "goal: 修复上下文预算";
    let shared = common_prefix_bytes(left, right);
    assert!(left.is_char_boundary(shared));
    assert_eq!(&left[..shared], "goal: 修复");
    // A shared string keeps the complete measured prefix.
    assert_eq!(common_prefix_bytes(left, left), left.len());
}

#[tokio::test]
async fn a_rejected_late_steer_never_leaks_into_the_next_run() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            ModelResponse {
                text: "finished".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
            ModelResponse {
                text: "second turn".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec![],
        0,
    );
    agent
        .run("first", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let steering = agent.steering_handle();
    assert_eq!(steering.push("too late"), SteeringSubmission::Closed);
    agent
        .run("second", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let requests = provider.requests.lock().unwrap().clone();
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
        "a rejected steer must never appear in a later run"
    );
    let events = store.events(sid).unwrap();
    assert_eq!(user_turns(&events), vec!["first", "second"]);
}

#[tokio::test]
async fn a_steer_retrieves_older_material_into_the_volatile_tail() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            tool_response("checking", "c1", "read_file", json!({"path":"a"})),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["what did we decide about the nebula protocol?".into()],
        1,
    );
    store
        .append(
            sid,
            EventPayload::AssistantMessageCompleted {
                text: "Earlier step: the nebula protocol uses ordered batching.".into(),
                tool_calls: vec![],
                reasoning_content: None,
            },
        )
        .unwrap();

    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(sid).unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ContextMemoryRecalled { query, .. } if query.contains("nebula")
        )),
        "the steer must drive the retrieval query"
    );
    let stats: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ContextMaterialized { stats } => Some(stats.clone()),
            _ => None,
        })
        .collect();
    assert!(stats.len() >= 2);
    assert!(stats[1].recall_tokens > 0, "{:?}", stats[1]);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].system, requests[1].system,
        "retrieval must not disturb the stable cache prefix"
    );
    let tail = requests[1].messages.last().expect("kernel context");
    assert!(tail.content.contains("nebula protocol"), "{tail:?}");
    assert!(
        !requests[1].system.contains("nebula protocol"),
        "retrieved material belongs in the volatile tail"
    );
}

#[tokio::test]
async fn a_steer_between_sequential_mutations_supersedes_the_stale_tail() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, _provider) = steering_agent(
        &d,
        vec![
            multi_tool_response(
                "working",
                vec![
                    ("m1", "shell", json!({"command":"sleep 0.5 && echo first"})),
                    ("m2", "shell", json!({"command":"printf stale > stale.txt"})),
                ],
            ),
            ModelResponse {
                text: "re-planned".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec![],
        0,
    );
    let steering = agent.steering_handle();
    let inject = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = steering.push("stop; switch to the other approach".to_owned());
    });
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    inject.await.unwrap();

    assert!(
        !d.path().join("stale.txt").exists(),
        "the stale mutation must not execute"
    );
    let events = store.events(sid).unwrap();
    // The in-flight call finished normally.
    let first: Vec<ToolResult> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCompleted { result } if result.call_id == "m1" => {
                Some(result.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(first.len(), 1, "the in-flight call has one terminal result");
    assert!(!first[0].is_error, "{:?}", first[0]);
    // Every not-yet-started mutation still gets exactly one terminal result.
    let second: Vec<ToolResult> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result }
                if result.call_id == "m2" =>
            {
                Some(result.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(second.len(), 1, "exactly one terminal result for m2");
    assert!(second[0].is_error);
    assert!(
        second[0]
            .output
            .contains("superseded by newer user steering"),
        "{}",
        second[0].output
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::FailureAttempt { .. })),
        "a kernel supersession is not a model failure"
    );
    assert_eq!(
        user_turns(&events),
        vec!["start", "stop; switch to the other approach"]
    );
    let superseded_index = events
            .iter()
            .position(|event| {
                matches!(&event.payload, EventPayload::ToolFailed { result } if result.call_id == "m2")
            })
            .unwrap();
    let steer_index = events
            .iter()
            .position(|event| {
                matches!(&event.payload, EventPayload::UserMessage { text } if text.contains("switch"))
            })
            .unwrap();
    assert!(
        superseded_index < steer_index,
        "the terminal result precedes the injected steer"
    );

    // Replayed continuity sees the complete transaction and every user
    // turn, exactly like the live run.
    let continuity = ContinuityEngine::new(store.clone(), ContextConfig::default());
    let budget = continuity.default_budget(128_000, 0);
    let ctx = continuity
        .materialize(
            sid,
            &Default::default(),
            None,
            &Default::default(),
            &FailureManager::new(3),
            "system".into(),
            &budget,
        )
        .unwrap();
    let recent_users: Vec<String> = ctx
        .recent
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        recent_users,
        vec!["start", "stop; switch to the other approach"]
    );
}

#[tokio::test]
async fn a_steer_does_not_supersede_started_read_only_calls() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, provider) = steering_agent(
        &d,
        vec![
            multi_tool_response(
                "reading",
                vec![
                    ("r1", "read_file", json!({"path":"a"})),
                    ("r2", "read_file", json!({"path":"b"})),
                ],
            ),
            ModelResponse {
                text: "adapted".into(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage: None,
                reasoning_content: None,
            },
        ],
        vec!["one more thought".into()],
        1,
    );
    agent
        .run("start", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();

    let events = store.events(sid).unwrap();
    for (call_id, content) in [("r1", "alpha"), ("r2", "beta")] {
        let terminal: Vec<ToolResult> = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result }
                    if result.call_id == call_id =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(terminal.len(), 1, "one terminal result for {call_id}");
        assert!(!terminal[0].is_error, "{terminal:?}");
        assert!(terminal[0].output.contains(content), "{terminal:?}");
    }
    assert!(
        !events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ToolFailed { result } if result.output.contains("superseded")
        )),
        "read-only calls are never superseded"
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "the steer still got its turn");
}

#[test]
fn canonical_task_state_is_rendered_once() {
    let d = tempdir().unwrap();
    let (_store, _sid, mut agent) = policy_agent(&d, PermissionConfig::default(), vec![]);
    agent.state.update(crate::state::StateUpdate {
        goal: Some("unique goal text".into()),
        add_decisions: vec!["unique decision text".into()],
        ..Default::default()
    });
    let context = agent.context(None).unwrap();
    // The compiled stable prefix must not duplicate canonical state; it is
    // rendered exactly once by the continuity engine's dynamic block.
    assert!(!context.system.contains("unique goal text"));
    assert!(!context.system.contains("Current canonical task state"));
    assert_eq!(
        context.canonical.matches("unique goal text").count(),
        1,
        "{}",
        context.canonical
    );
}

#[tokio::test]
async fn policy_changes_are_durable_for_resume() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent) = policy_agent(&d, PermissionConfig::default(), vec![]);
    assert_eq!(agent.safety(), Safety::Standard);
    assert_eq!(agent.permissions(), PermissionMode::Human);
    agent.set_safety(Safety::Strict).unwrap();
    agent.set_permissions(PermissionMode::AutoApprove).unwrap();
    let events = store.events(sid).unwrap();
    assert_eq!(
        crate::session::resumed_safety(&events, Safety::Standard),
        Safety::Strict
    );
    assert_eq!(
        crate::session::resumed_permissions(&events, PermissionMode::Human),
        PermissionMode::AutoApprove
    );
}

#[tokio::test]
async fn watermark_cursors_deliver_each_new_event_once() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, _provider) = steering_agent(
        &d,
        vec![ModelResponse {
            text: "unused".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        }],
        vec![],
        0,
    );
    store
        .append(sid, EventPayload::UserMessage { text: "one".into() })
        .unwrap();
    store
        .append(sid, EventPayload::UserMessage { text: "two".into() })
        .unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let collector = seen.clone();
    let sink: AgentEventSink = Arc::new(move |output| {
        if let AgentOutput::Durable(event) = output {
            collector.lock().unwrap().push(event.sequence);
        }
    });
    agent.forward_appended_events(&sink).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![1, 2]);

    store
        .append(
            sid,
            EventPayload::UserMessage {
                text: "three".into(),
            },
        )
        .unwrap();
    agent.forward_appended_events(&sink).unwrap();
    agent.forward_appended_events(&sink).unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![1, 2, 3],
        "each durable event is delivered exactly once"
    );

    // Progress supervision consumes the same sequence cursor and stays put
    // when no new events were appended.
    agent.observe_progress_events().unwrap();
    assert_eq!(
        agent.progress_watermark,
        store.last_sequence(sid).unwrap(),
        "the watermark tracks durable sequences"
    );
    agent.observe_progress_events().unwrap();
    assert_eq!(agent.progress_watermark, 3);
}

#[tokio::test]
async fn persistence_failures_abort_instead_of_claiming_success() {
    let d = tempdir().unwrap();
    let (store, sid, mut agent, _provider) = steering_agent(
        &d,
        vec![ModelResponse {
            text: "unused".into(),
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        }],
        vec![],
        0,
    );
    let sink: AgentEventSink = Arc::new(|_| {});
    store.fail_appends(true);

    // A run whose very first durable write fails must return an error, never a
    // silent best-effort success.
    let error = agent
        .run("start", CancellationToken::new(), sink.clone())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected event-store append failure"),
        "{error:#}"
    );

    // Kernel-owned task state is not applied live when its durable transition
    // cannot be committed.
    let call = ToolCall {
        id: "k1".into(),
        name: "task_update".into(),
        arguments: json!({"goal": "durable goal"}),
    };
    assert!(agent.execute_kernel_tool(&call, &sink).is_err());
    assert!(
        agent.state.state().goal.is_empty(),
        "canonical state must not advance on a failed durable write"
    );

    // Permission resolutions cannot be granted without durable provenance.
    assert!(
        agent
            .record_resolution(Uuid::new_v4(), true, "user", None, &sink)
            .is_err()
    );

    // Disabling the fault lets the same operations commit again.
    store.fail_appends(false);
    assert!(
        agent
            .record_resolution(Uuid::new_v4(), true, "user", None, &sink)
            .is_ok()
    );
    assert!(agent.execute_kernel_tool(&call, &sink).is_ok());
    let _ = sid;
}
