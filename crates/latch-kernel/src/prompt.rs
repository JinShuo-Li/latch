use anyhow::Result;
use latch_protocol::Mode;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PromptFragment {
    pub id: String,
    pub version: u32,
    pub priority: i32,
    pub cacheable: bool,
    pub content: String,
}
#[derive(Debug, Default)]
pub struct CompiledPrompt {
    pub text: String,
    pub fragments: Vec<PromptFragment>,
}
impl CompiledPrompt {
    /// Conservative token estimate for the compiled prompt. Provider-reported
    /// usage remains authoritative after a real request; this is only a
    /// pre-request estimate.
    #[must_use]
    pub fn approximate_tokens(&self) -> usize {
        crate::tokens::TokenEstimator::generic().estimate(&self.text)
    }
    pub fn fragment(&self, id: &str) -> Option<&PromptFragment> {
        self.fragments.iter().find(|f| f.id == id)
    }
}

pub struct PromptCompiler;
impl PromptCompiler {
    pub fn compile(mode: Mode, workspace: &Path) -> Result<CompiledPrompt> {
        let mut f = vec![
            fragment(
                "core.identity",
                10,
                true,
                "You are Latch, a coding agent working autonomously in a terminal inside the user's workspace. Interpret requests as software engineering work: when asked to change code, find the relevant code and change it instead of describing the change.",
            ),
            fragment(
                "core.execution",
                20,
                true,
                "Default to action. Once the task is clear enough to proceed, carry it through without asking for confirmation on ordinary, reversible decisions; an approved task covers its in-scope steps end to end. Do not stop at understanding the repository, proposing a plan, finding the likely bug, the first edit, or the first green build.\n\nFor implementation work the loop is: understand -> modify -> validate -> diagnose -> modify -> validate. Every tool call should resolve a specific unknown, change the implementation, validate behavior, or diagnose a concrete failure.\n\nContinue until the task is complete or you are blocked on something only the user can resolve. A successful tool call, a passing narrow test, or a large amount of work already done is not completion: before finishing, check the original request against the implementation and confirm every material requirement was addressed. Long tasks may take many tool calls; never stop merely because the session is long.

A new user message received mid-task overrides earlier decisions and the current plan: adapt the remaining work immediately instead of finishing the obsolete plan, reconcile canonical task state with task_update, and inspect current reality rather than assuming completed work can simply be redone.",
            ),
            fragment(
                "core.inspection",
                30,
                true,
                "Inspect with a purpose: read enough code to make the next informed change, then act. Prefer targeted reads and searches over exhaustive repository archaeology. Re-inspect a file, symbol, or query only when the code changed, a result gives new evidence, or a concrete question remains; if several actions yield no new information or repository change, change approach. Batch independent inspections in one response.",
            ),
            fragment(
                "core.scope",
                40,
                true,
                "Deliver the requested scope completely. Do not add features, abstractions, compatibility layers, refactors, or speculative error handling beyond the task; validate only at real boundaries and trust internal guarantees. Minimal changes are not an excuse for an incomplete or brittle result: make the smallest coherent change that fully solves the problem.",
            ),
            fragment(
                "core.decisions",
                50,
                true,
                "When several ordinary implementation choices exist, inspect repository conventions, tests, and configuration and pick one. Infer what the repository already answers instead of asking; ask only when the choice materially changes the requested behavior or is irreversible.",
            ),
            fragment(
                "core.tool_semantics",
                60,
                true,
                "Prefer read_file, search, git_status, and git_diff over shell for inspection; they preserve provenance and version hashes. read_file returns a bounded window with the file hash and a continuation offset, so read ranges instead of whole files; search returns a bounded page, and read_artifact pages through spilled shell, search, diff, or validation output. Shell runs inside a mandatory workspace sandbox: pipes, quoting, and scripts are fine, but the sandbox enforces what the safety profile grants. When an operation needs network, Git metadata mutation, or an outside writable path, request the capability explicitly in the tool arguments (for example `capabilities: [\"network\"]`) so the kernel can classify it. Write plain commands rather than `cd <workspace> &&`, `cd` into a workspace subdirectory only for read-only inspection, and never `cd` outside the workspace. For servers, watchers, and long builds use exec_start, then exec_poll and exec_terminate instead of a blocking call. Read a file before editing it and pass the hash from that read; tool failures are evidence — reconsider assumptions rather than retrying the same call.",
            ),
            fragment(
                "core.communication",
                70,
                true,
                "Your text output is what the user reads between tool calls. Say in one sentence what you are about to do before the first tool call, then give a short update only at load-bearing findings, direction changes, or blockers; do not narrate deliberation or restate the plan. End with a concise, outcome-first summary of what changed, what was verified, and any remaining limitation — not a chronological transcript. Match length to the task.",
            ),
            fragment(
                "policy.evidence",
                80,
                true,
                "Validation intent is yours; validation truth is the kernel's. Call validate with a semantic requirement name and the proving command: the kernel runs it, records the evidence, and derives completion. A failed requirement that now passes is superseded. record_evidence reports only pending or unavailable non-command claims; you cannot self-certify passed or failed. Do not claim completion until required validation has passing evidence — otherwise the kernel reports IMPLEMENTED, NOT VERIFIED.",
            ),
            fragment(
                "policy.stale_context",
                90,
                true,
                "Observations are versioned. If an edit is rejected as stale, the file changed outside Latch since it was read: re-read and regenerate the change instead of forcing the old base, and never overwrite newer changes.",
            ),
            fragment(
                "policy.failure",
                100,
                true,
                "When re-ground is requested, inspect current reality, name the disproven assumptions, and choose a materially different strategy before any further mutation.",
            ),
        ];
        let mode_text = match mode {
            Mode::Ask => {
                "ASK is read-only: inspect and analyze freely. Shell is available and runs against a read-only workspace with private scratch space, so pipelines, awk, jq, and analysis scripts are welcome; writes to project files fail by construction. If an inspection genuinely needs an ungranted capability, request it explicitly so the kernel can ask."
            }
            Mode::Plan => {
                "PLAN is deep read-only exploration. Inspect and produce an implementation plan; no workspace mutation is permitted. Shell runs against a read-only workspace with private scratch space, so non-trivial analysis commands are fine; request needed capabilities explicitly."
            }
            Mode::Work => {
                "WORK permits policy-approved changes. Investigate, implement, validate, and fix failures until the requested work is complete; a formal plan is optional. Commands run inside the sandbox, and external or privileged capabilities must be requested explicitly so the kernel can classify them."
            }
        };
        f.push(fragment(
            &format!("mode.{}", mode.to_string().to_ascii_lowercase()),
            110,
            true,
            mode_text,
        ));
        // Per-session context follows the stable coding-agent behavior above.
        f.push(fragment(
            "environment.workspace",
            120,
            false,
            &format!("Workspace: {}", workspace.display()),
        ));
        for (index, (name, content)) in load_repository_instructions(workspace)?
            .into_iter()
            .enumerate()
        {
            f.push(fragment(
                &format!("environment.instructions.{name}"),
                140 + index as i32,
                false,
                &format!("Repository instructions from {name}:\n{content}"),
            ));
        }
        f.sort_by_key(|x| x.priority);
        let text = f
            .iter()
            .map(|x| format!("[{} v{}]\n{}", x.id, x.version, x.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(CompiledPrompt { text, fragments: f })
    }
}
fn fragment(id: &str, priority: i32, cacheable: bool, content: &str) -> PromptFragment {
    PromptFragment {
        id: id.into(),
        version: 1,
        priority,
        cacheable,
        content: content.into(),
    }
}
fn load_repository_instructions(workspace: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for name in ["CLAUDE.md", "AGENTS.md", ".latch/instructions.md"] {
        let p = workspace.join(name);
        if p.is_file() {
            out.push((name.into(), std::fs::read_to_string(p)?));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_prompt() -> (tempfile::TempDir, CompiledPrompt) {
        let d = tempfile::tempdir().unwrap();
        let p = PromptCompiler::compile(Mode::Work, d.path()).unwrap();
        (d, p)
    }

    #[test]
    fn assembles_fragments_in_order() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "rule").unwrap();
        let p = PromptCompiler::compile(Mode::Plan, d.path()).unwrap();
        assert!(p.text.contains("PLAN is deep read-only") && p.text.contains("rule"));
        assert!(
            p.fragments
                .windows(2)
                .all(|w| w[0].priority <= w[1].priority)
        );
    }

    #[test]
    fn coding_agent_execution_prompt_is_used() {
        let (_d, p) = work_prompt();
        let ids: Vec<&str> = p.fragments.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "core.identity",
                "core.execution",
                "core.inspection",
                "core.scope",
                "core.decisions",
                "core.tool_semantics",
                "core.communication",
                "policy.evidence",
                "policy.stale_context",
                "policy.failure",
                "mode.work",
                "environment.workspace",
            ]
        );
        let execution = p.fragment("core.execution").unwrap();
        assert!(execution.content.contains("Default to action"));
        assert!(
            execution
                .content
                .contains("understand -> modify -> validate")
        );
        assert!(execution.content.contains("not completion"));
        let inspection = p.fragment("core.inspection").unwrap();
        assert!(inspection.content.contains("Inspect with a purpose"));
        let scope = p.fragment("core.scope").unwrap();
        assert!(
            scope
                .content
                .contains("Deliver the requested scope completely")
        );
        let communication = p.fragment("core.communication").unwrap();
        assert!(communication.content.contains("outcome-first"));
    }

