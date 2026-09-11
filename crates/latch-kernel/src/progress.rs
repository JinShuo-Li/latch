//! Deterministic kernel-side progress/stagnation supervision.
//!
//! The loop detector must never depend on model wording. This supervisor
//! tracks *semantic observations* of inspection tools by canonical subject and
//! result digest, scoped to a **progress epoch**. The epoch advances whenever
//! reality genuinely changes: a workspace mutation (Latch, shell, or detected
//! external), new validation or evidence, a meaningful canonical task-state
//! change, a mode switch, or a new user turn.
//!
//! Repeating the same observation with the same result inside one epoch is
//! redundant. The first redundant turn is tolerated (occasional re-checking is
//! legitimate). Consecutive redundant turns crossing the configured budget
//! produce a kernel-owned re-ground instruction that lists the unchanged
//! observations. If the model keeps repeating them after re-ground, the calls
//! are suppressed instead of executed again.
//!
//! Because state is derived only from durable events, live supervision and
//! replay after `--resume` agree exactly.

use crate::tools::is_read_only_shell;
use latch_protocol::{Event, EventPayload, ToolCall, ToolResult};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Consecutive redundant turns tolerated before the kernel re-grounds.
pub const DEFAULT_STAGNATION_BUDGET: u32 = 2;

/// Deterministic supervision outcome for a settled model turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagnationDecision {
    /// Redundant turns crossed the budget: tell the model to use existing
    /// evidence or name the concrete blocker, listing what is already known.
    Reground {
        unchanged: Vec<String>,
        redundant_turns: u32,
    },
}

#[derive(Debug, Clone)]
struct Observation {
    digest: String,
    label: String,
    /// Cheap freshness token for file reads, so an external edit between turns
    /// is never mistaken for redundancy.
    freshness: Option<Freshness>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Freshness {
    len: u64,
    modified_nanos: u128,
}

#[derive(Debug, Default)]
struct Turn {
    observations: usize,
    new_observations: usize,
    redundant: usize,
}

/// Tracks inspection observations and progress epochs across model turns for
/// one workspace.
#[derive(Debug)]
pub struct ProgressSupervisor {
    budget: u32,
    workspace: PathBuf,
    epoch: u64,
    known: HashMap<String, Observation>,
    pending_calls: HashMap<String, ToolCall>,
    redundant_turns: u32,
    regrounded: bool,
    turn: Turn,
    last_task_state: Option<String>,
}

impl ProgressSupervisor {
    #[must_use]
    pub fn new(budget: u32, workspace: PathBuf) -> Self {
        Self {
            budget: budget.max(1),
            workspace,
            epoch: 0,
            known: HashMap::new(),
            pending_calls: HashMap::new(),
            redundant_turns: 0,
            regrounded: false,
            turn: Turn::default(),
            last_task_state: None,
        }
    }

    pub fn set_budget(&mut self, budget: u32) {
        self.budget = budget.max(1);
    }

