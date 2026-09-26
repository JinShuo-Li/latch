use crate::sandbox::SandboxProfile;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Git Bash discovery is separate from the mandatory Windows security probe.
/// Finding a shell must never be interpreted as having established a sandbox.
#[derive(Debug, Clone)]
pub struct WindowsBackend {
    bash: PathBuf,
}

impl WindowsBackend {
    pub fn detect(_workspace: &Path) -> Result<Self> {
        let bash = discover_git_bash()?;
        bail!(
            "Git for Windows Bash was found at {}, but the Windows execution sandbox is unavailable; Latch refuses to execute commands without capability enforcement",
            bash.display()
        )
    }

    pub fn command(&self, _profile: &SandboxProfile, _command: &str) -> Result<Command> {
        bail!("Windows execution sandbox is unavailable; command refused")
    }

    pub fn fixed_command(
        &self,
        _profile: &SandboxProfile,
        _program: &str,
        _args: &[&str],
    ) -> Result<Command> {
        bail!("Windows execution sandbox is unavailable; command refused")
    }

    pub fn status(&self) -> String {
        format!("windows/Git Bash: {}", self.bash.display())
    }
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
}
