use crate::sandbox::{Capability, SandboxProfile};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const RUNNER_BYTES: &[u8] = include_bytes!(concat!(
    env!("LATCH_WINDOWS_NATIVE_DIR"),
    "/latch-windows-runner.exe"
));
const GUARD_BYTES: &[u8] = include_bytes!(concat!(
    env!("LATCH_WINDOWS_NATIVE_DIR"),
    "/msys-token-guard.exe"
));
const HOOK_BYTES: &[u8] = include_bytes!(concat!(
    env!("LATCH_WINDOWS_NATIVE_DIR"),
    "/msys-token-guard-hook.dll"
));
const REPARSE_POINT: u32 = 0x400;

/// Git Bash discovery is separate from the mandatory Windows security probe.
/// Finding a shell must never be interpreted as having established a sandbox.
#[derive(Debug, Clone)]
pub struct WindowsBackend {
    bash: PathBuf,
    toolchain: Option<PathBuf>,
}

impl WindowsBackend {
    pub fn detect(_workspace: &Path) -> Result<Self> {
        let bash = discover_git_bash()?;
        Ok(Self {
            bash,
            toolchain: discover_rust_toolchain(),
        })
    }

    pub fn command(&self, profile: &SandboxProfile, command: &str) -> Result<Command> {
        if profile.capabilities.any(&[
            Capability::PrivilegedOperation,
            Capability::DestructiveOperation,
        ]) {
            bail!("Windows execution refuses privileged or destructive capabilities");
        }
        let (runner, guard, native_dir) = prepare_native()?;
        let workspace = fs::canonicalize(&profile.workspace)
            .with_context(|| format!("resolve workspace {}", profile.workspace.display()))?;
        ensure_no_reparse(&workspace)?;
        let scratch = std::env::temp_dir().join(format!("latch-sandbox-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&scratch)
            .with_context(|| format!("create sandbox scratch {}", scratch.display()))?;
        ensure_no_reparse(&scratch)?;
        let mut child = Command::new(runner);
        child.arg(&guard).arg(&self.bash).arg(&workspace);
        child.arg(if profile.workspace_writable() {
            "write"
        } else {
            "read"
        });
        child.arg(command);
        if profile.git_writable() {
            child.arg("--git-write");
        }
        child.arg("--read-root").arg(&native_dir);
        child.arg("--write-root").arg(&scratch);
        if profile.state_dir.is_dir() {
            child.arg("--deny-read-root").arg(&profile.state_dir);
        }
        for path in &profile.external_roots {
            let root = fs::canonicalize(path)
                .with_context(|| format!("resolve external grant {}", path.display()))?;
            ensure_no_reparse(&root)?;
            child.arg("--write-root").arg(root);
        }
        if let Some(toolchain) = &self.toolchain {
            child.arg("--read-root").arg(toolchain);
        }
        child.current_dir(&workspace).env_clear();
        for name in [
            "SystemRoot",
            "WINDIR",
            "PATH",
            "PATHEXT",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "COMSPEC",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
        ] {
            if let Some(value) = std::env::var_os(name) {
                child.env(name, value);
            }
        }
        if let Some(toolchain) = &self.toolchain {
            let bin = toolchain.join("bin");
            let mut paths = vec![bin];
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            child.env(
                "PATH",
                std::env::join_paths(paths).context("build Windows agent PATH")?,
            );
        }
        child
            .env("TEMP", &scratch)
            .env("TMP", &scratch)
            .env("CARGO_HOME", scratch.join("cargo"))
            .env("HOME", &scratch)
            .stdin(Stdio::null());
        Ok(child)
    }

    pub fn status(&self) -> String {
        format!(
            "windows/restricted-token preview, Job Object, Git Bash: {}",
            self.bash.display()
        )
    }
}

fn discover_rust_toolchain() -> Option<PathBuf> {
    let output = std::process::Command::new("rustup")
        .args(["which", "cargo"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let cargo = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    let root = cargo.canonicalize().ok()?.parent()?.parent()?.to_path_buf();
    root.join("bin/cargo.exe").is_file().then_some(root)
}

fn ensure_no_reparse(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Windows path {}", path.display()))?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        bail!(
            "Windows sandbox refuses a reparse point at {}",
            path.display()
        );
    }
    Ok(())
}

fn prepare_native() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let cache = dirs::cache_dir().context("Windows cache directory is unavailable")?;
    let mut digest = Sha256::new();
    digest.update(RUNNER_BYTES);
    digest.update(GUARD_BYTES);
    digest.update(HOOK_BYTES);
    let hash = hex::encode(digest.finalize());
    let native = cache
        .join("latch")
        .join(format!("windows-native-{}", &hash[..16]));
    fs::create_dir_all(&native)
        .with_context(|| format!("create Windows native runtime {}", native.display()))?;
    ensure_no_reparse(&native)?;
    let runner = verified_artifact(&native, "latch-windows-runner.exe", RUNNER_BYTES)?;
    let guard = verified_artifact(&native, "msys-token-guard.exe", GUARD_BYTES)?;
    verified_artifact(&native, "msys-token-guard-hook.dll", HOOK_BYTES)?;
    Ok((runner, guard, native))
}

fn verified_artifact(directory: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let path = directory.join(name);
    if path.exists() {
        ensure_no_reparse(&path)?;
        if fs::read(&path)? != bytes {
            bail!(
                "Windows native runtime artifact changed: {}",
                path.display()
            );
        }
        return Ok(path);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create Windows native runtime artifact {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(path)
}

fn discover_git_bash() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("LATCH_BASH") {
        let path = PathBuf::from(explicit);
        return verify_git_bash(&path).context("invalid LATCH_BASH");
    }
    let mut candidates = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(program_files).join("Git/bin/bash.exe"));
    }
    candidates.push(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"));
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local_app_data).join("Programs/Git/bin/bash.exe"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join("bash.exe")));
    }
    let path = candidates
        .into_iter()
        .find(|path| is_git_bash(path))
        .ok_or_else(|| anyhow::anyhow!(
            "Git for Windows bin\\bash.exe is required. Install Git for Windows or set LATCH_BASH to its bin\\bash.exe; WSL Bash is unsupported"
        ))?;
    verify_git_bash(&path)
}

