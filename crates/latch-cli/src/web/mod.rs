//! Cross-platform loopback Web adapter. Every agent operation uses the shared controller.
mod actor;
mod http;
mod state;

use crate::cli::{
    command::Args,
    session::{ProfileOverrides, artifact_root},
};
use anyhow::{Context, Result};
use latch_kernel::{Config, EventStore, paths::ResolvedPaths};
use serde_json::{Value, json};
use std::{path::PathBuf, process::ExitCode, sync::Arc};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type Reply = Result<Value, ApiError>;

#[derive(Debug)]
pub struct ApiError(pub axum::http::StatusCode, pub String);
impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self(axum::http::StatusCode::BAD_REQUEST, message.into())
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self(axum::http::StatusCode::CONFLICT, message.into())
    }
    pub fn runtime(error: impl std::fmt::Display) -> Self {
        Self(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            error.to_string(),
        )
    }
}
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, axum::Json(json!({"error":self.1}))).into_response()
    }
}

#[derive(Clone)]
pub struct Host(Arc<HostInner>);
struct HostInner {
    token: String,
    cookie: String,
    instance: Uuid,
    workspace: PathBuf,
    config_path: Option<PathBuf>,
    state_dir: PathBuf,
    database: EventStore,
    view: Mutex<state::View>,
    changes: broadcast::Sender<u64>,
    actions: mpsc::Sender<actor::Request>,
    shutdown: CancellationToken,
}
impl Host {
    async fn request(&self, action: actor::Action) -> Reply {
        let (reply, receive) = oneshot::channel();
        self.0
            .actions
            .send(actor::Request { action, reply })
            .await
            .map_err(|_| ApiError::conflict("Web session controller stopped"))?;
        receive
            .await
            .map_err(|_| ApiError::conflict("Web session controller stopped"))?
    }
    fn database(&self) -> &EventStore {
        &self.0.database
    }
    async fn changed(&self) {
        let mut view = self.0.view.lock().await;
        view.sequence += 1;
        let _ = self.0.changes.send(view.sequence);
    }
    async fn snapshot(&self) -> Value {
        json!({"version":env!("CARGO_PKG_VERSION"),"server_time":chrono::Utc::now().timestamp_millis(),"commands":latch_ui::commands::SLASH_COMMANDS.iter().map(|c|json!({"name":c.name,"description":c.description})).collect::<Vec<_>>(),"schema_version":1,"instance_id":self.0.instance,"workspace":self.0.workspace,"state":self.0.view.lock().await.snapshot()})
    }
    fn artifacts(&self, id: Uuid) -> PathBuf {
        let config = Config {
            state_dir: self.0.state_dir.clone(),
            ..Config::default()
        };
        artifact_root(&config, id)
    }
}

pub async fn run(args: Args) -> Result<ExitCode> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let config = Config::load(args.config.as_deref())?;
    let port = args.ssh.or(args.web_port).unwrap_or(6006);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}; choose --web-port <PORT> or --ssh <REMOTE_WEB_PORT>"))?;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let (actions, receive) = mpsc::channel(32);
    let (changes, _) = broadcast::channel(256);
    // Open and migrate once for the host. Read-only HTTP requests must not
    // reopen EventStore, whose startup rebuild writes the group projections.
    let database = EventStore::open(&ResolvedPaths::for_state(&config.state_dir).database_path)?;
    let host = Host(Arc::new(HostInner {
        token: token.clone(),
        cookie: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        instance: Uuid::new_v4(),
        workspace,
        config_path: args.config.clone(),
        state_dir: config.state_dir.clone(),
        database,
        view: Mutex::new(state::View::default()),
        changes,
        actions,
        shutdown: CancellationToken::new(),
    }));
    let selected = if args.resume && (args.session.is_some() || args.latest) {
        let store = host.database();
        let selected = if let Some(selector) = &args.session {
            store.resolve_session(selector)?
        } else {
            store
                .list_sessions(Some(&host.0.workspace))?
                .into_iter()
                .next()
                .context("no previous session in this workspace")?
        };
        anyhow::ensure!(
            std::path::Path::new(&selected.workspace) == host.0.workspace,
            "session belongs to another workspace; start Latch in that directory"
        );
        Some(selected.id)
    } else {
        None
    };
    let overrides = ProfileOverrides {
        mode: args.mode,
        provider: args.provider,
        model: args.model,
        effort: args.effort,
        config_path: args.config,
    };
    let shutdown = host.0.shutdown.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.cancel();
    });
    let actor = tokio::spawn(actor::run(host.clone(), receive, overrides, args.attach));
    // A bare --resume starts with a browser picker. No terminal is required.
    if (!args.resume || selected.is_some())
        && let Err(error) = host
            .request(actor::Action::Activate(selected, Vec::new()))
            .await
    {
        host.0.shutdown.cancel();
        let _ = actor.await;
        signal.abort();
        anyhow::bail!("{}", error.1);
    }
    println!("Latch Web: http://localhost:{port}/#token={token}");
    println!("Workspace: {}", host.0.workspace.display());
    if args.ssh.is_some() {
        println!("On your local machine: ssh -N -L {port}:127.0.0.1:{port} user@remote-host");
        println!(
            "Local port occupied? Use -L 7000:127.0.0.1:{port}, then open http://localhost:7000/#token={token}"
        );
    } else {
        let url = format!("http://localhost:{port}/#token={token}");
        match browser_command(&url)?
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                tokio::spawn(async move {
                    if !child.wait().await.is_ok_and(|status| status.success()) {
                        eprintln!("Open the printed URL in your browser.");
                    }
                });
            }
            Err(_) => eprintln!("Open the printed URL in your browser."),
        }
    }
    let result = axum::serve(listener, http::router(host.clone()))
        .with_graceful_shutdown(host.0.shutdown.clone().cancelled_owned())
        .await;
    host.0.shutdown.cancel();
    signal.abort();
    actor.await??;
    result?;
    Ok(ExitCode::SUCCESS)
}

/// The URL is generated locally; no shell script or workspace command is used.
fn browser_command(url: &str) -> Result<tokio::process::Command> {
    #[cfg(target_os = "windows")]
    {
        let system =
            std::env::var_os("SystemRoot").context("SystemRoot is required to open a browser")?;
        let mut command =
            tokio::process::Command::new(PathBuf::from(system).join("System32/rundll32.exe"));
        command
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .creation_flags(0x08000000);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = tokio::process::Command::new("xdg-open");
        command.arg(url);
        Ok(command)
    }
}
