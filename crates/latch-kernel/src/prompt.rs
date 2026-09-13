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
    /// Proportional coding-agent architecture. The stable prefix is a small
    /// behavioral core (identity, general, effort, scope, planning, tools,
    /// editing, validation, communication) followed by Latch's kernel
    /// semantics, then mode and per-session context. Effort is the load-bearing
    /// rule: the smallest amount of inspection, implementation, reasoning, and
    /// validation that solves the task.
    pub fn compile(mode: Mode, workspace: &Path) -> Result<CompiledPrompt> {
        let mut f = vec![
            fragment(
                "core.identity",
                10,
                true,
                "You are Latch, a coding agent in the user's workspace. Treat requests as engineering work: when asked to change code, find it and change it instead of describing the change.",
            ),
            fragment(
                "core.general",
                20,
                true,
                "Follow existing patterns and style; never revert changes you did not make. Infer ordinary choices from repository conventions and tests; ask only when a choice materially changes behavior or is irreversible. A new user message overrides earlier decisions and the current plan: adapt the remaining work immediately and reconcile canonical task state with task_update.",
            ),
            fragment(
                "core.effort",
                30,
                true,
                "Match effort to the task. Simple, local work: inspect only what it touches, make the smallest coherent change, use the narrowest meaningful check, stop once the behavior is directly demonstrated. Standard work: inspect surrounding code, implement the scope, run targeted validation, broaden only on evidence. Complex or long-horizon work: plan when useful, inspect dependencies, validate broadly, delegate independent work.\n\nUse the smallest amount of inspection, implementation, reasoning, and validation that solves the task. Carry the requested work through to completion, and stop at direct, relevant evidence: do not keep searching for extra confidence, unrelated defects, or cleanup unless the task requires it.",
            ),
            fragment(
                "core.scope",
                40,
                true,
                "Deliver the requested scope completely; do not add features, abstractions, compatibility layers, refactors, or speculative handling. Trust internal guarantees, validate at real boundaries, and keep minimal from becoming brittle.",
            ),
            fragment(
                "core.planning",
                50,
                true,
                "Plan only when it improves execution: genuinely multi-step, ambiguous, or long-horizon work. Do not plan straightforward tasks or write one-step plans; for a simple fix, act directly.",
            ),
            fragment(
                "core.tool_use",
                60,
                true,
                "read_file returns a bounded window with the file hash and continuation offset; re-read only when code changed or evidence requires it, and never re-read merely to confirm a successful guarded patch. search and read_artifact return bounded pages. Prefer read_file, search, and git_diff over shell. Avoid `cd <workspace> &&`; `cd` into a subdirectory only for read-only inspection, and never `cd` outside the workspace. Use exec_start/exec_poll/exec_terminate for long commands. A failed tool call is evidence: change assumptions, do not retry unchanged.",
            ),
            fragment(
                "core.editing",
                70,
                true,
                "Read a file before editing it and pass the hash from that read; make targeted edits, not rewrites; comment only where code is not self-explanatory.",
            ),
            fragment(
                "core.validation",
                80,
                true,
                "Validate proportionally: a local one-file fix needs one focused check; a cross-cutting change may need workspace-wide validation. Run the narrowest command that demonstrates the behavior and stop when it passes; on failure, fix and re-check.",
            ),
            fragment(
                "core.communication",
                90,
                true,
                "Say in one sentence what you are about to do before the first tool call, then update only at load-bearing findings, direction changes, or blockers. Never narrate deliberation or restate the plan. End with a concise summary: what changed, what was verified, and any remaining limitation; reference paths instead of pasting diffs.",
            ),
            fragment(
                "latch.kernel_truth",
                110,
                true,
                "Validation intent is yours; validation truth is the kernel's. validate takes a requirement and the proving command: the kernel runs it, records evidence, and derives completion; a failed requirement that now passes is superseded. record_evidence accepts only pending or unavailable claims; passed and failed are kernel-owned. Without passing validation, completion stays IMPLEMENTED, NOT VERIFIED.",
            ),
            fragment(
                "latch.context_and_staleness",
                120,
                true,
                "Observations are versioned. If an edit is rejected as stale, the file changed outside Latch since it was read: re-read and regenerate rather than forcing the old base; never overwrite newer changes. On re-ground, inspect reality, name disproven assumptions, and change strategy.",
            ),
            fragment(
                "latch.permissions",
                130,
                true,
                "Shell runs in a mandatory sandbox: request network, Git metadata mutation, or outside-writable paths explicitly in tool arguments (for example `capabilities: [\"network\"]`) so the kernel can classify them.",
            ),
            fragment(
                "latch.subagents",
                140,
                true,
                "Delegate only parallelizable, isolated, or independent workstreams where a child saves context or time; never spawn children for simple, sequential, or tightly coupled work. Children are independent sessions whose evidence never certifies root completion.",
            ),
        ];
        let mode_text = match mode {
            Mode::Ask => {
                "ASK is read-only: inspect and analyze freely. Shell runs against a read-only workspace with private scratch, so pipelines and analysis scripts are welcome; project writes fail by construction. Request needed capabilities explicitly so the kernel can ask."
            }
            Mode::Plan => {
                "PLAN is deep read-only exploration: inspect and produce an implementation plan; no workspace mutation is permitted."
            }
            Mode::Work => {
                "WORK permits policy-approved changes: implement the requested change and validate it; a formal plan is optional."
            }
        };
        f.push(fragment(
            &format!("mode.{}", mode.to_string().to_ascii_lowercase()),
            150,
            true,
            mode_text,
        ));
        // Per-session context follows the stable coding-agent behavior above.
        f.push(fragment(
            "environment.workspace",
            160,
            false,
            &format!("Workspace: {}", workspace.display()),
        ));
        for (index, (name, content)) in load_repository_instructions(workspace)?
            .into_iter()
            .enumerate()
        {
            f.push(fragment(
                &format!("environment.instructions.{name}"),
                180 + index as i32,
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
        version: 2,
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
        assert!(
            p.text.contains("PLAN is deep read-only") && p.text.contains("rule"),
            "mode text and repository instructions are present"
        );
        assert!(
            p.fragments
                .windows(2)
                .all(|w| w[0].priority <= w[1].priority)
        );
    }

    #[test]
    fn proportional_effort_modules_are_used() {
        let (_d, p) = work_prompt();
        let ids: Vec<&str> = p.fragments.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "core.identity",
                "core.general",
                "core.effort",
                "core.scope",
                "core.planning",
                "core.tool_use",
                "core.editing",
                "core.validation",
                "core.communication",
                "latch.kernel_truth",
                "latch.context_and_staleness",
                "latch.permissions",
                "latch.subagents",
                "mode.work",
                "environment.workspace",
            ]
        );
        let effort = p.fragment("core.effort").unwrap();
        assert!(effort.content.contains("Match effort to the task"));
        assert!(effort.content.contains("Simple, local work"));
        assert!(effort.content.contains("Standard work"));
        assert!(effort.content.contains("Complex or long-horizon work"));
        assert!(effort.content.contains("smallest amount"));
        assert!(
            p.fragment("core.identity")
                .unwrap()
                .content
                .contains("Latch")
        );
    }

    #[test]
    fn simple_task_does_not_require_planning() {
        let (_d, p) = work_prompt();
        let planning = p.fragment("core.planning").unwrap().content.clone();
        assert!(planning.contains("Do not plan straightforward tasks"));
        assert!(planning.contains("one-step plans"));
        assert!(planning.contains("for a simple fix, act directly"));
        let effort = p.fragment("core.effort").unwrap().content.clone();
        assert!(
            effort.contains("Simple, local work"),
            "the simple tier must not carry a planning requirement"
        );
    }

    #[test]
    fn narrow_validation_can_complete_a_narrow_change() {
        let (_d, p) = work_prompt();
        let validation = p.fragment("core.validation").unwrap().content.clone();
        assert!(validation.contains("one focused check"));
        assert!(validation.contains("stop when it passes"));
        let effort = p.fragment("core.effort").unwrap().content.clone();
        assert!(effort.contains("direct, relevant evidence"));
    }

    #[test]
    fn direct_evidence_ends_investigation() {
        let (_d, p) = work_prompt();
        let effort = p
            .fragment("core.effort")
            .unwrap()
            .content
            .to_ascii_lowercase();
        assert!(effort.contains("do not keep searching for extra confidence"));
        assert!(effort.contains("unrelated defects, or cleanup"));
        let tools = p.fragment("core.tool_use").unwrap().content.clone();
        assert!(tools.contains("re-read only when code changed or evidence requires it"));
    }

    #[test]
    fn broader_validation_is_reserved_for_cross_cutting_changes() {
        let (_d, p) = work_prompt();
        let validation = p.fragment("core.validation").unwrap().content.clone();
        assert!(validation.contains("cross-cutting change"));
        assert!(validation.contains("workspace-wide validation"));
        assert!(validation.contains("narrowest command"));
    }

    #[test]
    fn subagents_are_discouraged_for_simple_sequential_work() {
        let (_d, p) = work_prompt();
        let subagents = p.fragment("latch.subagents").unwrap().content.clone();
        assert!(
            subagents
                .contains("never spawn children for simple, sequential, or tightly coupled work")
        );
        assert!(subagents.contains("parallelizable, isolated, or independent"));
        assert!(subagents.contains("evidence never certifies root completion"));
    }

    #[test]
    fn obsolete_blanket_persistence_instructions_are_gone() {
        let (_d, p) = work_prompt();
        for banned in [
            "Default to action",
            "Do not stop at understanding the repository",
            "the first green build",
            "is not completion",
            "Long tasks may take many tool calls",
            "never stop merely because the session is long",
            "understand -> modify -> validate",
            "check the original request against the implementation",
            "policy.scope",
            "Justify substantial growth",
            "quiet terminal coding agent",
        ] {
            assert!(
                !p.text.contains(banned),
                "obsolete instruction `{banned}` is still injected"
            );
        }
    }

    #[test]
    fn persistence_rule_appears_once_across_stable_fragments() {
        let (_d, p) = work_prompt();
        let persistence_markers = [
            "carry the requested work through to completion",
            "until it is complete",
            "until the task is complete",
            "until the requested work is complete",
            "keep working",
            "keep going",
        ];
        let holders: Vec<&str> = p
            .fragments
            .iter()
            .filter(|f| f.cacheable)
            .filter(|f| {
                let content = f.content.to_ascii_lowercase();
                persistence_markers
                    .iter()
                    .any(|marker| content.contains(marker))
            })
            .map(|f| f.id.as_str())
            .collect();
        assert_eq!(
            holders,
            vec!["core.effort"],
            "the continue-until-done rule must live in exactly one module"
        );
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
            "exec_terminate",
            "never `cd` outside the workspace",
            "hash from that read",
            "validate",
            "record_evidence",
            "IMPLEMENTED, NOT VERIFIED",
            "re-ground",
            "stale",
            "capabilities",
            "overrides earlier decisions",
            "task_update",
        ] {
            assert!(
                p.text.contains(required),
                "required Latch guidance `{required}` is missing"
            );
        }
    }

    #[test]
    fn latch_kernel_and_subagent_semantics_remain() {
        let (_d, p) = work_prompt();
        let truth = p.fragment("latch.kernel_truth").unwrap();
        assert!(truth.content.contains("validation truth is the kernel's"));
        assert!(
            truth
                .content
                .contains("record_evidence accepts only pending or unavailable")
        );
        assert!(truth.content.contains("passed and failed are kernel-owned"));
        assert!(truth.content.contains("derives completion"));
        assert!(truth.content.contains("superseded"));
        let subagents = p.fragment("latch.subagents").unwrap();
        assert!(
            subagents
                .content
                .contains("Children are independent sessions")
        );
        assert!(
            subagents
                .content
                .contains("never certifies root completion")
        );
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
    fn prompt_stays_within_compact_budget() {
        let (_d, p) = work_prompt();
        let estimator = crate::tokens::TokenEstimator::generic();
        let static_tokens: usize = p
            .fragments
            .iter()
            .filter(|f| f.cacheable)
            .map(|f| estimator.estimate(&f.content))
            .sum();
        // Proportional-effort architecture: the always-on stable prefix is a
        // small behavioral core plus kernel semantics. The cap is deliberately
        // just above today's size so accidental growth fails loudly.
        assert!(
            static_tokens <= 1_220,
            "static coding prompt grew to {static_tokens} tokens"
        );
        assert!(
            p.approximate_tokens() <= 1_360,
            "compiled prompt grew to {} tokens",
            p.approximate_tokens()
        );
        assert_eq!(p.fragments.len(), 15);
        let core = p
            .fragments
            .iter()
            .filter(|f| f.id.starts_with("core."))
            .count();
        let latch = p
            .fragments
            .iter()
            .filter(|f| f.id.starts_with("latch."))
            .count();
        assert_eq!((core, latch), (9, 4), "module split stays as designed");
    }
}
