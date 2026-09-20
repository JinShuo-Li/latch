//! `latch doctor`: a read-only preflight for the runtime prerequisites,
//! configuration, provider profile, and credentials a run needs.
//!
//! It is deliberately narrow. Doctor never contacts the provider, never runs a
//! task, and never prints secret material: a credential is reported only by its
//! symbolic reference (`env:NAME`, `file:NAME`), never by value. It is safe to
//! run in scripts, CI, and a fresh container.
//!
//! Exit codes are stable:
//!
//! - `0`: every required check passed;
//! - `1`: a runtime prerequisite is missing or unusable (`bwrap`, `rg`, or a
//!   writable state directory);
//! - `2`: CLI or configuration error (bad workspace, unreadable config, no
//!   resolvable provider/profile, or a missing credential).

use crate::cli::command::{Args, DoctorArgs, OutputFormat};
use crate::cli::output::{EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE};
use crate::cli::session::resolve_workspace;
use latch_kernel::{Config, CredentialStore, ProviderRegistry};
use latch_protocol::{InferenceProfile, ProviderId, ReasoningEffort};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Doctor's payload has no relationship to the run/resume schema, so it keeps
/// its own independent version.
const DOCTOR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Warning,
    Failed,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warn",
            Self::Failed => "fail",
        }
    }
}

#[derive(Debug, Serialize)]
struct Check {
    id: &'static str,
    status: Status,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Check {
    fn ok(id: &'static str, summary: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Ok,
            summary: summary.into(),
            detail: None,
        }
    }

