//! Shared session/provider/agent construction.
//!
//! The interactive TUI, the legacy `-p` one-shot path, and the machine
//! commands all construct sessions here. There is exactly one `Agent`
//! construction path: profile precedence, policy, continuity, and extension
//! loading are identical everywhere.

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use latch_kernel::{
    Agent, AgentRuntime, ArtifactMediaStore, Config, ContinuityEngine, CredentialStore, EventStore,
    ModelDescriptor, ModelProvider, PolicyEngine, ProviderRegistry, ToolExecutor,
    config::ProviderKind,
    paths::ResolvedPaths,
    provider::{MediaStore, StreamSink},
    session,
};
use latch_protocol::{EventPayload, InferenceProfile, MediaRef, Mode, ProviderId, ReasoningEffort};
use latch_tui::configuration_center::{ProviderStatus, ProviderSummary};
use latch_tui::{CatalogModel, CatalogProvider, InferenceCatalog, SetupKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// CLI profile/config overrides applied on top of the durable session profile
/// and the configured defaults. Precedence is unchanged: explicit override >
/// durable session profile > `[inference]` config > built-in default.
#[derive(Debug, Clone, Default)]
pub struct ProfileOverrides {
    pub mode: Option<Mode>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub config_path: Option<PathBuf>,
}

/// Why constructing a session failed, classified for the machine CLI:
///
/// - [`Self::Configuration`]: invalid CLI/config/provider/model/credential
///   input that no runtime setup can fix (exit 2).
/// - [`Self::Runtime`]: the configuration resolved, but durable storage,
///   tools, extensions, or the agent graph failed to initialize or restore
///   (exit 1).
///
/// The interactive TUI does not distinguish them; it collapses both into one
/// `anyhow` error through the [`From`] conversion.
#[derive(Debug)]
pub enum SessionBuildError {
    Configuration(anyhow::Error),
    Runtime(anyhow::Error),
}

impl std::fmt::Display for SessionBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(error) | Self::Runtime(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for SessionBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Configuration(error) | Self::Runtime(error) => error.source(),
        }
    }
}

fn configuration<T>(result: anyhow::Result<T>) -> Result<T, SessionBuildError> {
    result.map_err(SessionBuildError::Configuration)
}

fn runtime<T>(result: anyhow::Result<T>) -> Result<T, SessionBuildError> {
    result.map_err(SessionBuildError::Runtime)
}

/// Everything needed to resolve and switch inference profiles at runtime.
pub struct InferenceContext {
    pub registry: ProviderRegistry,
    pub credentials: CredentialStore,
    pub context: latch_kernel::config::ContextConfig,
    pub config: Config,
    pub config_path: Option<PathBuf>,
}

impl InferenceContext {
    pub fn setup_providers(&self) -> Vec<ProviderSummary> {
        self.registry
            .available_providers()
            .into_iter()
            .map(|profile| {
                let model_count = self.registry.available_models(profile.id.as_str()).len();
                let credential_ready = self
                    .credentials
                    .resolve(&profile.credential)
                    .ok()
                    .flatten()
                    .is_some();
                let default = self
                    .registry
                    .model_descriptor(profile.id.as_str(), &profile.default_model);
                let unresolved = profile.default_model.is_empty()
                    || default.as_ref().is_none_or(|descriptor| !descriptor.known);
                let status = if !credential_ready {
                    ProviderStatus::MissingCredential
                } else if unresolved {
                    ProviderStatus::UnresolvedModels
                } else {
                    ProviderStatus::Ready
                };
                ProviderSummary {
                    id: profile.id.as_str().to_owned(),
                    display_name: profile.display_name.clone(),
                    kind: profile.kind.id().to_owned(),
                    status,
                    model_count,
                    default_model: profile.default_model.clone(),
                    credential_ref: profile.credential.display(),
                }
            })
            .collect()
    }
    pub fn new(config: Config, config_path: Option<PathBuf>) -> Result<Self> {
        let registry = ProviderRegistry::from_config(&config)?;
        let credentials = CredentialStore::open(CredentialStore::default_path(&config.state_dir))?;
        let context = config.context.clone();
        Ok(Self {
            registry,
            credentials,
            context,
            config,
            config_path,
        })
    }

