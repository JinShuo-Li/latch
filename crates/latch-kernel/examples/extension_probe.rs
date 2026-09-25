use anyhow::{Context, Result};
use latch_kernel::config::Config;
use latch_kernel::extension::ExtensionHost;
use latch_kernel::sandbox::{Capability, CapabilitySet, SandboxProfile, SandboxRunner};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let script = std::env::args()
        .nth(1)
        .context("usage: extension_probe <compiled-extension.js>")?;
    let workspace = std::env::current_dir()?.canonicalize()?;
    let state_dir = Config::default().state_dir;
    std::fs::create_dir_all(&state_dir)?;
    let runner = SandboxRunner::detect(&workspace)?;
    let profile = SandboxProfile::new(
        workspace.clone(),
        dirs::home_dir().context("resolve home directory")?,
        state_dir,
        [
            Capability::WorkspaceRead,
            Capability::NetworkAccess,
            Capability::ExtensionExecution,
        ]
        .into_iter()
        .collect::<CapabilitySet>(),
    );
    let mut host = ExtensionHost::start(
        "typescript-example".into(),
        "node",
        &[script],
        &workspace.to_string_lossy(),
        (&runner, &profile),
    )
    .await?;
    let result = host
        .execute_tool(
            "example.echo",
            json!({"value":"latch-extension-ok"}),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    println!("{result}");
    host.shutdown().await
}
