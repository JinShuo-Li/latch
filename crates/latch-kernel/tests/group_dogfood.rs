//! Deterministic multi-agent Agent Group dogfood.
//!
//! Two independent child Latch sessions coordinate on one shared task DAG
//! through the real kernel loop and the real group tools: root creates the
//! DAG, spawns one child per ready task, waits, continues a child for the
//! dependent task, exchanges a durable peer message, and finally completes
//! only after every required task is actually resolved.

use async_trait::async_trait;
use latch_kernel::{
    Agent, AgentEventSink, AgentRuntime, ContinuityEngine, EventStore, ModelProvider, PolicyEngine,
    ToolExecutor,
};
use latch_protocol::{
    AgentStatus, CompletionState, EventPayload, GroupTaskStatus, Mode, ModelRequest, ModelResponse,
    StreamEvent, ToolCall,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct ProviderLog {
    turns: HashMap<Uuid, usize>,
    requests: Vec<(Uuid, ModelRequest)>,
    /// Dependent-task ids already handled by each child, so a continue that
    /// arrives mid-turn is acted on once no matter which request first sees it.
    handled: std::collections::HashSet<(Uuid, Uuid)>,
    /// Ids the root script has observed. The provider-visible request window
    /// rotates, so a script that only re-parsed the current request would
    /// forget its own DAG after a cache epoch; learning is durable here.
    root_tasks: Vec<Uuid>,
    root_agents: Vec<Uuid>,
}

impl ProviderLog {
    fn observe_root(&mut self, outputs: &[(String, String)]) {
        for (name, output) in outputs {
            if name == "group_task" {
                for id in uuids(output) {
                    if !self.root_tasks.contains(&id) {
                        self.root_tasks.push(id);
                    }
                }
            }
            if name == "spawn_agent"
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(output)
                && let Some(id) = value
                    .get("agent_id")
                    .and_then(|id| id.as_str())
                    .and_then(|id| Uuid::parse_str(id).ok())
                && !self.root_agents.contains(&id)
            {
                self.root_agents.push(id);
            }
        }
    }
}

/// One provider instance shared by the root and every child. Behavior is keyed
/// by durable session id, exactly how a real multi-session run behaves.
struct DogfoodProvider {
    root_session: Uuid,
    log: Arc<Mutex<ProviderLog>>,
}

impl ProviderLog {
    fn next_turn(&mut self, session: Uuid) -> usize {
        let turn = self.turns.entry(session).or_insert(0);
        let current = *turn;
        *turn += 1;
        current
    }
}

impl DogfoodProvider {
    fn new(root_session: Uuid) -> Self {
        Self {
            root_session,
            log: Arc::new(Mutex::new(ProviderLog::default())),
        }
    }
}

fn mark_handled(shared: &Mutex<ProviderLog>, session: Uuid, task: Uuid) -> bool {
    shared.lock().unwrap().handled.insert((session, task))
}

struct SessionProvider {
    session: Uuid,
    log: Arc<Mutex<ProviderLog>>,
}

#[async_trait]
impl ModelProvider for DogfoodProvider {
    fn name(&self) -> &str {
        "dogfood-group"
    }
    fn model(&self) -> &str {
        "dogfood-group"
    }
    fn for_session(&self, session_id: Uuid) -> Option<Arc<dyn ModelProvider>> {
        if session_id == self.root_session {
            return None;
        }
        Some(Arc::new(SessionProvider {
            session: session_id,
            log: Arc::clone(&self.log),
        }))
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: latch_kernel::provider::StreamSink,
    ) -> anyhow::Result<ModelResponse> {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled")
        }
        let turn = {
            let mut log = self.log.lock().unwrap();
            let turn = log.next_turn(self.root_session);
            log.requests.push((self.root_session, request.clone()));
            let outputs = tool_outputs(&request);
            log.observe_root(&outputs);
            turn
        };
        let response = root_response(&self.log, turn, &request);
        emit(&response, &sink);
        Ok(response)
    }
}

