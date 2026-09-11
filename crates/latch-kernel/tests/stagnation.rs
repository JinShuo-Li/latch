//! Deterministic progress/stagnation supervision tests.
//!
//! Real dogfood exposed an inspection loop: the model repeatedly re-read the
//! same files and git state, every tool succeeded, `FailureManager` never
//! intervened, and the run died at the 32-turn hard limit. These tests pin the
//! kernel-side progress epoch and its interaction with durable history replay.

use latch_kernel::config::{ContextConfig, PermissionConfig};
use latch_kernel::provider::{ModelProvider, StreamSink};
use latch_kernel::{Agent, AgentRuntime, ContinuityEngine, EventStore, PolicyEngine, ToolExecutor};
use latch_protocol::{
    Event, EventPayload, Mode, ModelRequest, ModelResponse, StreamEvent, ToolCall, ToolResult,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

type BeforeHook = Box<dyn FnOnce() + Send>;

struct Step {
    response: ModelResponse,
    before: Option<BeforeHook>,
}

fn step(response: ModelResponse) -> Step {
    Step {
        response,
        before: None,
    }
}

fn step_before(before: BeforeHook, response: ModelResponse) -> Step {
    Step {
        response,
        before: Some(before),
    }
}

struct ScriptedProvider {
    requests: Mutex<Vec<ModelRequest>>,
    steps: Mutex<VecDeque<Step>>,
}

impl ScriptedProvider {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            steps: Mutex::new(steps.into()),
        })
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn model(&self) -> &str {
        "scripted-test"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
        sink: StreamSink,
    ) -> anyhow::Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted response exhausted");
        if let Some(before) = step.before {
            before();
        }
        let response = step.response;
        for chunk in response.text.as_bytes().chunks(16) {
            sink(StreamEvent::TextDelta(
                String::from_utf8_lossy(chunk).into_owned(),
            ));
        }
        sink(StreamEvent::Completed(response.clone()));
        Ok(response)
    }
}

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

fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    for (name, contents) in files {
        let path = workspace.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    (dir, workspace)
}

fn init_git(workspace: &Path) {
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.email=t@l",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .status()
            .unwrap();
    }
}

fn build_agent(
    store: EventStore,
    session: uuid::Uuid,
    workspace: &Path,
    artifacts: PathBuf,
    mode: Mode,
    provider: Arc<dyn ModelProvider>,
    context: ContextConfig,
) -> Agent {
    let tools = ToolExecutor::new(
        workspace.to_path_buf(),
        artifacts,
        store.clone(),
        session,
        PolicyEngine::new(mode, workspace.to_path_buf(), PermissionConfig::default()),
    )
    .unwrap();
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.to_path_buf(),
        mode,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::new(store, context),
        retry_budget: 3,
    });
    agent.set_stagnation_budget(2);
    agent
}

fn fresh_agent(
    dir: &tempfile::TempDir,
    workspace: &Path,
    mode: Mode,
    provider: Arc<dyn ModelProvider>,
    context: ContextConfig,
) -> (Agent, EventStore, uuid::Uuid) {
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(workspace).unwrap();
    let agent = build_agent(
        store.clone(),
        session,
        workspace,
        dir.path().join("artifacts"),
        mode,
        provider,
        context,
    );
    (agent, store, session)
}

fn stagnation_events(events: &[Event]) -> Vec<(Vec<String>, u32)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ProgressStagnation {
                unchanged,
                redundant_turns,
            } => Some((unchanged.clone(), *redundant_turns)),
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[Event]) -> Vec<ToolResult> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                Some(result.clone())
            }
            _ => None,
        })
        .collect()
}

