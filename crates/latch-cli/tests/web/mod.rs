//! Linux and Windows Web transport tests run inside cli.rs to reuse its isolated provider.
use super::*;
use reqwest::{Client, StatusCode};
use std::process::Child;
use std::time::{Duration, Instant};

const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

struct Web {
    child: Child,
    url: String,
    cookie: String,
    client: Client,
}
impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Web {
    async fn start(f: &Fixture) -> Self {
        Self::start_with_args(f, &[]).await
    }
    async fn start_with_args(f: &Fixture, extra: &[&str]) -> Self {
        Self::start_command(f, extra, latch_command(f)).await
    }
    async fn start_command(f: &Fixture, extra: &[&str], mut command: Command) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut child = command
            .current_dir(&f.workspace)
            .env_remove("LATCH_WEB_TEST_MISSING_KEY")
            .args(["--web", "--ssh", &port.to_string()])
            .args(extra)
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(line.starts_with("Latch Web:"), "{line}");
        let token = line.trim().split("#token=").nth(1).unwrap();
        let url = format!("http://127.0.0.1:{port}");
        let client = Client::builder().no_proxy().build().unwrap();
        let response = client
            .post(format!("{url}/api/auth"))
            .header("Origin", &url)
            .json(&json!({"token":token}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            child,
            url,
            cookie,
            client,
        }
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.url))
            .header("Cookie", &self.cookie)
            .send()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.url))
            .header("Cookie", &self.cookie)
            .header("Origin", &self.url)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn snapshot(&self) -> Value {
        self.get("/api/bootstrap").await.json().await.unwrap()
    }
    async fn ready(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = self.snapshot().await;
            if snapshot["state"]["busy"] == false && snapshot["state"]["starting"] == false {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "session failed to become ready: {snapshot}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    fn command(snapshot: &Value, input: Value) -> Value {
        json!({"command_id":uuid::Uuid::new_v4(),"instance_id":snapshot["instance_id"],"issued_at":snapshot["server_time"],"input":input})
    }
}

