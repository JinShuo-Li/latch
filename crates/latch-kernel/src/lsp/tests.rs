use super::*;
use crate::{
    config::PermissionConfig,
    store::EventStore,
    tools::{PolicyEngine, ToolExecutor},
};
use latch_protocol::{EventPayload, Mode, Safety, ToolCall};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[test]
fn configuration_is_optional_bounded_and_unambiguous() {
    let old: crate::config::Config = toml::from_str("").unwrap();
    assert!(old.lsp_servers.is_empty());
    let config = config("normal");
    config.validate().unwrap();
    for text in [
        "command = ''",
        "timeout_seconds = 0",
        "extensions = ['../rs']",
        "language_id = 'bad/name'",
    ] {
        let partial: toml::Value = toml::from_str(text).unwrap();
        let mut value = toml::Value::try_from(&config).unwrap();
        for (k, v) in partial.as_table().unwrap() {
            value[k] = v.clone();
        }
        let invalid: LspServerConfig = value.try_into().unwrap();
        assert!(invalid.validate().is_err(), "{text}");
    }
    assert!(validate_configs(&[config.clone(), config.clone()]).is_err());
    let mut other = config.clone();
    other.name = "other".into();
    assert!(validate_configs(&[config, other]).is_err());
    assert!(!ToolExecutor::definitions().iter().any(|t| t.name == "lsp"));
}