    pub fn resolve(
        &self,
        requested: &InferenceProfile,
    ) -> Result<(InferenceProfile, ModelDescriptor)> {
        self.registry.resolve_profile(requested)
    }

    pub fn build(
        &self,
        profile: &InferenceProfile,
        descriptor: &ModelDescriptor,
        session_id: Uuid,
    ) -> Result<Arc<dyn ModelProvider>> {
        let media: MediaStore = Arc::new(ArtifactMediaStore::new(artifact_root(
            &self.config,
            session_id,
        )));
        self.registry.build_provider(
            profile,
            descriptor,
            &self.credentials,
            session_id,
            Some(media),
        )
    }

    /// Snapshot factory used to rebuild a child session pinned to a profile
    /// the root no longer runs. On `/setup` the registry is rebuilt, so the
    /// agent receives a fresh factory.
    pub fn provider_factory(&self) -> latch_kernel::ProviderFactory {
        let registry = Arc::new(self.registry.clone());
        let credentials = Arc::new(self.credentials.clone());
        let state_dir = self.config.state_dir.clone();
        Arc::new(move |profile: &InferenceProfile, session_id: Uuid| {
            let (resolved, descriptor) = registry.resolve_profile(profile)?;
            let root = ResolvedPaths::for_state(&state_dir)
                .artifacts_root
                .join(session_id.to_string());
            let media: MediaStore = Arc::new(ArtifactMediaStore::new(root));
            let provider = registry.build_provider(
                &resolved,
                &descriptor,
                &credentials,
                session_id,
                Some(media),
            )?;
            Ok(latch_kernel::ProviderBuild {
                provider,
                descriptor,
            })
        })
    }