    fn warning(id: &'static str, summary: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Warning,
            summary: summary.into(),
            detail: None,
        }
    }

    fn failed(id: &'static str, summary: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Failed,
            summary: summary.into(),
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    ok: bool,
    workspace: String,
    config_path: Option<String>,
    state_dir: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
struct DoctorError {
    schema_version: u32,
    error: String,
}

pub fn execute(args: &Args, doctor: DoctorArgs) -> ExitCode {
    let output = args.output;
    if output == OutputFormat::Jsonl {
        let message = "`doctor` supports --output text or --output json";
        report_error(message, output);
        return ExitCode::from(EXIT_USAGE);
    }
    let workspace_input = doctor
        .workspace
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let workspace = match resolve_workspace(&workspace_input) {
        Ok(workspace) => workspace,
        Err(error) => {
            report_error(&format!("{error:#}"), output);
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let mut checks = vec![
        check_sandbox(&workspace),
        check_search(),
        check_git(&workspace),
    ];

    let explicit_config = args.config.clone();
    if let Some(path) = &explicit_config
        && !path.is_file()
    {
        checks.push(Check::failed(
            "config",
            format!("config file {} does not exist", path.display()),
        ));
        emit(
            &config_error_report(&workspace, explicit_config.as_deref(), checks),
            output,
        );
        return ExitCode::from(EXIT_USAGE);
    }

    let config = match Config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            checks.push(Check::failed("config", format!("{error:#}")));
            emit(
                &config_error_report(&workspace, explicit_config.as_deref(), checks),
                output,
            );
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let config_path = explicit_config.or_else(Config::default_path);
    checks.push(config_check(config_path.as_deref()));

    checks.push(check_state_dir(&config.state_dir));

    let mut provider = None;
    let mut model = None;
    let mut effort = None;
    match resolve_requested_profile(&config, args) {
        Ok((profile, descriptor)) => {
            let window = descriptor
                .context_window_tokens
                .unwrap_or(latch_kernel::config::DEFAULT_CONTEXT_WINDOW_TOKENS);
            checks.push(
                Check::ok(
                    "provider",
                    format!(
                        "{} / {} / {}",
                        profile.provider.as_str(),
                        profile.model,
                        profile.effort.label()
                    ),
                )
                .with_detail(format!(
                    "context window {window} tokens ({})",
                    if descriptor.context_window_tokens.is_some() {
                        "declared"
                    } else {
                        "conservative default"
                    }
                )),
            );
            provider = Some(profile.provider.as_str().to_owned());
            model = Some(profile.model.clone());
            effort = Some(profile.effort.label().to_owned());
            checks.push(check_credential(&config, &profile));
        }
        Err(error) => {
            let message = format!("{error:#}");
            checks.push(Check::failed("provider", message.clone()));
            checks.push(Check::failed("credential", "skipped: provider unresolved"));
        }
    }

    let ok = checks.iter().all(|check| check.status != Status::Failed);
    let report = Report {
        schema_version: DOCTOR_SCHEMA_VERSION,
        ok,
        workspace: workspace.display().to_string(),
        config_path: config_path.map(|path| path.display().to_string()),
        state_dir: Some(config.state_dir.display().to_string()),
        provider,
        model,
        effort,
        checks,
    };
    let code = exit_code(&report.checks);
    emit(&report, output);
    ExitCode::from(code)
}

fn check_sandbox(workspace: &Path) -> Check {
    match latch_kernel::sandbox::probe(workspace) {
        Ok(probe) => Check::ok(
            "sandbox",
            format!("{} ({})", probe.bwrap.display(), probe.version),
        ),
        Err(error) => Check::failed("sandbox", format!("{error:#}")),
    }
}

fn check_search() -> Check {
    version_check("ripgrep", "rg", &["--version"]).unwrap_or_else(|error| {
        Check::failed("ripgrep", error.to_string())
            .with_detail("install ripgrep (for example `sudo apt install ripgrep`)")
    })
}

fn check_git(workspace: &Path) -> Check {
    let version = match version_check("git", "git", &["--version"]) {
        Ok(check) => check,
        Err(error) => {
            return Check::warning("git", error.to_string())
                .with_detail("Git-backed tools (git_status, git_diff) will fail");
        }
    };
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "true");
    if inside {
        version
    } else {
        Check::warning("git", "not a Git repository")
            .with_detail("Git-backed tools will fail until the workspace is a repository")
    }
}

/// Runs `<binary> --version` and returns an `ok` check carrying the first line.
/// The error string is the actionable "not found" message for a missing tool.
fn version_check(id: &'static str, binary: &str, args: &[&str]) -> Result<Check, String> {
    match std::process::Command::new(binary).args(args).output() {
        Ok(output) if output.status.success() => {
            let line = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_owned();
            let summary = if line.is_empty() {
                binary.to_owned()
            } else {
                line
            };
            Ok(Check::ok(id, summary))
        }
        Ok(output) => Err(format!(
            "`{binary} --version` failed with {}",
            output.status
        )),
        Err(error) => Err(format!("`{binary}` was not found on PATH ({error})")),
    }
}

fn config_check(path: Option<&Path>) -> Check {
    match path {
        Some(path) if path.is_file() => Check::ok("config", path.display().to_string()),
        Some(path) => Check::ok(
            "config",
            format!("built-in defaults (no file at {})", path.display()),
        ),
        None => Check::ok("config", "built-in defaults"),
    }
}

fn check_state_dir(state_dir: &Path) -> Check {
    if let Err(error) = std::fs::create_dir_all(state_dir) {
        return Check::failed(
            "state",
            format!("cannot create state directory {}", state_dir.display()),
        )
        .with_detail(error.to_string());
    }
    let probe = state_dir.join(".latch-doctor-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::ok("state", state_dir.display().to_string())
        }
        Err(error) => Check::failed(
            "state",
            format!("state directory {} is not writable", state_dir.display()),
        )
        .with_detail(error.to_string()),
    }
}

fn check_credential(config: &Config, profile: &InferenceProfile) -> Check {
    let registry = match ProviderRegistry::from_config(config) {
        Ok(registry) => registry,
        Err(error) => return Check::failed("credential", format!("{error:#}")),
    };
    let Some(reference) = registry
        .provider(profile.provider.as_str())
        .map(|provider| provider.credential.clone())
    else {
        return Check::failed("credential", "provider has no credential reference");
    };
    let store = match CredentialStore::open(CredentialStore::default_path(&config.state_dir)) {
        Ok(store) => store,
        Err(error) => return Check::failed("credential", format!("{error:#}")),
    };
    match store.resolve(&reference) {
        Ok(Some(_)) => Check::ok("credential", reference.display()),
        Ok(None) => {
            let detail = store
                .require(&reference)
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "set the credential and retry".to_owned());
            Check::failed("credential", format!("{} is not set", reference.display()))
                .with_detail(detail)
        }
        Err(error) => Check::failed("credential", format!("{error:#}")),
    }
}

/// Resolves the effective profile with the same precedence a run uses:
/// explicit CLI override > configured default. (Doctor has no durable session,
/// so the session tier does not apply.)
fn resolve_requested_profile(
    config: &Config,
    args: &Args,
) -> anyhow::Result<(InferenceProfile, latch_kernel::ModelDescriptor)> {
    let registry = ProviderRegistry::from_config(config)?;
    let (default_profile, _) = registry.default_profile(config)?;
    let requested = requested_profile(
        default_profile,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.effort.as_deref(),
    )?;
    registry.resolve_profile(&requested)
}

