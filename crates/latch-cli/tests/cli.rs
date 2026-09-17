//! Deterministic CLI integration tests.
//!
//! They run the real `latch` binary against a loopback SSE mock provider and
//! an isolated state directory. No live credentials, network access, or TTY is
//! required.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
enum Turn {
    Text(&'static str),
    ToolCall {
        name: &'static str,
        arguments: Value,
    },
}

/// One scripted model response.
struct MockResponse {
    turn: Turn,
    usage: Option<Value>,
}

struct MockProvider {
    base_url: String,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockProvider {
    fn start_with_delay(
        turns: Vec<Turn>,
        usage: Option<Value>,
        delay: std::time::Duration,
    ) -> Self {
        assert!(
            !turns.is_empty(),
            "the mock provider needs at least one turn"
        );
        let turns = Arc::new(turns);
        let usage = Arc::new(usage);
        Self::start_with_responder(delay, move |index, _body| {
            let turn = turns[index.min(turns.len() - 1)].clone();
            MockResponse {
                turn,
                usage: (*usage).clone(),
            }
        })
    }

    /// Serves responses from a per-request responder that sees the request
    /// body. The responder index is the request ordinal across all sessions.
    fn start_with_responder<F>(delay: std::time::Duration, responder: F) -> Self
    where
        F: Fn(usize, &str) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let addr = listener.local_addr().expect("mock provider address");
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responder = Arc::new(responder);
        {
            let hits = Arc::clone(&hits);
            let requests = Arc::clone(&requests);
            let responder = Arc::clone(&responder);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let body = read_request_body(&mut stream).unwrap_or_default();
                    let index = hits.fetch_add(1, Ordering::SeqCst);
                    requests.lock().expect("request log").push(body.clone());
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    let response = responder(index, &body);
                    let sse = sse_response(&response.turn, &response.usage);
                    let _ = stream.write_all(sse.as_bytes());
                    let _ = stream.flush();
                }
            });
        }
        Self {
            base_url: format!("http://{addr}/v1"),
            hits,
            requests,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log").clone()
    }
}

fn read_request_body(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn sse_response(turn: &Turn, usage: &Option<Value>) -> String {
    let mut events = Vec::new();
    match turn {
        Turn::Text(text) => {
            events.push(json!({"choices":[{"delta":{"content":text},"finish_reason":null}]}));
        }
        Turn::ToolCall { name, arguments } => {
            events.push(json!({"choices":[{"delta":{
                "tool_calls":[{
                    "index":0,
                    "id":"call_1",
                    "type":"function",
                    "function":{"name":name,"arguments":arguments.to_string()}
                }]
            },"finish_reason":null}]}));
        }
    }
    if let Some(usage) = usage {
        events.push(json!({"choices":[],"usage":usage}));
    }
    let finish = match turn {
        Turn::ToolCall { .. } => "tool_calls",
        Turn::Text(_) => "stop",
    };
    events.push(json!({"choices":[{"delta":{},"finish_reason":finish}]}));
    let mut body = String::new();
    for event in &events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
    config_path: PathBuf,
    mock: MockProvider,
}

fn fixture(turns: Vec<Turn>, usage: Option<Value>, safety: &str) -> Fixture {
    fixture_delayed(turns, usage, safety, std::time::Duration::ZERO)
}

fn fixture_delayed(
    turns: Vec<Turn>,
    usage: Option<Value>,
    safety: &str,
    delay: std::time::Duration,
) -> Fixture {
    fixture_with_mock(
        MockProvider::start_with_delay(turns, usage, delay),
        safety,
        "",
    )
}

fn fixture_with_mock(mock: MockProvider, safety: &str, extra_config: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let state_dir = root.join("state");
    let config_path = root.join("config.toml");
    let text = format!(
        "state_dir = \"{state}\"\n\
         default_mode = \"WORK\"\n\
         \n\
         [providers.mock]\n\
         kind = \"openai-compatible\"\n\
         base_url = \"{base}\"\n\
         credential = \"env:MOCK_API_KEY\"\n\
         default_model = \"mock-model\"\n\
         \n\
         [inference]\n\
         provider = \"mock\"\n\
         model = \"mock-model\"\n\
         \n\
         [permissions]\n\
         mode = \"human\"\n\
         \n\
         [safety]\n\
         level = \"{safety}\"\n\
         {extra_config}",
        state = state_dir.display(),
        base = mock.base_url,
    );
    std::fs::write(&config_path, text).expect("write config");
    Fixture {
        _dir: dir,
        root,
        workspace,
        config_path,
        mock,
    }
}

fn latch_command(fixture: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_latch"));
    command
        .arg("--config")
        .arg(&fixture.config_path)
        .env("MOCK_API_KEY", "test-secret-key")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run_latch(fixture: &Fixture, args: &[&str]) -> Output {
    run_latch_in(fixture, &fixture.root, args)
}

fn run_latch_in(fixture: &Fixture, cwd: &Path, args: &[&str]) -> Output {
    latch_command(fixture)
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run latch")
}

fn run_latch_without_config(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_latch"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run latch")
}

fn run_latch_stdin(fixture: &Fixture, args: &[&str], input: &str) -> Output {
    let mut child = latch_command(fixture)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn latch");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for latch")
}

