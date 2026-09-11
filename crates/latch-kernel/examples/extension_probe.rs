use anyhow::{Context, Result};
use latch_kernel::extension::ExtensionHost;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let script = std::env::args()
        .nth(1)
        .context("usage: extension_probe <compiled-extension.js>")?;
    let mut host = ExtensionHost::start(
        "typescript-example".into(),
        "node",
        &[script],
        &std::env::current_dir()?.to_string_lossy(),
        None,
    )
    .await?;
    let result = host
        .execute_tool("example.echo", json!({"value":"latch-extension-ok"}))
        .await?;
    println!("{result}");
    host.shutdown().await
}