#[tokio::test]
async fn auth_forwarding_assets_and_command_replay() {
    let f = fixture(vec![Turn::Text("Web response")], None, "standard");
    let foreign = run_latch(&f, &["-p", "Other workspace", "--output", "json"]);
    assert!(foreign.status.success());
    let foreign_id = stdout_json(&foreign)["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let web = Web::start(&f).await;
    let snapshot = web.ready().await;
    let id = snapshot["state"]["session_id"].as_str().unwrap();
    assert_eq!(
        web.post(&format!("/api/sessions/{foreign_id}/activate"), json!({}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        snapshot["workspace"],
        f.workspace
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert!(!snapshot["commands"].as_array().unwrap().is_empty());
    let response = web
        .client
        .get(format!("{}/api/bootstrap", web.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    for file in [
        "/",
        "/app.js",
        "/state.js",
        "/view.js",
        "/markdown.js",
        "/transport.js",
        "/settings.js",
        "/icons.js",
        "/styles.css",
    ] {
        let response = web.get(file).await;
        assert_eq!(response.status(), StatusCode::OK, "{file}");
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    assert_eq!(
        web.get("/config.toml").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        web.client
            .get(format!("{}/api/bootstrap", web.url))
            .header("Host", "evil.test")
            .header("Cookie", &web.cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        web.client
            .post(format!("{}/api/sessions", web.url))
            .header("Cookie", &web.cookie)
            .header("Origin", "https://evil.test")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    // SSH forwards the request's original authority, not the server port.
    let forwarded = web
        .client
        .post(format!("{}/api/auth", web.url))
        .header("Host", "localhost:7000")
        .header("Origin", "http://localhost:7000")
        .json(&json!({"token":"invalid"}))
        .send()
        .await
        .unwrap();
    assert_eq!(forwarded.status(), StatusCode::UNAUTHORIZED);
    let path = format!("/api/sessions/{id}/commands");
    let command = Web::command(
        &snapshot,
        json!({"type":"submit","data":{"text":"Hello","media":[]}}),
    );
    assert_eq!(
        web.post(&path, command.clone()).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        web.post(&path, command.clone()).await.status(),
        StatusCode::OK
    );
    let done = web.ready().await;
    assert!(done["state"]["cells"].to_string().contains("Web response"));
    assert_eq!(f.mock.hits(), 2);
    let mut changed = command.clone();
    changed["input"]["data"]["text"] = json!("Different");
    assert_eq!(
        web.post(&path, changed).await.status(),
        StatusCode::CONFLICT
    );
    let mut expired = command;
    expired["issued_at"] = json!(0);
    assert_eq!(
        web.post(&path, expired).await.status(),
        StatusCode::CONFLICT
    );
    let mut events = web.get("/api/events").await;
    let chunk = events.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("event: snapshot"));
    drop(events);
    let new = web
        .post("/api/sessions", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_ne!(new["session_id"], id);
    web.ready().await;
    assert_eq!(
        web.post(&format!("/api/sessions/{id}/activate"), json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    let restored = web.ready().await;
    assert!(
        restored["state"]["cells"]
            .to_string()
            .contains("Web response")
    );
    assert_eq!(web.post(&path,Web::command(&restored,json!({"type":"set_inference_profile","data":{"provider":"missing","model":"bad","effort":"provider_default"}}))).await.status(),StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        web.ready().await["state"]["cells"]
            .to_string()
            .contains("error:")
    );
}

#[tokio::test]
async fn cancel_busy_switch_and_pending_approval() {
    let f = fixture_delayed(
        vec![
            Turn::ToolCall {
                name: "shell",
                arguments: json!({"command":"touch approved.txt"}),
            },
            Turn::Text("Finished"),
        ],
        None,
        "strict",
        Duration::from_millis(100),
    );
    let web = Web::start(&f).await;
    let snapshot = web.ready().await;
    let id = snapshot["state"]["session_id"].as_str().unwrap();
    let path = format!("/api/sessions/{id}/commands");
    assert_eq!(
        web.post(
            &path,
            Web::command(
                &snapshot,
                json!({"type":"submit","data":{"text":"Create file","media":[]}})
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        web.post("/api/sessions", json!({})).await.status(),
        StatusCode::CONFLICT
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    let pending = loop {
        let s = web.snapshot().await;
        if !s["state"]["pending_permissions"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break s;
        }
        assert!(Instant::now() < deadline, "no approval: {s}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let request = &pending["state"]["pending_permissions"][0];
    assert_eq!(request["tool"], "shell");
    assert!(!request["reason"].as_str().unwrap().is_empty());
    let command = Web::command(
        &pending,
        json!({"type":"permission","data":{"request_id":request["request_id"],"approved":false}}),
    );
    assert_eq!(
        web.post(&path, command.clone()).await.status(),
        StatusCode::OK
    );
    assert_eq!(web.post(&path, command).await.status(), StatusCode::OK);
    let done = web.ready().await;
    assert!(
        done["state"]["pending_permissions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!f.workspace.join("approved.txt").exists());
    assert_eq!(
        web.post(
            &path,
            Web::command(
                &done,
                json!({"type":"submit","data":{"text":"Again","media":[]}})
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    let active = web.snapshot().await;
    assert_eq!(
        web.post(&path, Web::command(&active, json!({"type":"cancel"})))
            .await
            .status(),
        StatusCode::OK
    );
    web.ready().await;
}

#[tokio::test]
async fn image_upload_reconnect_detach_and_session_boundary() {
    let f = fixture_with_mock(
        MockProvider::start_with_delay(vec![Turn::Text("Image received")], None, Duration::ZERO),
        "standard",
        "[providers.mock.models.mock-model]\ninput_modalities = [\"text\", \"image\"]\ntransport = \"chat_completions\"\n",
    );
    let web = Web::start(&f).await;
    let snapshot = web.ready().await;
    let id = snapshot["state"]["session_id"].as_str().unwrap();
    let path = format!("/api/sessions/{id}/attachments");
    let upload = |bytes: Vec<u8>| {
        web.client
            .post(format!("{}{path}", web.url))
            .header("Origin", &web.url)
            .header("Cookie", &web.cookie)
            .header("X-Image-Name", "sample.png")
            .body(bytes)
            .send()
    };
    assert_eq!(
        upload(b"invalid image".to_vec()).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(vec![0; 600 * 1024]).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        upload(vec![0; 5 * 1024 * 1024 + 1]).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let response = upload(TINY_PNG.to_vec()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let reference: Value = response.json().await.unwrap();
    assert_eq!(
        web.snapshot().await["state"]["pending_attachments"][0],
        reference
    );
    let artifact = reference["id"].as_str().unwrap();
    assert_eq!(
        web.get(&format!("/api/sessions/{id}/media/{artifact}"))
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        TINY_PNG
    );
    let detached = web
        .client
        .delete(format!("{}{path}/{artifact}", web.url))
        .header("Origin", &web.url)
        .header("Cookie", &web.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(detached.status(), StatusCode::OK);
    assert!(
        web.snapshot().await["state"]["pending_attachments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    upload(TINY_PNG.to_vec()).await.unwrap();
    let command = Web::command(
        &snapshot,
        json!({"type":"submit","data":{"text":"Review image","media":[reference]}}),
    );
    assert_eq!(
        web.post(&format!("/api/sessions/{id}/commands"), command)
            .await
            .status(),
        StatusCode::OK
    );
    let done = web.ready().await;
    assert!(
        done["state"]["pending_attachments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(done["state"]["cells"][0]["User"]["media"][0], reference);
    let request: Value = serde_json::from_str(&f.mock.requests()[0]).unwrap();
    assert!(request.to_string().contains("data:image/png;base64,"));
    let new = web
        .post("/api/sessions", json!({}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    web.ready().await;
    assert_eq!(
        web.get(&format!("/api/sessions/{id}/media/{artifact}"))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let current = web.snapshot().await;
    assert_eq!(
        web.post(
            &format!(
                "/api/sessions/{}/commands",
                new["session_id"].as_str().unwrap()
            ),
            Web::command(
                &current,
                json!({"type":"submit","data":{"text":"Foreign image","media":[reference]}})
            )
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn steering_survives_reconnect_and_cancel_releases_session() {
    let f = fixture_delayed(
        vec![Turn::Text("Steered response")],
        None,
        "standard",
        Duration::from_millis(300),
    );
    let mut web = Web::start(&f).await;
    let snapshot = web.ready().await;
    let id = snapshot["state"]["session_id"].as_str().unwrap();
    let path = format!("/api/sessions/{id}/commands");
    for text in ["Initial request", "Additional instruction"] {
        assert_eq!(
            web.post(
                &path,
                Web::command(
                    &snapshot,
                    json!({"type":"submit","data":{"text":text,"media":[]}})
                )
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    // Closing/reopening observation must leave execution and queued input intact.
    let mut events = web.get("/api/events").await;
    events.chunk().await.unwrap();
    drop(events);
    let done = web.ready().await;
    let history = done["state"]["history"].as_array().unwrap();
    assert!(history.contains(&json!("Initial request")));
    assert!(history.contains(&json!("Additional instruction")));
    assert_eq!(
        web.post(&path, Web::command(&done, json!({"type":"quit"})))
            .await
            .status(),
        StatusCode::OK
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if web.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "Web server did not exit");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn web_launch_flags_are_unambiguous() {
    for args in [
        vec!["--ssh", "6006"],
        vec!["--web", "--ssh", "0"],
        vec!["--web", "--ssh", "6006", "--web-port", "6007"],
        vec!["--web", "-p", "message"],
        vec!["--web", "run", "message"],
    ] {
        let output = run_latch_without_config(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

// Unix home lookup can be isolated with HOME; Windows known-folder APIs cannot.
#[cfg(not(windows))]
#[tokio::test]
async fn missing_default_config_requires_setup_and_saved_config_is_reused() {
    let f = fixture(vec![Turn::Text("unused")], None, "standard");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latch"));
        command
            .env("HOME", &f.root)
            .env("USERPROFILE", &f.root)
            .env("XDG_CONFIG_HOME", f.root.join("xdg-config"))
            .env("XDG_STATE_HOME", f.root.join("xdg-state"))
            // An ambient key must not bypass first-run setup with OpenAI defaults.
            .env("OPENAI_API_KEY", "ambient-test-key")
            .env("MOCK_API_KEY", "test-secret-key")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };
    let web = Web::start_command(&f, &[], command()).await;
    let initial = web.ready().await;
    assert_eq!(initial["state"]["metadata"]["setup_required"], true);
    let paths = &initial["state"]["metadata"]["setup_paths"];
    assert_eq!(paths["config_exists"], false);
    let config_path = f.root.join(".latch/config.toml");
    assert_eq!(
        Path::new(paths["config_path"].as_str().unwrap()),
        config_path
    );
    let id = initial["state"]["session_id"].as_str().unwrap();
    let plan = json!({"Apply":{"name":"real-provider","provider_kind":"openai-compatible","base_url":f.mock.base_url,"credential":{"Env":"MOCK_API_KEY"},"model":"mock-model","enabled_models":["mock-model"],"custom_model_display_name":"Mock","custom_transport":"chat_completions","effort":"provider_default"}});
    assert_eq!(
        web.post(
            &format!("/api/sessions/{id}/commands"),
            Web::command(&initial, json!({"type":"setup_apply","data":plan}))
        )
        .await
        .status(),
        StatusCode::OK
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = web.snapshot().await;
        if snapshot["state"]["metadata"]["header"]["provider_id"] == "real-provider" {
            assert_eq!(
                snapshot["state"]["metadata"]["setup_paths"]["config_exists"],
                true
            );
            break;
        }
        assert!(Instant::now() < deadline, "setup failed: {snapshot}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(config_path.is_file());
    let saved = latch_kernel::Config::load(Some(&config_path)).unwrap();
    assert_eq!(saved.providers.len(), 1);
    assert!(saved.providers.contains_key("real-provider"));
    drop(web);
    let restarted = Web::start_command(&f, &[], command()).await;
    let snapshot = restarted.ready().await;
    assert!(
        snapshot["state"]["metadata"]
            .get("setup_required")
            .is_none()
    );
    assert_eq!(
        snapshot["state"]["metadata"]["header"]["provider_id"],
        "real-provider"
    );
    assert!(
        f.mock.requests().is_empty(),
        "setup must not call a provider"
    );
}

#[tokio::test]
async fn session_list_remains_read_only_while_a_writer_holds_the_database() {
    let f = fixture(vec![Turn::Text("unused")], None, "standard");
    let web = Web::start(&f).await;
    let initial = web.ready().await;
    let database = f.root.join("state/latch.sqlite3");
    let writer = rusqlite::Connection::open(database).unwrap();
    // WAL readers must keep working while another connection owns a write
    // transaction. Reopening EventStore here would rebuild group projections
    // and try to acquire the same writer lock.
    writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
    for _ in 0..3 {
        let response = tokio::time::timeout(Duration::from_secs(2), web.get("/api/sessions"))
            .await
            .expect("listing sessions tried to acquire the writer lock");
        assert_eq!(response.status(), StatusCode::OK);
        let listed: Value = response.json().await.unwrap();
        assert!(
            listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|session| { session["id"] == initial["state"]["session_id"] })
        );
    }
    writer.execute_batch("ROLLBACK;").unwrap();
    assert!(f.mock.requests().is_empty());
}

#[tokio::test]
async fn provider_setup_unlocks_a_missing_credential_session() {
    let f = fixture(vec![Turn::Text("Configured response")], None, "standard");
    let config = std::fs::read_to_string(&f.config_path)
        .unwrap()
        .replace("env:MOCK_API_KEY", "env:LATCH_WEB_TEST_MISSING_KEY");
    std::fs::write(&f.config_path, config).unwrap();
    let web = Web::start(&f).await;
    let initial = web.ready().await;
    assert_eq!(initial["state"]["metadata"]["setup_required"], true);
    let id = initial["state"]["session_id"].as_str().unwrap();
    let path = format!("/api/sessions/{id}/commands");
    let provider = initial["state"]["metadata"]["header"]["provider_id"]
        .as_str()
        .unwrap();
    let plan = json!({"SetCredential":{"name":provider,"credential":{"Env":"MOCK_API_KEY"}}});
    assert_eq!(
        web.post(
            &path,
            Web::command(&initial, json!({"type":"setup_apply","data":plan}))
        )
        .await
        .status(),
        StatusCode::OK
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    let configured = loop {
        let s = web.snapshot().await;
        if s["state"]["metadata"].get("setup_required").is_none() {
            break s;
        }
        assert!(Instant::now() < deadline, "setup failed: {s}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        configured["state"]["metadata"]
            .get("setup_required")
            .is_none()
    );
    assert!(!configured.to_string().contains("test-secret-key"));
    assert_eq!(
        web.post(
            &path,
            Web::command(
                &configured,
                json!({"type":"submit","data":{"text":"Configured","media":[]}})
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(
        web.ready().await["state"]["cells"]
            .to_string()
            .contains("Configured response")
    );
}

#[tokio::test]
async fn browser_resume_picker_retains_launch_attachments_once() {
    let f = fixture(vec![Turn::Text("Prior conversation")], None, "standard");
    let previous = run_latch_in(&f, &f.workspace, &["-p", "Prior", "--output", "json"]);
    assert!(previous.status.success());
    let id = stdout_json(&previous)["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let image = f.root.join("startup.png");
    std::fs::write(&image, TINY_PNG).unwrap();
    let web = Web::start_with_args(&f, &["--resume", "--attach", image.to_str().unwrap()]).await;
    assert!(web.ready().await["state"]["session_id"].is_null());
    assert_eq!(
        web.post(&format!("/api/sessions/{id}/activate"), json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    let selected = web.ready().await;
    assert_eq!(
        selected["state"]["pending_attachments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        selected["state"]["pending_attachments"][0]["display_name"],
        "startup.png"
    );
    assert_eq!(
        web.post("/api/sessions", json!({})).await.status(),
        StatusCode::OK
    );
    assert!(
        web.ready().await["state"]["pending_attachments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn web_executes_native_directory_enumeration() {
    let command = if cfg!(windows) { "dir /b" } else { "ls" };
    let f = fixture(
        vec![
            Turn::ToolCall {
                name: "shell",
                arguments: json!({"command":command}),
            },
            Turn::Text("Listed workspace"),
        ],
        None,
        "standard",
    );
    std::fs::write(f.workspace.join("enumeration-proof.txt"), "fixture").unwrap();
    let web = Web::start(&f).await;
    let snapshot = web.ready().await;
    let id = snapshot["state"]["session_id"].as_str().unwrap();
    assert_eq!(
        web.post(
            &format!("/api/sessions/{id}/commands"),
            Web::command(
                &snapshot,
                json!({"type":"submit","data":{"text":"List this workspace","media":[]}})
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    let done = web.ready().await;
    let cells = done["state"]["cells"].to_string();
    assert!(cells.contains("enumeration-proof.txt"), "{cells}");
    assert!(cells.contains("Listed workspace"), "{cells}");
    assert!(!cells.contains("error:"), "{cells}");
}

#[tokio::test]
async fn activity_distinguishes_provider_silence_from_reasoning_without_exposing_text() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (reason, receive_reason) = std::sync::mpsc::channel();
    let (finish, receive_finish) = std::sync::mpsc::channel();
    let provider_hits = Arc::clone(&hits);
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request_body(&mut stream).unwrap();
        provider_hits.fetch_add(1, Ordering::SeqCst);
        receive_reason
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"private-thought-fixture\"},\"finish_reason\":null}]}\n\n").unwrap();
        stream.flush().unwrap();
        receive_finish
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        stream.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"Public response\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").unwrap();
    });
    let f = fixture_with_mock(
        MockProvider {
            base_url: format!("http://{addr}/v1"),
            hits,
            requests,
        },
        "standard",
        "",
    );
    let web = Web::start(&f).await;
    let snapshot = web.ready().await;
    let id = snapshot["state"]["session_id"].as_str().unwrap();
    assert_eq!(
        web.post(
            &format!("/api/sessions/{id}/commands"),
            Web::command(
                &snapshot,
                json!({"type":"submit","data":{"text":"Check activity","media":[]}})
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    async fn phase(web: &Web, expected: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = web.snapshot().await;
            if snapshot["state"]["sidebar"]["activity"]["phase"] == expected {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "missing {expected}: {snapshot}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let waiting = phase(&web, "waiting_model").await;
    assert!(waiting["state"]["sidebar"]["activity"]["last_provider_activity_at"].is_null());
    reason.send(()).unwrap();
    let thinking = phase(&web, "thinking").await;
    assert!(!thinking.to_string().contains("private-thought-fixture"));
    assert!(thinking["state"]["sidebar"]["activity"]["last_provider_activity_at"].is_string());
    finish.send(()).unwrap();
    let done = web.ready().await;
    assert_eq!(done["state"]["sidebar"]["activity"]["phase"], "completed");
    assert!(
        done["state"]["cells"]
            .to_string()
            .contains("Public response")
    );
    assert!(!done.to_string().contains("private-thought-fixture"));
}
