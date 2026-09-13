//! Opt-in live dogfood benchmark: runs the same trivial fix with a real
//! provider and reports concrete inference metrics per reasoning effort.
//!
//! Not part of CI or `cargo test --workspace` (it is `#[ignore]`d and refuses
//! to run without `LATCH_LIVE_TESTS=1`). Run with:
//!
//! ```sh
//! LATCH_LIVE_TESTS=1 cargo test -p latch-kernel --test live_benchmark -- --ignored --nocapture
//! ```
//!
//! Optional selectors:
//! - `LATCH_BENCH_PROVIDER` (default: configured default provider)
//! - `LATCH_BENCH_MODEL` (default: provider default model)
//! - `LATCH_BENCH_EFFORTS` (default: `low,high`; `max` where advertised)

use latch_kernel::config::PermissionConfig;
use latch_kernel::{
    Agent, AgentRuntime, Config, ContinuityEngine, CredentialStore, EventStore, PolicyEngine,
    ProviderRegistry, ToolExecutor,
};
use latch_protocol::{EventPayload, InferenceProfile, Mode, ReasoningEffort, Usage};
use serde_json::json;
use std::path::Path;
use std::time::Instant;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default, serde::Serialize)]
struct RunMetrics {
    provider: String,
    model: String,
    effort: String,
    wall_clock_ms: u128,
    model_requests: u64,
    tool_calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
    cache_miss_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    /// Cumulative over every request in the run.
    reasoning_replay_tokens: usize,
    tool_argument_tokens: usize,
    tool_result_tokens: usize,
    last_reasoning_replay_tokens: usize,
    files_read: u64,
    files_changed: u64,
    validation_commands: u64,
    completion: String,
    verify_failed: bool,
}

fn fixture(workspace: &Path) {
    std::fs::create_dir_all(workspace).unwrap();
    std::fs::write(
        workspace.join("calc.py"),
        "def add(a, b):\n    return a - b\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("test_calc.py"),
        "import unittest\nfrom calc import add\n\nclass TestCalc(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\nif __name__ == '__main__':\n    unittest.main()\n",
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "live provider benchmark; requires LATCH_LIVE_TESTS=1 and network credentials"]
async fn live_trivial_fix_by_effort() {
    if std::env::var("LATCH_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set LATCH_LIVE_TESTS=1 to run the live benchmark");
        return;
    }
    let config = Config::load(None).expect("config");
    let registry = ProviderRegistry::from_config(&config).expect("registry");
    let credentials = CredentialStore::open(CredentialStore::default_path(&config.state_dir))
        .expect("credentials");

    let requested_efforts: Vec<ReasoningEffort> = std::env::var("LATCH_BENCH_EFFORTS")
        .unwrap_or_else(|_| "low,high".into())
        .split(',')
        .filter_map(|raw| raw.trim().parse::<ReasoningEffort>().ok())
        .collect();
    let mut reports = Vec::new();
    for effort in requested_efforts {
        match run_once(&config, &registry, &credentials, effort).await {
            Ok(metrics) => reports.push(metrics),
            Err(error) => eprintln!("benchmark run failed at {effort:?}: {error:#}"),
        }
    }

    let report = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "runs": reports,
    });
    // Reports land in the workspace target directory regardless of the
    // package-relative test working directory.
    let dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/live-benchmark");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!(
        "bench-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    println!("live benchmark report: {}", path.display());
    for run in report["runs"].as_array().unwrap_or(&Vec::new()) {
        println!(
            "{} · {} · {} | {} ms | {} req | {} tools | in {} out {} | cache read {} miss {} | replay {} | changed {}",
            run["provider"].as_str().unwrap_or(""),
            run["model"].as_str().unwrap_or(""),
            run["effort"].as_str().unwrap_or(""),
            run["wall_clock_ms"].as_u64().unwrap_or(0),
            run["model_requests"].as_u64().unwrap_or(0),
            run["tool_calls"].as_u64().unwrap_or(0),
            run["input_tokens"].as_u64().unwrap_or(0),
            run["output_tokens"].as_u64().unwrap_or(0),
            run["cache_read_tokens"]
                .as_u64()
                .map_or("—".into(), |v| v.to_string()),
            run["cache_miss_tokens"]
                .as_u64()
                .map_or("—".into(), |v| v.to_string()),
            run["reasoning_replay_tokens"].as_u64().unwrap_or(0),
            run["files_changed"].as_u64().unwrap_or(0),
        );
    }
}