/// Applies the explicit overrides exactly as the session builder does: a
/// provider override also clears the model and resets effort, and an invalid
/// effort is a configuration error rather than a silent fallback.
fn requested_profile(
    mut base: InferenceProfile,
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> anyhow::Result<InferenceProfile> {
    if let Some(provider) = provider {
        base.provider = ProviderId::new(provider.to_owned());
        base.model.clear();
        base.effort = ReasoningEffort::ProviderDefault;
    }
    if let Some(model) = model {
        base.model = model.to_owned();
    }
    if let Some(effort) = effort {
        base.effort = effort
            .parse::<ReasoningEffort>()
            .map_err(anyhow::Error::msg)?;
    }
    Ok(base)
}

/// A configuration failure is a usage error (2); a missing runtime
/// prerequisite is a failure (1).
fn exit_code(checks: &[Check]) -> u8 {
    let configuration_ids = ["config", "provider", "credential"];
    let failed: Vec<&Check> = checks
        .iter()
        .filter(|check| check.status == Status::Failed)
        .collect();
    if failed.is_empty() {
        return EXIT_SUCCESS;
    }
    if failed
        .iter()
        .any(|check| configuration_ids.contains(&check.id))
    {
        return EXIT_USAGE;
    }
    EXIT_FAILURE
}

/// A report for a fatal configuration problem observed before a provider can
/// be resolved; only the checks gathered so far are included.
fn config_error_report(workspace: &Path, config_path: Option<&Path>, checks: Vec<Check>) -> Report {
    Report {
        schema_version: DOCTOR_SCHEMA_VERSION,
        ok: false,
        workspace: workspace.display().to_string(),
        config_path: config_path.map(|path| path.display().to_string()),
        state_dir: None,
        provider: None,
        model: None,
        effort: None,
        checks,
    }
}

fn emit(report: &Report, output: OutputFormat) {
    if output.is_machine() {
        print_json(report);
    } else {
        print_text(report);
    }
}

fn print_text(report: &Report) {
    println!("latch doctor");
    println!("workspace  {}", report.workspace);
    match &report.config_path {
        Some(path) => println!("config     {path}"),
        None => println!("config     built-in defaults"),
    }
    if let Some(state_dir) = &report.state_dir {
        println!("state      {state_dir}");
    }
    if let (Some(provider), Some(model)) = (&report.provider, &report.model) {
        println!(
            "profile    {provider} / {model} / {}",
            report.effort.as_deref().unwrap_or("provider default")
        );
    }
    println!();
    for check in &report.checks {
        println!(
            "{:<5} {:<10} {}",
            check.status.label(),
            check.id,
            check.summary
        );
        if let Some(detail) = &check.detail {
            for line in detail.lines() {
                println!("      {line}");
            }
        }
    }
    let count = |status: Status| report.checks.iter().filter(|c| c.status == status).count();
    println!();
    println!(
        "doctor: {} ok, {} warning, {} failed",
        count(Status::Ok),
        count(Status::Warning),
        count(Status::Failed)
    );
}

fn report_error(message: &str, output: OutputFormat) {
    if output.is_machine() {
        print_json(&DoctorError {
            schema_version: DOCTOR_SCHEMA_VERSION,
            error: message.to_owned(),
        });
    }
    eprintln!("error: {message}");
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("error: serialize doctor report: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_checks_exit_zero() {
        let checks = vec![
            Check::ok("sandbox", "bwrap"),
            Check::warning("git", "not a repository"),
            Check::ok("provider", "openai / gpt / default"),
        ];
        assert_eq!(exit_code(&checks), EXIT_SUCCESS);
    }

    #[test]
    fn runtime_prerequisite_failure_exits_one() {
        let checks = vec![
            Check::failed("sandbox", "bwrap missing"),
            Check::ok("provider", "openai / gpt / default"),
        ];
        assert_eq!(exit_code(&checks), EXIT_FAILURE);
    }

    #[test]
    fn configuration_failure_exits_two() {
        for id in ["config", "provider", "credential"] {
            let checks = vec![Check::failed(id, "bad")];
            assert_eq!(exit_code(&checks), EXIT_USAGE, "{id} must be a usage error");
        }
    }

    #[test]
    fn provider_override_clears_model_and_effort() {
        let base = InferenceProfile::new("openai", "gpt-5-mini", ReasoningEffort::High);
        let resolved = requested_profile(base, Some("opencode-go"), None, None).expect("override");
        assert_eq!(resolved.provider.as_str(), "opencode-go");
        assert!(resolved.model.is_empty());
        assert_eq!(resolved.effort, ReasoningEffort::ProviderDefault);
    }

    #[test]
    fn effort_override_parses_and_rejects_garbage() {
        let base = InferenceProfile::new("openai", "gpt-5-mini", ReasoningEffort::ProviderDefault);
        let resolved =
            requested_profile(base.clone(), None, None, Some("high")).expect("effort override");
        assert_eq!(resolved.effort, ReasoningEffort::High);
        assert!(requested_profile(base, None, None, Some("turbo")).is_err());
    }

    #[test]
    fn report_serializes_checks_and_statuses() {
        let report = Report {
            schema_version: DOCTOR_SCHEMA_VERSION,
            ok: false,
            workspace: "/tmp/ws".into(),
            config_path: None,
            state_dir: Some("/tmp/state".into()),
            provider: Some("openai".into()),
            model: Some("gpt-5-mini".into()),
            effort: Some("default".into()),
            checks: vec![
                Check::ok("sandbox", "bwrap").with_detail("version 0.12"),
                Check::failed("credential", "env:OPENAI_API_KEY is not set"),
            ],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["checks"][0]["status"], "ok");
        assert_eq!(json["checks"][1]["status"], "failed");
        assert_eq!(json["checks"][1]["id"], "credential");
        assert!(json["checks"][0]["detail"].is_string());
    }
}
