use crate::sandbox::{SandboxProfile, SandboxRunner};
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct LinuxBackend(SandboxRunner);

impl LinuxBackend {
    pub fn detect(workspace: &Path) -> Result<Self> {
        Ok(Self(SandboxRunner::detect(workspace)?))
    }

    pub fn command(&self, profile: &SandboxProfile, command: &str) -> Result<Command> {
        self.0.command(profile, command)
    }

    pub fn status(&self) -> String {
        format!("linux/bubblewrap: {} ready", self.0.bwrap().display())
    }
}
