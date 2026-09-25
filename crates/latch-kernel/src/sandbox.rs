//! Mandatory Bubblewrap sandbox for every command execution path.
//!
//! Latch is Linux-first: `shell`, `exec_start`, kernel validation, and the
//! fixed inspection commands all run through [`SandboxRunner`]. There is no
//! unsandboxed fallback. If the runtime probe fails, the kernel reports an
//! actionable error and refuses to execute commands rather than silently
//! dropping the boundary.
//!
//! The sandbox exposes only what a [`SandboxProfile`] grants: the workspace
//! (read-only or writable, with `.git` protected unless explicitly granted),
//! private scratch space, and any explicitly approved external roots. Home
//! secrets and host sockets are masked; the network namespace is unshared
//! unless the profile grants network access.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// A capability the kernel can reason about and grant to one sandboxed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    WorkspaceRead,
    WorkspaceSourceWrite,
    BuildArtifactWrite,
    WorkspaceMetadataWrite,
    GitMetadataWrite,
    ExternalFilesystemWrite,
    NetworkAccess,
    RemoteSideEffect,
    PrivilegedOperation,
    DestructiveOperation,
    ExtensionExecution,
    UnknownCapability,
}

impl Capability {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WorkspaceRead => "workspace_read",
            Self::WorkspaceSourceWrite => "workspace_source_write",
            Self::BuildArtifactWrite => "build_artifact_write",
            Self::WorkspaceMetadataWrite => "workspace_metadata_write",
            Self::GitMetadataWrite => "git_metadata_write",
            Self::ExternalFilesystemWrite => "external_filesystem_write",
            Self::NetworkAccess => "network_access",
            Self::RemoteSideEffect => "remote_side_effect",
            Self::PrivilegedOperation => "privileged_operation",
            Self::DestructiveOperation => "destructive_operation",
            Self::ExtensionExecution => "extension_execution",
            Self::UnknownCapability => "unknown_capability",
        }
    }

    /// Parses a model-requested capability name from tool arguments.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
        Some(match normalized.as_str() {
            "workspace_read" | "read" => Self::WorkspaceRead,
            "workspace_source_write" | "source_write" | "edit" => Self::WorkspaceSourceWrite,
            "build_artifact_write" | "build_output" => Self::BuildArtifactWrite,
            "workspace_metadata_write" | "metadata_write" => Self::WorkspaceMetadataWrite,
            "git_metadata_write" | "git_metadata" | "git" => Self::GitMetadataWrite,
            "external_filesystem_write" | "external_write" | "outside" => {
                Self::ExternalFilesystemWrite
            }
            "network_access" | "network" => Self::NetworkAccess,
            "remote_side_effect" | "remote" => Self::RemoteSideEffect,
            "privileged_operation" | "privileged" => Self::PrivilegedOperation,
            "destructive_operation" | "destructive" => Self::DestructiveOperation,
            "extension_execution" | "extension" => Self::ExtensionExecution,
            _ => return None,
        })
    }

    /// Capabilities that can write outside the workspace, mutate Git metadata,
    /// or cross the machine boundary. Every one of these must become an `Ask`
    /// before it can be granted, regardless of safety profile or resolver.
    #[must_use]
    pub const fn requires_explicit_ask(self) -> bool {
        matches!(
            self,
            Self::ExternalFilesystemWrite
                | Self::GitMetadataWrite
                | Self::NetworkAccess
                | Self::RemoteSideEffect
                | Self::UnknownCapability
        )
    }

    /// Capabilities that stay denied no matter which resolver is active.
    #[must_use]
    pub const fn is_hard_deny(self) -> bool {
        matches!(self, Self::PrivilegedOperation | Self::DestructiveOperation)
    }
}