#[tokio::test]
async fn framing_rejects_oversize_duplicate_missing_and_truncated_frames() {
    for bytes in [
        b"Content-Length: 99999999\r\n\r\n".as_slice(),
        b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
        b"Content-Type: x\r\n\r\n",
        b"Content-Length: 40\r\n\r\n{}",
    ] {
        assert!(
            protocol::read_frame(&mut BufReader::new(bytes))
                .await
                .is_err()
        );
    }
    let header = vec![b'x'; 9000];
    assert!(
        protocol::read_frame(&mut BufReader::new(header.as_slice()))
            .await
            .is_err()
    );
    let (mut tx, rx) = tokio::io::duplex(64);
    let writer = tokio::spawn(async move {
        for byte in b"Content-Length: 38\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}" {
            tx.write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let result = protocol::read_frame(&mut BufReader::new(rx)).await.unwrap();
    assert!(result["result"].is_null());
    writer.await.unwrap();
}
fn config(mode: &str) -> LspServerConfig {
    LspServerConfig {
        name: "fixture".into(),
        command: if cfg!(windows) { "python" } else { "python3" }.into(),
        args: vec!["fixture.py".into(), mode.into()],
        extensions: vec!["rs".into()],
        language_id: "rust".into(),
        enabled: true,
        timeout_seconds: 2,
        initialization_options: Value::Null,
        settings: Value::Null,
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    tools: ToolExecutor,
    store: EventStore,
    session: Uuid,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            workspace.join("fixture.py"),
            include_str!("../../tests/fixtures/lsp_server.py"),
        )
        .unwrap();
        std::fs::write(workspace.join("a.rs"), "foo").unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let store = EventStore::open(&state.join("events.sqlite3")).unwrap();
        let session = store.create_session(&workspace).unwrap();
        let policy = PolicyEngine::with_defaults(
            Mode::Work,
            workspace.clone(),
            PermissionConfig::default(),
            Safety::Standard,
        );
        let tools = ToolExecutor::new_with_state_dir(
            workspace.clone(),
            state.join("artifacts"),
            state.clone(),
            store.clone(),
            session,
            policy,
        )
        .unwrap()
        .with_lsp_servers(vec![config(mode)])
        .unwrap();
        Self {
            _dir: dir,
            workspace,
            tools,
            store,
            session,
        }
    }
    async fn query(&self, args: Value) -> Value {
        query(&self.tools, args).await
    }
}
async fn query(tools: &ToolExecutor, args: Value) -> Value {
    let call = ToolCall {
        id: Uuid::new_v4().to_string(),
        name: "lsp".into(),
        arguments: args,
    };
    let result = tools.execute(&call, CancellationToken::new()).await;
    assert!(!result.is_error, "{}: {}", call.arguments, result.output);
    assert!(result.output.len() <= OUTPUT_BYTES);
    serde_json::from_str(&result.output).unwrap()
}
fn instance(value: &Value) -> &str {
    value["text"].as_str().unwrap().split(';').next().unwrap()
}

#[tokio::test]
async fn sandboxed_navigation_filters_paths_denies_server_edits_and_syncs_external_changes() {
    let fixture = Fixture::new("normal");
    assert_eq!(fixture.tools.language_definitions().len(), 1);
    let tokens = crate::tokens::TokenEstimator::generic()
        .estimate_tools(&fixture.tools.language_definitions());
    assert!(tokens < 400, "optional LSP schema grew to {tokens} tokens");
    let first = fixture
        .query(json!({"action":"hover","path":"a.rs","line":1,"symbol":"foo"}))
        .await;
    assert!(
        first["text"]
            .as_str()
            .unwrap()
            .contains("write_denied=True")
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("a.rs")).unwrap(),
        "foo"
    );
    assert!(!fixture.workspace.join("forbidden").exists());
    for action in ["definition", "references"] {
        let value = fixture
            .query(json!({"action":action,"path":"a.rs","line":1,"symbol":"foo"}))
            .await;
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
        assert_eq!(value["omitted_unavailable_or_external"], 1);
        assert_eq!(value["items"][0]["range"]["start"]["line"], 1);
        assert_eq!(value["advisory"], true);
    }
    assert_eq!(
        fixture
            .query(json!({"action":"symbols","path":"a.rs"}))
            .await["items"][0]["name"],
        "fixture"
    );
    for _ in 0..2 {
        let diag = fixture
            .query(json!({"action":"diagnostics","path":"a.rs"}))
            .await;
        assert_eq!(diag["items"][0]["message"], "foo");
    }
    std::fs::write(fixture.workspace.join("a.rs"), "bar").unwrap();
    let second = fixture
        .query(json!({"action":"hover","path":"a.rs","line":1,"symbol":"bar"}))
        .await;
    if !cfg!(windows) {
        assert_eq!(instance(&first), instance(&second));
    }
    assert!(second["text"].as_str().unwrap().ends_with("bar"));
    assert_eq!(
        fixture
            .query(json!({"action":"diagnostics","path":"a.rs"}))
            .await["items"][0]["message"],
        "bar"
    );
    fixture.tools.shutdown_lsp().await;
}
#[tokio::test]
async fn durable_mutations_restart_indexes_children_and_resume_are_independent() {
    let fixture = Fixture::new("normal");
    let args = json!({"action":"hover","path":"a.rs","line":1,"symbol":"foo"});
    let first = fixture.query(args.clone()).await;
    let other = fixture.store.create_session(&fixture.workspace).unwrap();
    fixture
        .store
        .append(
            other,
            EventPayload::WorkspaceMutationPossible {
                operation: "another session wrote a dependency".into(),
            },
        )
        .unwrap();
    let second = fixture.query(args.clone()).await;
    assert_ne!(instance(&first), instance(&second));
    let child = fixture.tools.for_child(other).unwrap();
    let third = query(&child, args.clone()).await;
    assert_ne!(instance(&second), instance(&third));
    child.shutdown_lsp().await;
    fixture.query(json!({"action":"stop"})).await;
    let restarted = fixture.query(args).await;
    assert_ne!(instance(&second), instance(&restarted));
    let events = fixture.store.events(fixture.session).unwrap();
    assert!(
        events.iter().any(
            |e| matches!(&e.payload,EventPayload::ToolCompleted{result} if result.name=="lsp")
        )
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(&e.payload, EventPayload::WorkspaceMutationPossible { .. }))
    );
    fixture.tools.shutdown_lsp().await;
}
#[tokio::test]
async fn pull_diagnostics_and_unavailable_servers_are_explicit() {
    let fixture = Fixture::new("pull");
    let value = fixture
        .query(json!({"action":"diagnostics","path":"a.rs"}))
        .await;
    assert_eq!(value["diagnostic_source"], "pull");
    assert_eq!(value["items"][0]["message"], "foo");
    fixture.tools.shutdown_lsp().await;
    let unavailable = fixture.tools.clone().with_lsp_servers(vec![]).unwrap();
    let status = query(&unavailable, json!({"action":"status"})).await;
    assert_eq!(status["configured"], false);
    let result = unavailable
        .execute(
            &ToolCall {
                id: "missing".into(),
                name: "lsp".into(),
                arguments: json!({"action":"symbols","path":"a.rs"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_error && result.output.contains("search/read_file"));
    let mut missing_config = config("normal");
    missing_config.command = "/latch-no-such-language-server".into();
    let missing = fixture
        .tools
        .clone()
        .with_lsp_servers(vec![missing_config])
        .unwrap();
    let result = missing
        .execute(
            &ToolCall {
                id: "missing-binary".into(),
                name: "lsp".into(),
                arguments: json!({"action":"symbols","path":"a.rs"}),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(
        result.is_error
            && result.output.contains("installed server")
            && result.output.contains("search/read_file"),
        "{}",
        result.output
    );
}
#[tokio::test]
#[ignore = "optional local qualification: requires clangd and a C++ toolchain"]
async fn real_clangd_resolves_overloads_and_reports_diagnostics() {
    let fixture = Fixture::new("normal");
    std::fs::write(fixture.workspace.join("main.cpp"), "namespace billing {\nint quote(int n) { return n * 10; }\ndouble quote(double n) { return n * 1.5; }\n}\nint main() {\n  auto cents = billing::quote(3);\n  auto fractional = billing::quote(2.0);\n  return cents == 30 && fractional == 3.0 ? 0 : 1;\n}\n").unwrap();
    let tools = fixture
        .tools
        .clone()
        .with_lsp_servers(vec![LspServerConfig {
            name: "clangd".into(),
            command: "clangd".into(),
            args: vec![
                "--background-index=false".into(),
                "--clang-tidy=false".into(),
                "--enable-config=false".into(),
            ],
            extensions: vec!["cpp".into()],
            language_id: "cpp".into(),
            timeout_seconds: 30,
            ..config("normal")
        }])
        .unwrap();
    let definition = query(
        &tools,
        json!({"action":"definition","path":"main.cpp","line":6,"symbol":"quote"}),
    )
    .await;
    assert_eq!(definition["items"][0]["range"]["start"]["line"], 2);
    let references = query(
        &tools,
        json!({"action":"references","path":"main.cpp","line":2,"symbol":"quote"}),
    )
    .await;
    let lines = references["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["range"]["start"]["line"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(lines.contains(&2) && lines.contains(&6), "{references}");
    assert!(!lines.contains(&3) && !lines.contains(&7), "{references}");
    assert!(
        !query(
            &tools,
            json!({"action":"hover","path":"main.cpp","line":6,"symbol":"quote"})
        )
        .await["text"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let diagnostics = query(&tools, json!({"action":"diagnostics","path":"main.cpp"})).await;
    assert_eq!(diagnostics["total"], 0);
    std::fs::write(
        fixture.workspace.join("main.cpp"),
        "int main() { return missing_name; }\n",
    )
    .unwrap();
    let diagnostics = query(&tools, json!({"action":"diagnostics","path":"main.cpp"})).await;
    assert!(diagnostics["total"].as_u64().unwrap() > 0, "{diagnostics}");
    tools.shutdown_lsp().await;
}

#[tokio::test]
async fn cancellation_deadlines_bad_protocol_and_paths_fail_closed() {
    for mode in ["stall-init", "stall-query", "bad-frame"] {
        let fixture = Fixture::new(mode);
        let cancel = CancellationToken::new();
        if mode == "stall-query" {
            let token = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                token.cancel();
            });
        }
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fixture.tools.execute(
                &ToolCall {
                    id: "bounded".into(),
                    name: "lsp".into(),
                    arguments: json!({"action":"symbols","path":"a.rs"}),
                },
                cancel,
            ),
        )
        .await
        .unwrap();
        assert!(result.is_error, "{mode}");
        assert!(
            result.output.contains("deadline")
                || result.output.contains("cancelled")
                || result.output.contains("frame"),
            "{}",
            result.output
        );
        assert!(fixture.tools.lsp_context().contains("fixture"));
        fixture.tools.shutdown_lsp().await;
    }
    let fixture = Fixture::new("normal");
    for path in ["../state/events.sqlite3", "/etc/passwd"] {
        let result = fixture
            .tools
            .execute(
                &ToolCall {
                    id: Uuid::new_v4().to_string(),
                    name: "lsp".into(),
                    arguments: json!({"action":"symbols","path":path}),
                },
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
    }
    for mode in [Mode::Ask, Mode::Plan, Mode::Work] {
        fixture.tools.set_mode(mode);
        fixture.tools.set_safety(Safety::Strict);
        assert!(matches!(
            fixture
                .tools
                .classify_call("lsp", &json!({"action":"hover"}))
                .decision,
            crate::safety::Decision::Ask(_)
        ));
        assert!(matches!(
            fixture
                .tools
                .classify_call("lsp", &json!({"action":"status"}))
                .decision,
            crate::safety::Decision::Allow
        ));
    }
}