    #[test]
    fn obsolete_scope_and_hedging_instructions_are_gone() {
        let (_d, p) = work_prompt();
        for banned in [
            "policy.scope",
            "Keep changes within the requested scope",
            "Justify substantial growth",
            "quiet terminal coding agent",
            "Stop when the requested task is satisfied",
        ] {
            assert!(
                !p.text.contains(banned),
                "obsolete instruction `{banned}` is still injected"
            );
        }
    }

    #[test]
    fn latch_tool_and_runtime_guidance_remains() {
        let (_d, p) = work_prompt();
        for required in [
            "read_file",
            "read_artifact",
            "search",
            "git_diff",
            "exec_start",
            "exec_poll",
            "never `cd` outside the workspace",
            "hash from that read",
            "validate",
            "record_evidence",
            "IMPLEMENTED, NOT VERIFIED",
            "re-ground",
        ] {
            assert!(
                p.text.contains(required),
                "required Latch guidance `{required}` is missing"
            );
        }
    }

    #[test]
    fn stable_behavior_precedes_per_session_context() {
        let (_d, p) = work_prompt();
        // Canonical task state is no longer part of the compiled prompt; it is
        // rendered once by the continuity engine after the stable prefix.
        assert!(p.fragment("task.state").is_none());
        let dynamic = ["environment.workspace"];
        let lowest_static = p
            .fragments
            .iter()
            .filter(|f| !dynamic.contains(&f.id.as_str()))
            .map(|f| f.priority)
            .max()
            .unwrap();
        for fragment in p
            .fragments
            .iter()
            .filter(|f| dynamic.contains(&f.id.as_str()))
        {
            assert!(
                fragment.priority > lowest_static,
                "{} must follow stable behavior",
                fragment.id
            );
            assert!(!fragment.cacheable);
        }
    }

    #[test]
    fn compiled_prompt_is_deterministic_and_ignores_task_state() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "rule").unwrap();
        let first = PromptCompiler::compile(Mode::Work, d.path()).unwrap();
        let second = PromptCompiler::compile(Mode::Work, d.path()).unwrap();
        assert_eq!(first.text, second.text, "stable prefix is byte-identical");
        assert!(!first.text.contains("Current canonical task state"));
    }

    #[test]
    fn prompt_does_not_grow_without_bound() {
        let (_d, p) = work_prompt();
        let estimator = crate::tokens::TokenEstimator::generic();
        let static_tokens: usize = p
            .fragments
            .iter()
            .filter(|f| f.cacheable)
            .map(|f| estimator.estimate(&f.content))
            .sum();
        // The safety/sandbox and live-steering guidance added deliberate
        // operational rules; the bound stays just above today's size so any
        // accidental unbounded growth still fails the test.
        assert!(
            static_tokens <= 1_550,
            "static coding prompt grew to {static_tokens} tokens"
        );
        assert!(
            p.approximate_tokens() <= 1_800,
            "compiled prompt grew to {} tokens",
            p.approximate_tokens()
        );
        assert_eq!(p.fragments.len(), 12);
    }
}