/// An ordered, deduplicated set of capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    pub fn insert(&mut self, capability: Capability) {
        self.0.insert(capability);
    }

    pub fn extend(&mut self, other: &CapabilitySet) {
        self.0.extend(other.0.iter().copied());
    }

    #[must_use]
    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    /// True when every capability in `self` is also in `other`. Used by the
    /// runtime capability vocabulary to prove a requested surface stays inside
    /// a declared one; enforcement remains the sandbox's job.
    #[must_use]
    pub fn is_subset(&self, other: &CapabilitySet) -> bool {
        self.0.is_subset(&other.0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True when the set is non-empty and every capability only observes the
    /// workspace. An empty set is not read-only: kernel bookkeeping tools
    /// (`task_update`, `record_evidence`, `complete`) mutate durable session
    /// state without needing an OS capability.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        !self.0.is_empty()
            && self
                .0
                .iter()
                .all(|capability| matches!(capability, Capability::WorkspaceRead))
    }

    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|capability| capability.name().to_owned())
            .collect()
    }

    #[must_use]
    pub fn any(&self, capabilities: &[Capability]) -> bool {
        capabilities
            .iter()
            .any(|capability| self.contains(*capability))
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// What a sandboxed process may actually do. Built from the classified call
/// plus any single-use, call-scoped capability grant the resolver approved.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    pub workspace: PathBuf,
    pub home: PathBuf,
    /// The configured Latch state directory, resolved by the command builder.
    pub state_dir: PathBuf,
    pub capabilities: CapabilitySet,
    /// External roots explicitly approved for this call, mounted writable.
    pub external_roots: Vec<PathBuf>,
}

impl SandboxProfile {
    #[must_use]
    pub fn new(
        workspace: PathBuf,
        home: PathBuf,
        state_dir: PathBuf,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            workspace,
            home,
            state_dir,
            capabilities,
            external_roots: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_external_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.external_roots = roots;
        self
    }

    /// Build-artifact writes never make the workspace writable; they only
    /// enable the private scratch target directory on read-only profiles.
    #[must_use]
    pub fn workspace_writable(&self) -> bool {
        self.capabilities.any(&[
            Capability::WorkspaceSourceWrite,
            Capability::WorkspaceMetadataWrite,
            Capability::GitMetadataWrite,
        ])
    }

    #[must_use]
    pub fn git_writable(&self) -> bool {
        self.capabilities.contains(Capability::GitMetadataWrite)
    }

    #[must_use]
    pub fn network(&self) -> bool {
        self.capabilities.contains(Capability::NetworkAccess)
    }
}

/// Result of the startup capability probe.
#[derive(Debug, Clone)]
pub struct SandboxProbe {
    pub bwrap: PathBuf,
    pub version: String,
}

/// Runs commands inside a Bubblewrap sandbox.
#[derive(Debug, Clone)]
pub struct SandboxRunner {
    bwrap: PathBuf,
}

impl SandboxRunner {
    /// Detects `bwrap`, then probes a minimal sandbox command with the same
    /// namespace isolation the real profiles use. Any failure is returned as
    /// an actionable error; callers must refuse execution rather than fall
    /// back to the host.
    pub fn detect(workspace: &Path) -> Result<Self> {
        let probe = probe(workspace)?;
        Ok(Self { bwrap: probe.bwrap })
    }

    #[must_use]
    pub fn bwrap(&self) -> &Path {
        &self.bwrap
    }