fn run_with_timeout(command: &mut Command, timeout: std::time::Duration) -> Output {
    let mut child = command.spawn().expect("spawn latch");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("latch did not exit within {timeout:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    Output {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_json(output: &Output) -> Value {
    let stdout = stdout_text(output);
    serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object: {error}\nstdout: {stdout}\nstderr: {}",
            stderr_text(output)
        )
    })
}

fn workspace_arg(fixture: &Fixture) -> String {
    fixture
        .workspace
        .canonicalize()
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn run_text_mode_streams_assistant_text() {
    let fixture = fixture(vec![Turn::Text("hello from mock")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &["run", "--prompt", "hi", "--workspace", &workspace],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert_eq!(stdout_text(&output), "hello from mock\n");
    assert_eq!(fixture.mock.hits(), 1);
}

#[test]
fn run_json_emits_one_versioned_object() {
    let usage = json!({
        "prompt_tokens": 11,
        "completion_tokens": 7,
        "prompt_cache_hit_tokens": 3,
        "prompt_cache_miss_tokens": 8,
    });
    let fixture = fixture(vec![Turn::Text("json answer")], Some(usage), "standard");
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "hi there",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let stdout = stdout_text(&output);
    assert_eq!(
        stdout.lines().count(),
        1,
        "exactly one stdout line: {stdout}"
    );
    assert!(
        !stdout.contains("test-secret-key"),
        "secrets must never appear in structured output"
    );
    let value = stdout_json(&output);
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["command"], "run");
    assert_eq!(value["status"], "completed");
    assert_eq!(value["workspace"], workspace);
    assert_eq!(value["session_id"].as_str().unwrap().len(), 36);
    assert_eq!(value["profile"]["provider"], "mock");
    assert_eq!(value["profile"]["model"], "mock-model");
    assert_eq!(value["result"]["text"], "json answer");
    // `status` describes the invocation; `task` is the durable kernel state,
    // which is still in progress because the model never claimed completion.
    assert_eq!(value["task"]["completion"], "in_progress");
    assert_eq!(value["task"]["goal"], "hi there");
    assert_eq!(value["task"]["evidence_count"], 0);
    assert_eq!(value["task"]["validation"]["passed"], 0);
    assert_eq!(value["task"]["validation"]["failed"], 0);
    assert_eq!(value["task"]["validation"]["pending"], 0);
    // Usage is scoped: root only vs. the whole durable session graph.
    assert_eq!(value["usage"]["scope"], "invocation_graph");
    assert_eq!(value["usage"]["root"]["input_tokens"], 11);
    assert_eq!(value["usage"]["root"]["output_tokens"], 7);
    assert_eq!(value["usage"]["root"]["cache_read_tokens"], 3);
    assert_eq!(value["usage"]["root"]["cache_miss_tokens"], 8);
    assert!(value["usage"]["root"]["cache_write_tokens"].is_null());
    assert_eq!(value["usage"]["graph"]["input_tokens"], 11);
    assert_eq!(value["usage"]["graph"]["output_tokens"], 7);
    assert_eq!(value["usage"]["graph"]["cache_read_tokens"], 3);
    assert_eq!(value["usage"]["graph"]["cache_miss_tokens"], 8);
    assert!(value["usage"]["graph"]["cache_write_tokens"].is_null());
    assert!(value["context"]["request_tokens"].as_u64().unwrap() > 0);
    assert!(value["context"]["cache_epoch"].is_u64());
    assert_eq!(value["events"]["scope"], "root");
    assert!(value["events"]["count"].as_u64().unwrap() >= 2);
    assert_eq!(value["agent_graph"]["sessions"], 1);
    assert_eq!(value["agent_graph"]["child_sessions"], 0);
    assert!(value["agent_graph"]["events"].as_u64().unwrap() >= 2);
    assert!(value["error"].is_null());
}

#[test]
fn durable_task_state_is_reported_after_the_model_updates_it() {
    let fixture = fixture(
        vec![
            Turn::ToolCall {
                name: "task_update",
                arguments: json!({"required_validations": ["unit tests pass"]}),
            },
            Turn::ToolCall {
                name: "record_evidence",
                arguments: json!({
                    "claim": "kept a note",
                    "status": "pending",
                    "detail": "not verified yet",
                }),
            },
            Turn::ToolCall {
                name: "complete",
                arguments: json!({"implementation_done": true}),
            },
            Turn::Text("finished"),
        ],
        None,
        "standard",
    );
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "implement the change",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let value = stdout_json(&output);
    assert_eq!(value["status"], "completed");
    // The durable state the kernel actually holds, not the run outcome.
    assert_eq!(value["task"]["completion"], "implemented_not_verified");
    assert_eq!(value["task"]["goal"], "implement the change");
    assert_eq!(value["task"]["evidence_count"], 1);
    assert_eq!(value["task"]["validation"]["passed"], 0);
    assert_eq!(value["task"]["validation"]["failed"], 0);
    assert_eq!(value["task"]["validation"]["pending"], 1);
}

const ROOT_MARKER: &str = "ROOT-ORCHESTRATE-MARKER-7";

#[test]
fn multi_agent_usage_aggregates_root_and_graph() {
    let root_usage = json!({
        "prompt_tokens": 100,
        "completion_tokens": 10,
        "prompt_cache_hit_tokens": 40,
        "prompt_cache_miss_tokens": 60,
    });
    let child_usage = json!({
        "prompt_tokens": 50,
        "completion_tokens": 5,
    });
    let root_requests = Arc::new(AtomicUsize::new(0));
    let mock = {
        let root_requests = Arc::clone(&root_requests);
        MockProvider::start_with_responder(std::time::Duration::ZERO, move |_index, body| {
            if body.contains(ROOT_MARKER) {
                let request = root_requests.fetch_add(1, Ordering::SeqCst);
                let turn = match request {
                    0 => Turn::ToolCall {
                        name: "spawn_agent",
                        arguments: json!({
                            "task_name": "child work",
                            "message": "Do the child work. Reply with CHILD-DONE.",
                        }),
                    },
                    1 => Turn::ToolCall {
                        name: "wait_agents",
                        arguments: json!({"timeout_ms": 30_000}),
                    },
                    _ => Turn::Text("root answer"),
                };
                MockResponse {
                    turn,
                    usage: Some(root_usage.clone()),
                }
            } else {
                assert!(body.contains("CHILD-DONE"), "unexpected request: {body}");
                MockResponse {
                    turn: Turn::Text("child answer"),
                    usage: Some(child_usage.clone()),
                }
            }
        })
    };
    let fixture = fixture_with_mock(mock, "standard", "");
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            ROOT_MARKER,
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));

    let requests = fixture.mock.requests();
    let root_hits = requests
        .iter()
        .filter(|body| body.contains(ROOT_MARKER))
        .count() as u64;
    let child_hits = requests.len() as u64 - root_hits;
    assert!(root_hits >= 3, "spawn, wait, and final: {root_hits}");
    assert_eq!(child_hits, 1, "the child makes exactly one request");

    let value = stdout_json(&output);
    // Every root response reports the same usage, so totals are exact.
    assert_eq!(value["usage"]["root"]["input_tokens"], root_hits * 100);
    assert_eq!(value["usage"]["root"]["output_tokens"], root_hits * 10);
    assert_eq!(value["usage"]["root"]["cache_read_tokens"], root_hits * 40);
    assert_eq!(value["usage"]["root"]["cache_miss_tokens"], root_hits * 60);
    assert!(value["usage"]["root"]["cache_write_tokens"].is_null());
    assert_eq!(
        value["usage"]["graph"]["input_tokens"],
        root_hits * 100 + child_hits * 50
    );
    assert_eq!(
        value["usage"]["graph"]["output_tokens"],
        root_hits * 10 + child_hits * 5
    );
    // The child never reported cache categories: they stay the root's known
    // values instead of being fabricated as zero.
    assert_eq!(value["usage"]["graph"]["cache_read_tokens"], root_hits * 40);
    assert_eq!(value["usage"]["graph"]["cache_miss_tokens"], root_hits * 60);
    assert!(value["usage"]["graph"]["cache_write_tokens"].is_null());
    assert_eq!(value["usage"]["scope"], "invocation_graph");

    assert_eq!(value["events"]["scope"], "root");
    assert_eq!(value["agent_graph"]["sessions"], 2);
    assert_eq!(value["agent_graph"]["child_sessions"], 1);
    assert!(
        value["agent_graph"]["events"].as_u64().unwrap()
            > value["events"]["count"].as_u64().unwrap(),
        "the graph summary covers child events the root range does not"
    );
}