#[async_trait]
impl ModelProvider for SessionProvider {
    fn name(&self) -> &str {
        "dogfood-group"
    }
    fn model(&self) -> &str {
        "dogfood-group"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
        sink: latch_kernel::provider::StreamSink,
    ) -> anyhow::Result<ModelResponse> {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled")
        }
        let turn = {
            let mut log = self.log.lock().unwrap();
            let turn = log.next_turn(self.session);
            log.requests.push((self.session, request.clone()));
            turn
        };
        let response = child_response(&self.log, self.session, turn, &request);
        emit(&response, &sink);
        Ok(response)
    }
}

fn emit(response: &ModelResponse, sink: &latch_kernel::provider::StreamSink) {
    for chunk in response.text.as_bytes().chunks(8) {
        sink(StreamEvent::TextDelta(
            String::from_utf8_lossy(chunk).into_owned(),
        ));
    }
    for call in &response.tool_calls {
        sink(StreamEvent::ToolCallDelta(call.clone()));
    }
    sink(StreamEvent::Completed(response.clone()));
}

fn response(text: &str, tool_calls: Vec<ToolCall>) -> ModelResponse {
    let stop_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    ModelResponse {
        text: text.into(),
        tool_calls,
        stop_reason: stop_reason.into(),
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

fn last_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

/// The first task id in the newest message carrying a semantic marker. Kernel
/// context and delivered messages can follow a user turn, so a script that
/// only looked at the very last message would be fragile.
fn task_id_marked(request: &ModelRequest, marker: &str) -> Option<Uuid> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.content.contains(marker))
        .and_then(|message| uuids(&message.content).into_iter().next())
}

fn uuids(text: &str) -> Vec<Uuid> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 36 <= bytes.len() {
        if text.is_char_boundary(index)
            && text.is_char_boundary(index + 36)
            && let Ok(id) = Uuid::parse_str(&text[index..index + 36])
        {
            found.push(id);
            index += 36;
        } else {
            index += 1;
        }
    }
    found
}

fn tool_outputs(request: &ModelRequest) -> Vec<(String, String)> {
    let mut names = HashMap::new();
    for message in &request.messages {
        for tool_call in &message.tool_calls {
            names.insert(tool_call.id.clone(), tool_call.name.clone());
        }
    }
    request
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .filter_map(|message| {
            let name = message
                .tool_call_id
                .as_ref()
                .and_then(|id| names.get(id))
                .cloned()?;
            Some((name, message.content.clone()))
        })
        .collect()
}

