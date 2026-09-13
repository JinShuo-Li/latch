//! Opt-in live-model acceptance harness.
//!
//! These tests are `#[ignore]`d and additionally gated on `LATCH_LIVE_TESTS=1`.
//! They call the configured provider for real and are never part of ordinary
//! `cargo test`. Run one scenario with:
//!
//!     LATCH_LIVE_TESTS=1 cargo test -p latch-kernel --test live_acceptance \
//!         -- --ignored --nocapture
//!
//! Select a subset with `LATCH_LIVE_SCENARIO=small_bug` (comma-separated
//! names). Reports are written to `target/live-acceptance/<scenario>.json`.
//!
//! The scripted FakeProvider dogfood tests remain the authoritative kernel
//! invariant suite; this harness answers whether an actual model can complete
//! real coding tasks under the real policy, tools, and validation loop.

use latch_kernel::config::{ContextConfig, DEFAULT_CONTEXT_WINDOW_TOKENS, PermissionConfig};
use latch_kernel::{
    Agent, AgentRuntime, Config, ContinuityEngine, CredentialStore, EventStore, ModelDescriptor,
    ModelProvider, PolicyEngine, ProviderRegistry, ToolExecutor,
};
use latch_protocol::{CompletionState, Event, EventPayload, InferenceProfile, Mode, Usage};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn enabled() -> bool {
    std::env::var("LATCH_LIVE_TESTS").is_ok_and(|value| value == "1")
}

fn selected(name: &str) -> bool {
    let filter = std::env::var("LATCH_LIVE_SCENARIO").unwrap_or_default();
    filter.is_empty() || filter.split(',').any(|candidate| candidate.trim() == name)
}

fn live_config() -> Config {
    Config::load(None).expect("config")
}

/// Resolves the live provider, profile, and descriptor through the same
/// registry the CLI uses, so the acceptance harness can never drift from the
/// production provider construction path.
fn live_provider(
    config: &Config,
    session: uuid::Uuid,
) -> (Arc<dyn ModelProvider>, InferenceProfile, ModelDescriptor) {
    let registry = ProviderRegistry::from_config(config).expect("provider registry");
    let credentials = CredentialStore::open(CredentialStore::default_path(&config.state_dir))
        .expect("credential store");
    let (profile, descriptor) = registry
        .default_profile(config)
        .expect("default inference profile");
    let provider = registry
        .build_provider(&profile, &descriptor, &credentials, session)
        .expect("provider");
    (provider, profile, descriptor)
}

/// Metrics recorded for one live scenario.
#[derive(Debug, Default)]
struct Metrics {
    turns: u64,
    tool_calls: u64,
    validation_passes: u64,
    validation_failures: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    estimated_context_tokens: u64,
    context_window_tokens: u64,
    completion: String,
    elapsed: Duration,
}

impl Metrics {
    fn from_events(events: &[Event], elapsed: Duration) -> Self {
        let mut metrics = Metrics {
            elapsed,
            ..Metrics::default()
        };
        for event in events {
            match &event.payload {
                EventPayload::ModelRequestStarted { .. } => metrics.turns += 1,
                EventPayload::ToolRequested { .. } => metrics.tool_calls += 1,
                EventPayload::ValidationResult { passed, .. } => {
                    if *passed {
                        metrics.validation_passes += 1;
                    } else {
                        metrics.validation_failures += 1;
                    }
                }
                EventPayload::ModelUsage {
                    usage:
                        Usage {
                            input_tokens,
                            output_tokens,
                            cache_miss_tokens: _,
                            cache_read_tokens,
                            cache_write_tokens,
                        },
                } => {
                    metrics.input_tokens += input_tokens;
                    metrics.output_tokens += output_tokens;
                    metrics.cache_read_tokens =
                        sum_opt(metrics.cache_read_tokens, *cache_read_tokens);
                    metrics.cache_write_tokens =
                        sum_opt(metrics.cache_write_tokens, *cache_write_tokens);
                }
                EventPayload::ContextMaterialized { stats } => {
                    metrics.estimated_context_tokens = stats.total_tokens as u64;
                    metrics.context_window_tokens = stats.window_tokens as u64;
                }
                EventPayload::CompletionChanged { completion } => {
                    metrics.completion = format!("{completion:?}");
                }
                _ => {}
            }
        }
        metrics
    }

