//! Mode, Safety, and Permissions are three orthogonal concepts.
//!
//! Mode answers *what kind of work may the agent perform*. Safety answers
//! *should a proposed capability be Allow, Ask, or Deny*. Permissions answer
//! *how is an Ask resolved*. This module owns the classification step: it maps
//! a tool call to concrete [`Capability`] values and then applies the safety
//! profile.
//!
//! Hard deny is independent of all three: privileged, system-destructive, and
//! sandbox-defeating operations are never allowed, and no resolver can turn an
//! `Ask` classification into an implicit allow for them.

use crate::config::OutsidePolicy;
use crate::sandbox::{Capability, CapabilitySet};
use latch_protocol::{Mode, Safety};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The kernel's decision for one proposed operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

/// A classified call: what it needs, what the safety profile decided, and the
/// human-readable operation text used by approval surfaces.
#[derive(Debug, Clone)]
pub struct Classification {
    pub decision: Decision,
    pub capabilities: CapabilitySet,
    /// External roots the operation would write, when it names them.
    pub external_roots: Vec<PathBuf>,
    pub operation: String,
    pub reason: String,
}

impl Classification {
    fn allow(capabilities: CapabilitySet, operation: String) -> Self {
        Self {
            decision: Decision::Allow,
            capabilities,
            external_roots: Vec::new(),
            operation,
            reason: String::new(),
        }
    }

    fn ask(
        capabilities: CapabilitySet,
        external_roots: Vec<PathBuf>,
        operation: String,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self {
            decision: Decision::Ask(reason.clone()),
            reason,
            capabilities,
            external_roots,
            operation,
        }
    }

    fn deny(capabilities: CapabilitySet, operation: String, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            decision: Decision::Deny(reason.clone()),
            reason,
            capabilities,
            external_roots: Vec::new(),
            operation,
        }
    }
}

/// Inputs to classification that do not change per call.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub mode: Mode,
    pub safety: Safety,
    pub workspace: &'a Path,
    pub outside: OutsidePolicy,
    pub workspace_write: bool,
}

/// Extension-provided tools run inside the sandboxed extension host, not the
/// core tool executor. They are classified as `ExtensionExecution`: allowed in
/// Standard and Autonomous, `Ask` in Strict. The extension host itself is
/// sandboxed with a read-only workspace and network; individual extension tool
/// arguments are a documented cooperative boundary.
#[must_use]
pub fn extension_classification(safety: Safety) -> Classification {
    let mut capabilities = CapabilitySet::new();
    capabilities.insert(Capability::ExtensionExecution);
    if safety == Safety::Strict {
        Classification::ask(
            capabilities,
            Vec::new(),
            "extension tool".to_owned(),
            "Strict safety requires approval for extension execution",
        )
    } else {
        Classification::allow(capabilities, "extension tool".to_owned())
    }
}

/// Classifies one tool call into capabilities and a decision.
#[must_use]
pub fn classify(tool: &str, args: &Value, context: Context<'_>) -> Classification {
    match tool {
        // Kernel-owned bookkeeping tools never touch the OS; they are always
        // available in every mode and safety profile.
        "task_update" | "record_evidence" | "complete" => {
            Classification::allow(CapabilitySet::new(), format!("{tool} (kernel state)"))
        }
        "read_file" | "search" | "read_artifact" | "git_status" | "git_diff" | "exec_poll"
        | "exec_terminate" => {
            let mut capabilities = CapabilitySet::new();
            capabilities.insert(Capability::WorkspaceRead);
            Classification::allow(capabilities, format!("{tool} (workspace read)"))
        }
        "patch" | "write" => classify_file_write(tool, args, context),
        "undo" | "checkpoint" => {
            let mut capabilities = CapabilitySet::new();
            capabilities.insert(Capability::WorkspaceRead);
            capabilities.insert(Capability::WorkspaceSourceWrite);
            mode_gate(capabilities, format!("{tool} (workspace change)"), context)
        }
        "shell" | "validate" | "exec_start" => classify_command(tool, args, context),
        _ => {
            let mut capabilities = CapabilitySet::new();
            capabilities.insert(Capability::UnknownCapability);
            Classification::ask(
                capabilities,
                Vec::new(),
                format!("{tool} (unclassified tool)"),
                "unclassified tool requires explicit approval",
            )
        }
    }
}