    /// Builds the bwrap invocation for one call. Every mount is explicit; the
    /// host root is bound read-only first and narrowed afterwards.
    pub fn command(&self, profile: &SandboxProfile, command: &str) -> Result<Command> {
        // Resolve on the host before constructing mounts. Masking the real
        // directory also hides paths through symlinked state-dir aliases.
        let state_dir = std::fs::canonicalize(&profile.state_dir).with_context(|| {
            format!(
                "resolve Latch state directory {}",
                profile.state_dir.display()
            )
        })?;
        if !state_dir.is_dir() {
            bail!(
                "Latch state directory {} is not a directory",
                state_dir.display()
            );
        }
        let secret = state_dir.join("secrets.toml");
        let secret_target =
            if secret.exists() {
                Some(std::fs::canonicalize(&secret).with_context(|| {
                    format!("resolve Latch credential file {}", secret.display())
                })?)
            } else {
                None
            };
        let mut cmd = Command::new(&self.bwrap);
        cmd.arg("--die-with-parent");
        cmd.arg("--new-session");
        // User, PID, IPC, UTS isolation are mandatory. The network namespace is
        // unshared unless the profile grants network access.
        cmd.args([
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
        ]);
        if profile.network() {
            cmd.arg("--share-net");
        } else {
            cmd.arg("--unshare-net");
        }
        // Baseline: host filesystem read-only, minimal /dev, fresh /proc and
        // private writable /tmp. /run is masked so system and session sockets
        // (D-Bus, Docker, SSH/GPG agents) are not reachable.
        cmd.args(["--ro-bind", "/", "/"]);
        cmd.args(["--dev", "/dev"]);
        cmd.args(["--proc", "/proc"]);
        cmd.args(["--tmpfs", "/tmp"]);
        cmd.args(["--tmpfs", "/run"]);
        if is_real_directory(Path::new("/var/run")) {
            cmd.args(["--tmpfs", "/var/run"]);
        }
        // Workspace visibility is explicit. Writable profiles remount the
        // workspace after the home masks; `.git` stays read-only unless the
        // call was granted Git metadata mutation.
        let workspace = profile.workspace.as_path();
        if profile.workspace_writable() {
            cmd.arg("--bind").arg(workspace).arg(workspace);
            let git = workspace.join(".git");
            if !profile.git_writable() && git.exists() {
                cmd.args(["--ro-bind"]).arg(&git).arg(&git);
            }
        } else {
            cmd.arg("--ro-bind").arg(workspace).arg(workspace);
        }
        for root in &profile.external_roots {
            if root.exists() {
                cmd.arg("--bind").arg(root).arg(root);
            }
        }
        for secret in secret_paths(&profile.home)
            .into_iter()
            .filter(|path| mount_exposes(path, profile))
        {
            cmd.args(["--tmpfs"]).arg(secret);
        }
        // These mounts come after workspace and external grants, so neither
        // can remount Latch's state back into view. /tmp and /run are already
        // private unless a later workspace/external bind exposes the path.
        if mount_exposes(&state_dir, profile) {
            cmd.arg("--tmpfs").arg(&state_dir);
        }
        if let Some(target) = secret_target {
            // A symlinked secrets.toml can point outside the state directory.
            // Hide that real file as well as the directory containing the link.
            if !target.starts_with(&state_dir) && mount_exposes(&target, profile) {
                cmd.args(["--ro-bind", "/dev/null"]).arg(target);
            }
        }
        // Minimal environment: no host secrets leak through variables.
        cmd.arg("--clearenv");
        cmd.args(["--setenv", "HOME"]).arg(&profile.home);
        cmd.arg("--setenv").arg("PATH").arg(default_path());
        cmd.args(["--setenv", "TMPDIR", "/tmp"]);
        cmd.args(["--setenv", "XDG_RUNTIME_DIR", "/tmp/latch-runtime"]);
        cmd.args(["--setenv", "LATCH_SANDBOX", "1"]);
        for key in [
            "TERM", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "USER", "LOGNAME",
        ] {
            if let Ok(value) = std::env::var(key) {
                cmd.arg("--setenv").arg(key).arg(value);
            }
        }
        for key in [
            "RUSTUP_HOME",
            "CARGO_HOME",
            "GOPATH",
            "JAVA_HOME",
            "VIRTUAL_ENV",
        ] {
            if let Ok(value) = std::env::var(key) {
                cmd.arg("--setenv").arg(key).arg(value);
            }
        }
        // A read-only workspace still gets controlled build output: cargo and
        // friends write into the private scratch tmpfs instead of the sources.
        if !profile.workspace_writable() {
            cmd.args(["--setenv", "CARGO_TARGET_DIR", "/tmp/latch-target"]);
            cmd.args(["--setenv", "PYTHONDONTWRITEBYTECODE", "1"]);
        }
        cmd.arg("--chdir").arg(workspace);
        cmd.args(["--", "/bin/bash", "-lc", command]);
        cmd.stdin(Stdio::null());
        Ok(cmd)
    }
}

fn mount_exposes(path: &Path, profile: &SandboxProfile) -> bool {
    if !path.starts_with("/tmp") && !path.starts_with("/run") {
        return true;
    }
    std::iter::once(&profile.workspace)
        .chain(&profile.external_roots)
        .any(|root| std::fs::canonicalize(root).is_ok_and(|real| path.starts_with(real)))
}

fn default_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned())
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