fn is_git_bash(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("bash.exe"))
        && path.parent().is_some_and(|parent| {
            parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
                && parent.parent().is_some_and(|root| {
                    root.join("cmd/git.exe").is_file()
                        && root.join("usr/bin/msys-2.0.dll").is_file()
                })
        })
}

fn verify_git_bash(path: &Path) -> Result<PathBuf> {
    if !is_git_bash(path) {
        bail!(
            "{} is not a Git for Windows bin\\bash.exe installation",
            path.display()
        );
    }
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("resolve Git Bash at {}", path.display()))?;
    let output = std::process::Command::new(&path)
        .args(["--noprofile", "--norc", "-c", "printf LATCH_GIT_BASH_OK"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("probe Git Bash at {}", path.display()))?;
    if !output.status.success() || output.stdout != b"LATCH_GIT_BASH_OK" {
        bail!("Git Bash at {} failed its startup probe", path.display());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CapabilitySet;

    #[test]
    fn only_git_for_windows_layout_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("Git");
        std::fs::create_dir_all(git.join("bin")).unwrap();
        std::fs::create_dir_all(git.join("cmd")).unwrap();
        std::fs::create_dir_all(git.join("usr/bin")).unwrap();
        let bash = git.join("bin/bash.exe");
        std::fs::write(&bash, "").unwrap();
        assert!(!is_git_bash(&bash));
        std::fs::write(git.join("cmd/git.exe"), "").unwrap();
        std::fs::write(git.join("usr/bin/msys-2.0.dll"), "").unwrap();
        assert!(is_git_bash(&bash));
        let wsl = dir.path().join("WindowsApps/bash.exe");
        std::fs::create_dir_all(wsl.parent().unwrap()).unwrap();
        std::fs::write(&wsl, "").unwrap();
        assert!(!is_git_bash(&wsl));
    }

    #[tokio::test]
    async fn restricted_git_bash_enforces_workspace_write() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let state = directory.path().join("state");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(workspace.join("read.txt"), "WINDOWS_BACKEND_READ").unwrap();
        fs::write(state.join("secrets.toml"), "PRIVATE_BACKEND_SECRET").unwrap();
        let git_init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(git_init.success());
        let backend = WindowsBackend::detect(&workspace).unwrap();
        let read = SandboxProfile::new(
            workspace.clone(),
            directory.path().to_path_buf(),
            state.clone(),
            CapabilitySet::from_iter([Capability::WorkspaceRead]),
        );
        let output = backend
            .command(&read, "cat read.txt")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"WINDOWS_BACKEND_READ");
        let secret_command = format!(
            "powershell.exe -NoProfile -Command 'Get-Content -LiteralPath \"{}\"'",
            state.join("secrets.toml").display()
        );
        let output = backend
            .command(&read, &secret_command)
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE_BACKEND_SECRET"));
        let output = backend
            .command(&read, "printf forbidden > blocked.txt")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(!workspace.join("blocked.txt").exists());
        let write = SandboxProfile::new(
            workspace.clone(),
            directory.path().to_path_buf(),
            state,
            CapabilitySet::from_iter([Capability::WorkspaceSourceWrite]),
        );
        let output = backend
            .command(&write, "printf allowed > allowed.txt")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(workspace.join("allowed.txt")).unwrap(), b"allowed");
        let output = backend
            .command(&write, "git config --local latch.proof denied")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        let git_write = SandboxProfile::new(
            workspace.clone(),
            directory.path().to_path_buf(),
            directory.path().join("state"),
            CapabilitySet::from_iter([
                Capability::WorkspaceSourceWrite,
                Capability::GitMetadataWrite,
            ]),
        );
        let output = backend
            .command(&git_write, "git config --local latch.proof allowed")
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn killing_runner_terminates_grandchild() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let state = directory.path().join("state");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&state).unwrap();
        let backend = WindowsBackend::detect(&workspace).unwrap();
        let write = SandboxProfile::new(
            workspace.clone(),
            directory.path().to_path_buf(),
            state,
            CapabilitySet::from_iter([Capability::WorkspaceSourceWrite]),
        );
        let pid_file = workspace.join("child.pid");
        let script = format!(
            "powershell.exe -NoProfile -Command '$p = Start-Process -FilePath ping.exe -ArgumentList \"-n 60 127.0.0.1\" -PassThru -WindowStyle Hidden; Set-Content -LiteralPath \"{}\" -Value $p.Id; Wait-Process -Id $p.Id'",
            pid_file.display()
        );
        let mut child = backend
            .command(&write, &script)
            .unwrap()
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut pid = None;
        for _ in 0..100 {
            if let Ok(value) = fs::read_to_string(&pid_file) {
                pid = value.trim().parse::<u32>().ok();
                if pid.is_some() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let pid = pid.expect("grandchild wrote its Windows PID");
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let query = format!(
            "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}"
        );
        for _ in 0..50 {
            let gone = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-Command", &query])
                .status()
                .unwrap()
                .success();
            if gone {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("Windows grandchild {pid} survived runner termination");
    }

    #[tokio::test]
    #[ignore = "security regression: Users-readable files outside the workspace are currently exposed"]
    async fn users_readable_private_file_outside_workspace_is_denied() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let private = directory.path().join("private.txt");
        fs::write(&private, "USERS_READABLE_SECRET").unwrap();
        let acl = std::process::Command::new("icacls")
            .arg(&private)
            .args(["/grant", "*S-1-5-32-545:(R)"])
            .status()
            .unwrap();
        assert!(acl.success());
        let backend = WindowsBackend::detect(&workspace).unwrap();
        let read = SandboxProfile::new(
            workspace,
            directory.path().to_path_buf(),
            directory.path().join("state"),
            CapabilitySet::from_iter([Capability::WorkspaceRead]),
        );
        let script = format!(
            "powershell.exe -NoProfile -Command 'Get-Content -LiteralPath \"{}\"'",
            private.display()
        );
        let output = backend
            .command(&read, &script)
            .unwrap()
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("USERS_READABLE_SECRET"));
    }
}