fn mode_gate(
    capabilities: CapabilitySet,
    operation: String,
    context: Context<'_>,
) -> Classification {
    if !context.mode.can_mutate() {
        return Classification::deny(
            capabilities,
            operation,
            format!("{} mode cannot mutate the workspace", context.mode),
        );
    }
    Classification::allow(capabilities, operation)
}

fn classify_file_write(tool: &str, args: &Value, context: Context<'_>) -> Classification {
    if !context.mode.can_mutate() {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or_default();
        return Classification::deny(
            CapabilitySet::new(),
            format!("{tool} {raw}"),
            format!("{} mode cannot mutate the workspace", context.mode),
        );
    }
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let operation = format!("{tool} {raw}");
    let workspace = context
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| context.workspace.to_path_buf());
    let absolute = match resolve_path(&workspace, &raw) {
        Ok(path) => path,
        Err(error) => {
            return Classification::deny(
                CapabilitySet::new(),
                operation,
                format!("invalid path: {error}"),
            );
        }
    };
    let mut capabilities = CapabilitySet::new();
    capabilities.insert(Capability::WorkspaceRead);
    if !absolute.starts_with(&workspace) {
        if system_destructive_target(&absolute) {
            capabilities.insert(Capability::DestructiveOperation);
            return Classification::deny(
                capabilities,
                operation,
                "system-destructive path denied by policy",
            );
        }
        capabilities.insert(Capability::ExternalFilesystemWrite);
        if matches!(context.outside, OutsidePolicy::Deny) {
            return Classification::deny(
                capabilities,
                operation,
                "path escapes workspace and outside writes are denied by configuration",
            );
        }
        let root = absolute
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| absolute.clone());
        return Classification::ask(
            capabilities,
            vec![root],
            operation,
            "outside-workspace write requires explicit approval",
        );
    }
    if is_git_metadata_path(&workspace, &absolute) {
        capabilities.insert(Capability::GitMetadataWrite);
    } else if is_metadata_file(&absolute) {
        capabilities.insert(Capability::WorkspaceMetadataWrite);
    } else {
        capabilities.insert(Capability::WorkspaceSourceWrite);
    }
    if !context.workspace_write {
        return Classification::deny(
            capabilities,
            operation,
            "workspace writes are disabled by configuration",
        );
    }
    match safety_decision(&capabilities, context) {
        Decision::Allow => Classification::allow(capabilities, operation),
        Decision::Ask(reason) => Classification::ask(capabilities, Vec::new(), operation, reason),
        Decision::Deny(reason) => Classification::deny(capabilities, operation, reason),
    }
}

fn classify_command(tool: &str, args: &Value, context: Context<'_>) -> Classification {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let operation = if command.is_empty() {
        format!("{tool} (no command)")
    } else {
        format!("{tool}: {command}")
    };
    if tool == "exec_start" && !context.mode.can_mutate() {
        return Classification::deny(
            CapabilitySet::new(),
            operation,
            format!("{} mode cannot start managed processes", context.mode),
        );
    }
    if let Some(reason) = hard_deny_command(&command) {
        let mut capabilities = CapabilitySet::new();
        capabilities.insert(Capability::PrivilegedOperation);
        return Classification::deny(capabilities, operation, reason);
    }

    let mut capabilities = CapabilitySet::new();
    capabilities.insert(Capability::WorkspaceRead);
    capabilities.insert(Capability::BuildArtifactWrite);
    if context.mode.can_mutate() {
        capabilities.insert(Capability::WorkspaceSourceWrite);
    }
    if let Some(path) = inferred_external_write(&command, context.workspace) {
        if system_destructive_target(&path) {
            capabilities.insert(Capability::DestructiveOperation);
            return Classification::deny(
                capabilities,
                operation,
                "system-destructive command target denied by policy",
            );
        }
        if matches!(context.outside, OutsidePolicy::Deny) {
            capabilities.insert(Capability::ExternalFilesystemWrite);
            return Classification::deny(
                capabilities,
                operation,
                "outside-workspace write denied by configuration",
            );
        }
        capabilities.insert(Capability::ExternalFilesystemWrite);
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.clone());
        let mut capabilities = capabilities;
        capabilities.extend(&requested_capabilities(args));
        capabilities.extend(&inferred_command_capabilities(&command));
        return Classification::ask(
            capabilities,
            vec![root],
            operation,
            "external filesystem write requires explicit approval",
        );
    }
    capabilities.extend(&requested_capabilities(args));
    capabilities.extend(&inferred_command_capabilities(&command));
    match safety_decision(&capabilities, context) {
        Decision::Allow => Classification::allow(capabilities, operation),
        Decision::Ask(reason) => Classification::ask(capabilities, Vec::new(), operation, reason),
        Decision::Deny(reason) => Classification::deny(capabilities, operation, reason),
    }
}