    fn report(&self, scenario: &str, success: bool, detail: &str) {
        let report = json!({
            "scenario": scenario,
            "success": success,
            "detail": detail,
            "model_turns": self.turns,
            "tool_calls": self.tool_calls,
            "validation_passes": self.validation_passes,
            "validation_failures": self.validation_failures,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "estimated_request_context_tokens": self.estimated_context_tokens,
            "context_window_tokens": self.context_window_tokens,
            "completion": self.completion,
            "elapsed_ms": self.elapsed.as_millis(),
        });
        println!("LIVE-ACCEPTANCE {report}");
        let dir = PathBuf::from("target/live-acceptance");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join(format!("{scenario}.json")),
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        );
    }
}

fn sum_opt(current: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (current, value) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

struct LiveRun {
    store: EventStore,
    session: uuid::Uuid,
    agent: Agent,
}

async fn live_agent(workspace: PathBuf, artifacts: PathBuf, mode: Mode) -> LiveRun {
    let config = live_config();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(&workspace).unwrap();
    let tools = ToolExecutor::new(
        workspace.clone(),
        artifacts,
        store.clone(),
        session,
        PolicyEngine::new(mode, workspace.clone(), PermissionConfig::default()),
    )
    .unwrap();
    let (provider, profile, descriptor) = live_provider(&config, session);
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.clone(),
        mode,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::for_model(
            store.clone(),
            ContextConfig::default(),
            &profile.model,
        ),
        retry_budget: config.failure.retry_budget,
    });
    agent.set_stagnation_budget(config.failure.stagnation_budget);
    agent.set_max_model_turns(config.failure.max_model_turns);
    agent.set_context_budget(
        ContextConfig::default(),
        descriptor
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS),
    );
    LiveRun {
        store,
        session,
        agent,
    }
}

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, contents).unwrap();
    }
}

fn init_git(root: &Path) {
    // Keep build output out of the shell-drift ledger so ownership metrics are
    // about the task, not cargo artifacts.
    let _ = std::fs::write(root.join(".gitignore"), "/target\n");
    for args in [
        vec!["init", "-q"],
        vec!["-c", "user.email=t@l", "-c", "user.name=t", "add", "."],
        vec![
            "-c",
            "user.email=t@l",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

async fn run_prompt(run: &mut LiveRun, prompt: &str) -> Result<(), anyhow::Error> {
    run.agent
        .run(prompt, CancellationToken::new(), Arc::new(|_| {}))
        .await
        .map(|_| ())
}

fn events(run: &LiveRun) -> Vec<Event> {
    run.store.events(run.session).unwrap()
}

fn completion(run: &LiveRun) -> CompletionState {
    run.agent.state().completion.clone()
}

fn validation_passed(run: &LiveRun) -> bool {
    run.agent.state().required_validations.iter().all(|claim| {
        run.agent.evidence().status_of(claim) == Some(latch_protocol::EvidenceStatus::Passed)
    })
}

/// Small bug fix: one file, one failing test, one validation.
#[tokio::test]
#[ignore]
async fn live_small_bug_fix() {
    if !enabled() || !selected("small_bug") {
        return;
    }
    let workspace = tempdir().unwrap();
    write_tree(
        workspace.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='live-small'\nversion='0.1.0'\nedition='2024'\n",
            ),
            (
                "src/lib.rs",
                "pub fn add(a: i32, b: i32) -> i32 { a - b }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn adds() { assert_eq!(add(2, 3), 5); }\n}\n",
            ),
        ],
    );
    init_git(workspace.path());
    let mut run = live_agent(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        Mode::Work,
    )
    .await;
    let started = Instant::now();
    let result = run_prompt(
        &mut run,
        "Fix the failing test in src/lib.rs with the smallest possible change. Run the test suite with validate to prove it passes, then report the result.",
    )
    .await;
    let metrics = Metrics::from_events(&events(&run), started.elapsed());
    let success = result.is_ok() && validation_passed(&run);
    metrics.report(
        "small_bug",
        success,
        &format!("completion={:?}", completion(&run)),
    );
    assert!(success, "small bug fix did not reach passing validation");
}

/// Medium change spanning several files.
#[tokio::test]
#[ignore]
async fn live_medium_multi_file_change() {
    if !enabled() || !selected("medium") {
        return;
    }
    let workspace = tempdir().unwrap();
    write_tree(
        workspace.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='live-medium'\nversion='0.1.0'\nedition='2024'\n",
            ),
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n\npub fn total() -> i32 { a::value() + b::value() + c::value() + d::value() }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn totals() { assert_eq!(total(), 4); }\n}\n",
            ),
            ("src/a.rs", "pub fn value() -> i32 { 0 }\n"),
            ("src/b.rs", "pub fn value() -> i32 { 0 }\n"),
            ("src/c.rs", "pub fn value() -> i32 { 0 }\n"),
            ("src/d.rs", "pub fn value() -> i32 { 0 }\n"),
        ],
    );
    init_git(workspace.path());
    let mut run = live_agent(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        Mode::Work,
    )
    .await;
    let started = Instant::now();
    let result = run_prompt(
        &mut run,
        "The test `totals` fails because every module returns 0. Fix all four modules to return 1 so the total is 4, without changing the public API or unrelated code. Validate with validate and the command `cargo test`.",
    )
    .await;
    let metrics = Metrics::from_events(&events(&run), started.elapsed());
    let success = result.is_ok() && validation_passed(&run);
    metrics.report(
        "medium",
        success,
        &format!("completion={:?}", completion(&run)),
    );
    assert!(success, "medium multi-file change did not verify");
}

