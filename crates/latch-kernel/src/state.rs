use latch_protocol::{CompletionState, Evidence, EvidenceStatus, Hypothesis, TaskState, Validity};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct TaskStateManager {
    state: TaskState,
}

/// Model-facing task update. Constraints here are *task* constraints: the
/// kernel records them with `TaskConstraint` provenance, never as user
/// constraints. Superseded/resolved items disappear from the active canonical
/// view while their history remains in durable events and memory records.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateUpdate {
    pub goal: Option<String>,
    pub add_constraints: Vec<String>,
    /// Model-authored constraint texts superseded by newer instructions.
    pub supersede_constraints: Vec<String>,
    pub add_decisions: Vec<String>,
    /// Decisions the model replaces with a different approach.
    pub supersede_decisions: Vec<String>,
    pub add_hypotheses: Vec<String>,
    pub reject_hypotheses: Vec<String>,
    pub touched_files: Vec<String>,
    /// Requirements that must hold before completion can be Verified. Whether
    /// each requirement passes is kernel evidence, not model input.
    pub required_validations: Vec<String>,
    pub open_questions: Option<Vec<String>>,
    /// Open questions answered or made moot by later events.
    pub resolve_questions: Vec<String>,
    pub next_actions: Option<Vec<String>>,
    pub completion_criteria: Vec<String>,
}

impl TaskStateManager {
    #[must_use]
    pub fn new(state: TaskState) -> Self {
        Self { state }
    }
    #[must_use]
    pub fn state(&self) -> &TaskState {
        &self.state
    }
    pub fn update(&mut self, u: StateUpdate) {
        if let Some(goal) = u.goal.filter(|s| !s.trim().is_empty()) {
            self.state.goal = goal;
        }
        extend_unique(&mut self.state.constraints, u.add_constraints);
        self.state
            .constraints
            .retain(|c| !u.supersede_constraints.iter().any(|s| same_text(s, c)));
        extend_unique(&mut self.state.decisions, u.add_decisions);
        self.state
            .decisions
            .retain(|d| !u.supersede_decisions.iter().any(|s| same_text(s, d)));
        for text in u.add_hypotheses {
            if !self.state.hypotheses.iter().any(|h| h.text == text) {
                self.state.hypotheses.push(Hypothesis {
                    text,
                    validity: Validity::Active,
                });
            }
        }
        for rejected in u.reject_hypotheses {
            if let Some(h) = self
                .state
                .hypotheses
                .iter_mut()
                .find(|h| h.text == rejected)
            {
                h.validity = Validity::Rejected;
            }
            extend_unique(&mut self.state.rejected_hypotheses, [rejected]);
        }
        // A rejected hypothesis is never promoted to a decision or fact, in
        // either update order.
        self.state
            .decisions
            .retain(|d| !self.state.rejected_hypotheses.contains(d));
        extend_unique(&mut self.state.touched_files, u.touched_files);
        extend_unique(&mut self.state.required_validations, u.required_validations);
        extend_unique(&mut self.state.completion_criteria, u.completion_criteria);
        if let Some(v) = u.open_questions {
            self.state.open_questions = v;
        }
        self.state
            .open_questions
            .retain(|q| !u.resolve_questions.iter().any(|r| same_text(r, q)));
        if let Some(v) = u.next_actions {
            self.state.next_actions = v;
        }
    }

    /// The kernel registers a validated requirement so completion state always
    /// tracks actually-observed validations.
    pub fn require_validation(&mut self, requirement: &str) {
        let name = requirement.trim();
        if !name.is_empty()
            && !self
                .state
                .required_validations
                .iter()
                .any(|r| same_text(r, name))
        {
            self.state.required_validations.push(name.to_owned());
        }
    }

    /// Recomputes completion from the implementation claim and the CURRENT
    /// evidence state of every required validation. Historical evidence stays
    /// in the ledger for audit but never blocks a requirement that now passes.
    pub fn recompute_completion(&mut self, evidence: &EvidenceLedger) {
        let required = self.state.required_validations.clone();
        self.state.completion = if !self.state.implementation_done {
            CompletionState::InProgress
        } else if required.is_empty() {
            CompletionState::ImplementedNotVerified
        } else {
            let mut blocked = false;
            let mut unverified = false;
            for requirement in &required {
                match evidence.status_of(requirement) {
                    Some(EvidenceStatus::Passed) => {}
                    Some(EvidenceStatus::Unavailable) => blocked = true,
                    Some(EvidenceStatus::Failed) | Some(EvidenceStatus::Pending) | None => {
                        unverified = true
                    }
                }
            }
            if blocked {
                CompletionState::Blocked
            } else if unverified {
                CompletionState::ImplementedNotVerified
            } else {
                CompletionState::Verified
            }
        };
    }