/// Safety semantics by capability. Mode eligibility was applied by the caller.
fn safety_decision(capabilities: &CapabilitySet, context: Context<'_>) -> Decision {
    if capabilities.any(&[
        Capability::PrivilegedOperation,
        Capability::DestructiveOperation,
    ]) {
        return Decision::Deny(
            "privileged or system-destructive operation denied by policy".into(),
        );
    }
    if capabilities.contains(Capability::ExternalFilesystemWrite) {
        return Decision::Ask("outside-workspace write requires explicit approval".into());
    }
    if capabilities.contains(Capability::RemoteSideEffect) {
        return Decision::Ask("remote side effect requires explicit approval".into());
    }
    if capabilities.contains(Capability::GitMetadataWrite) {
        return Decision::Ask("Git metadata mutation requires explicit approval".into());
    }
    if capabilities.contains(Capability::UnknownCapability) {
        return Decision::Ask("unknown capability requires explicit approval".into());
    }
    if capabilities.contains(Capability::NetworkAccess) && context.safety != Safety::Autonomous {
        return Decision::Ask("network access requires explicit approval".into());
    }
    if context.safety == Safety::Strict
        && capabilities.any(&[
            Capability::WorkspaceSourceWrite,
            Capability::WorkspaceMetadataWrite,
        ])
    {
        return Decision::Ask("Strict safety requires approval for workspace writes".into());
    }
    Decision::Allow
}

/// Capabilities the model explicitly requested in the call arguments.
fn requested_capabilities(args: &Value) -> CapabilitySet {
    let mut capabilities = CapabilitySet::new();
    let Some(items) = args.get("capabilities").and_then(Value::as_array) else {
        return capabilities;
    };
    for item in items {
        match item.as_str().and_then(Capability::parse) {
            Some(capability) => capabilities.insert(capability),
            None => capabilities.insert(Capability::UnknownCapability),
        }
    }
    capabilities
}

fn inferred_command_capabilities(command: &str) -> CapabilitySet {
    let mut capabilities = CapabilitySet::new();
    let tokens = command_tokens(command);
    for token in &tokens {
        if NETWORK_TOOLS.contains(&token.as_str()) {
            capabilities.insert(Capability::NetworkAccess);
        }
        if REMOTE_TOOLS.contains(&token.as_str()) {
            capabilities.insert(Capability::RemoteSideEffect);
            capabilities.insert(Capability::NetworkAccess);
        }
    }
    if let Some(verb) = git_verb(&tokens) {
        if GIT_NETWORK_VERBS.contains(&verb.as_str()) {
            capabilities.insert(Capability::NetworkAccess);
        }
        if GIT_REMOTE_VERBS.contains(&verb.as_str()) {
            capabilities.insert(Capability::RemoteSideEffect);
        }
        if (GIT_WRITE_VERBS.contains(&verb.as_str()) && !git_listing_form(&tokens, &verb))
            || command.contains(".git/")
        {
            capabilities.insert(Capability::GitMetadataWrite);
        }
    }
    if tokens
        .iter()
        .any(|token| token == ".git" || token.contains(".git/"))
    {
        capabilities.insert(Capability::GitMetadataWrite);
    }
    capabilities
}

