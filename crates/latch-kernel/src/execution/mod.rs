//! Platform-neutral command boundary. Capability policy is expressed by
//! `SandboxProfile`; each backend must enforce it before returning a command.

use crate::sandbox::SandboxProfile;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(all(windows, test))]
mod windows_runtime;

#[cfg(all(windows, test))]
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
    /// model shell commands. Linux quotes for Bash; Windows delegates native
    /// argument handling to its backend. Inspection arguments cannot become
    /// additional shell commands.
    pub fn fixed_command(
        &self,
        profile: &SandboxProfile,
        program: &str,
        args: &[&str],
    ) -> Result<Command> {
        #[cfg(windows)]
        {
            let Self::Windows(backend) = self;
            backend.fixed_command(profile, program, args)
        }
        #[cfg(not(windows))]
        {
            let script = std::iter::once(program)
                .chain(args.iter().copied())
                .map(bash_quote)
                .collect::<Vec<_>>()
                .join(" ");
            self.command(profile, &format!("exec {script}"))
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

#[cfg(any(not(windows), test))]
fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::bash_quote;

    #[test]
    fn fixed_arguments_cannot_escape_bash_quotes() {
        assert_eq!(bash_quote("a'b; touch outside"), "'a'\\''b; touch outside'");
    }
}