/// Wide refactor: rename a function used across many modules.
#[tokio::test]
#[ignore]
async fn live_wide_refactor() {
    if !enabled() || !selected("refactor") {
        return;
    }
    let workspace = tempdir().unwrap();
    let mut files: Vec<(String, String)> = vec![(
        "Cargo.toml".into(),
        "[package]\nname='live-refactor'\nversion='0.1.0'\nedition='2024'\n".into(),
    )];
    let mut lib = String::new();
    for index in 0..12 {
        lib.push_str(&format!("pub mod m{index};\n"));
        files.push((
            format!("src/m{index}.rs"),
            format!("pub fn helper() -> i32 {{ {index} }}\n"),
        ));
    }
    lib.push_str(
        "\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn all_helpers() {\n",
    );
    for index in 0..12 {
        lib.push_str(&format!(
            "        assert_eq!(m{index}::helper(), {index});\n"
        ));
    }
    lib.push_str("    }\n}\n");
    files.push(("src/lib.rs".into(), lib));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect();
    write_tree(workspace.path(), &refs);
    init_git(workspace.path());
    let mut run = live_agent(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        Mode::Work,
    )
    .await;
    let started = Instant::now();
    let result = run_prompt(
        &mut run,
        "Rename `helper` to `index_value` in all 12 modules and every call site under src/, keeping behavior identical. Do not change unrelated code. Validate with `cargo test`.",
    )
    .await;
    let metrics = Metrics::from_events(&events(&run), started.elapsed());
    let files_touched = run
        .store
        .events(run.session)
        .unwrap()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::FileChanged {
                after,
                owner: latch_protocol::ChangeOwner::Latch,
                ..
            } => Some(after.path.clone()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let success = result.is_ok() && validation_passed(&run) && files_touched >= 10;
    metrics.report(
        "refactor",
        success,
        &format!(
            "files_touched={files_touched} completion={:?}",
            completion(&run)
        ),
    );
    assert!(success, "wide refactor did not touch 10+ files and verify");
}

/// Large source file plus a large validation log.
#[tokio::test]
#[ignore]
async fn live_large_file_and_log() {
    if !enabled() || !selected("large_file") {
        return;
    }
    let workspace = tempdir().unwrap();
    let mut source = String::from("pub fn checksum() -> i32 {\n");
    for line in 0..4_000 {
        source.push_str(&format!("    // filler line {line}\n"));
    }
    // The bug sits deep in the file so a whole-file read would be wasteful.
    source.push_str("    0\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn checksum_is_one() { assert_eq!(checksum(), 1); }\n}\n");
    write_tree(
        workspace.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='live-large'\nversion='0.1.0'\nedition='2024'\n",
            ),
            ("src/lib.rs", &source),
        ],
    );
    init_git(workspace.path());
    let mut run = live_agent(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        Mode::Work,
    )
    .await;
    let started = Instant::now();
    let result = run_prompt(
        &mut run,
        "src/lib.rs is large; inspect it with bounded reads, fix `checksum` so the test passes, and validate with `cargo test`. A large test log is expected.",
    )
    .await;
    let metrics = Metrics::from_events(&events(&run), started.elapsed());
    let success = result.is_ok() && validation_passed(&run);
    metrics.report(
        "large_file",
        success,
        &format!("completion={:?}", completion(&run)),
    );
    assert!(success, "large-file fix did not verify");
}