const NETWORK_TOOLS: &[&str] = &[
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "ftp",
    "telnet",
    "ping",
    "traceroute",
    "dig",
    "nslookup",
    "host",
];
const REMOTE_TOOLS: &[&str] = &["scp", "sftp", "rsync", "gh", "docker"];
const GIT_WRITE_VERBS: &[&str] = &[
    "add",
    "am",
    "apply",
    "branch",
    "checkout",
    "cherry-pick",
    "clean",
    "commit",
    "config",
    "gc",
    "init",
    "merge",
    "mv",
    "rebase",
    "remote",
    "reset",
    "restore",
    "revert",
    "rm",
    "stash",
    "switch",
    "tag",
    "update-ref",
    "worktree",
];
const GIT_NETWORK_VERBS: &[&str] = &["fetch", "pull", "push", "clone", "ls-remote", "submodule"];
const GIT_REMOTE_VERBS: &[&str] = &["push", "fetch", "pull", "clone"];

/// Splits a command into lowercased word tokens, ignoring quoting noise. This
/// is a heuristic only: enforcement remains the sandbox's job.
fn command_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|ch: char| {
                    ch == '"' || ch == '\'' || ch == ';' || ch == '&' || ch == '(' || ch == ')'
                })
                .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

/// `git branch`, `git remote`, `git tag`, `git stash`, and `git config` are
/// read-only when every remaining token is a flag.
fn git_listing_form(tokens: &[String], verb: &str) -> bool {
    if !matches!(verb, "branch" | "remote" | "tag" | "stash" | "config") {
        return false;
    }
    let position = tokens.iter().position(|token| token == "git");
    let Some(position) = position else {
        return false;
    };
    let after = &tokens[position + 1..];
    let verb_position = after.iter().position(|token| token == verb);
    let Some(verb_position) = verb_position else {
        return false;
    };
    after[verb_position + 1..]
        .iter()
        .all(|token| token.starts_with('-') || token.is_empty())
}

fn git_verb(tokens: &[String]) -> Option<String> {
    let position = tokens.iter().position(|token| token == "git")?;
    let mut index = position + 1;
    while index < tokens.len() {
        let token = &tokens[index];
        if token.starts_with('-') {
            // Skip options and their arguments (`-C path`, `-c key=value`).
            index += if token == "-c" || token == "-C" { 2 } else { 1 };
            continue;
        }
        return Some(token.clone());
    }
    None
}

fn is_git_metadata_path(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace)
        .map(|relative| {
            relative
                .components()
                .next()
                .is_some_and(|component| component.as_os_str() == ".git")
        })
        .unwrap_or(false)
}

fn is_metadata_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            "Cargo.toml"
                | "Cargo.lock"
                | "package.json"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "pyproject.toml"
                | "poetry.lock"
                | "go.mod"
                | "go.sum"
                | "Gemfile"
                | "Gemfile.lock"
        )
    )
}

fn system_destructive_target(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("/proc")
        || text.starts_with("/sys")
        || text.starts_with("/dev")
        || text.starts_with("/boot")
        || text.starts_with("/usr")
        || text.starts_with("/bin")
        || text.starts_with("/sbin")
        || text.starts_with("/lib")
        || text.starts_with("/root")
        || matches!(
            text.as_ref(),
            "/etc/shadow" | "/etc/passwd" | "/etc/sudoers" | "/etc/fstab"
        )
}

/// A conservative best-effort detection of a shell command that names an
/// absolute write target outside the workspace. The sandbox is the real
/// boundary; this exists so the capability escalation becomes an `Ask`.
fn inferred_external_write(command: &str, workspace: &Path) -> Option<PathBuf> {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let mut candidate: Option<String> = None;
    for (index, token) in tokens.iter().enumerate() {
        let token = token.trim_matches(|ch: char| ch == '"' || ch == '\'');
        if let Some(path) = token.strip_prefix(">>").or_else(|| token.strip_prefix('>')) {
            if path.is_empty() {
                if let Some(next) = tokens.get(index + 1) {
                    candidate = Some(
                        next.trim_matches(|ch: char| ch == '"' || ch == '\'')
                            .to_owned(),
                    );
                }
            } else {
                candidate = Some(path.to_owned());
            }
        } else if token.starts_with('/')
            && tokens.get(index.wrapping_sub(1)).is_some_and(|previous| {
                WRITE_COMMANDS.contains(&previous.trim_matches('"').trim_matches('\''))
            })
        {
            candidate = Some((*token).to_owned());
        }
    }
    let candidate = candidate?;
    let path = PathBuf::from(candidate);
    if !path.is_absolute() || path.starts_with(workspace) {
        return None;
    }
    Some(path)
}

