//! Platform-neutral command boundary. Capability policy is expressed by
//! `SandboxProfile`; each backend must enforce it before returning a command.

use crate::sandbox::SandboxProfile;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

#[cfg(target_os = "linux")]
mod linux;
mod shell;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_runtime;
#[cfg(windows)]
mod windows_runtime_tools;

pub(crate) use shell::is_read_only_shell;

/// Model-facing syntax belongs to the selected execution platform, not the
/// tool dispatcher. New shells supply their own policy and guidance here.
pub(crate) fn shell_description() -> &'static str {
    shell::shell_description()
}

pub(crate) fn process_description() -> &'static str {
    shell::process_description()
}

pub(crate) fn shell_guidance() -> &'static str {
    shell::shell_guidance()
}

pub(crate) fn shell_executable_is_unmodelled(token: &str) -> bool {
    shell::executable_is_unmodelled(token)
}

/// The Linux namespace leader can leave its setup child holding output pipes
/// after interruption. The Windows runner's Job Object owns descendants.
pub(crate) fn kill_sandbox_children(pid: Option<u32>) {
    #[cfg(target_os = "linux")]
    linux::kill_direct_children(pid);
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

/// Probe the search program using the same platform discovery rules used by
/// fixed commands. This never starts an unsandboxed search in production.
pub(crate) fn search_runtime_error() -> Option<String> {
    #[cfg(windows)]
    {
        (!windows_runtime_tools::has_ripgrep()).then(|| {
            "ripgrep (`rg.exe`) is required by the search tool but was not found on PATH; install ripgrep and restart Latch".to_owned()
        })
    }
    #[cfg(not(windows))]
    {
        use std::process::{Command, Stdio};
        match Command::new("rg")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!(
                "ripgrep (`rg`) is required by the search tool, but `rg --version` failed with {status}"
            )),
            Err(error) => Some(format!(
                "ripgrep (`rg`) is required by the search tool but was not found on PATH ({error}); \
                 install ripgrep and restart Latch"
            )),
        }
    }
}

/// On Windows, `rg` can report inaccessible entries with code 2 after
/// returning valid matches. Preserve those matches and mark the result partial.
pub(crate) fn search_result_is_partial(status: &std::process::ExitStatus, stdout: &[u8]) -> bool {
    cfg!(windows) && status.code() == Some(2) && !stdout.is_empty()
}

#[cfg(windows)]
fn windows_secret_paths(home: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = crate::sandbox::secret_paths(home);
    for relative in [
        ".latch",
        "AppData/Roaming/Microsoft/Credentials",
        "AppData/Local/Microsoft/Credentials",
        "AppData/Local/Microsoft/Vault",
        "AppData/Roaming/gh",
        "AppData/Roaming/gcloud",
    ] {
        let path = home.join(relative);
        if path.exists() {
            paths.push(path);
        }
    }
    paths
}

#[derive(Debug, Clone)]
pub enum ExecutionBackend {
    #[cfg(target_os = "linux")]
    Linux(linux::LinuxBackend),
    #[cfg(windows)]
    Windows(windows::WindowsBackend),
}

impl ExecutionBackend {
    /// Read-only prerequisite check for `doctor`. Installation and a sandbox
    /// launch happen only when a task actually starts.
    pub fn inspect(workspace: &Path) -> Result<String> {
        #[cfg(windows)]
        {
            let _ = workspace;
            windows::WindowsBackend::inspect()
        }
        #[cfg(not(windows))]
        {
            Ok(Self::detect(workspace)?.status())
        }
    }

    pub fn detect(workspace: &Path) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self::Linux(linux::LinuxBackend::detect(workspace)?))
        }
        #[cfg(windows)]
        {
            Ok(Self::Windows(windows::WindowsBackend::detect(workspace)?))
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        anyhow::bail!("Latch has no execution backend for this operating system")
    }

    pub fn command(&self, profile: &SandboxProfile, command: &str) -> Result<Command> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(backend) => backend.command(profile, command),
            #[cfg(windows)]
            Self::Windows(backend) => backend.command(profile, command),
        }
    }

    /// Run a fixed inspection program through the same capability boundary as
    /// model shell commands. Each backend owns its argument encoding.
    pub fn fixed_command(
        &self,
        profile: &SandboxProfile,
        program: &str,
        args: &[&str],
    ) -> Result<Command> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(backend) => backend.fixed_command(profile, program, args),
            #[cfg(windows)]
            Self::Windows(backend) => backend.fixed_command(profile, program, args),
        }
    }

    pub fn status(&self) -> String {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(backend) => backend.status(),
            #[cfg(windows)]
            Self::Windows(backend) => backend.status(),
        }
    }
}