    pub fn provider_label(&self, id: &str) -> String {
        self.registry
            .provider(id)
            .map(|profile| profile.display_name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// Provider-neutral catalog for the live `/model` selector.
    pub fn catalog(&self) -> InferenceCatalog {
        InferenceCatalog {
            providers: self
                .registry
                .available_providers()
                .into_iter()
                .map(|provider| CatalogProvider {
                    id: provider.id.to_string(),
                    display_name: provider.display_name.clone(),
                    default_model: provider.default_model.clone(),
                    models: provider
                        .available_models()
                        .into_iter()
                        .map(catalog_model)
                        .collect(),
                })
                .collect(),
        }
    }

    /// Provider kinds and built-in models available to `/setup`.
    pub fn setup_catalog(&self) -> Vec<SetupKind> {
        [
            ProviderKind::OpenCodeGo,
            ProviderKind::OpenCodeZen,
            ProviderKind::DeepSeek,
            ProviderKind::OpenAi,
            ProviderKind::Anthropic,
            ProviderKind::OpenAiCompatible,
        ]
        .into_iter()
        .map(|kind| SetupKind {
            kind: kind.id().to_owned(),
            label: kind.display_name().to_owned(),
            default_base_url: kind.default_base_url().to_owned(),
            requires_base_url: kind.default_base_url().is_empty(),
            credential_label: kind.default_credential().to_owned(),
            default_model: latch_kernel::providers::builtin_catalog(kind)
                .first()
                .map(|descriptor| descriptor.model.clone())
                .unwrap_or_default(),
            models: latch_kernel::providers::builtin_catalog(kind)
                .iter()
                .map(catalog_model_ref)
                .collect(),
        })
        .collect()
    }
}

fn catalog_model(descriptor: &ModelDescriptor) -> CatalogModel {
    catalog_model_ref(descriptor)
}

fn catalog_model_ref(descriptor: &ModelDescriptor) -> CatalogModel {
    CatalogModel {
        id: descriptor.model.clone(),
        display_name: descriptor.display_name.clone(),
        efforts: descriptor.supported_efforts.clone(),
        default_effort: descriptor.default_effort,
        input_modalities: descriptor.input_modalities.clone(),
    }
}

/// One resolved session plus everything the interactive layer needs.
pub struct SessionInfo {
    pub profile: InferenceProfile,
    pub descriptor: ModelDescriptor,
    pub needs_setup: bool,
}

pub struct BuiltSession {
    pub agent: Agent,
    pub info: SessionInfo,
    pub context: InferenceContext,
    pub restored: Option<Restored>,
}

/// What the caller asked for when selecting a session.
#[derive(Debug, Clone, Default)]
pub struct SessionRequest {
    pub resume: bool,
    pub session: Option<String>,
    pub latest: bool,
    /// True when no interactive picker is possible (machine mode, a one-shot
    /// prompt, or a non-TTY invocation): ambiguity is an error instead of a
    /// prompt.
    pub non_interactive: bool,
}

/// A resolved session selection.
pub enum SelectedSession {
    Session(Uuid, PathBuf),
    Fresh,
    Exit,
}

pub async fn select_session(
    workspace: &Path,
    config: &Config,
    request: &SessionRequest,
) -> Result<SelectedSession> {
    if !request.resume {
        return Ok(SelectedSession::Fresh);
    }
    let store = EventStore::open(&ResolvedPaths::for_state(&config.state_dir).database_path)?;
    if let Some(selector) = &request.session {
        let selected = store.resolve_session(selector)?;
        if Path::new(&selected.workspace) != workspace {
            eprintln!(
                "resuming session {} from workspace {} (current workspace is {})",
                &selected.id.to_string()[..8],
                selected.workspace,
                workspace.display()
            );
        }
        return Ok(SelectedSession::Session(
            selected.id,
            selected.workspace.into(),
        ));
    }
    if request.latest {
        return store
            .latest_session(Some(workspace))?
            .map(|id| SelectedSession::Session(id, workspace.to_path_buf()))
            .ok_or_else(|| {
                anyhow!(
                    "no previous session for {}; remove --latest to start fresh",
                    workspace.display()
                )
            });
    }
    let matching = store.list_sessions(Some(workspace))?;
    match matching.as_slice() {
        [] => Err(anyhow!("no previous session for {}", workspace.display())),
        [session] => Ok(SelectedSession::Session(
            session.id,
            session.workspace.clone().into(),
        )),
        _ if request.non_interactive => {
            bail!(
                "{} sessions match {}; choose one with --session <uuid-or-prefix> or --latest",
                matching.len(),
                workspace.display()
            )
        }
        _ => pick_session(workspace, config).await,
    }
}

/// Opens the interactive newest-first session picker. The TUI `/resume`
/// command always uses it, regardless of how many sessions match.
pub async fn pick_session(workspace: &Path, config: &Config) -> Result<SelectedSession> {
    let store = EventStore::open(&ResolvedPaths::for_state(&config.state_dir).database_path)?;
    let sessions = store
        .list_sessions(None)?
        .into_iter()
        .map(|session| latch_tui::SessionItem {
            id: session.id,
            workspace: session.workspace,
            updated_at: session.updated_at,
            mode: session
                .mode
                .map_or_else(|| config.default_mode.to_string(), |mode| mode.to_string()),
            model: match (&session.model, session.effort) {
                (Some(model), Some(effort))
                    if !matches!(effort, ReasoningEffort::ProviderDefault) =>
                {
                    format!("{model} · {}", effort.short())
                }
                (Some(model), _) => model.clone(),
                (None, _) => "—".into(),
            },
            prompt: session
                .prompt_preview
                .unwrap_or_else(|| "No user prompt".into()),
            event_count: session.event_count,
        })
        .collect();
    let preview_store = store.clone();
    let preview = Arc::new(move |id| {
        preview_store
            .session_preview(id, 6)
            .unwrap_or_default()
            .into_iter()
            .map(|line| latch_tui::SessionPreviewLine {
                speaker: line.speaker.into(),
                text: line.text,
            })
            .collect()
    });
    Ok(
        match latch_tui::run_session_picker(sessions, workspace, preview).await? {
            latch_tui::PickerSelection::Resume(id) => {
                let selected = store.resolve_session(&id.to_string())?;
                SelectedSession::Session(id, selected.workspace.into())
            }
            latch_tui::PickerSelection::StartFresh => SelectedSession::Fresh,
            latch_tui::PickerSelection::Exit | latch_tui::PickerSelection::Cancel => {
                SelectedSession::Exit
            }
        },
    )
}

/// One restored session for the TUI: visible transcript items and prompt
/// history, both derived from durable events by the shared formatter.
pub struct Restored {
    pub events: Vec<latch_protocol::Event>,
    pub history: Vec<String>,
}

pub async fn build_agent(
    workspace: &Path,
    config: &Config,
    overrides: &ProfileOverrides,
    resume_session: Option<Uuid>,
    interactive: bool,
    cancel: &CancellationToken,
) -> Result<BuiltSession, SessionBuildError> {
    let db = ResolvedPaths::for_state(&config.state_dir).database_path;
    let store = runtime(EventStore::open(&db))?;
    let mut restored = None;
    let resume = resume_session.is_some();
    let session_id = if let Some(session) = resume_session {
        // Approval requests that were pending at exit can no longer be
        // answered; mark them durably before the transcript replay so resume
        // shows honest state instead of a phantom prompt.
        let expired = runtime(Agent::expire_pending_permissions(&store, session))?;
        if expired > 0 {
            tracing::info!("expired {expired} unresolved permission request(s)");
        }
        let events = runtime(store.events(session))?;
        runtime(store.append(session, EventPayload::SessionResumed))?;
        for (id, description) in runtime(store.interrupted_operations(session))? {
            runtime(store.append(
                session,
                EventPayload::OperationInterrupted {
                    operation_id: id,
                    description,
                },
            ))?;
            runtime(store.mark_operation_reported(id))?;
        }
        restored = Some(Restored {
            events: events.clone(),
            history: session::prompt_history(&events),
        });
        session
    } else {
        let session = runtime(store.create_session(workspace))?;
        let (head, dirty_paths) = observe_git(workspace);
        runtime(store.append(
            session,
            EventPayload::GitStateObserved { head, dirty_paths },
        ))?;
        session
    };
    let events = runtime(store.events(session_id))?;
    // Mode precedence (both fresh and resumed): explicit CLI --mode > the
    // session's durable mode history > configured default.
    let mode = session::resumed_mode(&events, overrides.mode, config.default_mode);

    // Inference profile precedence: explicit CLI override > the session's own
    // durable profile > configured default. Credentials are resolved fresh
    // from the environment/local store at this moment and are never persisted.
    let context = configuration(InferenceContext::new(
        config.clone(),
        overrides.config_path.clone(),
    ))?;
    let (default_profile, _default_descriptor) =
        configuration(context.registry.default_profile(config))?;
    let resumed_profile = resume
        .then(|| session::resumed_inference_profile(&events))
        .flatten();
    let mut requested = resumed_profile.clone().unwrap_or_default();
    if !resume || resumed_profile.is_none() {
        requested = default_profile.clone();
    }
    let mut overridden = false;
    if let Some(provider) = &overrides.provider {
        requested.provider = ProviderId::new(provider.clone());
        requested.model.clear();
        requested.effort = ReasoningEffort::ProviderDefault;
        overridden = true;
    }
    if let Some(model) = &overrides.model {
        requested.model = model.clone();
        overridden = true;
    }
    if let Some(effort) = &overrides.effort {
        requested.effort = configuration(
            effort
                .parse::<ReasoningEffort>()
                .map_err(anyhow::Error::msg),
        )?;
        overridden = true;
    }
    let (profile, descriptor) = configuration(context.resolve(&requested))?;
    let (provider, needs_setup) = match context.build(&profile, &descriptor, session_id) {
        Ok(provider) => (provider, false),
        Err(error) if interactive => {
            // First-run UX: rather than failing before the TUI can offer
            // /setup, start the session with an actionable stub provider.
            tracing::warn!("{error:#}");
            (
                Arc::new(UnconfiguredProvider {
                    message: format!("{error:#}"),
                }) as Arc<dyn ModelProvider>,
                true,
            )
        }
        Err(error) => return Err(SessionBuildError::Configuration(error)),
    };
    let policy = PolicyEngine::with_defaults(
        mode,
        workspace.to_path_buf(),
        config.permissions.clone(),
        config.safety.level,
    );
    let artifacts = artifact_root(config, session_id);
    let tools = runtime(ToolExecutor::new_with_state_dir(
        workspace.to_path_buf(),
        artifacts.clone(),
        config.state_dir.clone(),
        store.clone(),
        session_id,
        policy,
    ))?;
    if resume {
        // Restore durable change ownership before anything can mutate.
        let count = runtime(tools.restore_ownership().await)?;
        tracing::info!("restored {count} owned change records");
    }
    let continuity =
        ContinuityEngine::for_model(store.clone(), config.context.clone(), &profile.model);
    let mut agent = Agent::new(AgentRuntime {
        session_id,
        workspace: workspace.to_path_buf(),
        mode,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity,
        retry_budget: config.failure.retry_budget,
    });
    agent.set_stagnation_budget(config.failure.stagnation_budget);
    agent.set_max_model_turns(config.failure.max_model_turns);
    agent.set_extension_lifecycle((&config.extension_lifecycle).into());
    agent.set_provider_factory(context.provider_factory());
    if overridden {
        // A command-line override is durable provenance so a later resume
        // keeps the profile the user actually selected.
        runtime(agent.set_inference_profile(
            provider,
            profile.clone(),
            &descriptor,
            config.context.clone(),
            "command line override",
        ))?;
    } else {
        agent.restore_inference_profile(
            provider,
            profile.clone(),
            &descriptor,
            config.context.clone(),
        );
    }
    for extension in config
        .extensions
        .iter()
        .filter(|extension| extension.enabled)
    {
        runtime(
            agent
                .load_extension(
                    extension.name.clone(),
                    &extension.command,
                    &extension.args,
                    cancel,
                )
                .await
                .with_context(|| format!("initialize extension {}", extension.name)),
        )?;
    }
    if resume {
        // Resume restores the exact policy the session ended in; it never
        // silently broadens or narrows permissions.
        agent.restore_policy(
            session::resumed_safety(&events, config.safety.level),
            session::resumed_permissions(&events, config.permissions.mode),
        );
        if let Some(state) = events.iter().rev().find_map(|e| match &e.payload {
            EventPayload::TaskStateUpdated { state } => Some(state.clone()),
            _ => None,
        }) {
            agent.restore_state(state);
        }
        runtime(
            agent.restore_evidence(
                events
                    .iter()
                    .filter_map(|event| match &event.payload {
                        EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
        )?;
        // Failure supervision reconstructs its streaks so a stalled loop is
        // not silently forgotten, and progress supervision reconstructs an
        // active inspection loop.
        runtime(agent.restore_failures())?;
        runtime(agent.restore_progress())?;
    }
    Ok(BuiltSession {
        agent,
        info: SessionInfo {
            profile,
            descriptor,
            needs_setup,
        },
        context,
        restored,
    })
}

/// Stand-in provider used when no credential is configured and Latch is
/// interactive: the session starts so `/setup` can run, and any attempt to
/// use the model fails with the actionable configuration error.
struct UnconfiguredProvider {
    message: String,
}

#[async_trait]
impl ModelProvider for UnconfiguredProvider {
    fn name(&self) -> &str {
        "unconfigured"
    }
    fn model(&self) -> &str {
        "unconfigured"
    }
    async fn stream(
        &self,
        _request: latch_protocol::ModelRequest,
        _cancel: CancellationToken,
        _sink: StreamSink,
    ) -> Result<latch_protocol::ModelResponse> {
        bail!("{}", self.message)
    }
}

/// One session's immutable artifact store, shared by tool ingestion and the
/// provider media resolver.
pub fn artifact_root(config: &Config, session_id: Uuid) -> PathBuf {
    ResolvedPaths::for_state(&config.state_dir)
        .artifacts_root
        .join(session_id.to_string())
}

/// Validates and ingests CLI `--attach` images through exactly the same
/// kernel path the TUI uses. Identical bytes deduplicate by content hash.
pub fn ingest_attachments(
    config: &Config,
    session_id: Uuid,
    paths: &[PathBuf],
) -> Result<Vec<MediaRef>> {
    let root = artifact_root(config, session_id);
    let mut media = Vec::new();
    for path in paths {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
        latch_kernel::media::ensure_size(metadata.len())
            .with_context(|| format!("attach {}", path.display()))?;
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let reference = latch_kernel::ingest_image_bytes(&root, &bytes, Some(name))
            .with_context(|| format!("attach {}", path.display()))?;
        media.push(reference);
    }
    Ok(media)
}

/// Resolves an explicit `--workspace` to an absolute, canonical directory.
pub fn resolve_workspace(path: &Path) -> Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .with_context(|| format!("resolve workspace {}", path.display()))?;
    if !resolved.is_dir() {
        bail!("workspace {} is not a directory", resolved.display());
    }
    Ok(resolved)
}

fn observe_git(workspace: &Path) -> (Option<String>, Vec<String>) {
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned());
    let dirty_paths = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.get(3..).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (head, dirty_paths)
}