const WRITE_COMMANDS: &[&str] = &[
    "tee", "touch", "mkdir", "rm", "cp", "mv", "ln", "chmod", "chown", "install",
];

/// Defense-in-depth semantic hard deny. It is deliberately small; the sandbox
/// prevents most damage even when a command slips past this check.
#[must_use]
pub fn hard_deny_command(command: &str) -> Option<String> {
    let lower = command.to_ascii_lowercase();
    let words = lower.split_whitespace().collect::<Vec<_>>();
    let first = words.first().copied().unwrap_or("");
    let recursive_rm = first == "rm"
        && words.iter().any(|word| {
            let flag = word.trim_start_matches('-');
            word.starts_with('-')
                && (word.starts_with("--recursive")
                    || (flag.chars().any(|ch| ch == 'r' || ch == 'R') && !flag.is_empty()))
        });
    let denied = first == "sudo"
        || recursive_rm
        || (words.starts_with(&["git", "push"])
            && words.iter().any(|word| word.starts_with("--force")))
        || lower.contains("git reset --hard")
        || lower.contains("git clean -f")
        || first.starts_with("mkfs")
        || first == "dd"
        || lower.contains(" > /dev/sd")
        || (first == "chmod" && lower.contains("777 /"))
        || matches!(
            first,
            "mount" | "umount" | "insmod" | "modprobe" | "capsh" | "setcap" | "pivot_root"
        )
        || lower.contains("unshare");
    denied.then(|| "destructive or privileged shell command denied by policy".to_owned())
}