/// Root script: build the DAG, delegate ready tasks, then coordinate through
/// durable status until every required task is resolved.
fn root_response(
    shared: &Mutex<ProviderLog>,
    turn: usize,
    request: &ModelRequest,
) -> ModelResponse {
    let outputs = tool_outputs(request);
    let (created, spawned) = {
        let log = shared.lock().unwrap();
        (log.root_tasks.clone(), log.root_agents.clone())
    };
    match turn {
        0 => response(
            "",
            vec![
                call(
                    "t1",
                    "group_task",
                    serde_json::json!({
                        "op": "create",
                        "title": "parser implementation",
                        "description": "implement the expression parser",
                        "required": true,
                        "expected_paths": ["parser.rs"]
                    }),
                ),
                call(
                    "t2",
                    "group_task",
                    serde_json::json!({
                        "op": "create",
                        "title": "evaluator implementation",
                        "description": "implement the evaluator",
                        "required": true,
                        "expected_paths": ["evaluator.rs"]
                    }),
                ),
            ],
        ),
        1 => response(
            "",
            vec![call(
                "t3",
                "group_task",
                serde_json::json!({
                    "op": "create",
                    "title": "integration tests",
                    "description": "tests for parser and evaluator",
                    "dependencies": [created[0].to_string(), created[1].to_string()],
                    "required": true
                }),
            )],
        ),
        2 => response(
            "",
            vec![call(
                "t4",
                "group_task",
                serde_json::json!({
                    "op": "create",
                    "title": "docs and integration review",
                    "description": "document the evaluator and review integration",
                    "dependencies": [created[2].to_string()],
                    "required": true
                }),
            )],
        ),
        3 => response(
            "",
            vec![
                call(
                    "s1",
                    "spawn_agent",
                    serde_json::json!({
                        "task_name": "A-parser",
                        "message": "Implement the parser workstream.",
                        "task_id": created[0].to_string()
                    }),
                ),
                call(
                    "s2",
                    "spawn_agent",
                    serde_json::json!({
                        "task_name": "B-evaluator",
                        "message": "Implement the evaluator workstream.",
                        "task_id": created[1].to_string()
                    }),
                ),
            ],
        ),
        _ => {
            // Coordination loop: refresh status, wait for workers, delegate
            // ready tasks, and finish only once every required task resolved.
            let last_continue = outputs
                .iter()
                .rposition(|(name, _)| name == "continue_agent");
            let fresh = |name: &str| {
                outputs
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(index, (tool, _))| {
                        tool == name && last_continue.is_none_or(|continued| *index > continued)
                    })
                    .map(|(_, (_, output))| output.as_str())
            };
            let agents_output = fresh("list_agents");
            let status_output = fresh("group_status");
            let parser = created.first().copied();
            let a_idle = agents_output
                .and_then(|output| serde_json::from_str::<serde_json::Value>(output).ok())
                .and_then(|value| value.as_array().cloned())
                .and_then(|agents| {
                    let target = spawned.first()?;
                    agents.into_iter().find(|agent| {
                        agent.get("agent_id").and_then(|id| id.as_str())
                            == Some(target.to_string().as_str())
                    })
                })
                .and_then(|agent| {
                    agent
                        .get("status")
                        .and_then(|status| status.as_str())
                        .map(|status| status == "completed")
                });
            let all_done = status_output.is_some_and(|output| output.contains("done 4"));
            if all_done {
                return response(
                    "All required group tasks are complete.",
                    vec![call(
                        "done",
                        "complete",
                        serde_json::json!({"implementation_done": true}),
                    )],
                );
            }
            let ready_task = status_output.and_then(|output| {
                output
                    .lines()
                    .find(|line| line.starts_with("ready:"))
                    .and_then(|line| uuids(line).into_iter().next())
            });
            if a_idle == Some(true)
                && let (Some(agent), Some(task_id)) = (spawned.first(), ready_task)
            {
                return response(
                    "",
                    vec![call(
                        "c",
                        "continue_agent",
                        serde_json::json!({
                            "agent_id": agent.to_string(),
                            "message": format!(
                                "Claim and complete the dependent task {task_id} now."
                            )
                        }),
                    )],
                );
            }
            if parser.is_none() {
                return response("", vec![]);
            }
            if agents_output.is_none() {
                return response("", vec![call("l", "list_agents", serde_json::json!({}))]);
            }
            if status_output.is_none() {
                return response("", vec![call("s", "group_status", serde_json::json!({}))]);
            }
            response(
                "",
                vec![call(
                    "w",
                    "wait_agents",
                    serde_json::json!({"agent_ids": [spawned.first().unwrap().to_string()]}),
                )],
            )
        }
    }
}

/// Child script: work the assigned task, then follow an explicit dependent-task
/// continuation exactly once. Any other message is information and is only
/// acknowledged.
fn child_response(
    shared: &Mutex<ProviderLog>,
    session: Uuid,
    turn: usize,
    request: &ModelRequest,
) -> ModelResponse {
    let prompt = last_user_text(request);
    if turn == 0 {
        let Some(task_id) = task_id_marked(request, "Agent group task") else {
            return response("acknowledged", vec![]);
        };
        let (file, summary) = if prompt.contains("parser implementation") {
            ("parser.rs", "parser implemented")
        } else {
            ("evaluator.rs", "evaluator implemented")
        };
        return response(
            "",
            vec![
                call(
                    "k1",
                    "group_task",
                    serde_json::json!({"op":"start","task_id":task_id.to_string()}),
                ),
                call(
                    "k2",
                    "write",
                    serde_json::json!({
                        "path": file,
                        "base_hash": null,
                        "content": format!("// {summary}\n")
                    }),
                ),
                call(
                    "k3",
                    "group_task",
                    serde_json::json!({
                        "op":"complete",
                        "task_id":task_id.to_string(),
                        "summary": summary,
                        "touched_files":[file]
                    }),
                ),
            ],
        );
    }
    if let Some(task_id) = task_id_marked(request, "dependent task")
        && mark_handled(shared, session, task_id)
    {
        return response(
            "",
            vec![
                call(
                    "m1",
                    "group_message",
                    serde_json::json!({
                        "op":"send",
                        "to":"group",
                        "text":"dependent work claimed; parser and evaluator passed their checks"
                    }),
                ),
                call(
                    "k4",
                    "group_task",
                    serde_json::json!({"op":"claim","task_id":task_id.to_string()}),
                ),
                call(
                    "k5",
                    "group_task",
                    serde_json::json!({"op":"start","task_id":task_id.to_string()}),
                ),
                call(
                    "k6",
                    "group_task",
                    serde_json::json!({
                        "op":"complete",
                        "task_id":task_id.to_string(),
                        "summary":"dependent work completed and checked"
                    }),
                ),
            ],
        );
    }
    response("acknowledged", vec![])
}

