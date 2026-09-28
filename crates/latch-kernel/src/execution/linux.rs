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
        format!("linux/bubblewrap: {} ready", self.0.bwrap().display())
    }
}

/// Best-effort SIGKILL for a sandbox leader's direct children. The namespace
/// init is a direct child; signalling it before killing the leader prevents a
/// mid-setup init from surviving as an orphan that holds the output pipes.
/// Uses the system `kill` utility so no unsafe signal call is needed; if the
/// utility is unavailable the bounded drain still bounds teardown.
pub(super) fn kill_direct_children(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return;
    };
    for task in tasks.flatten() {
        let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        for child in children.split_whitespace() {
            let _ = std::process::Command::new("kill")
                .args(["-9", child])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}