/// Validation fails, the agent debugs, validation passes.
#[tokio::test]
#[ignore]
async fn live_debug_until_green() {
    if !enabled() || !selected("debug") {
        return;
    }
    let workspace = tempdir().unwrap();
    write_tree(
        workspace.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='live-debug'\nversion='0.1.0'\nedition='2024'\n",
            ),
            (
                "src/lib.rs",
                "pub fn even_sum(values: &[i32]) -> i32 {\n    values.iter().filter(|v| *v % 2 != 0).sum()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn sums_only_evens() { assert_eq!(even_sum(&[1, 2, 3, 4]), 6); }\n}\n",
            ),
        ],
    );
    init_git(workspace.path());
    let mut run = live_agent(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        Mode::Work,
    )
    .await;
    let started = Instant::now();
    let result = run_prompt(
        &mut run,
        "Run the test suite with validate first, then debug any failure until `cargo test` passes. Explain the root cause before fixing it.",
    )
    .await;
    let all = events(&run);
    let metrics = Metrics::from_events(&all, started.elapsed());
    // A real fail → debug → pass sequence: the fixture is broken, so the first
    // validation must fail and the final state must be green.
    let success = result.is_ok()
        && validation_passed(&run)
        && metrics.validation_failures >= 1
        && metrics.validation_passes >= 1;
    metrics.report(
        "debug",
        success,
        &format!(
            "passes={} failures={} completion={:?}",
            metrics.validation_passes,
            metrics.validation_failures,
            completion(&run)
        ),
    );
    assert!(success, "debug scenario did not run fail → debug → pass");
}

/// Interrupt a run mid-flight, then resume the same durable session and finish.
#[tokio::test]
#[ignore]
async fn live_interrupt_and_resume() {
    if !enabled() || !selected("interrupt") {
        return;
    }
    let workspace = tempdir().unwrap();
    write_tree(
        workspace.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='live-resume'\nversion='0.1.0'\nedition='2024'\n",
            ),
            (
                "src/lib.rs",
                "pub fn greet(name: &str) -> String { format!(\"hello {name}\") }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn greets() { assert_eq!(greet(\"world\"), \"HELLO world\"); }\n}\n",
            ),
        ],
    );
    init_git(workspace.path());
    let config = live_config();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(workspace.path()).unwrap();
    let tools = ToolExecutor::new(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(
            Mode::Work,
            workspace.path().into(),
            PermissionConfig::default(),
        ),
    )
    .unwrap();
    let (provider, profile, descriptor) = live_provider(&config, session);
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::for_model(
            store.clone(),
            ContextConfig::default(),
            &profile.model,
        ),
        retry_budget: config.failure.retry_budget,
    });
    agent.set_context_budget(
        ContextConfig::default(),
        descriptor
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS),
    );
    let started = Instant::now();
    let cancel = CancellationToken::new();
    // Cancel shortly after the first model turn starts.
    let canceller = {
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            cancel.cancel();
        }
    };
    let first = tokio::join!(
        agent.run(
            "Make the failing test pass. Read the file, fix it, and validate with cargo test.",
            cancel.clone(),
            Arc::new(|_| {})
        ),
        canceller
    )
    .0;
    // Cancellation may surface as an error or as a partial final answer; both
    // are honest. We only require the durable session to exist.
    let _ = first;
    drop(agent);

    // Resume the same durable session with a fresh agent.
    let resumed_store = store.clone();
    let tools = ToolExecutor::new(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        resumed_store.clone(),
        session,
        PolicyEngine::new(
            Mode::Work,
            workspace.path().into(),
            PermissionConfig::default(),
        ),
    )
    .unwrap();
    let (provider, profile, descriptor) = live_provider(&config, session);
    let mut resumed = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.path().into(),
        mode: Mode::Work,
        store: resumed_store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::for_model(
            resumed_store.clone(),
            ContextConfig::default(),
            &profile.model,
        ),
        retry_budget: config.failure.retry_budget,
    });
    resumed.set_context_budget(
        ContextConfig::default(),
        descriptor
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS),
    );
    // Reconstruct durable state exactly like the CLI resume path.
    let durable = resumed_store.events(session).unwrap();
    if let Some(state) = durable.iter().rev().find_map(|event| match &event.payload {
        EventPayload::TaskStateUpdated { state } => Some(state.clone()),
        _ => None,
    }) {
        resumed.restore_state(state);
    }
    resumed.restore_evidence(
        durable
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::EvidenceCreated { evidence } => Some(evidence.clone()),
                _ => None,
            })
            .collect(),
    );
    resumed.restore_failures().unwrap();
    resumed.restore_progress().unwrap();
    let result = resumed
        .run(
            "Continue from the durable state, finish the fix, and validate with cargo test.",
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
    let metrics = Metrics::from_events(
        &events(&LiveRun {
            store: resumed_store,
            session,
            agent: resumed,
        }),
        started.elapsed(),
    );
    let success = result.is_ok();
    metrics.report("interrupt", success, "resumed session completed");
}

