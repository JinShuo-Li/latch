//! Operator diagnostics; no inference provider is contacted.
use super::command::Args;
use anyhow::Result;
use latch_kernel::{
    Config,
    execution::ExecutionBackend,
    mcp::{McpClient, McpTransportConfig},
    sandbox::{Capability, CapabilitySet, SandboxProfile},
    skills::SkillCatalog,
};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;
pub fn skills() -> Result<ExitCode> {
    let catalog = SkillCatalog::discover(&std::env::current_dir()?, dirs::home_dir().as_deref());
    for skill in catalog.skills.values() {
        println!(
            "{}\t{}\t{}",
            skill.metadata.name,
            skill.metadata.description,
            skill.root.display()
        );
    }
    for error in &catalog.diagnostics {
        eprintln!("{error}");
    }
    Ok(ExitCode::from(u8::from(!catalog.diagnostics.is_empty())))
}
pub async fn mcp(args: &Args, check: bool) -> Result<ExitCode> {
    let config = Config::load(args.config.as_deref())?;
    for server in &config.mcp_servers {
        server.validate()?;
    }
    if !check {
        for server in &config.mcp_servers {
            println!(
                "{}\t{}\t{}",
                server.name,
                if server.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                match server.transport {
                    McpTransportConfig::Stdio { .. } => "stdio",
                    McpTransportConfig::StreamableHttp { .. } => "streamable_http",
                }
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    let workspace = std::env::current_dir()?;
    let backend = ExecutionBackend::detect(&workspace)?;
    std::fs::create_dir_all(&config.state_dir)?;
    let profile = SandboxProfile::new(
        workspace.clone(),
        dirs::home_dir().unwrap_or(workspace),
        config.state_dir,
        [
            Capability::WorkspaceRead,
            Capability::NetworkAccess,
            Capability::ExtensionExecution,
        ]
        .into_iter()
        .collect::<CapabilitySet>(),
    );
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let watcher = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let mut failed = false;
    for server in config.mcp_servers.iter().filter(|s| s.enabled) {
        match McpClient::connect(server, (&backend, &profile), &cancel).await {
            Ok(mut client) => {
                println!(
                    "{}: protocol {}, {} tools",
                    client.name,
                    client.version,
                    client.definitions().len()
                );
                if let Err(error) = client.shutdown().await {
                    eprintln!("{}: {error:#}", server.name);
                    failed = true;
                }
            }
            Err(error) => {
                eprintln!("{}: {error:#}", server.name);
                failed = true;
            }
        }
    }
    watcher.abort();
    Ok(ExitCode::from(u8::from(failed)))
}
