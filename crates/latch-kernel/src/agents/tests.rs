use super::*;
use crate::{
    Agent, AgentRuntime, ContinuityEngine, EventStore, ModelProvider, PolicyEngine, ToolExecutor,
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use latch_protocol::{
    AgentStatus, EventPayload, Mode, ModelRequest, ModelResponse, StreamEvent, ToolCall,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct TestProvider {
    calls: Mutex<Vec<ModelRequest>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    delay_ms: u64,
    first_calls: Vec<ToolCall>,
}

impl TestProvider {
    fn plain(delay_ms: u64) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            delay_ms,
            first_calls: Vec::new(),
        }
    }

    fn with_first_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            first_calls: calls,
            ..Self::plain(0)
        }
    }
}

#[async_trait]
impl ModelProvider for TestProvider {
    fn name(&self) -> &str {
        "agent-test"
    }

    fn model(&self) -> &str {
        "agent-test"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: crate::provider::StreamSink,
    ) -> Result<ModelResponse> {
        let first = !request
            .messages
            .iter()
            .any(|message| message.role == "assistant");
        self.calls.lock().unwrap().push(request);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let slept = tokio::time::sleep(Duration::from_millis(self.delay_ms));
        tokio::pin!(slept);
        tokio::select! {
            () = &mut slept => {}
            () = cancel.cancelled() => {
                self.active.fetch_sub(1, Ordering::SeqCst);
                bail!("cancelled")
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        let response = ModelResponse {
            text: if first && !self.first_calls.is_empty() {
                String::new()
            } else {
                "child semantic result".into()
            },
            tool_calls: if first {
                self.first_calls.clone()
            } else {
                Vec::new()
            },
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        };
        sink(StreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

fn test_agent(
    provider: Arc<dyn ModelProvider>,
    mode: Mode,
) -> (tempfile::TempDir, EventStore, Agent) {
    let workspace = tempfile::tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let session_id = store.create_session(workspace.path()).unwrap();
    let agent = root_agent(
        provider,
        mode,
        store.clone(),
        workspace.path().to_path_buf(),
        session_id,
    );
    (workspace, store, agent)
}

/// Reconstructs the root agent for an existing durable session, exactly as
/// `--resume` does after the previous process ended.
fn root_agent(
    provider: Arc<dyn ModelProvider>,
    mode: Mode,
    store: EventStore,
    workspace: std::path::PathBuf,
    session_id: uuid::Uuid,
) -> Agent {
    let policy = PolicyEngine::new(mode, workspace.clone(), Default::default());
    let tools = ToolExecutor::new(
        workspace.clone(),
        workspace.join("artifacts").join(session_id.to_string()),
        store.clone(),
        session_id,
        policy,
    )
    .unwrap();
    let continuity = ContinuityEngine::new(store.clone(), Default::default());
    Agent::new(AgentRuntime {
        session_id,
        workspace,
        mode,
        store,
        provider,
        tools,
        continuity,
        retry_budget: 1,
    })
}

async fn wait_for_status(supervisor: &AgentSupervisor, id: uuid::Uuid, wanted: AgentStatus) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if supervisor
                .list_agents()
                .iter()
                .any(|agent| agent.agent_id == id && agent.status == wanted)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("agent status transition");
}

#[tokio::test]
async fn spawn_runs_asynchronously_and_produces_compact_report() {
    let (_workspace, store, agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "inspect".into(),
            "inspect only the delegated concern".into(),
            Some("general".into()),
            DelegationContext::default(),
        )
        .await
        .unwrap();
    assert_eq!(child.status, AgentStatus::Starting);
    let waited = supervisor
        .wait_agents(&[child.agent_id], Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(waited.reports[0].summary, "child semantic result");
    let child_events = store.events(child.agent_id).unwrap();
    assert!(matches!(
        child_events.first().unwrap().payload,
        EventPayload::AgentSpawned { .. }
    ));
    assert!(
        child_events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::AgentReportCreated { .. }))
    );
}

#[tokio::test]
async fn multiple_children_run_concurrently_with_isolated_contexts() {
    let provider = Arc::new(TestProvider::plain(80));
    let (_workspace, store, agent) = test_agent(provider.clone(), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let one = supervisor
        .spawn_agent(
            "one".into(),
            "first unique assignment".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    let two = supervisor
        .spawn_agent(
            "two".into(),
            "second unique assignment".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, one.agent_id, AgentStatus::Completed).await;
    wait_for_status(&supervisor, two.agent_id, AgentStatus::Completed).await;
    assert!(provider.max_active.load(Ordering::SeqCst) >= 2);
    let first = store.events(one.agent_id).unwrap();
    let second = store.events(two.agent_id).unwrap();
    assert!(first.iter().any(|event| matches!(&event.payload, EventPayload::UserMessage { text } if text.contains("first unique"))));
    assert!(!first.iter().any(|event| matches!(&event.payload, EventPayload::UserMessage { text } if text.contains("second unique"))));
    assert!(second.iter().any(|event| matches!(&event.payload, EventPayload::UserMessage { text } if text.contains("second unique"))));
}

#[tokio::test]
async fn child_evidence_and_depth_are_isolated_from_root() {
    let provider = Arc::new(TestProvider::with_first_calls(vec![
        ToolCall {
            id: "evidence".into(),
            name: "record_evidence".into(),
            arguments: serde_json::json!({"claim":"child-only check","status":"unavailable","detail":"not run"}),
        },
        ToolCall {
            id: "nested".into(),
            name: "spawn_agent".into(),
            arguments: serde_json::json!({"task_name":"nested","message":"forbidden"}),
        },
    ]));
    let (_workspace, store, agent) = test_agent(provider, Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "isolation".into(),
            "record isolated evidence".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    assert!(agent.evidence().entries().is_empty());
    let events = store.events(child.agent_id).unwrap();
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::EvidenceCreated { evidence } if evidence.claim == "child-only check")));
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ToolFailed { result } if result.call_id == "nested" && result.output.contains("maximum spawn depth"))));
}