fn resolve_path(workspace: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    use anyhow::bail;
    if raw.is_empty() {
        bail!("path is required");
    }
    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    // Normalize `.`/`..` lexically so escape detection does not need the file
    // to exist yet.
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.components().count() == 0 {
        bail!("path is required");
    }
    // Canonicalize the existing prefix, then re-attach the missing suffix so a
    // new file inside the workspace still resolves.
    let mut existing = normalized.clone();
    let mut suffix = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => suffix.push(name.to_os_string()),
            None => break,
        }
        existing.pop();
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(mode: Mode, safety: Safety, workspace: &Path) -> Context<'_> {
        Context {
            mode,
            safety,
            workspace,
            outside: OutsidePolicy::Ask,
            workspace_write: true,
        }
    }

    #[test]
    fn ordinary_workspace_edit_follows_safety() {
        let workspace = Path::new("/tmp/ws");
        for (safety, expect_ask) in [
            (Safety::Strict, true),
            (Safety::Standard, false),
            (Safety::Autonomous, false),
        ] {
            let classification = classify(
                "patch",
                &json!({"path": "src/lib.rs"}),
                context(Mode::Work, safety, workspace),
            );
            assert_eq!(
                matches!(classification.decision, Decision::Ask(_)),
                expect_ask,
                "{safety:?}"
            );
        }
    }

    #[test]
    fn read_only_modes_deny_file_writes() {
        for mode in [Mode::Ask, Mode::Plan] {
            let classification = classify(
                "write",
                &json!({"path": "src/lib.rs", "content": "x"}),
                context(mode, Safety::Autonomous, Path::new("/tmp/ws")),
            );
            assert!(matches!(classification.decision, Decision::Deny(_)));
        }
    }

    #[test]
    fn git_metadata_is_always_ask_not_allow() {
        for safety in [Safety::Strict, Safety::Standard, Safety::Autonomous] {
            let classification = classify(
                "patch",
                &json!({"path": ".git/HEAD"}),
                context(Mode::Work, safety, Path::new("/tmp/ws")),
            );
            assert!(
                matches!(classification.decision, Decision::Ask(_)),
                "{safety:?} must ask for .git writes"
            );
            assert!(
                classification
                    .capabilities
                    .contains(Capability::GitMetadataWrite)
            );
        }
    }

    #[test]
    fn outside_write_is_ask_even_under_autonomous() {
        let classification = classify(
            "write",
            &json!({"path": "/tmp/elsewhere/file.txt"}),
            context(Mode::Work, Safety::Autonomous, Path::new("/tmp/ws")),
        );
        assert!(matches!(classification.decision, Decision::Ask(_)));
        assert!(
            classification
                .capabilities
                .contains(Capability::ExternalFilesystemWrite)
        );
    }

    #[test]
    fn privileged_and_destructive_targets_are_denied() {
        for path in ["/etc/sudoers", "/usr/lib/x", "/proc/1/mem", "/dev/sda"] {
            let classification = classify(
                "write",
                &json!({"path": path, "content": "x"}),
                context(Mode::Work, Safety::Autonomous, Path::new("/tmp/ws")),
            );
            assert!(
                matches!(classification.decision, Decision::Deny(_)),
                "{path} must stay denied"
            );
        }
        assert!(hard_deny_command("sudo rm -rf /").is_some());
        assert!(hard_deny_command("rm -rf build").is_some());
        assert!(hard_deny_command("mkfs.ext4 /dev/sda1").is_some());
        // A plain file removal is not a privileged/destructive shell denial;
        // the sandbox still bounds what it can touch.
        assert!(hard_deny_command("rm control-note.txt").is_none());
        assert!(hard_deny_command("git status").is_none());
    }

    #[test]
    fn command_capabilities_classify_network_git_and_remote() {
        let network = inferred_command_capabilities("curl https://example.test");
        assert!(network.contains(Capability::NetworkAccess));
        let git = inferred_command_capabilities("git commit -m x");
        assert!(git.contains(Capability::GitMetadataWrite));
        let remote = inferred_command_capabilities("git push origin main");
        assert!(remote.contains(Capability::RemoteSideEffect));
        let read = inferred_command_capabilities("git status --short");
        assert!(!read.contains(Capability::GitMetadataWrite));
        assert!(!read.contains(Capability::NetworkAccess));
    }

    #[test]
    fn network_follows_safety() {
        let workspace = Path::new("/tmp/ws");
        let strict = classify(
            "shell",
            &json!({"command": "curl https://example.test"}),
            context(Mode::Work, Safety::Strict, workspace),
        );
        assert!(matches!(strict.decision, Decision::Ask(_)));
        let autonomous = classify(
            "shell",
            &json!({"command": "curl https://example.test"}),
            context(Mode::Work, Safety::Autonomous, workspace),
        );
        assert!(matches!(autonomous.decision, Decision::Allow));
    }

    #[test]
    fn explicit_capability_requests_are_classified() {
        let workspace = Path::new("/tmp/ws");
        let classification = classify(
            "shell",
            &json!({"command": "git commit -m x", "capabilities": ["network"]}),
            context(Mode::Work, Safety::Autonomous, workspace),
        );
        assert!(
            classification
                .capabilities
                .contains(Capability::NetworkAccess)
        );
        assert!(
            classification
                .capabilities
                .contains(Capability::GitMetadataWrite)
        );
        // Git metadata still forces Ask even in Autonomous.
        assert!(matches!(classification.decision, Decision::Ask(_)));

        let unknown = classify(
            "shell",
            &json!({"command": "echo hi", "capabilities": ["teleport"]}),
            context(Mode::Work, Safety::Autonomous, workspace),
        );
        assert!(unknown.capabilities.contains(Capability::UnknownCapability));
        assert!(matches!(unknown.decision, Decision::Ask(_)));
    }

    #[test]
    fn command_external_write_is_detected() {
        let workspace = Path::new("/tmp/ws");
        let classification = classify(
            "shell",
            &json!({"command": "echo hi > /tmp/outside/file.txt"}),
            context(Mode::Work, Safety::Standard, workspace),
        );
        assert!(matches!(classification.decision, Decision::Ask(_)));
        assert!(
            classification
                .capabilities
                .contains(Capability::ExternalFilesystemWrite)
        );
    }

    #[test]
    fn dependency_manifests_are_metadata_not_source() {
        let classification = classify(
            "patch",
            &json!({"path": "Cargo.toml"}),
            context(Mode::Work, Safety::Standard, Path::new("/tmp/ws")),
        );
        assert!(
            classification
                .capabilities
                .contains(Capability::WorkspaceMetadataWrite)
        );
        assert!(
            !classification
                .capabilities
                .contains(Capability::WorkspaceSourceWrite)
        );
    }
}
