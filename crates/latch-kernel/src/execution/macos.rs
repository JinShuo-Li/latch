//! macOS Seatbelt command boundary. No unsandboxed fallback.
use crate::sandbox::SandboxProfile;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;

const RUNNER: &str = "/usr/bin/sandbox-exec";

#[derive(Debug, Clone)]
pub struct MacosBackend {
    scratch: Arc<tempfile::TempDir>,
}

impl MacosBackend {
    pub fn inspect() -> Result<String> {
        if !Path::new(RUNNER).is_file() {
            bail!("macOS requires /usr/bin/sandbox-exec; execution is refused without Seatbelt");
        }
        Ok("macos/seatbelt: sandbox-exec present (runtime probe runs at session startup)".into())
    }

    pub fn detect(_workspace: &Path) -> Result<Self> {
        Self::inspect()?;
        let output = std::process::Command::new(RUNNER)
            .args([
                "-p",
                "(version 1)(deny default)(import \"system.sb\")(allow file-read*)(allow process*)",
                "/usr/bin/true",
            ])
            .output()
            .context("probe macOS Seatbelt")?;
        if !output.status.success() {
            bail!(
                "macOS Seatbelt probe failed: {}; no unsandboxed fallback",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Self {
            scratch: Arc::new(
                tempfile::Builder::new()
                    .prefix("latch-sandbox-")
                    .tempdir()?,
            ),
        })
    }

    pub fn command(&self, profile: &SandboxProfile, script: &str) -> Result<Command> {
        let scratch = self.scratch.path().canonicalize()?;
        let rules = seatbelt_profile(profile, &scratch)?;
        let mut command = Command::new(RUNNER);
        command.args([
            "-p",
            &rules,
            "/bin/bash",
            "--noprofile",
            "--norc",
            "-c",
            script,
        ]);
        command
            .current_dir(&profile.workspace)
            .env_clear()
            .env("HOME", &profile.home)
            .env(
                "PATH",
                std::env::var_os("PATH")
                    .unwrap_or_else(|| "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".into()),
            )
            .env("TMPDIR", &scratch)
            .env("LATCH_SANDBOX", "1")
            .stdin(Stdio::null());
        for key in [
            "TERM",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TZ",
            "USER",
            "LOGNAME",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "GOPATH",
            "JAVA_HOME",
            "VIRTUAL_ENV",
            "DEVELOPER_DIR",
            "SDKROOT",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if !profile.workspace_writable() {
            command
                .env("CARGO_TARGET_DIR", scratch.join("target"))
                .env("PYTHONDONTWRITEBYTECODE", "1");
        }
        command.process_group(0);
        Ok(command)
    }

    pub fn fixed_command(
        &self,
        profile: &SandboxProfile,
        program: &str,
        args: &[&str],
    ) -> Result<Command> {
        let script = std::iter::once(program)
            .chain(args.iter().copied())
            .map(super::shell::bash_quote)
            .collect::<Vec<_>>()
            .join(" ");
        self.command(profile, &format!("exec {script}"))
    }

    pub fn status(&self) -> String {
        "macos/seatbelt: mandatory sandbox ready".into()
    }
}

fn quoted(path: &Path) -> Result<String> {
    let text = path.to_str().context("Seatbelt paths must be UTF-8")?;
    if text.chars().any(char::is_control) {
        bail!("Seatbelt paths cannot contain control characters");
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn seatbelt_profile(profile: &SandboxProfile, scratch: &Path) -> Result<String> {
    let workspace = profile.workspace.canonicalize()?;
    let state = profile
        .state_dir
        .canonicalize()
        .context("resolve Latch state directory")?;
    let mut protected = crate::sandbox::secret_paths(&profile.home);
    protected.extend([
        state.clone(),
        profile.home.join("Library/Keychains"),
        profile.home.join(".latch"),
    ]);
    let credential = crate::paths::ResolvedPaths::for_state(&state).secrets_path;
    if let Ok(real) = credential.canonicalize() {
        protected.push(real);
    }
    let mut rules = String::from(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(allow process*)\n(allow sysctl-read)\n(allow file-read*)\n(allow file-write* (literal \"/dev/null\"))\n",
    );
    let mut writable = vec![scratch.to_path_buf()];
    if profile.workspace_writable() {
        writable.push(workspace.clone());
    }
    for root in &profile.external_roots {
        let root = root.canonicalize()?;
        if root.parent().is_none() {
            bail!("cannot authorize filesystem root writes");
        }
        writable.push(root);
    }
    let mut excluded = protected.clone();
    if !profile.git_writable() {
        let dotgit = workspace.join(".git");
        if dotgit.is_file() {
            let pointer = std::fs::read_to_string(&dotgit).context("read worktree .git pointer")?;
            let target = pointer
                .trim()
                .strip_prefix("gitdir:")
                .context("invalid worktree .git pointer")?
                .trim();
            let gitdir = workspace
                .join(target)
                .canonicalize()
                .context("resolve worktree Git directory")?;
            excluded.push(gitdir.clone());
            if gitdir.join("commondir").exists() {
                let common = std::fs::read_to_string(gitdir.join("commondir"))?;
                excluded.push(gitdir.join(common.trim()).canonicalize()?);
            }
        }
    }
    if !profile.git_writable() {
        // Protect metadata in nested repositories as well as worktrees whose
        // .git pointer resolves to an external common directory.
        rules.push_str("(deny file-write* (regex #\"(^|/)\\.git(/|$)\"))\n");
        if let Ok(real) = workspace.join(".git").canonicalize() {
            excluded.push(real);
        }
    }
    for root in writable {
        rules.push_str(&format!(
            "(allow file-write* (require-all (subpath {})",
            quoted(&root)?
        ));
        if !profile.git_writable() {
            rules.push_str(" (require-not (regex #\"(^|/)\\.git(/|$)\"))");
        }
        for path in &excluded {
            let real = path.canonicalize().unwrap_or_else(|_| path.clone());
            rules.push_str(&format!(" (require-not (subpath {}))", quoted(&real)?));
        }
        rules.push_str("))\n");
    }
    for path in protected {
        for path in [path.clone(), path.canonicalize().unwrap_or(path)] {
            rules.push_str(&format!(
                "(deny file-read* file-write* (subpath {}))\n",
                quoted(&path)?
            ));
        }
    }
    // Never expose host Unix sockets, even with network approval.
    if profile.network() {
        rules.push_str("(system-network)\n(allow network-outbound (remote ip \"*:*\"))\n(allow network-inbound (local ip \"*:*\"))\n");
    }
    Ok(rules)
}

pub(super) fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Capability;

    fn fixture() -> (tempfile::TempDir, SandboxProfile) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["workspace/.git", "home/.ssh", "state", "scratch"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        let profile = SandboxProfile::new(
            dir.path().join("workspace"),
            dir.path().join("home"),
            dir.path().join("state"),
            [Capability::WorkspaceRead].into_iter().collect(),
        );
        (dir, profile)
    }

    #[test]
    fn profile_restricts_writes_and_escapes_paths() {
        let (dir, mut profile) = fixture();
        let scratch = dir.path().join("scratch").canonicalize().unwrap();
        let rules = seatbelt_profile(&profile, &scratch).unwrap();
        assert!(rules.contains("(deny default)"));
        assert!(!rules.contains("(allow network"));
        assert!(!rules.contains(&format!(
            "(allow file-write* (require-all (subpath {})",
            quoted(&profile.workspace.canonicalize().unwrap()).unwrap()
        )));
        profile
            .capabilities
            .insert(Capability::WorkspaceSourceWrite);
        let rules = seatbelt_profile(&profile, &scratch).unwrap();
        assert!(rules.contains("(require-not (subpath"));
        assert!(rules.contains(".ssh"));
        assert!(rules.contains("(deny file-write* (regex"));
        assert_eq!(quoted(Path::new("/a\"b\\c")).unwrap(), "\"/a\\\"b\\\\c\"");
        assert!(quoted(Path::new("/a\nb")).is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_boundary_enforces_capabilities_and_child_inheritance() {
        let (dir, mut profile) = fixture();
        let backend = MacosBackend::detect(&profile.workspace).unwrap();
        std::fs::write(profile.home.join(".ssh/key"), "secret").unwrap();
        std::fs::write(profile.state_dir.join("private"), "secret").unwrap();
        for script in ["echo bad > denied", "echo bad > .git/HEAD"] {
            let out = backend
                .command(&profile, script)
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(!out.status.success(), "{script}");
        }
        profile
            .capabilities
            .insert(Capability::WorkspaceSourceWrite);
        let out = backend
            .command(&profile, "echo ok > allowed")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for target in [
            profile.home.join(".ssh/key"),
            profile.state_dir.join("private"),
        ] {
            let out = backend
                .fixed_command(&profile, "/bin/cat", &[target.to_str().unwrap()])
                .unwrap()
                .output()
                .await
                .unwrap();
            assert!(!out.status.success());
            assert!(!String::from_utf8_lossy(&out.stdout).contains("secret"));
        }
        let out = backend
            .command(&profile, "/bin/bash -c 'echo bad > .git/HEAD'")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(!out.status.success());
        profile.capabilities.insert(Capability::GitMetadataWrite);
        assert!(
            backend
                .command(&profile, "echo ok > .git/HEAD")
                .unwrap()
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("workspace/allowed")).unwrap(),
            "ok\n"
        );
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_network_requires_capability_and_external_writes_are_scoped() {
        let (dir, mut profile) = fixture();
        let backend = MacosBackend::detect(&profile.workspace).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let script = format!(
            "echo granted > {}",
            super::super::shell::bash_quote(outside.join("proof").to_str().unwrap())
        );
        assert!(
            !backend
                .command(&profile, &script)
                .unwrap()
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        profile.external_roots.push(outside.clone());
        assert!(
            backend
                .command(&profile, &script)
                .unwrap()
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("proof")).unwrap(),
            "granted\n"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connect = format!("/usr/bin/nc -z -G 2 127.0.0.1 {port}");
        assert!(
            !backend
                .command(&profile, &connect)
                .unwrap()
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        profile.capabilities.insert(Capability::NetworkAccess);
        assert!(
            backend
                .command(&profile, &connect)
                .unwrap()
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
    }
}