#[tokio::test]
async fn idle_information_is_delivered_with_follow_up() {
    let (_workspace, store, agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "mail".into(),
            "initial".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    supervisor
        .send_message(child.agent_id, "use this fact".into())
        .await
        .unwrap();
    assert_eq!(
        super::supervisor::undelivered_messages(&store, child.agent_id)
            .unwrap()
            .len(),
        1
    );
    supervisor
        .continue_agent(child.agent_id, "do another pass".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let messages = store
                .events(child.agent_id)
                .unwrap()
                .into_iter()
                .filter(|event| matches!(event.payload, EventPayload::AgentMessageReceived { .. }))
                .count();
            if messages == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let events = store.events(child.agent_id).unwrap();
    let received = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::AgentMessageReceived { message } => Some(message.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(received, vec!["use this fact", "do another pass"]);
    assert!(
        super::supervisor::undelivered_messages(&store, child.agent_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn interrupt_keeps_child_reusable_while_close_is_terminal() {
    let (_workspace, _store, agent) = test_agent(Arc::new(TestProvider::plain(60_000)), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "slow".into(),
            "wait".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Running).await;
    supervisor.interrupt_agent(child.agent_id).await.unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Interrupted).await;
    supervisor
        .continue_agent(child.agent_id, "try again".into())
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Running).await;
    supervisor.close_agent(child.agent_id).await.unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Closed).await;
    assert!(
        supervisor
            .continue_agent(child.agent_id, "cannot".into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn resume_reconstructs_graph_deterministically_and_children_do_not_pollute_latest() {
    let (_workspace, store, agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    let root_id = agent.session_id;
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "resume".into(),
            "finish".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    assert_eq!(store.latest_session(None).unwrap(), Some(root_id));
    let replayed = super::graph::AgentGraph::replay(&store.agent_events(root_id).unwrap());
    assert_eq!(replayed.snapshots()[0].identity.agent_id, child.agent_id);
    assert_eq!(replayed.snapshots()[0].status, AgentStatus::Completed);
}

#[tokio::test]
async fn child_policy_never_exceeds_live_parent_ceiling() {
    let mut test_provider = TestProvider::with_first_calls(vec![ToolCall {
        id: "write".into(),
        name: "patch".into(),
        arguments: serde_json::json!({"path":"src.rs","base_hash":"missing","patch":"x"}),
    }]);
    test_provider.delay_ms = 50;
    let provider = Arc::new(test_provider);
    let (_workspace, store, mut agent) = test_agent(provider, Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "policy".into(),
            "attempt a write".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Running).await;
    agent.set_mode(Mode::Ask).unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    assert!(
        store
            .events(child.agent_id)
            .unwrap()
            .iter()
            .any(|event| matches!(
                &event.payload,
                EventPayload::ToolFailed { result }
                    if result.call_id == "write" && result.output.contains("ASK mode cannot mutate")
            ))
    );
}

#[tokio::test]
async fn provider_tool_definitions_do_not_change_with_agent_lifecycle() {
    let (_workspace, _store, agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    let before = agent.tool_definitions();
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "schema".into(),
            "finish".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    supervisor.close_agent(child.agent_id).await.unwrap();
    assert_eq!(agent.tool_definitions(), before);
}

#[tokio::test]
async fn close_all_cancels_every_worker_without_orphans() {
    let provider = Arc::new(TestProvider::plain(60_000));
    let (_workspace, _store, agent) = test_agent(provider.clone(), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let one = supervisor
        .spawn_agent(
            "cleanup-one".into(),
            "wait".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    let two = supervisor
        .spawn_agent(
            "cleanup-two".into(),
            "wait".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, one.agent_id, AgentStatus::Running).await;
    wait_for_status(&supervisor, two.agent_id, AgentStatus::Running).await;
    supervisor.close_all().await.unwrap();
    assert!(
        supervisor
            .list_agents()
            .iter()
            .all(|child| child.status == AgentStatus::Closed)
    );
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn child_notification_enters_parent_only_after_terminal_tool_result() {
    let provider = Arc::new(TestProvider::plain(0));
    let (_workspace, store, mut agent) = test_agent(provider.clone(), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "notify".into(),
            "finish".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    let old_call = ToolCall {
        id: "old-call".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({"path":"old"}),
    };
    store
        .append(
            agent.session_id,
            EventPayload::AssistantMessageCompleted {
                text: String::new(),
                tool_calls: vec![old_call.clone()],
                reasoning_content: None,
            },
        )
        .unwrap();
    store
        .append(
            agent.session_id,
            EventPayload::ToolCompleted {
                result: latch_protocol::ToolResult {
                    call_id: old_call.id,
                    name: old_call.name,
                    output: "old result".into(),
                    is_error: false,
                    artifact_id: None,
                },
            },
        )
        .unwrap();
    agent
        .run("root resumes", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let requests = provider.calls.lock().unwrap();
    let root_request = requests
        .iter()
        .find(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content.contains("root resumes"))
        })
        .unwrap();
    let tool_index = root_request
        .messages
        .iter()
        .position(|message| message.tool_call_id.as_deref() == Some("old-call"))
        .unwrap();
    let notification_index = root_request
        .messages
        .iter()
        .position(|message| message.content.contains("Kernel child-agent report"))
        .unwrap();
    assert!(tool_index < notification_index);
}

#[tokio::test]
async fn wait_agents_result_carries_statuses_while_reports_arrive_once() {
    // The root model drives the control tools itself: spawn and wait in one
    // batch, then answer. The wait result must carry statuses only — report
    // bodies travel exactly once, on the durable kernel notification.
    let provider = Arc::new(TestProvider::with_first_calls(vec![
        ToolCall {
            id: "spawn".into(),
            name: "spawn_agent".into(),
            arguments: serde_json::json!({"task_name":"dup","message":"inspect"}),
        },
        ToolCall {
            id: "wait".into(),
            name: "wait_agents".into(),
            arguments: serde_json::json!({"timeout_ms": 2_000}),
        },
    ]));
    let (_workspace, store, mut agent) = test_agent(provider.clone(), Mode::Work);
    agent
        .run("root task", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let root_events = store.events(agent.session_id).unwrap();
    let wait_output = root_events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCompleted { result } if result.call_id == "wait" => {
                Some(result.output.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(wait_output.contains("reports_pending"), "{wait_output}");
    assert!(
        !wait_output.contains("child semantic result"),
        "report body leaked into the wait result: {wait_output}"
    );
    let delivered = root_events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::AgentNotificationDelivered { .. }
            )
        })
        .count();
    assert_eq!(delivered, 1, "the report must be delivered exactly once");
    let root_requests = provider.calls.lock().unwrap();
    let mentions = root_requests
        .iter()
        .filter(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content.contains("Kernel child-agent report"))
        })
        .count();
    assert_eq!(mentions, 1, "the report enters parent context exactly once");
    let notified_request = root_requests
        .iter()
        .find(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content.contains("Kernel child-agent report"))
        })
        .unwrap();
    let wait_result_at = notified_request
        .messages
        .iter()
        .position(|message| message.tool_call_id.as_deref() == Some("wait"))
        .unwrap();
    let notification_at = notified_request
        .messages
        .iter()
        .position(|message| message.content.contains("Kernel child-agent report"))
        .unwrap();
    assert!(
        wait_result_at < notification_at,
        "the single report copy must follow the wait tool result"
    );
}

#[tokio::test]
async fn follow_up_reaches_a_running_child_within_its_current_turn() {
    let provider = Arc::new(TestProvider::plain(300));
    let (_workspace, store, agent) = test_agent(provider.clone(), Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "live".into(),
            "initial assignment".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Running).await;
    // Delivered while the child's first request is still in flight: no second
    // turn is started; the message lands at the child's safe boundary.
    supervisor
        .continue_agent(child.agent_id, "mid-turn addition".into())
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    let events = store.events(child.agent_id).unwrap();
    let received = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::AgentMessageReceived { message } => Some(message.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(received, vec!["mid-turn addition"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| { matches!(event.payload, EventPayload::AgentReportCreated { .. }) })
            .count(),
        1,
        "the follow-up must not start a second turn"
    );
    assert!(provider.calls.lock().unwrap().iter().any(|request| {
        request
            .messages
            .iter()
            .any(|message| message.content.contains("mid-turn addition"))
    }));
}

#[tokio::test]
async fn child_permission_asks_never_reach_a_human() {
    // Children share the parent's live policy ceiling but run without the
    // interactive broker: an Ask resolves as an explicit durable denial
    // instead of prompting anyone.
    let provider = Arc::new(TestProvider::with_first_calls(vec![ToolCall {
        id: "outside".into(),
        name: "patch".into(),
        arguments: serde_json::json!({"path":"../outside-workspace.rs","base_hash":"","patch":"x"}),
    }]));
    let (_workspace, store, agent) = test_agent(provider, Mode::Work);
    let supervisor = agent.agent_supervisor().unwrap();
    let child = supervisor
        .spawn_agent(
            "ceiling".into(),
            "attempt an outside write".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    wait_for_status(&supervisor, child.agent_id, AgentStatus::Completed).await;
    let events = store.events(child.agent_id).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::PermissionResolved { approved: false, source, .. } if source == "non_interactive"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolFailed { result } if result.call_id == "outside" && result.output.contains("permission denied")
    )));
}

#[tokio::test]
async fn resume_reconciles_live_turns_and_restores_undelivered_reports_once() {
    let (workspace, store, agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    let root_id = agent.session_id;
    drop(agent);
    let spec = |name: &str| crate::store::AgentSessionSpec {
        root_session_id: root_id,
        parent_session_id: root_id,
        task_name: name.into(),
        agent_type: None,
        depth: 1,
    };
    // Completed with an undelivered report: resume must restore it to the
    // notification mailbox exactly once.
    let reported = store
        .create_agent_session(workspace.path(), spec("reported"), "task a".into())
        .unwrap();
    let report = latch_protocol::AgentReport {
        report_id: uuid::Uuid::new_v4(),
        agent_id: reported.agent_id,
        task_name: "reported".into(),
        status: AgentStatus::Completed,
        completion: latch_protocol::CompletionState::InProgress,
        summary: "done".into(),
        findings: vec![],
        touched_files: vec![],
        evidence: vec![],
        unresolved_questions: vec![],
    };
    store
        .append(
            reported.agent_id,
            EventPayload::AgentStatusChanged {
                status: AgentStatus::Running,
                reason: None,
            },
        )
        .unwrap();
    store
        .append(
            reported.agent_id,
            EventPayload::AgentReportCreated {
                report: report.clone(),
            },
        )
        .unwrap();
    // Completed with an already-delivered report: not queued again.
    let delivered = store
        .create_agent_session(workspace.path(), spec("delivered"), "task b".into())
        .unwrap();
    let delivered_report = latch_protocol::AgentReport {
        report_id: uuid::Uuid::new_v4(),
        agent_id: delivered.agent_id,
        task_name: "delivered".into(),
        status: AgentStatus::Completed,
        completion: latch_protocol::CompletionState::InProgress,
        summary: "seen".into(),
        findings: vec![],
        touched_files: vec![],
        evidence: vec![],
        unresolved_questions: vec![],
    };
    store
        .append(
            delivered.agent_id,
            EventPayload::AgentReportCreated {
                report: delivered_report.clone(),
            },
        )
        .unwrap();
    store
        .append(
            root_id,
            EventPayload::AgentNotificationDelivered {
                report: delivered_report,
            },
        )
        .unwrap();
    // Still running when the process ended: reconciled to Interrupted, never
    // rerun.
    let running = store
        .create_agent_session(workspace.path(), spec("running"), "task c".into())
        .unwrap();
    store
        .append(
            running.agent_id,
            EventPayload::AgentStatusChanged {
                status: AgentStatus::Running,
                reason: None,
            },
        )
        .unwrap();
    let resumed = root_agent(
        Arc::new(TestProvider::plain(0)),
        Mode::Work,
        store.clone(),
        workspace.path().to_path_buf(),
        root_id,
    );
    let supervisor = resumed.agent_supervisor().unwrap();
    let statuses = supervisor
        .list_agents()
        .into_iter()
        .map(|agent| (agent.task_name.clone(), agent.status))
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![
            ("delivered".to_owned(), AgentStatus::Completed),
            ("reported".to_owned(), AgentStatus::Completed),
            ("running".to_owned(), AgentStatus::Interrupted),
        ]
    );
    assert!(
        store
            .events(running.agent_id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::AgentInterrupted { .. }))
    );
    let restored = supervisor.drain_notifications();
    assert_eq!(restored, vec![report]);
    assert!(supervisor.drain_notifications().is_empty());
}

#[tokio::test]
async fn resumed_child_owes_its_delegation_brief_before_any_follow_up() {
    let (workspace, store, agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    let root_id = agent.session_id;
    drop(agent);
    // The process ended between spawn and the child's first model request:
    // the brief exists only inside the durable AgentSpawned event.
    let identity = store
        .create_agent_session(
            workspace.path(),
            crate::store::AgentSessionSpec {
                root_session_id: root_id,
                parent_session_id: root_id,
                task_name: "crashed".into(),
                agent_type: None,
                depth: 1,
            },
            "the original delegated task".into(),
        )
        .unwrap();
    store
        .append(
            identity.agent_id,
            EventPayload::AgentStatusChanged {
                status: AgentStatus::Running,
                reason: None,
            },
        )
        .unwrap();
    let resumed = root_agent(
        Arc::new(TestProvider::plain(0)),
        Mode::Work,
        store.clone(),
        workspace.path().to_path_buf(),
        root_id,
    );
    let supervisor = resumed.agent_supervisor().unwrap();
    assert_eq!(supervisor.list_agents()[0].status, AgentStatus::Interrupted);
    supervisor
        .continue_agent(identity.agent_id, "and now the follow-up".into())
        .await
        .unwrap();
    wait_for_status(&supervisor, identity.agent_id, AgentStatus::Completed).await;
    let received = store
        .events(identity.agent_id)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AgentMessageReceived { message } => Some(message.text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        received,
        vec![
            "the original delegated task".to_owned(),
            "and now the follow-up".to_owned(),
        ],
        "the resumed child must see its brief before the follow-up"
    );
}

/// Minimal provider with a distinct model id, used to prove inheritance.
struct FixedProvider {
    model: String,
}

#[async_trait]
impl ModelProvider for FixedProvider {
    fn name(&self) -> &str {
        "fixed"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
        sink: crate::provider::StreamSink,
    ) -> Result<ModelResponse> {
        let response = ModelResponse {
            text: "child work done".into(),
            tool_calls: Vec::new(),
            stop_reason: "stop".into(),
            usage: None,
            reasoning_content: None,
        };
        sink(StreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

#[tokio::test]
async fn child_inherits_the_live_inference_profile() {
    let (_workspace, store, mut agent) = test_agent(Arc::new(TestProvider::plain(0)), Mode::Work);
    // Switch the root to a distinct provider/model/effort before spawning.
    let inherited = Arc::new(FixedProvider {
        model: "inherited-model".into(),
    });
    let descriptor = crate::providers::ModelDescriptor {
        provider: latch_protocol::ProviderId::new("custom"),
        model: "inherited-model".into(),
        display_name: "inherited-model".into(),
        context_window_tokens: Some(64_000),
        supported_efforts: vec![
            latch_protocol::ReasoningEffort::Low,
            latch_protocol::ReasoningEffort::High,
        ],
        default_effort: latch_protocol::ReasoningEffort::Low,
        reasoning_replay: crate::provider::ReasoningReplay::Omit,
        pricing: None,
        aliases: Vec::new(),
        known: true,
    };
    agent
        .set_inference_profile(
            inherited.clone(),
            latch_protocol::InferenceProfile::new(
                "custom",
                "inherited-model",
                latch_protocol::ReasoningEffort::High,
            ),
            &descriptor,
            crate::config::ContextConfig::default(),
            "test switch",
        )
        .unwrap();

    let supervisor = agent.agent_supervisor().expect("root supervisor");
    let snapshot = supervisor
        .spawn_agent(
            "inherit".into(),
            "do a bounded task".into(),
            None,
            DelegationContext::default(),
        )
        .await
        .unwrap();
    supervisor
        .wait_agents(&[snapshot.agent_id], Duration::from_secs(2))
        .await
        .unwrap();

    // The child's own durable request provenance proves it inherited the live
    // profile instead of reverting to a config default.
    let child_events = store.events(snapshot.agent_id).unwrap();
    let model = child_events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelRequestStarted { model, .. } => Some(model.clone()),
            _ => None,
        })
        .expect("child made a request");
    assert_eq!(model, "inherited-model");
    // The root's own profile is unchanged by the child existing.
    assert_eq!(agent.profile().model, "inherited-model");
    agent.shutdown_extensions().await.unwrap();
}
