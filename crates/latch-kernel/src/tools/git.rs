//! Workspace Git inspection tools.

use super::*;

impl ToolExecutor {
    pub(super) async fn git_status(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let profile = self.sandbox_profile(call);
        let runner = self.sandbox_runner()?;
        let output = runner
            .command(&profile, "git status --short --branch; git diff --stat")?
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "git status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok((String::from_utf8_lossy(&output.stdout).into_owned(), None))
    }
    pub(super) async fn git_diff(&self, call: &ToolCall) -> Result<(String, Option<String>)> {
        let profile = self.sandbox_profile(call);
        let runner = self.sandbox_runner()?;
        let output = runner
            .command(&profile, "git diff --no-ext-diff --")?
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        self.bound_output(String::from_utf8_lossy(&output.stdout).into_owned(), "diff")
    }
}
