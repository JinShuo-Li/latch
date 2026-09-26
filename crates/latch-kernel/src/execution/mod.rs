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
            return Ok(Self::Linux(linux::LinuxBackend::detect(workspace)?));
        }
        #[cfg(windows)]
        {
            return Ok(Self::Windows(windows::WindowsBackend::detect(workspace)?));
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

    pub fn status(&self) -> String {
        match self {
            #[cfg(target_os = "linux")]
            Self::Linux(backend) => backend.status(),
            #[cfg(windows)]
            Self::Windows(backend) => backend.status(),
        }
    }
}