    /// Clears all derived supervision state while keeping the configured
    /// workspace and budget. Used to make reconstruction from events
    /// idempotent.
    pub fn reset(&mut self) {
        self.epoch = 0;
        self.known.clear();
        self.pending_calls.clear();
        self.redundant_turns = 0;
        self.regrounded = false;
        self.turn = Turn::default();
        self.last_task_state = None;
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub const fn redundant_turns(&self) -> u32 {
        self.redundant_turns
    }

    #[must_use]
    pub const fn regrounded(&self) -> bool {
        self.regrounded
    }

    /// Semantic labels of every observation known unchanged in this epoch,
    /// sorted for deterministic instructions and tests.
    #[must_use]
    pub fn known_unchanged(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .known
            .values()
            .map(|observation| observation.label.clone())
            .collect();
        labels.sort();
        labels.dedup();
        labels
    }

    /// Advances the progress epoch: reality moved, so re-observing anything is
    /// legitimate again and stagnation bookkeeping restarts.
    pub fn advance_epoch(&mut self) {
        self.epoch += 1;
        self.known.clear();
        self.redundant_turns = 0;
        self.regrounded = false;
        self.turn = Turn::default();
    }

    /// Feeds one durable event exactly as recorded. Live supervision and
    /// resume replay share this path.
    pub fn observe_event(&mut self, event: &Event) {
        match &event.payload {
            EventPayload::AssistantMessageCompleted { tool_calls, .. }
                if !tool_calls.is_empty() =>
            {
                // A new assistant turn is starting; settle the previous one.
                self.finish_turn();
            }
            EventPayload::ToolRequested { call } => {
                // Terminal results carry only the call id; the semantic key
                // lives in the request.
                self.pending_calls.insert(call.id.clone(), call.clone());
            }
            EventPayload::ToolCompleted { result } => {
                if let Some(call) = self.pending_calls.remove(&result.call_id) {
                    self.observe_call_result(&call, result);
                }
            }
            EventPayload::ToolFailed { result } => {
                self.pending_calls.remove(&result.call_id);
            }
            EventPayload::UserMessage { .. } => {
                // Progress ends the current turn: settle it before reality
                // moves so live scanning and full replay count identically.
                self.finish_turn();
                self.advance_epoch();
            }
            EventPayload::FileChanged { .. }
            | EventPayload::ChangeReverted { .. }
            | EventPayload::ShellMutationObserved { .. }
            | EventPayload::ExternalFileChangeDetected { .. } => {
                self.finish_turn();
                self.advance_epoch();
            }
            EventPayload::ValidationResult { .. } | EventPayload::EvidenceCreated { .. } => {
                self.finish_turn();
                self.advance_epoch();
            }
            EventPayload::TaskStateUpdated { state } => {
                let serialized = serde_json::to_string(state).unwrap_or_default();
                if self.last_task_state.as_deref() != Some(serialized.as_str()) {
                    self.finish_turn();
                    self.last_task_state = Some(serialized);
                    self.advance_epoch();
                }
            }
            EventPayload::ModeChanged { .. } => {
                self.finish_turn();
                self.advance_epoch();
            }
            _ => {}
        }
    }

    /// Settles the current model turn. A turn whose only inspection results
    /// were repeats of unchanged observations escalates the streak; a turn with
    /// fresh observations or no inspection at all clears it.
    pub fn finish_turn(&mut self) -> Option<StagnationDecision> {
        if self.turn.observations == 0 {
            // Nothing to settle: a turn that observed nothing neither extends
            // nor breaks the consecutive-redundancy streak.
            return None;
        }
        let turn = std::mem::take(&mut self.turn);
        if turn.new_observations > 0 {
            self.redundant_turns = 0;
            self.regrounded = false;
            return None;
        }
        if turn.redundant == 0 {
            self.redundant_turns = 0;
            return None;
        }
        self.redundant_turns += 1;
        if self.redundant_turns >= self.budget && !self.regrounded {
            self.regrounded = true;
            return Some(StagnationDecision::Reground {
                unchanged: self.known_unchanged(),
                redundant_turns: self.redundant_turns,
            });
        }
        None
    }

    /// The kernel-owned re-ground instruction injected into the next request
    /// while stagnation supervision is active.
    #[must_use]
    pub fn reground_instruction(&self) -> Option<String> {
        if !self.regrounded {
            return None;
        }
        let labels = self.known_unchanged();
        let list = if labels.is_empty() {
            "- (no observations tracked)".to_owned()
        } else {
            labels
                .iter()
                .map(|label| format!("- {label}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Some(format!(
            "KERNEL RE-GROUND (inspection stagnation): these observations were already made in the current progress epoch and their results have not changed:\n{list}\nDo not inspect them again unless relevant reality changes. Act on the existing evidence, or state the concrete blocker that prevents action."
        ))
    }

    /// Returns the observation label when `call` is a redundant re-observation
    /// that must be suppressed instead of executed. Only active after the model
    /// ignored an explicit re-ground; legitimate re-reads after a mutation or
    /// external edit are never suppressed.
    #[must_use]
    pub fn suppression_reason(&self, call: &ToolCall) -> Option<String> {
        if !self.regrounded {
            return None;
        }
        let (key, label, current_freshness) = observation_key(call, &self.workspace)?;
        let known = self.known.get(&key)?;
        if known.freshness.is_some() && current_freshness != known.freshness {
            // The file's size or mtime moved: let the read run so change
            // detection can classify the external edit.
            return None;
        }
        Some(label)
    }

    /// Rebuilds supervision from the durable event log. Replaying the same
    /// events the live loop observed yields the same epoch, known
    /// observations, redundancy streak, and re-ground state.
    pub fn replay(&mut self, events: &[Event]) {
        for event in events {
            self.observe_event(event);
        }
        self.finish_turn();
    }

    fn observe_call_result(&mut self, call: &ToolCall, result: &ToolResult) {
        let Some((key, label, freshness)) = observation_key(call, &self.workspace) else {
            return;
        };
        let digest = digest(&result.output);
        self.turn.observations += 1;
        match self.known.get(&key) {
            Some(previous) if previous.digest == digest => self.turn.redundant += 1,
            Some(_) => {
                // The same subject now reports different content without any
                // progress event: reality changed externally.
                self.advance_epoch();
                self.known.insert(
                    key,
                    Observation {
                        digest,
                        label,
                        freshness,
                    },
                );
                self.turn.new_observations += 1;
            }
            None => {
                self.known.insert(
                    key,
                    Observation {
                        digest,
                        label,
                        freshness,
                    },
                );
                self.turn.new_observations += 1;
            }
        }
    }
}

/// Canonical observation identity for a call: stable across re-issuance with
/// formatting differences, and `None` for tools that are not inspections or
/// could mutate.
fn observation_key(
    call: &ToolCall,
    workspace: &Path,
) -> Option<(String, String, Option<Freshness>)> {
    match call.name.as_str() {
        "read_file" => {
            let raw = string_arg(call, "path")?;
            let path = normalize_path(workspace, raw);
            let freshness = freshness_of_file(call, workspace);
            Some((
                format!("read_file:{path}"),
                format!("read_file {path}"),
                freshness,
            ))
        }
        "search" => {
            let query = collapse_whitespace(string_arg(call, "query")?);
            let target = string_arg(call, "path")
                .map(|path| normalize_path(workspace, path))
                .unwrap_or_else(|| ".".into());
            Some((
                format!("search:{target}:{query}"),
                format!("search \"{query}\" in {target}"),
                None,
            ))
        }
        "git_status" => Some(("git_status".into(), "git status".into(), None)),
        "git_diff" => Some(("git_diff".into(), "git diff".into(), None)),
        "shell" => {
            let command = string_arg(call, "command")?;
            if !is_read_only_shell(command, workspace) {
                return None;
            }
            let normalized = collapse_whitespace(command);
            Some((
                format!("shell:{normalized}"),
                format!("shell: {normalized}"),
                None,
            ))
        }
        _ => None,
    }
}

fn freshness_of_file(call: &ToolCall, workspace: &Path) -> Option<Freshness> {
    if call.name != "read_file" {
        return None;
    }
    let path = Path::new(string_arg(call, "path")?);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let metadata = std::fs::metadata(absolute).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    Some(Freshness {
        len: metadata.len(),
        modified_nanos,
    })
}

fn string_arg<'a>(call: &'a ToolCall, name: &str) -> Option<&'a str> {
    call.arguments.get(name).and_then(serde_json::Value::as_str)
}

fn normalize_path(workspace: &Path, raw: &str) -> String {
    let path = Path::new(raw);
    let relative = if path.is_absolute() {
        path.strip_prefix(workspace)
            .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
    } else {
        path.to_path_buf()
    };
    relative
        .components()
        .filter_map(|component| match component {
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_owned()),
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn digest(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn event(sequence: u64, payload: EventPayload) -> Event {
        Event {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            sequence,
            timestamp: Utc::now(),
            parent_id: None,
            payload,
        }
    }

    fn requested(id: &str, name: &str, arguments: serde_json::Value) -> Event {
        event(
            1,
            EventPayload::ToolRequested {
                call: ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                },
            },
        )
    }

    fn completed(id: &str, name: &str, output: &str) -> Event {
        event(
            2,
            EventPayload::ToolCompleted {
                result: ToolResult {
                    call_id: id.into(),
                    name: name.into(),
                    output: output.into(),
                    is_error: false,
                    artifact_id: None,
                },
            },
        )
    }

    fn read_call(path: &str) -> ToolCall {
        ToolCall {
            id: "r".into(),
            name: "read_file".into(),
            arguments: json!({"path": path}),
        }
    }

    fn read_result(output: &str) -> ToolResult {
        ToolResult {
            call_id: "r".into(),
            name: "read_file".into(),
            output: output.into(),
            is_error: false,
            artifact_id: None,
        }
    }

    #[test]
    fn identical_reads_become_redundant_and_reground_at_budget() {
        let workspace = PathBuf::from("/tmp/ws");
        let path = "/tmp/ws/src/lib.rs";
        let mut supervisor = ProgressSupervisor::new(2, workspace);
        supervisor.observe_call_result(&read_call(path), &read_result("hash: abc\nfn main() {}"));
        supervisor.finish_turn();
        assert!(!supervisor.regrounded());
        // First redundant turn: tolerated.
        supervisor.observe_call_result(&read_call(path), &read_result("hash: abc\nfn main() {}"));
        assert!(supervisor.finish_turn().is_none());
        assert!(!supervisor.regrounded());
        // Second consecutive redundant turn: re-ground.
        supervisor.observe_call_result(&read_call(path), &read_result("hash: abc\nfn main() {}"));
        let decision = supervisor.finish_turn().expect("stagnation detected");
        assert_eq!(
            decision,
            StagnationDecision::Reground {
                unchanged: vec!["read_file src/lib.rs".into()],
                redundant_turns: 2,
            }
        );
        let instruction = supervisor.reground_instruction().unwrap();
        assert!(instruction.contains("read_file src/lib.rs"));
        assert!(instruction.contains("concrete blocker"));
    }

    #[test]
    fn progress_event_advances_epoch_and_allows_reread() {
        let mut supervisor = ProgressSupervisor::new(2, PathBuf::from("/tmp/ws"));
        let result = read_result("hash: abc\nsame");
        supervisor.observe_call_result(&read_call("/tmp/ws/a"), &result);
        supervisor.finish_turn();
        supervisor.observe_call_result(&read_call("/tmp/ws/a"), &result);
        supervisor.finish_turn();
        assert_eq!(supervisor.redundant_turns(), 1);
        // A file mutation is meaningful new reality.
        supervisor.advance_epoch();
        assert_eq!(supervisor.epoch(), 1);
        assert!(supervisor.known_unchanged().is_empty());
        supervisor.observe_call_result(&read_call("/tmp/ws/a"), &result);
        assert!(supervisor.finish_turn().is_none());
        assert_eq!(supervisor.redundant_turns(), 0);
    }

    #[test]
    fn changed_digest_counts_as_external_mutation_progress() {
        let mut supervisor = ProgressSupervisor::new(1, PathBuf::from("/tmp/ws"));
        supervisor.observe_call_result(&read_call("/tmp/ws/a"), &read_result("hash: abc\nold"));
        supervisor.finish_turn();
        assert_eq!(supervisor.epoch(), 0);
        supervisor.observe_call_result(&read_call("/tmp/ws/a"), &read_result("hash: def\nnew"));
        assert!(
            supervisor.finish_turn().is_none(),
            "changed content is not stagnation"
        );
        assert_eq!(supervisor.epoch(), 1, "external change advanced the epoch");
        assert!(!supervisor.regrounded());
    }

    #[test]
    fn suppression_requires_reground_and_allows_moved_files() {
        let workspace = std::env::temp_dir().join(format!("latch-progress-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let call = read_call(file.to_str().unwrap());
        let result = read_result("hash: abc\none");
        let mut supervisor = ProgressSupervisor::new(1, workspace.clone());
        supervisor.observe_call_result(&call, &result);
        supervisor.finish_turn();
        assert!(supervisor.suppression_reason(&call).is_none());
        supervisor.observe_call_result(&call, &result);
        let decision = supervisor.finish_turn().expect("re-ground");
        assert_eq!(
            decision,
            StagnationDecision::Reground {
                unchanged: vec!["read_file a.txt".into()],
                redundant_turns: 1,
            }
        );
        supervisor.observe_call_result(&call, &result);
        supervisor.finish_turn();
        assert!(supervisor.suppression_reason(&call).is_some());
        // An external edit changes the freshness stamp and is never suppressed.
        std::fs::write(&file, "two changed").unwrap();
        assert!(
            supervisor.suppression_reason(&call).is_none(),
            "a moved file must be re-readable"
        );
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn read_only_shell_is_observed_but_mutating_shell_is_not() {
        let workspace = PathBuf::from("/tmp/ws");
        let readonly = ToolCall {
            id: "s".into(),
            name: "shell".into(),
            arguments: json!({"command":"git  status" }),
        };
        let mutating = ToolCall {
            id: "s".into(),
            name: "shell".into(),
            arguments: json!({"command":"touch x"}),
        };
        assert!(observation_key(&readonly, &workspace).is_some());
        assert_eq!(
            observation_key(&readonly, &workspace).unwrap().0,
            "shell:git status",
            "commands normalize whitespace"
        );
        assert!(observation_key(&mutating, &workspace).is_none());
    }

    #[test]
    fn replay_reconstructs_live_state() {
        let workspace = std::env::temp_dir().join(format!("latch-replay-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let path = file.to_str().unwrap();
        let events = vec![
            event(
                1,
                EventPayload::UserMessage {
                    text: "do it".into(),
                },
            ),
            requested("r1", "read_file", json!({"path": path})),
            completed("r1", "read_file", "hash: abc\none"),
            event(
                4,
                EventPayload::AssistantMessageCompleted {
                    text: "again".into(),
                    tool_calls: vec![read_call(path)],
                    reasoning_content: None,
                },
            ),
            requested("r2", "read_file", json!({"path": path})),
            completed("r2", "read_file", "hash: abc\none"),
            event(
                7,
                EventPayload::AssistantMessageCompleted {
                    text: "again".into(),
                    tool_calls: vec![read_call(path)],
                    reasoning_content: None,
                },
            ),
            requested("r3", "read_file", json!({"path": path})),
            completed("r3", "read_file", "hash: abc\none"),
        ];
        let mut replayed = ProgressSupervisor::new(2, workspace.clone());
        replayed.replay(&events);
        assert!(replayed.regrounded());
        assert!(replayed.reground_instruction().is_some());
        std::fs::remove_dir_all(&workspace).ok();
    }
}