/// Long-horizon task: many sequential user turns, requiring >100 model turns
/// in total. This is expensive; select it explicitly with LATCH_LIVE_SCENARIO.
#[tokio::test]
#[ignore]
async fn live_long_horizon_over_100_turns() {
    if !enabled() || !selected("long") {
        return;
    }
    let workspace = tempdir().unwrap();
    write_tree(
        workspace.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='live-long'\nversion='0.1.0'\nedition='2024'\n",
            ),
            ("src/lib.rs", "pub mod notes;\n"),
            (
                "src/notes.rs",
                "pub fn count() -> usize { NOTES.len() }\n\npub static NOTES: &[&str] = &[];\n",
            ),
        ],
    );
    init_git(workspace.path());
    let config = live_config();
    let store = EventStore::open_memory().unwrap();
    let session = store.create_session(workspace.path()).unwrap();
    let tools = ToolExecutor::new(
        workspace.path().into(),
        workspace.path().join("artifacts"),
        store.clone(),
        session,
        PolicyEngine::new(
            Mode::Work,
            workspace.path().into(),
            PermissionConfig::default(),
        ),
    )
    .unwrap();
    let (provider, profile, descriptor) = live_provider(&config, session);
    let mut agent = Agent::new(AgentRuntime {
        session_id: session,
        workspace: workspace.path().into(),
        mode: Mode::Work,
        store: store.clone(),
        provider,
        tools,
        continuity: ContinuityEngine::for_model(
            store.clone(),
            ContextConfig::default(),
            &profile.model,
        ),
        retry_budget: config.failure.retry_budget,
    });
    agent.set_context_budget(
        ContextConfig::default(),
        descriptor
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS),
    );
    let started = Instant::now();
    // 40 small increments; each typically needs 2-4 model turns.
    for index in 0..40 {
        let prompt = format!(
            "Append the note `note {index}` to NOTES in src/notes.rs, keeping the file valid, and run `cargo test` if the file has tests. One note per request; do not add anything else."
        );
        agent
            .run(&prompt, CancellationToken::new(), Arc::new(|_| {}))
            .await
            .expect("long-horizon turn failed");
    }
    let metrics = Metrics::from_events(
        &events(&LiveRun {
            store,
            session,
            agent,
        }),
        started.elapsed(),
    );
    let success = metrics.turns > 100;
    metrics.report(
        "long",
        success,
        &format!("turns={} (target >100)", metrics.turns),
    );
    assert!(success, "long-horizon run made {} turns", metrics.turns);
}
