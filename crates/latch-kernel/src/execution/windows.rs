use super::windows_runtime::NativeRuntime;
use crate::sandbox::SandboxProfile;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct WindowsBackend {
    native: NativeRuntime,
}

impl WindowsBackend {
    pub fn inspect() -> Result<String> {
        let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
            anyhow::anyhow!("LOCALAPPDATA is required for the native Windows runtime")
        })?;
        anyhow::ensure!(
            std::path::Path::new(&local).is_dir(),
            "LOCALAPPDATA must be an existing directory"
        );
        let system = std::env::var_os("SystemRoot")
            .ok_or_else(|| anyhow::anyhow!("SystemRoot is required on Windows"))?;
        anyhow::ensure!(
            Path::new(&system).join("System32/cmd.exe").is_file(),
            "native cmd.exe is required"
        );
        Ok("windows/native AppContainer + Job Object (cmd.exe); assets embedded".to_owned())
    }

    pub fn detect(_workspace: &Path) -> Result<Self> {
        let root = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!("LOCALAPPDATA is required for the native Windows runtime")
            })?
            .join("Latch")
            .join("runtime");
        Ok(Self {
            native: NativeRuntime::install(&root)?,
        })
    }

    pub fn command(&self, profile: &SandboxProfile, command: &str) -> Result<Command> {
        self.native.command(profile, command)
    }

    pub fn fixed_command(
        &self,
        profile: &SandboxProfile,
        program: &str,
        args: &[&str],
    ) -> Result<Command> {
        self.native.fixed_command(profile, program, args)
    }

    pub fn status(&self) -> String {
        "windows/native AppContainer + Job Object (cmd.exe)".to_owned()
    }
}