fn assert_tool_transaction_invariant(events: &[Event]) {
    let mut terminals: HashMap<&str, usize> = HashMap::new();
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

#[tokio::test]
async fn unchanged_reread_regrounds_and_suppresses_within_budget() {
    let (dir, workspace) = workspace_with(&[("a.txt", "alpha")]);
    let provider = ScriptedProvider::new(vec![
        step(response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading again",
            vec![call("r2", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading again",
            vec![call("r3", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading again",
            vec![call("r4", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Ask,
        provider.clone(),
        ContextConfig::default(),
    );
    agent
        .run("inspect", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    // Stagnation triggered from the third inspection turn, far before 32.
    let stagnation = stagnation_events(&events);
    assert_eq!(stagnation.len(), 1, "one re-ground event: {stagnation:?}");
    assert_eq!(stagnation[0].1, 2);
    assert_eq!(stagnation[0].0, vec!["read_file a.txt".to_owned()]);
    assert!(
        provider.requests().len() < 32,
        "supervision must fire long before the hard turn limit"
    );
    // The repeated call after re-ground is rejected without execution.
    let suppression = tool_results(&events).into_iter().find(|result| {
        result
            .output
            .contains("Kernel suppressed redundant observation")
    });
    let suppression = suppression.expect("suppressed repeat");
    assert_eq!(suppression.call_id, "r4");
    assert!(suppression.is_error);
    assert!(suppression.output.contains("read_file a.txt"));
    assert_tool_transaction_invariant(&events);
    // The request after re-ground carried the kernel instruction listing the
    // unchanged observation.
    let instruction = &provider.requests()[3];
    assert!(instruction.messages.iter().any(|message| {
        message.role == "user"
            && message.content.contains("KERNEL RE-GROUND")
            && message.content.contains("read_file a.txt")
            && message.content.contains("concrete blocker")
    }));
}

#[tokio::test]
async fn reread_after_guarded_mutation_is_allowed() {
    let (dir, workspace) = workspace_with(&[("a.txt", "alpha")]);
    let hash = digest("alpha");
    let provider = ScriptedProvider::new(vec![
        step(response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "editing",
            vec![call(
                "p1",
                "patch",
                json!({"path":"a.txt","base_hash":hash,"old":"alpha","new":"beta"}),
            )],
        )),
        step(response(
            "re-reading",
            vec![call("r2", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Work,
        provider,
        ContextConfig::default(),
    );
    agent
        .run("fix it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    assert!(stagnation_events(&events).is_empty());
    let results = tool_results(&events);
    assert!(
        results
            .iter()
            .any(|result| result.call_id == "p1" && !result.is_error)
    );
    let reread = results
        .iter()
        .find(|result| result.call_id == "r2")
        .expect("re-read ran");
    assert!(!reread.is_error, "{}", reread.output);
    assert!(reread.output.contains("beta"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "beta"
    );
}

#[tokio::test]
async fn reread_after_external_mutation_is_allowed() {
    let (dir, workspace) = workspace_with(&[("a.txt", "alpha")]);
    let external = workspace.join("a.txt");
    let provider = ScriptedProvider::new(vec![
        step(response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"a.txt"}))],
        )),
        step_before(
            Box::new(move || std::fs::write(&external, "externally updated").unwrap()),
            response(
                "re-reading",
                vec![call("r2", "read_file", json!({"path":"a.txt"}))],
            ),
        ),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Ask,
        provider,
        ContextConfig::default(),
    );
    agent
        .run("inspect", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    assert!(stagnation_events(&events).is_empty());
    assert!(
        events.iter().any(|event| matches!(
            event.payload,
            EventPayload::ExternalFileChangeDetected { .. }
        )),
        "external edit is detected and classified"
    );
    let reread = tool_results(&events)
        .into_iter()
        .find(|result| result.call_id == "r2")
        .expect("re-read ran");
    assert!(!reread.is_error, "{}", reread.output);
    assert!(reread.output.contains("externally updated"));
}

#[tokio::test]
async fn repeated_git_status_regrounds() {
    let (dir, workspace) = workspace_with(&[("a.txt", "alpha")]);
    init_git(&workspace);
    let provider = ScriptedProvider::new(vec![
        step(response(
            "checking",
            vec![call("g1", "git_status", json!({}))],
        )),
        step(response(
            "checking",
            vec![call("g2", "git_status", json!({}))],
        )),
        step(response(
            "checking",
            vec![call("g3", "git_status", json!({}))],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Ask,
        provider,
        ContextConfig::default(),
    );
    agent
        .run("inspect git", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    let stagnation = stagnation_events(&events);
    assert_eq!(stagnation.len(), 1, "{stagnation:?}");
    assert_eq!(stagnation[0].0, vec!["git status".to_owned()]);
}

#[tokio::test]
async fn repeated_search_with_identical_result_regrounds() {
    let (dir, workspace) = workspace_with(&[("data.txt", "needle here\n")]);
    let provider = ScriptedProvider::new(vec![
        step(response(
            "searching",
            vec![call("s1", "search", json!({"query":"needle"}))],
        )),
        step(response(
            "searching",
            vec![call("s2", "search", json!({"query":"needle"}))],
        )),
        step(response(
            "searching",
            vec![call("s3", "search", json!({"query":"needle"}))],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Ask,
        provider,
        ContextConfig::default(),
    );
    agent
        .run("search it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    let stagnation = stagnation_events(&events);
    assert_eq!(stagnation.len(), 1, "{stagnation:?}");
    assert_eq!(stagnation[0].0, vec!["search \"needle\" in .".to_owned()]);
}

#[tokio::test]
async fn mixed_useful_exploration_does_not_stagnate() {
    let (dir, workspace) =
        workspace_with(&[("a.txt", "alpha"), ("b.txt", "beta"), ("c.txt", "gamma")]);
    let provider = ScriptedProvider::new(vec![
        step(response(
            "read a",
            vec![call("a1", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "status plus repeated a",
            vec![
                call("g1", "git_status", json!({})),
                call("a2", "read_file", json!({"path":"a.txt"})),
            ],
        )),
        step(response(
            "read b plus repeated status",
            vec![
                call("b1", "read_file", json!({"path":"b.txt"})),
                call("g2", "git_status", json!({})),
            ],
        )),
        step(response(
            "search plus repeated a",
            vec![
                call("s1", "search", json!({"query":"gamma"})),
                call("a3", "read_file", json!({"path":"a.txt"})),
            ],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Ask,
        provider,
        ContextConfig::default(),
    );
    agent
        .run("explore", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    assert!(
        stagnation_events(&events).is_empty(),
        "every turn gathered new information"
    );
}

#[tokio::test]
async fn reground_breaks_the_loop() {
    let (dir, workspace) = workspace_with(&[("a.txt", "alpha")]);
    let hash = digest("alpha");
    let provider = ScriptedProvider::new(vec![
        step(response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading",
            vec![call("r2", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading",
            vec![call("r3", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "acting on existing evidence",
            vec![call(
                "p1",
                "patch",
                json!({"path":"a.txt","base_hash":hash,"old":"alpha","new":"fixed"}),
            )],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, store, session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Work,
        provider,
        ContextConfig::default(),
    );
    agent
        .run("fix it", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let events = store.events(session).unwrap();
    assert_eq!(stagnation_events(&events).len(), 1);
    assert!(
        !tool_results(&events)
            .iter()
            .any(|result| result.output.contains("Kernel suppressed")),
        "the model acted instead of repeating after re-ground"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "fixed"
    );
}

#[tokio::test]
async fn live_and_resume_supervision_are_deterministic() {
    let (dir, workspace) = workspace_with(&[("a.txt", "alpha")]);
    let db = dir.path().join("latch.sqlite3");
    let store = EventStore::open(&db).unwrap();
    let session = store.create_session(&workspace).unwrap();
    let provider = ScriptedProvider::new(vec![
        step(response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading",
            vec![call("r2", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response(
            "reading",
            vec![call("r3", "read_file", json!({"path":"a.txt"}))],
        )),
        step(response("done", vec![])),
    ]);
    let mut live = build_agent(
        store.clone(),
        session,
        &workspace,
        dir.path().join("artifacts"),
        Mode::Ask,
        provider,
        ContextConfig::default(),
    );
    live.run("inspect", CancellationToken::new(), Arc::new(|_| {}))
        .await
        .unwrap();
    let live_state = (
        live.progress().epoch(),
        live.progress().redundant_turns(),
        live.progress().regrounded(),
        live.progress().known_unchanged(),
    );
    assert!(live_state.2, "live supervisor is re-grounded");
    drop(live);
    drop(store);

    // Resume reconstructs the exact same supervisor state from durable events.
    let store = EventStore::open(&db).unwrap();
    let resumed_provider = ScriptedProvider::new(vec![]);
    let mut resumed = build_agent(
        store.clone(),
        session,
        &workspace,
        dir.path().join("artifacts"),
        Mode::Ask,
        resumed_provider,
        ContextConfig::default(),
    );
    resumed.restore_progress().unwrap();
    let resumed_state = (
        resumed.progress().epoch(),
        resumed.progress().redundant_turns(),
        resumed.progress().regrounded(),
        resumed.progress().known_unchanged(),
    );
    assert_eq!(live_state, resumed_state);
    assert!(resumed.progress().reground_instruction().is_some());
    // Replaying again is idempotent: no duplicate durable events, same state.
    let before = store.events(session).unwrap().len();
    resumed.restore_progress().unwrap();
    assert_eq!(store.events(session).unwrap().len(), before);
    assert_eq!(
        (
            resumed.progress().epoch(),
            resumed.progress().redundant_turns(),
            resumed.progress().regrounded(),
            resumed.progress().known_unchanged(),
        ),
        live_state
    );
}

/// The real replay regression: once the recent byte budget rotates past the
/// original user prompt, the window no longer contains a `user` event. The
/// transcript must not collapse to nothing — that gave the model amnesia and
/// restarted the inspection loop.
#[tokio::test]
async fn recent_window_without_user_prompt_still_replays_history() {
    let big = "x".repeat(4_000);
    let (dir, workspace) = workspace_with(&[("big.txt", &big)]);
    let provider = ScriptedProvider::new(vec![
        step(response(
            "reading",
            vec![call("r1", "read_file", json!({"path":"big.txt"}))],
        )),
        step(response(
            "reading",
            vec![call("r2", "read_file", json!({"path":"big.txt"}))],
        )),
        step(response("done", vec![])),
    ]);
    let (mut agent, _store, _session) = fresh_agent(
        &dir,
        &workspace,
        Mode::Ask,
        provider.clone(),
        ContextConfig {
            active_bytes: 20_000,
            recent_bytes: 1_500,
            reserve_bytes: 1_000,
        },
    );
    agent
        .run(
            "inspect the workspace",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let second = &requests[1];
    assert!(
        !second.messages.is_empty(),
        "a window without the user prompt must never replay as empty"
    );
    assert_eq!(second.messages[0].role, "user");
    assert!(
        second.messages[0]
            .content
            .contains("scrolled out of the active recent window"),
        "provider-valid kernel continuation anchor: {}",
        second.messages[0].content
    );
    assert!(
        second
            .messages
            .iter()
            .any(|message| message.role == "assistant" && !message.tool_calls.is_empty()),
        "the previous assistant tool call must be replayed"
    );
    assert!(
        second
            .messages
            .iter()
            .any(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some("r1")),
        "the previous tool result must be replayed"
    );
}