/// Host paths under the user's home that are masked inside the sandbox. These
/// are the conventional locations for credentials and agent sockets; the
/// sandbox never exposes them unless a future profile explicitly binds them.
fn secret_paths(home: &Path) -> Vec<PathBuf> {
    const RELATIVE: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".netrc",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
        ".password-store",
        ".config/gh",
        ".config/gcloud",
        ".config/op",
        ".cargo/credentials",
        ".cargo/credentials.toml",
    ];
    RELATIVE
        .iter()
        .map(|relative| home.join(relative))
        .filter(|path| path.exists())
        .collect()
}

/// Finds `bwrap` and verifies a minimal sandbox command succeeds with the
/// namespace features Latch depends on.
pub fn probe(workspace: &Path) -> Result<SandboxProbe> {
    let bwrap = locate_bwrap()?;
    probe_bwrap(bwrap, workspace)
}

/// Probe with an explicit `bwrap` path. Exposed so tests and diagnostics can
/// verify the refusal path without mutating the process environment.
pub fn probe_bwrap(bwrap: PathBuf, workspace: &Path) -> Result<SandboxProbe> {
    if !bwrap.is_file() {
        bail!(
            "bwrap (bubblewrap) is required: Latch executes shell and process commands only inside the Bubblewrap sandbox. Install it (for example `sudo apt install bubblewrap` or `sudo dnf install bubblewrap`) and restart Latch."
        );
    }
    let output = std::process::Command::new(&bwrap)
        .args(["--die-with-parent", "--new-session"])
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
            "--unshare-net",
        ])
        .args(["--ro-bind", "/", "/"])
        .args(["--dev", "/dev"])
        .args(["--proc", "/proc"])
        .args(["--tmpfs", "/tmp"])
        .args(["--tmpfs", "/run"])
        // The workspace bind also proves mount construction works, including
        // when the workspace itself lives under a masked path such as /tmp.
        .arg("--ro-bind")
        .arg(workspace)
        .arg(workspace)
        .arg("--clearenv")
        .arg("--setenv")
        .arg("PATH")
        .arg(default_path())
        .arg("--chdir")
        .arg(workspace)
        .args([
            "--",
            "/bin/bash",
            "-c",
            "printf 'LATCH_SANDBOX_PROBE_OK pid=%s\\n' $$",
        ])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run sandbox probe with {}", bwrap.display()))?;
    if !output.status.success() {
        bail!(
            "bubblewrap sandbox probe failed ({}): {}\nLatch refuses to execute commands unsandboxed. Ensure unprivileged user namespaces are enabled (sysctl kernel.unprivileged_userns_clone=1) or run in an environment that allows them.",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("LATCH_SANDBOX_PROBE_OK") {
        bail!(
            "bubblewrap sandbox probe produced no expected output; Latch refuses to execute commands unsandboxed"
        );
    }
    let version = std::process::Command::new(&bwrap)
        .arg("--version")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "bubblewrap".to_owned());
    Ok(SandboxProbe { bwrap, version })
}