fn build_root(
    store: &EventStore,
    workspace: &Path,
    session: Uuid,
    provider: Arc<DogfoodProvider>,
) -> Agent {
    let policy = PolicyEngine::new(Mode::Work, workspace.to_path_buf(), Default::default());
    let tools = ToolExecutor::new(
        workspace.to_path_buf(),
        workspace.join("artifacts").join(session.to_string()),
        store.clone(),
        session,
        policy,
    )
    .unwrap();
    let continuity = ContinuityEngine::new(store.clone(), Default::default());
    Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.to_path_buf(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity,
        retry_budget: 1,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_child_sessions_coordinate_on_one_shared_dag() {
    let workspace = tempfile::tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let root_session = store.create_session(workspace.path()).unwrap();
    let provider = Arc::new(DogfoodProvider::new(root_session));
    let mut root = build_root(&store, workspace.path(), root_session, provider.clone());
    let sink: AgentEventSink = Arc::new(|_| {});
    let text = root
        .run(
            "Implement a tiny expression evaluator with tests and docs.",
            CancellationToken::new(),
            sink,
        )
        .await
        .unwrap();
    assert!(text.contains("All required group tasks are complete"));

    // Root terminal completion is real: with no required validations the
    // kernel-derived state is ImplementedNotVerified, and it only exists
    // because the group gate passed after every required task resolved.
    assert_eq!(
        root.state().completion,
        CompletionState::ImplementedNotVerified
    );
    let group = root.group().unwrap();
    let status = group.status().unwrap();
    assert_eq!(status.counts.total, 4);
    assert_eq!(status.counts.completed, 4);
    assert_eq!(status.counts.active(), 0);
    assert!(status.ready.is_empty());

    let state = group.snapshot();
    let by_title = |title: &str| {
        state
            .tasks()
            .values()
            .find(|task| task.title == title)
            .cloned()
            .unwrap()
    };
    let parser = by_title("parser implementation");
    let evaluator = by_title("evaluator implementation");
    let tests = by_title("integration tests");
    let docs = by_title("docs and integration review");
    assert_eq!(parser.status, GroupTaskStatus::Completed);
    assert_eq!(evaluator.status, GroupTaskStatus::Completed);
    assert_eq!(tests.status, GroupTaskStatus::Completed);
    assert_eq!(docs.status, GroupTaskStatus::Completed);
    assert_ne!(
        parser.assignee, evaluator.assignee,
        "two independent children"
    );

    // Exactly one claim exists for the dependent task, and it committed after
    // both dependencies completed.
    let events = store.events(root_session).unwrap();
    let kinds = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::GroupTaskClaimed { task_id, .. } => Some(("claimed", *task_id)),
            EventPayload::GroupTaskStatusChanged {
                task_id, status, ..
            } if *status == GroupTaskStatus::Completed => Some(("completed", *task_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let claimed_tests = kinds
        .iter()
        .position(|(kind, task)| *kind == "claimed" && *task == tests.task_id)
        .expect("the dependent task was claimed");
    let completed_parser = kinds
        .iter()
        .position(|(kind, task)| *kind == "completed" && *task == parser.task_id)
        .unwrap();
    let completed_evaluator = kinds
        .iter()
        .position(|(kind, task)| *kind == "completed" && *task == evaluator.task_id)
        .unwrap();
    assert!(claimed_tests > completed_parser);
    assert!(claimed_tests > completed_evaluator);
    assert_eq!(
        kinds
            .iter()
            .filter(|(kind, task)| *kind == "claimed" && *task == tests.task_id)
            .count(),
        1,
        "exactly one child may claim the dependent task"
    );

    // Every queued peer message was delivered to root exactly once at a safe
    // boundary, where the provider-visible request included it.
    let queued = store
        .events_of_kinds(root_session, &["group_message_queued"])
        .unwrap();
    let delivered = store
        .events_of_kinds(root_session, &["group_message_delivered"])
        .unwrap();
    assert!(!queued.is_empty(), "the child sent peer messages");
    assert_eq!(
        queued.len(),
        delivered.len(),
        "every queued message is delivered exactly once to root"
    );
    let requests = provider.log.lock().unwrap().requests.clone();
    assert!(
        requests
            .iter()
            .any(|(session, request)| *session == root_session
                && request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("dependent work claimed"))),
        "the durable message reached the root model context"
    );

    // Each child is an independent durable session with its own transcript, and
    // the workspace files they wrote both exist.
    for task in [&parser, &evaluator] {
        let agent_id = task.assignee.unwrap();
        let child_events = store.events(agent_id).unwrap();
        assert!(!child_events.is_empty());
        assert!(
            child_events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::AgentReportCreated { .. }))
        );
    }
    assert!(workspace.path().join("parser.rs").exists());
    assert!(workspace.path().join("evaluator.rs").exists());

    // A fresh root agent (process restart) reconstructs the same coordination
    // state from durable events alone.
    let resumed_provider = Arc::new(DogfoodProvider::new(root_session));
    let resumed = build_root(&store, workspace.path(), root_session, resumed_provider);
    let resumed_state = resumed.group().unwrap().snapshot();
    assert_eq!(resumed_state.tasks().len(), 4);
    assert!(resumed_state.required_unfinished().is_empty());
    assert_eq!(
        resumed_state.task(tests.task_id).unwrap().status,
        GroupTaskStatus::Completed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_information_never_wakes_an_idle_child_and_delivers_once() {
    let workspace = tempfile::tempdir().unwrap();
    let store = EventStore::open_memory().unwrap();
    let root_session = store.create_session(workspace.path()).unwrap();
    let provider = Arc::new(DogfoodProvider::new(root_session));
    let root = build_root(&store, workspace.path(), root_session, provider);
    let supervisor = root.agent_supervisor().unwrap();
    let group = root.group().unwrap();
    let task = group
        .create_task(
            root_session,
            "parser implementation".into(),
            "implement".into(),
            vec![],
            true,
            vec![],
        )
        .unwrap();
    let child = supervisor
        .spawn_agent_with_task(
            "A-parser".into(),
            "Implement the parser workstream.".into(),
            None,
            Default::default(),
            Some(task.task_id),
        )
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = supervisor
            .list_agents()
            .into_iter()
            .find(|agent| agent.agent_id == child.agent_id)
            .map(|agent| agent.status);
        if let Some(status) = status
            && !matches!(status, AgentStatus::Starting | AgentStatus::Running)
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child did not finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The child explicitly completed its task during its turn.
    assert_eq!(
        group.snapshot().task(task.task_id).unwrap().status,
        GroupTaskStatus::Completed
    );
    assert_eq!(
        group.snapshot().task(task.task_id).unwrap().assignee,
        Some(child.agent_id)
    );
    // Information-only traffic queues durably and never wakes the idle child.
    group
        .send_message(
            root_session,
            latch_protocol::GroupMessageTarget::Agent(child.agent_id),
            "heads up: the parser contract changed".into(),
        )
        .unwrap();
    assert!(
        store
            .events_of_kinds(child.agent_id, &["group_message_delivered"])
            .unwrap()
            .is_empty(),
        "no delivery outside a safe model boundary"
    );
    supervisor
        .continue_agent(child.agent_id, "acknowledge the update".into())
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if store
            .events_of_kinds(child.agent_id, &["group_message_delivered"])
            .unwrap()
            .len()
            == 1
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "message was not delivered"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A later boundary never redelivers it.
    supervisor
        .continue_agent(child.agent_id, "anything else?".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store
            .events_of_kinds(child.agent_id, &["group_message_delivered"])
            .unwrap()
            .len(),
        1
    );
}