#[test]
fn resume_usage_is_scoped_to_the_invocation() {
    let usage = json!({
        "prompt_tokens": 100,
        "completion_tokens": 10,
        "prompt_cache_hit_tokens": 40,
        "prompt_cache_miss_tokens": 60,
    });
    let fixture = fixture(vec![Turn::Text("resumed")], Some(usage), "standard");
    let workspace = workspace_arg(&fixture);

    let first = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "first invocation",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(first.status.success(), "stderr: {}", stderr_text(&first));
    let first_value = stdout_json(&first);
    let session = first_value["session_id"].as_str().unwrap().to_owned();
    assert_eq!(first_value["usage"]["scope"], "invocation_graph");
    assert_eq!(first_value["usage"]["root"]["input_tokens"], 100);

    let second = run_latch(
        &fixture,
        &[
            "resume",
            "--session",
            &session,
            "--prompt",
            "second invocation",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(second.status.success(), "stderr: {}", stderr_text(&second));
    let second_value = stdout_json(&second);
    assert_eq!(second_value["session_id"], first_value["session_id"]);
    // The second invocation reports only its own usage, never 100 + 100.
    assert_eq!(
        second_value["usage"]["root"]["input_tokens"], 100,
        "{second_value}"
    );
    assert_eq!(second_value["usage"]["graph"]["input_tokens"], 100);
    assert_eq!(second_value["usage"]["root"]["output_tokens"], 10);
    assert_eq!(
        second_value["usage"]["root"]["cache_read_tokens"], 40,
        "cache categories are also invocation-scoped"
    );
}

#[test]
fn resumed_root_excludes_existing_child_history_from_usage() {
    let root_usage = json!({
        "prompt_tokens": 100,
        "completion_tokens": 10,
    });
    let child_usage = json!({
        "prompt_tokens": 50,
        "completion_tokens": 5,
    });
    let root_requests = Arc::new(AtomicUsize::new(0));
    let mock = {
        let root_requests = Arc::clone(&root_requests);
        MockProvider::start_with_responder(std::time::Duration::ZERO, move |_index, body| {
            if body.contains(ROOT_MARKER) {
                let request = root_requests.fetch_add(1, Ordering::SeqCst);
                let turn = match request {
                    0 => Turn::ToolCall {
                        name: "spawn_agent",
                        arguments: json!({
                            "task_name": "child work",
                            "message": "Do the child work. Reply with CHILD-DONE.",
                        }),
                    },
                    1 => Turn::ToolCall {
                        name: "wait_agents",
                        arguments: json!({"timeout_ms": 30_000}),
                    },
                    _ => Turn::Text("root answer"),
                };
                MockResponse {
                    turn,
                    usage: Some(root_usage.clone()),
                }
            } else {
                assert!(body.contains("CHILD-DONE"), "unexpected request: {body}");
                MockResponse {
                    turn: Turn::Text("child answer"),
                    usage: Some(child_usage.clone()),
                }
            }
        })
    };
    let fixture = fixture_with_mock(mock, "standard", "");
    let workspace = workspace_arg(&fixture);

    let first = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            ROOT_MARKER,
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(first.status.success(), "stderr: {}", stderr_text(&first));
    let first_value = stdout_json(&first);
    let session = first_value["session_id"].as_str().unwrap().to_owned();
    assert_eq!(first_value["usage"]["root"]["input_tokens"], 300);
    assert_eq!(first_value["usage"]["graph"]["input_tokens"], 350);

    // The resumed invocation spawns nothing: the existing child's first-run
    // usage must not leak into this invocation's totals.
    let second = run_latch(
        &fixture,
        &[
            "resume",
            "--session",
            &session,
            "--prompt",
            ROOT_MARKER,
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(second.status.success(), "stderr: {}", stderr_text(&second));
    let second_value = stdout_json(&second);
    assert_eq!(second_value["usage"]["scope"], "invocation_graph");
    assert_eq!(
        second_value["usage"]["root"]["input_tokens"], 100,
        "{second_value}"
    );
    assert_eq!(
        second_value["usage"]["graph"]["input_tokens"], 100,
        "the existing child's prior usage must be excluded"
    );
    assert_eq!(second_value["agent_graph"]["child_sessions"], 1);
}

#[test]
fn jsonl_is_valid_on_every_line_and_ends_with_final() {
    let fixture = fixture(
        vec![
            Turn::ToolCall {
                name: "write",
                arguments: json!({"path": "note.txt", "content": "note"}),
            },
            Turn::Text("done"),
        ],
        None,
        "standard",
    );
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "write a note",
            "--workspace",
            &workspace,
            "--output",
            "jsonl",
        ],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let stdout = stdout_text(&output);
    assert!(!stdout.is_empty(), "jsonl must stream records");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid JSONL line {line:?}: {error}"))
        })
        .collect();
    assert_eq!(lines.last().unwrap()["type"], "final");
    assert!(
        lines
            .iter()
            .any(|line| line["type"] == "text_delta" && line["text"] == "done")
    );
    assert!(
        lines
            .iter()
            .any(|line| line["type"] == "durable_event" && line["event_type"].is_string())
    );
    let tool = lines
        .iter()
        .find(|line| line["type"] == "tool_result")
        .expect("tool_result record");
    assert_eq!(tool["name"], "write");
    assert_eq!(tool["status"], "ok");
    let final_record = lines.last().unwrap();
    assert_eq!(final_record["result"]["status"], "completed");
    assert_eq!(final_record["result"]["result"]["text"], "done");
    assert!(fixture.workspace.join("note.txt").exists());
    assert_eq!(fixture.mock.hits(), 2);
}

#[test]
fn prompt_source_conflicts_are_rejected() {
    for args in [
        vec!["run", "--prompt", "a", "--stdin"],
        vec!["run"],
        vec!["run", "--prompt-file", "x.txt", "--stdin"],
        vec![
            "resume",
            "--session",
            "deadbeef",
            "--latest",
            "--prompt",
            "a",
        ],
        vec!["resume", "--latest"],
    ] {
        let output = run_latch_without_config(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "args {args:?} must be a usage error\nstderr: {}",
            stderr_text(&output)
        );
        assert!(
            stdout_text(&output).is_empty(),
            "usage errors must not write to stdout"
        );
    }
}

#[test]
fn workspace_selection_controls_session_and_listing() {
    let fixture = fixture(vec![Turn::Text("workspace answer")], None, "standard");
    let workspace = workspace_arg(&fixture);
    // The process runs from the fixture root, not the workspace.
    let output = run_latch_in(
        &fixture,
        &fixture.root,
        &[
            "run",
            "--prompt",
            "hi",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert_eq!(stdout_json(&output)["workspace"], workspace);

    let listing = run_latch(
        &fixture,
        &[
            "sessions",
            "list",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(
        listing.status.success(),
        "stderr: {}",
        stderr_text(&listing)
    );
    let list = stdout_json(&listing);
    assert_eq!(list["schema_version"], 1);
    let sessions = list["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["workspace"], workspace);
    assert_eq!(sessions[0]["mode"], "WORK");
    assert_eq!(sessions[0]["provider"], "mock");
    assert_eq!(sessions[0]["model"], "mock-model");
    assert_eq!(sessions[0]["prompt_preview"], "hi");
    assert!(sessions[0]["event_count"].as_u64().unwrap() > 0);
    assert!(sessions[0]["updated_at"].is_string());

    let other = fixture.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("other workspace");
    let listing = run_latch(
        &fixture,
        &[
            "sessions",
            "list",
            "--workspace",
            other.to_str().unwrap(),
            "--output",
            "json",
        ],
    );
    assert_eq!(listing.status.code(), Some(0));
    assert_eq!(
        stdout_json(&listing)["sessions"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn sessions_show_exposes_a_compact_semantic_summary() {
    let fixture = fixture(vec![Turn::Text("shown")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let run = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "summarize me",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    let session_id = stdout_json(&run)["session_id"].as_str().unwrap().to_owned();
    let prefix = &session_id[..8];
    let output = run_latch(&fixture, &["sessions", "show", prefix, "--output", "json"]);
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let show = stdout_json(&output);
    assert_eq!(show["schema_version"], 1);
    assert_eq!(show["session"]["id"], session_id);
    assert_eq!(show["session"]["provider"], "mock");
    assert_eq!(show["session"]["goal"], "summarize me");
    assert!(
        !show["session"]["recent"].as_array().unwrap().is_empty(),
        "show includes a transcript preview"
    );
    // Text output works without a TTY too.
    let text = run_latch(&fixture, &["sessions", "show", prefix]);
    assert!(text.status.success(), "stderr: {}", stderr_text(&text));
    assert!(stdout_text(&text).contains(&session_id));
}

#[test]
fn resume_reuses_the_session_by_prefix_exact_id_and_latest() {
    let fixture = fixture(vec![Turn::Text("resumed")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let first = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "first",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(first.status.success(), "stderr: {}", stderr_text(&first));
    let session_id = stdout_json(&first)["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    for selector in [&session_id[..8], &session_id] {
        let output = run_latch(
            &fixture,
            &[
                "resume",
                "--session",
                selector,
                "--prompt",
                "again",
                "--workspace",
                &workspace,
                "--output",
                "json",
            ],
        );
        assert!(output.status.success(), "stderr: {}", stderr_text(&output));
        assert_eq!(stdout_json(&output)["session_id"], session_id);
        assert_eq!(stdout_json(&output)["command"], "resume");
    }

    let latest = run_latch(
        &fixture,
        &[
            "resume",
            "--latest",
            "--prompt",
            "once more",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(latest.status.success(), "stderr: {}", stderr_text(&latest));
    assert_eq!(stdout_json(&latest)["session_id"], session_id);
    assert_eq!(fixture.mock.hits(), 4);
}

#[test]
fn prompt_can_be_read_from_stdin() {
    let fixture = fixture(vec![Turn::Text("stdin answer")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let output = run_latch_stdin(
        &fixture,
        &[
            "run",
            "--stdin",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
        "task from stdin\n",
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let listing = run_latch(
        &fixture,
        &[
            "sessions",
            "list",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert_eq!(
        stdout_json(&listing)["sessions"][0]["prompt_preview"],
        "task from stdin"
    );
}

#[test]
fn unresolved_permission_is_denied_and_never_approved() {
    let fixture = fixture(
        vec![
            Turn::ToolCall {
                name: "write",
                arguments: json!({"path": "denied.txt", "content": "x"}),
            },
            Turn::Text("blocked"),
        ],
        None,
        "strict",
    );
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "write a file",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "unresolved permission must exit non-zero\nstderr: {}",
        stderr_text(&output)
    );
    let value = stdout_json(&output);
    assert_eq!(value["status"], "permission_denied");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("permission denied"), "{message}");
    assert!(
        !fixture.workspace.join("denied.txt").exists(),
        "the denied write must not execute"
    );
}

#[test]
fn diagnostics_stay_on_stderr_in_json_mode() {
    let fixture = fixture(vec![Turn::Text("quiet")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let first = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "first",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert!(first.status.success(), "stderr: {}", stderr_text(&first));
    let session_id = stdout_json(&first)["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let output = latch_command(&fixture)
        .env("RUST_LOG", "info")
        .current_dir(&fixture.root)
        .args([
            "resume",
            "--session",
            &session_id,
            "--prompt",
            "again",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ])
        .output()
        .expect("run latch");
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert!(
        !stderr_text(&output).is_empty(),
        "resume logs to stderr when RUST_LOG is set"
    );
    let stdout = stdout_text(&output);
    assert_eq!(stdout.lines().count(), 1);
    let value: Value = serde_json::from_str(&stdout).expect("stdout stays pure JSON");
    assert_eq!(value["status"], "completed");
}

#[test]
fn legacy_prompt_flag_uses_the_shared_run_path() {
    let fixture = fixture(vec![Turn::Text("legacy answer")], None, "standard");
    let output = run_latch_in(&fixture, &fixture.workspace, &["-p", "legacy hi"]);
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    assert_eq!(stdout_text(&output), "legacy answer\n");

    let output = run_latch_in(
        &fixture,
        &fixture.workspace,
        &["-p", "legacy hi", "--output", "json"],
    );
    assert!(output.status.success(), "stderr: {}", stderr_text(&output));
    let value = stdout_json(&output);
    assert_eq!(value["command"], "run");
    assert_eq!(value["result"]["text"], "legacy answer");
    assert_eq!(value["workspace"], workspace_arg(&fixture));
}

#[test]
fn legacy_resume_flags_reuse_the_session() {
    let fixture = fixture(vec![Turn::Text("legacy resumed")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let first = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "first",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    let session_id = stdout_json(&first)["session_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let by_session = run_latch_in(
        &fixture,
        &fixture.workspace,
        &[
            "--resume",
            "--session",
            &session_id,
            "-p",
            "again",
            "--output",
            "json",
        ],
    );
    assert!(
        by_session.status.success(),
        "stderr: {}",
        stderr_text(&by_session)
    );
    assert_eq!(stdout_json(&by_session)["session_id"], session_id);

    let by_latest = run_latch_in(
        &fixture,
        &fixture.workspace,
        &["--resume", "--latest", "-p", "again", "--output", "json"],
    );
    assert!(
        by_latest.status.success(),
        "stderr: {}",
        stderr_text(&by_latest)
    );
    assert_eq!(stdout_json(&by_latest)["session_id"], session_id);
}

#[test]
fn session_prefix_ambiguity_and_missing_selector_fail_closed() {
    let fixture = fixture(vec![Turn::Text("x")], None, "standard");
    let output = run_latch(
        &fixture,
        &["resume", "--session", "deadbeef", "--prompt", "hi"],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_text(&output)
    );

    let output = run_latch(
        &fixture,
        &["sessions", "show", "deadbeef", "--output", "json"],
    );
    assert_eq!(output.status.code(), Some(2));
    let value = stdout_json(&output);
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("no session matches")
    );
}

#[test]
fn legacy_flags_cannot_mix_with_machine_subcommands() {
    for args in [
        vec!["-p", "x", "run", "--prompt", "y"],
        vec!["--resume", "sessions", "list"],
        vec!["--resume", "--session", "deadbeef", "run", "--prompt", "y"],
    ] {
        let output = run_latch_without_config(&args);
        assert_eq!(output.status.code(), Some(2), "args {args:?}");
        assert!(stderr_text(&output).contains("interactive path"));
    }
}

#[test]
fn sigterm_ends_a_run_with_an_orderly_cancelled_result() {
    // A 30s provider delay keeps the run in flight until the signal lands.
    let fixture = fixture_delayed(
        vec![Turn::Text("never returned")],
        None,
        "standard",
        std::time::Duration::from_secs(30),
    );
    let workspace = workspace_arg(&fixture);
    let child = latch_command(&fixture)
        .current_dir(&fixture.root)
        .args([
            "run",
            "--prompt",
            "hi",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ])
        .spawn()
        .expect("spawn latch");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fixture.mock.hits() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the mock provider never received a request"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let signal = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(signal.success(), "kill -TERM failed");

    let output = child.wait_with_output().expect("wait for latch");
    assert_eq!(
        output.status.code(),
        Some(4),
        "SIGTERM must end in the documented cancelled exit\nstderr: {}",
        stderr_text(&output)
    );
    let value = stdout_json(&output);
    assert_eq!(value["status"], "cancelled");
    assert!(value["session_id"].is_string());
    assert!(value["error"]["message"].is_string());
}

#[test]
fn bare_invocation_routes_to_the_interactive_path() {
    // With piped stdio the TUI cannot start, but a bare `latch` must reach the
    // interactive path instead of being treated as a usage or subcommand
    // error.
    let fixture = fixture(vec![Turn::Text("unused")], None, "standard");
    let mut command = latch_command(&fixture);
    command.current_dir(&fixture.workspace);
    let output = run_with_timeout(&mut command, std::time::Duration::from_secs(10));
    assert_ne!(
        output.status.code(),
        Some(2),
        "bare `latch` is not a usage error\nstderr: {}",
        stderr_text(&output)
    );
    assert!(!stderr_text(&output).contains("Usage: latch"));
    assert!(!stdout_text(&output).contains("schema_version"));
}

#[test]
fn sessions_reject_jsonl_output() {
    let fixture = fixture(vec![Turn::Text("x")], None, "standard");
    let output = run_latch(&fixture, &["sessions", "list", "--output", "jsonl"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_text(&output).contains("text or --output json"));
}

#[test]
fn malformed_config_is_a_configuration_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "this is not = = toml").expect("write config");
    let output = Command::new(env!("CARGO_BIN_EXE_latch"))
        .arg("--config")
        .arg(&config_path)
        .args(["run", "--prompt", "hi", "--output", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run latch");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_text(&output)
    );
    let value = stdout_json(&output);
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["status"], "configuration_error");
    assert!(value["task"].is_null());
    assert!(value["error"]["message"].is_string());
}

#[test]
fn unknown_provider_selection_is_a_configuration_error() {
    let fixture = fixture(vec![Turn::Text("unused")], None, "standard");
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "hi",
            "--provider",
            "missing",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_text(&output)
    );
    let value = stdout_json(&output);
    assert_eq!(value["status"], "configuration_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
}

#[test]
fn extension_initialization_failure_is_a_runtime_failure() {
    let mock =
        MockProvider::start_with_delay(vec![Turn::Text("unused")], None, std::time::Duration::ZERO);
    let fixture = fixture_with_mock(
        mock,
        "standard",
        "\n[[extensions]]\nname = \"broken\"\ncommand = \"/nonexistent/latch-extension\"\nargs = []\n",
    );
    let workspace = workspace_arg(&fixture);
    let output = run_latch(
        &fixture,
        &[
            "run",
            "--prompt",
            "hi",
            "--workspace",
            &workspace,
            "--output",
            "json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "runtime initialization failure must exit 1, not 2\nstderr: {}",
        stderr_text(&output)
    );
    let value = stdout_json(&output);
    assert_eq!(value["status"], "failed");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("initialize extension broken"), "{message}");
    assert_eq!(
        fixture.mock.hits(),
        0,
        "initialization fails before any model request"
    );
}