fn locate_bwrap() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LATCH_BWRAP") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "LATCH_BWRAP points at {}, which is not a file; Latch refuses to execute commands unsandboxed",
            path.display()
        );
    }
    if let Some(path) = find_on_path("bwrap") {
        return Ok(path);
    }
    bail!(
        "bwrap (bubblewrap) is required: Latch executes shell and process commands only inside the Bubblewrap sandbox. Install it (for example `sudo apt install bubblewrap` or `sudo dnf install bubblewrap`) and restart Latch."
    )
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn runner() -> Option<SandboxRunner> {
        let dir = tempdir().unwrap();
        SandboxRunner::detect(dir.path()).ok()
    }

    async fn run(
        runner: &SandboxRunner,
        profile: &SandboxProfile,
        command: &str,
    ) -> std::process::Output {
        runner
            .command(profile, command)
            .expect("build sandboxed command")
            .output()
            .await
            .expect("spawn sandboxed command")
    }

    fn base_profile(workspace: &Path, home: &Path) -> SandboxProfile {
        let mut capabilities = CapabilitySet::new();
        capabilities.insert(Capability::WorkspaceRead);
        SandboxProfile::new(
            workspace.to_path_buf(),
            home.to_path_buf(),
            home.to_path_buf(),
            capabilities,
        )
    }

    #[test]
    fn read_only_classification_is_conservative() {
        let mut read = CapabilitySet::new();
        read.insert(Capability::WorkspaceRead);
        assert!(read.is_read_only());

        // Kernel bookkeeping has no OS capability but mutates session state.
        assert!(!CapabilitySet::new().is_read_only());

        let mut write = CapabilitySet::new();
        write.insert(Capability::WorkspaceRead);
        write.insert(Capability::WorkspaceSourceWrite);
        assert!(!write.is_read_only());

        let mut network = CapabilitySet::new();
        network.insert(Capability::NetworkAccess);
        assert!(!network.is_read_only());
    }

    #[tokio::test]
    async fn sensitive_host_sockets_are_not_exposed() {
        let Some(runner) = runner() else {
            return;
        };
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let output = run(
            &runner,
            &base_profile(dir.path(), &home),
            "for f in /run/docker.sock /run/dbus/system_bus_socket /run/user; do test ! -e \"$f\" || echo LEAK:$f; done; echo SOCKETS_CHECKED",
        )
        .await;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("SOCKETS_CHECKED"), "{stdout}");
        assert!(!stdout.contains("LEAK:"), "{stdout}");
    }

    #[test]
    fn probe_succeeds_on_this_host() {
        let dir = tempdir().unwrap();
        let probe = probe(dir.path()).expect("bwrap probe");
        assert!(probe.version.contains("bubblewrap"), "{}", probe.version);
    }

    #[test]
    fn missing_bwrap_is_refused() {
        let error = probe_bwrap(PathBuf::from("/nonexistent/latch-bwrap"), Path::new("/"))
            .expect_err("missing bwrap must be refused");
        assert!(error.to_string().contains("bubblewrap"), "{error}");
    }

    #[tokio::test]
    async fn read_only_workspace_rejects_shell_redirection() {
        let Some(runner) = runner() else {
            return;
        };
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(dir.path().join("keep.txt"), "original").unwrap();
        let profile = base_profile(dir.path(), &home);
        let output = run(&runner, &profile, "echo changed > keep.txt").await;
        assert!(!output.status.success(), "redirection must fail");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
            "original"
        );
    }

    #[tokio::test]
    async fn writable_workspace_keeps_git_read_only() {
        let Some(runner) = runner() else {
            return;
        };
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.path().join("src.txt"), "one").unwrap();
        let mut capabilities = CapabilitySet::new();
        capabilities.insert(Capability::WorkspaceRead);
        capabilities.insert(Capability::WorkspaceSourceWrite);
        let profile =
            SandboxProfile::new(dir.path().to_path_buf(), home.clone(), home, capabilities);
        let output = run(
            &runner,
            &profile,
            "echo two > src.txt && echo other > .git/HEAD && echo SHOULD_NOT_REACH",
        )
        .await;
        assert!(!output.status.success(), "git metadata write must fail");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
    }

    #[tokio::test]
    async fn granted_git_metadata_is_writable() {
        let Some(runner) = runner() else {
            return;
        };
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let mut capabilities = CapabilitySet::new();
        capabilities.insert(Capability::WorkspaceRead);
        capabilities.insert(Capability::GitMetadataWrite);
        let profile =
            SandboxProfile::new(dir.path().to_path_buf(), home.clone(), home, capabilities);
        let output = run(&runner, &profile, "echo updated > .git/HEAD").await;
        assert!(output.status.success(), "granted git write must succeed");
    }

    #[tokio::test]
    async fn home_secrets_are_invisible() {
        let Some(runner) = runner() else {
            return;
        };
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "SECRET").unwrap();
        let profile = base_profile(dir.path(), &home);
        let output = run(&runner, &profile, "cat \"$HOME/.ssh/id_ed25519\"").await;
        assert!(!output.status.success(), "home secret must be masked");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
    }

    async fn assert_state_hidden(
        runner: &SandboxRunner,
        workspace: &Path,
        home: &Path,
        state_dir: &Path,
        secret_path: &Path,
    ) {
        let profile = base_profile(workspace, home);
        let profile = SandboxProfile::new(
            profile.workspace,
            profile.home,
            state_dir.to_path_buf(),
            profile.capabilities,
        );
        let output = run(
            runner,
            &profile,
            &format!(
                "if cat '{}' >/dev/null 2>&1; then exit 42; fi; cat readable.txt",
                secret_path.display()
            ),
        )
        .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "workspace data");
    }

    #[tokio::test]
    async fn default_state_credentials_are_hidden_and_workspace_is_readable() {
        let Some(runner) = runner() else { return };
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let home = workspace.join("home");
        let state_dir = home.join(".local/state/latch");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(workspace.join("readable.txt"), "workspace data").unwrap();
        let secret = state_dir.join("secrets.toml");
        std::fs::write(&secret, "fake default secret").unwrap();
        assert_state_hidden(&runner, &workspace, &home, &state_dir, &secret).await;
    }

    #[tokio::test]
    async fn custom_symlinked_state_and_secret_targets_are_hidden() {
        let Some(runner) = runner() else { return };
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let home = dir.path().join("home");
        let real_state = workspace.join("custom-state");
        std::fs::create_dir_all(&real_state).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(workspace.join("readable.txt"), "workspace data").unwrap();
        let target = workspace.join("fake-credential-target");
        std::fs::write(&target, "fake custom secret").unwrap();
        std::os::unix::fs::symlink(&target, real_state.join("secrets.toml")).unwrap();
        let alias = workspace.join("state-alias");
        std::os::unix::fs::symlink(&real_state, &alias).unwrap();
        assert_state_hidden(&runner, &workspace, &home, &alias, &target).await;
        assert_state_hidden(
            &runner,
            &workspace,
            &home,
            &alias,
            &alias.join("secrets.toml"),
        )
        .await;
    }

    #[tokio::test]
    async fn changed_home_default_state_credentials_are_hidden() {
        let Some(runner) = runner() else { return };
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let changed_home = workspace.join("different-home");
        let state_dir = changed_home.join(".local/state/latch");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(workspace.join("readable.txt"), "workspace data").unwrap();
        let secret = state_dir.join("secrets.toml");
        std::fs::write(&secret, "fake changed-home secret").unwrap();
        assert_state_hidden(&runner, &workspace, &changed_home, &state_dir, &secret).await;
        let profile = SandboxProfile::new(
            workspace,
            changed_home.clone(),
            state_dir,
            [Capability::WorkspaceRead].into_iter().collect(),
        );
        let output = run(
            &runner,
            &profile,
            &format!(
                "test \"$HOME\" = '{}' && test ! -r \"$HOME/.local/state/latch/secrets.toml\"",
                changed_home.display()
            ),
        )
        .await;
        assert!(output.status.success());
    }

    #[tokio::test]
    async fn network_namespace_is_isolated_by_default_and_shared_when_granted() {
        // Compare interface names, not per-interface byte counters: host
        // counters advance between reads and made this test flaky on shared CI.
        fn interface_names(output: &str) -> Vec<String> {
            output
                .lines()
                .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim().to_owned()))
                .filter(|name| !name.is_empty() && name != "lo")
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        }

        let Some(runner) = runner() else {
            return;
        };
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let host = interface_names(&std::fs::read_to_string("/proc/net/dev").unwrap_or_default());
        if host.is_empty() {
            return; // host has no non-loopback interface; nothing to compare
        }

        let isolated = run(
            &runner,
            &base_profile(dir.path(), &home),
            "cat /proc/net/dev",
        )
        .await;
        let isolated = interface_names(&String::from_utf8_lossy(&isolated.stdout));
        assert!(
            isolated.is_empty(),
            "default sandbox must not share the host netns, saw {isolated:?}"
        );

        let mut capabilities = CapabilitySet::new();
        capabilities.insert(Capability::WorkspaceRead);
        capabilities.insert(Capability::NetworkAccess);
        let profile =
            SandboxProfile::new(dir.path().to_path_buf(), home.clone(), home, capabilities);
        let shared = run(&runner, &profile, "cat /proc/net/dev").await;
        let shared = interface_names(&String::from_utf8_lossy(&shared.stdout));
        assert_eq!(shared, host, "granted network must share the host netns");
    }
}
