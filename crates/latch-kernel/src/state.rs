use latch_protocol::{CompletionState, Evidence, EvidenceStatus, Hypothesis, TaskState, Validity};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct TaskStateManager {
    state: TaskState,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateUpdate {
    pub goal: Option<String>,
    pub add_constraints: Vec<String>,
    pub add_decisions: Vec<String>,
    pub add_hypotheses: Vec<String>,
    pub reject_hypotheses: Vec<String>,
    pub touched_files: Vec<String>,
    pub required_validations: Vec<String>,
    pub validation_status: HashMap<String, bool>,
    pub open_questions: Option<Vec<String>>,
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
        extend_unique(&mut self.state.decisions, u.add_decisions);
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
        extend_unique(&mut self.state.touched_files, u.touched_files);
        extend_unique(&mut self.state.required_validations, u.required_validations);
        extend_unique(&mut self.state.completion_criteria, u.completion_criteria);
        self.state.validation_status.extend(u.validation_status);
        if let Some(v) = u.open_questions {
            self.state.open_questions = v;
        }
        if let Some(v) = u.next_actions {
            self.state.next_actions = v;
        }
    }
    pub fn recompute_completion(
        &mut self,
        implementation_claimed: bool,
        evidence: &EvidenceLedger,
    ) {
        let required_ok = !self.state.required_validations.is_empty()
            && self
                .state
                .required_validations
                .iter()
                .all(|v| self.state.validation_status.get(v) == Some(&true));
        self.state.completion = if implementation_claimed && required_ok && evidence.all_passed() {
            CompletionState::Verified
        } else if implementation_claimed {
            CompletionState::ImplementedNotVerified
        } else {
            CompletionState::InProgress
        };
    }
}

fn extend_unique<I: IntoIterator<Item = String>>(target: &mut Vec<String>, source: I) {
    for v in source {
        if !target.contains(&v) {
            target.push(v);
        }
    }
}

#[derive(Debug, Default)]
pub struct EvidenceLedger {
    entries: Vec<Evidence>,
}
impl EvidenceLedger {
    #[must_use]
    pub fn new(entries: Vec<Evidence>) -> Self {
        Self { entries }
    }
    pub fn add(
        &mut self,
        claim: impl Into<String>,
        source_event: Uuid,
        status: EvidenceStatus,
        detail: impl Into<String>,
    ) -> Evidence {
        let e = Evidence {
            id: Uuid::new_v4(),
            claim: claim.into(),
            source_event,
            status,
            detail: detail.into(),
        };
        self.entries.push(e.clone());
        e
    }
    #[must_use]
    pub fn entries(&self) -> &[Evidence] {
        &self.entries
    }
    #[must_use]
    pub fn all_passed(&self) -> bool {
        !self.entries.is_empty()
            && self
                .entries
                .iter()
                .all(|e| e.status == EvidenceStatus::Passed)
    }
}

#[derive(Debug)]
pub struct FailureManager {
    retry_budget: u32,
    attempts: HashMap<String, u32>,
}
impl FailureManager {
    #[must_use]
    pub fn new(retry_budget: u32) -> Self {
        Self {
            retry_budget,
            attempts: HashMap::new(),
        }
    }
    pub fn record(&mut self, command: &str, output: &str) -> FailureDecision {
        let sig = normalize_failure(command, output);
        let count = self.attempts.entry(sig.clone()).or_default();
        *count += 1;
        FailureDecision {
            signature: sig,
            count: *count,
            reground: *count >= self.retry_budget,
        }
    }
    pub fn improvement(&mut self) {
        self.attempts.clear();
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDecision {
    pub signature: String,
    pub count: u32,
    pub reground: bool,
}
#[must_use]
pub fn normalize_failure(command: &str, output: &str) -> String {
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
        command.split_whitespace().next().unwrap_or(""),
        normalized
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn failures_trigger_reground() {
        let mut f = FailureManager::new(2);
        assert!(!f.record("cargo test", "error at 12").reground);
        assert!(f.record("cargo test", "error at 99").reground);
    }
    #[test]
    fn completion_needs_evidence() {
        let mut m = TaskStateManager::default();
        m.update(StateUpdate {
            required_validations: vec!["test".into()],
            validation_status: HashMap::from([("test".into(), true)]),
            ..Default::default()
        });
        let mut e = EvidenceLedger::default();
        m.recompute_completion(true, &e);
        assert_eq!(m.state.completion, CompletionState::ImplementedNotVerified);
        e.add("tests", Uuid::new_v4(), EvidenceStatus::Passed, "ok");
        m.recompute_completion(true, &e);
        assert_eq!(m.state.completion, CompletionState::Verified);
    }
}