async fn run_once(
    config: &Config,
    registry: &ProviderRegistry,
    credentials: &CredentialStore,
    effort: ReasoningEffort,
) -> anyhow::Result<RunMetrics> {
    let provider_id = std::env::var("LATCH_BENCH_PROVIDER")
        .unwrap_or_else(|_| registry.default_provider().id.to_string());
    let model = std::env::var("LATCH_BENCH_MODEL").unwrap_or_else(|_| {
        registry
            .provider(&provider_id)
            .map(|provider| provider.default_model.clone())
            .unwrap_or_default()
    });
    let dir = tempdir()?;
    let workspace = dir.path().join("calc");
    fixture(&workspace);
    let store = EventStore::open_memory()?;
    let session = store.create_session(&workspace)?;
    let (profile, descriptor) =
        registry.resolve_profile(&InferenceProfile::new(provider_id, model, effort))?;
    let provider = registry.build_provider(&profile, &descriptor, credentials, session)?;
    let tools = ToolExecutor::new(
        workspace.clone(),
        dir.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(Mode::Work, workspace.clone(), PermissionConfig::default()),
    )?;
    let continuity =
        ContinuityEngine::for_model(store.clone(), config.context.clone(), &profile.model);
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode: Mode::Work,
        store: store.clone(),
        provider: provider.clone(),
        tools,
        continuity,
        retry_budget: config.failure.retry_budget,
    });
    agent.restore_inference_profile(
        provider,
        profile.clone(),
        &descriptor,
        config.context.clone(),
    );

    let started = Instant::now();
    agent
        .run(
            "Fix the bug in calc.py and verify it.",
            CancellationToken::new(),
            std::sync::Arc::new(|_| {}),
        )
        .await?;
    let wall_clock_ms = started.elapsed().as_millis();

    let events = store.events(session)?;
    let mut metrics = RunMetrics {
        provider: profile.provider.to_string(),
        model: profile.model.clone(),
        effort: profile.effort.label().to_owned(),
        wall_clock_ms,
        completion: format!("{:?}", agent.state().completion),
        ..Default::default()
    };
    let mut usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cache_miss_tokens: None,

        reasoning_tokens: None,
    };
    for event in &events {
        match &event.payload {
            EventPayload::ModelRequestStarted { .. } => metrics.model_requests += 1,
            EventPayload::ToolRequested { call } => {
                metrics.tool_calls += 1;
                if call.name == "read_file" {
                    metrics.files_read += 1;
                }
                if call.name == "validate" {
                    metrics.validation_commands += 1;
                }
            }
            EventPayload::FileChanged { .. } => metrics.files_changed += 1,
            EventPayload::ValidationResult { .. } => {}
            EventPayload::ModelUsage { usage: item } => {
                usage.input_tokens = usage.input_tokens.saturating_add(item.input_tokens);
                usage.output_tokens = usage.output_tokens.saturating_add(item.output_tokens);
                usage.cache_read_tokens = sum_opt(usage.cache_read_tokens, item.cache_read_tokens);
                usage.cache_miss_tokens = sum_opt(usage.cache_miss_tokens, item.cache_miss_tokens);
                usage.cache_write_tokens =
                    sum_opt(usage.cache_write_tokens, item.cache_write_tokens);
                usage.reasoning_tokens = sum_opt(usage.reasoning_tokens, item.reasoning_tokens);
            }
            EventPayload::ContextMaterialized { stats } => {
                // One event per request: accumulate run totals, keep the last
                // request visible separately.
                metrics.reasoning_replay_tokens += stats.reasoning_replay_tokens;
                metrics.tool_argument_tokens += stats.tool_arguments_tokens;
                metrics.tool_result_tokens += stats.tool_result_tokens;
                metrics.last_reasoning_replay_tokens = stats.reasoning_replay_tokens;
            }
            _ => {}
        }
    }
    metrics.input_tokens = usage.input_tokens;
    metrics.output_tokens = usage.output_tokens;
    metrics.cache_read_tokens = usage.cache_read_tokens;
    metrics.cache_miss_tokens = usage.cache_miss_tokens;
    metrics.cache_write_tokens = usage.cache_write_tokens;
    metrics.reasoning_tokens = usage.reasoning_tokens;
    let fixed = std::fs::read_to_string(workspace.join("calc.py"))?.contains("a + b");
    metrics.verify_failed = !fixed || metrics.tool_calls == 0;
    Ok(metrics)
}

fn sum_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}