    /// Applies the model's implementation claim. The derived completion value
    /// is kernel-computed; this only records the claim itself.
    pub fn set_implementation_done(&mut self, done: bool) {
        self.state.implementation_done = done;
    }
}

fn extend_unique<I: IntoIterator<Item = String>>(target: &mut Vec<String>, source: I) {
    for v in source {
        if !target.iter().any(|existing| same_text(existing, &v)) {
            target.push(v);
        }
    }
}

/// Case-insensitive trimmed comparison used for semantic claim/requirement
/// matching. Provenance keeps the original text verbatim.
#[must_use]
pub fn same_text(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Append-only evidence ledger with current-state lookup.
///
/// The newest entry for a claim is the CURRENT evidence for that claim; older
/// entries remain auditable history. A FAIL followed by a PASS therefore
/// resolves the requirement, while every raw attempt stays in the durable
/// event log.
#[derive(Debug, Default)]
pub struct EvidenceLedger {
    entries: Vec<Evidence>,
}
impl EvidenceLedger {
    #[must_use]
    pub fn new(entries: Vec<Evidence>) -> Self {
        Self { entries }
    }
    /// Records a new evidence observation for a claim, superseding the current
    /// entry for that claim when one exists.
    pub fn add(
        &mut self,
        claim: impl Into<String>,
        source_event: Uuid,
        status: EvidenceStatus,
        detail: impl Into<String>,
    ) -> Evidence {
        let claim = claim.into();
        let supersedes = self.current(&claim).map(|e| e.id);
        let e = Evidence {
            id: Uuid::new_v4(),
            claim,
            source_event,
            status,
            detail: detail.into(),
            created_at: chrono::Utc::now(),
            supersedes,
        };
        self.entries.push(e.clone());
        e
    }
    #[must_use]
    pub fn entries(&self) -> &[Evidence] {
        &self.entries
    }
    /// Newest entry for a claim, matched case-insensitively.
    #[must_use]
    pub fn current(&self, claim: &str) -> Option<&Evidence> {
        self.entries
            .iter()
            .rev()
            .find(|e| same_text(&e.claim, claim))
    }
    /// Current status for a claim; `None` when no evidence exists yet.
    #[must_use]
    pub fn status_of(&self, claim: &str) -> Option<EvidenceStatus> {
        self.current(claim).map(|e| e.status.clone())
    }
    /// Current summary lines for canonical materialization.
    #[must_use]
    pub fn current_summary(&self) -> Vec<String> {
        let mut claims: Vec<String> = Vec::new();
        for entry in &self.entries {
            if !claims.iter().any(|claim| same_text(claim, &entry.claim)) {
                claims.push(entry.claim.clone());
            }
        }
        claims
            .into_iter()
            .filter_map(|claim| {
                self.current(&claim)
                    .map(|entry| format!("- {} [{:?}] {}", entry.claim, entry.status, entry.detail))
            })
            .collect()
    }
}

/// Failure supervision keyed by validation lineage.
///
/// A lineage is the meaningful subject of an attempt: the validation
/// requirement, or the executed command family for shell work. Unrelated
/// successful inspection tools (read_file, search, git_status) never touch a
/// lineage, so `validation FAIL → read_file PASS → validation FAIL` still
/// accumulates toward the re-ground threshold. Only meaningful progress — the
/// same lineage now passing, or a materially different failure signature —
/// resolves a streak.
#[derive(Debug)]
pub struct FailureManager {
    retry_budget: u32,
    lineages: HashMap<String, Lineage>,
}
#[derive(Debug, Clone, Default)]
struct Lineage {
    count: u32,
    signature: String,
}
impl FailureManager {
    #[must_use]
    pub fn new(retry_budget: u32) -> Self {
        Self {
            retry_budget,
            lineages: HashMap::new(),
        }
    }
    /// Records a failed attempt for `subject` and reports the supervision
    /// decision. A repeated identical failure escalates the streak; a materially
    /// different failure signature restarts the count at one because the model
    /// demonstrably changed the failure mode.
    pub fn record(&mut self, subject: &str, output: &str) -> FailureDecision {
        let signature = normalize_failure(subject, output);
        let lineage = self.lineages.entry(subject.trim().to_owned()).or_default();
        if lineage.count > 0 && lineage.signature != signature {
            lineage.count = 0;
        }
        lineage.signature = signature.clone();
        lineage.count += 1;
        FailureDecision {
            signature,
            count: lineage.count,
            reground: lineage.count >= self.retry_budget,
            lineage: subject.trim().to_owned(),
        }
    }
    /// Resolves the lineage when its own subject now succeeds. A passing
    /// validation clears the whole streak; other tools never call this.
    pub fn resolve(&mut self, subject: &str) {
        self.lineages.remove(subject.trim());
    }
    /// True when the lineage has an active failure streak (used for resume
    /// diagnostics and context summaries).
    #[must_use]
    pub fn active_lineages(&self) -> Vec<(String, u32)> {
        let mut items: Vec<(String, u32)> = self
            .lineages
            .iter()
            .map(|(subject, lineage)| (subject.clone(), lineage.count))
            .collect();
        items.sort();
        items
    }
    /// Rebuilds supervision state from an ordered history of attempts so
    /// `--resume` does not silently forget a stalled loop.
    pub fn replay<'a, I: Iterator<Item = (&'a str, bool, &'a str)>>(&mut self, attempts: I) {
        for (subject, failed, output) in attempts {
            if failed {
                self.record(subject, output);
            } else {
                self.resolve(subject);
            }
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDecision {
    pub signature: String,
    pub count: u32,
    pub reground: bool,
    pub lineage: String,
}
#[must_use]
pub fn normalize_failure(subject: &str, output: &str) -> String {
    let normalized = output
        .lines()
        .take(8)
        .map(|l| {
            l.chars()
                .map(|c| if c.is_ascii_digit() { '#' } else { c })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}:{}",
        subject.split_whitespace().next().unwrap_or(""),
        normalized
    )
}

/// Meaningful supervision subject for a tool call: the validation requirement
/// for `validate`, the command for `shell`, otherwise the tool name. Read-only
/// inspection tools each map to their own name so their successes can never be
/// confused with a validation lineage.
#[must_use]
pub fn failure_subject(tool: &str, arguments: &serde_json::Value) -> String {
    match tool {
        "validate" => arguments
            .get("requirement")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(tool)
            .to_owned(),
        "shell" => arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(|command| command.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| tool.to_owned()),
        _ => tool.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[allow(dead_code)]
    fn evidence(claim: &str, status: EvidenceStatus) -> Evidence {
        Evidence {
            id: Uuid::new_v4(),
            claim: claim.into(),
            source_event: Uuid::new_v4(),
            status,
            detail: "test".into(),
            created_at: Utc::now(),
            supersedes: None,
        }
    }

    #[test]
    fn hypothesis_never_becomes_fact() {
        let mut m = TaskStateManager::default();
        m.update(StateUpdate {
            add_hypotheses: vec!["cache race".into()],
            ..Default::default()
        });
        m.update(StateUpdate {
            reject_hypotheses: vec!["cache race".into()],
            ..Default::default()
        });
        assert_eq!(m.state.hypotheses[0].validity, Validity::Rejected);
        assert!(m.state.decisions.is_empty());
        // Even a later attempt to add the rejected text as a decision cannot
        // resurrect it as fact.
        m.update(StateUpdate {
            add_decisions: vec!["cache race".into()],
            ..Default::default()
        });
        assert!(m.state.decisions.is_empty());
    }

    #[test]
    fn updates_are_additive() {
        let mut m = TaskStateManager::default();
        m.update(StateUpdate {
            add_constraints: vec!["A".into()],
            ..Default::default()
        });
        m.update(StateUpdate {
            add_constraints: vec!["B".into()],
            ..Default::default()
        });
        assert_eq!(m.state.constraints, ["A", "B"]);
    }

    #[test]
    fn superseded_items_leave_canonical_but_stay_auditable() {
        let mut m = TaskStateManager::default();
        m.update(StateUpdate {
            add_decisions: vec!["use polling".into()],
            add_constraints: vec!["keep API stable".into()],
            open_questions: Some(vec!["which port?".into()]),
            ..Default::default()
        });
        m.update(StateUpdate {
            add_decisions: vec!["use event stream".into()],
            supersede_decisions: vec!["use polling".into()],
            resolve_questions: vec!["which port?".into()],
            ..Default::default()
        });
        assert_eq!(m.state.decisions, ["use event stream"]);
        assert_eq!(m.state.constraints, ["keep API stable"]);
        assert!(m.state.open_questions.is_empty());
    }

    #[test]
    fn fail_then_pass_supersedes_current_evidence() {
        let mut e = EvidenceLedger::default();
        e.add(
            "unittest passes",
            Uuid::new_v4(),
            EvidenceStatus::Failed,
            "2 failed",
        );
        assert_eq!(e.status_of("unittest passes"), Some(EvidenceStatus::Failed));
        e.add(
            "unittest passes",
            Uuid::new_v4(),
            EvidenceStatus::Passed,
            "ok",
        );
        assert_eq!(e.status_of("unittest passes"), Some(EvidenceStatus::Passed));
        // History remains intact: both entries, newest linked to the old one.
        assert_eq!(e.entries().len(), 2);
        assert_eq!(e.entries()[0].id, e.entries()[1].supersedes.unwrap());
    }

    #[test]
    fn completion_transitions_through_states() {
        let mut m = TaskStateManager::default();
        let mut e = EvidenceLedger::default();
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::InProgress);

        m.set_implementation_done(true);
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::ImplementedNotVerified);

        m.update(StateUpdate {
            required_validations: vec!["tests pass".into()],
            ..Default::default()
        });
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::ImplementedNotVerified);

        e.add(
            "tests pass",
            Uuid::new_v4(),
            EvidenceStatus::Unavailable,
            "no runner",
        );
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::Blocked);

        e.add(
            "tests pass",
            Uuid::new_v4(),
            EvidenceStatus::Failed,
            "2 failed",
        );
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::ImplementedNotVerified);

        e.add("tests pass", Uuid::new_v4(), EvidenceStatus::Passed, "ok");
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::Verified);
    }

    #[test]
    fn historical_failure_does_not_poison_completion() {
        let mut m = TaskStateManager::default();
        m.update(StateUpdate {
            required_validations: vec!["tests".into()],
            ..Default::default()
        });
        m.set_implementation_done(true);
        let mut e = EvidenceLedger::default();
        e.add("tests", Uuid::new_v4(), EvidenceStatus::Failed, "fail 1");
        e.add("tests", Uuid::new_v4(), EvidenceStatus::Failed, "fail 2");
        e.add("tests", Uuid::new_v4(), EvidenceStatus::Passed, "now green");
        m.recompute_completion(&e);
        assert_eq!(m.state.completion, CompletionState::Verified);
    }

    #[test]
    fn unrelated_success_keeps_failure_streak() {
        let mut f = FailureManager::new(3);
        assert!(!f.record("python3 -m unittest", "FAIL: test_calc").reground);
        // Successful inspection tools do not touch the lineage.
        assert!(f.active_lineages()[0].1 == 1);
        assert!(!f.record("python3 -m unittest", "FAIL: test_calc").reground);
        assert!(f.record("python3 -m unittest", "FAIL: test_calc").reground);
    }

    #[test]
    fn passing_validation_resolves_lineage() {
        let mut f = FailureManager::new(2);
        f.record("python3 -m unittest", "FAIL");
        f.record("python3 -m unittest", "FAIL");
        f.resolve("python3 -m unittest");
        assert!(f.active_lineages().is_empty());
        assert!(!f.record("python3 -m unittest", "FAIL").reground);
    }

    #[test]
    fn material_signature_change_resets_streak() {
        let mut f = FailureManager::new(3);
        f.record("cargo test", "error[E0308]: mismatched types");
        f.record("cargo test", "error[E0308]: mismatched types");
        // The model fixed type errors; now it is a link failure — a materially
        // different failure mode restarts the streak.
        let d = f.record("cargo test", "error: linker `cc` not found");
        assert_eq!(d.count, 1);
        assert!(!d.reground);
    }

    #[test]
    fn replay_reconstructs_stalled_lineage() {
        let mut f = FailureManager::new(3);
        let attempts = [
            ("python3 -m unittest test_calc", true, "FAIL: test_two"),
            ("read_file", false, "contents"),
            ("python3 -m unittest test_calc", true, "FAIL: test_two"),
        ];
        f.replay(
            attempts
                .iter()
                .map(|(subject, failed, output)| (*subject, *failed, *output)),
        );
        assert_eq!(
            f.active_lineages(),
            vec![("python3 -m unittest test_calc".into(), 2)]
        );
        // The next live failure crosses the re-ground threshold.
        assert!(
            f.record("python3 -m unittest test_calc", "FAIL: test_two")
                .reground
        );
    }

    #[test]
    fn subject_derivation_matches_validation_semantics() {
        assert_eq!(
            failure_subject("validate", &serde_json::json!({"requirement":"tests pass"})),
            "tests pass"
        );
        assert_eq!(
            failure_subject("shell", &serde_json::json!({"command":"cargo test --lib"})),
            "cargo test --lib"
        );
        assert_eq!(
            failure_subject("read_file", &serde_json::json!({})),
            "read_file"
        );
    }
}
