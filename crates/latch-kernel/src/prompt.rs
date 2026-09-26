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
    /// Full compiled prompt (stable prefix followed by session context), for
    /// diagnostics such as `latch debug prompt`.
    pub text: String,
    /// Session-independent provider system prompt: the cacheable fragments
    /// only. Byte-identical across modes and workspaces.
    pub stable: String,
    /// Session-specific prompt content (workspace, repository instructions,
    /// mode), rendered by the agent as the first provider-visible message so
    /// it never invalidates the stable `system` + tools prefix.
    pub session: String,
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
    /// Proportional coding-agent architecture. Fragments are ordered
    /// least-volatile first so the provider's prefix cache survives the most
    /// change: the session-independent behavioral core (identity, general,
    /// effort, scope, planning, tools, editing, validation, communication)
    /// and Latch's kernel semantics lead, then per-session context
    /// (workspace, repository instructions), and finally the mode. A mode
    /// change (Ask/Plan/Work) rewrites only the tiny trailing fragment
    /// instead of invalidating the workspace and repository instructions that
    /// follow it in the request. Effort is the load-bearing rule: the smallest
    /// amount of inspection, implementation, reasoning, and validation that
    /// solves the task.
    pub fn compile(mode: Mode, workspace: &Path) -> Result<CompiledPrompt> {
        let mut f = vec![
            fragment(
                "core.identity",
                10,
                true,
                "You are Latch, a coding agent in the user's workspace. When asked to change code, find and change it instead of describing it.",
            ),
            fragment(
                "core.general",
                20,
                true,
                "Follow existing patterns and style; never revert changes you did not make. Infer ordinary choices from repository conventions and tests; ask only when a choice materially changes behavior or is irreversible. A new user message overrides earlier decisions and the plan: adapt remaining work and reconcile state with task_update.",
            ),
            fragment(
                "core.effort",
                30,
                true,
                "Match effort to the task. Simple, local work: inspect only what it touches, make the smallest coherent change, use the narrowest meaningful check, stop once the behavior is directly demonstrated. Standard work: inspect surrounding code, implement the scope, run targeted validation, broaden only on evidence. Complex or long-horizon work: plan when useful, inspect dependencies, validate broadly, delegate independent work.\n\nUse the smallest amount of inspection, implementation, reasoning, and validation that solves the task: carry the requested work through to completion, and stop at direct, relevant evidence. Do not keep searching for extra confidence, unrelated defects, or cleanup unless the task requires it.",
            ),
            fragment(
                "core.scope",
                40,
                true,
                "Deliver the requested scope completely; add no features, abstractions, compatibility layers, refactors, or speculative handling. Trust internal guarantees and validate at real boundaries.",
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
                "Read to act: do not open files the change does not touch. read_file returns bounded text, a hash for small files, and continuation; copy byte_offset and cursor_line when shown. Never re-read to confirm a guarded patch; re-read only when code changed or evidence requires it. search and read_artifact return bounded pages. Use read_image for PNG/JPEG/WebP (read_file refuses binary images); user-attached images are already visible when the model accepts image input. Prefer read_file, search, and git_diff over shell; never `cd` outside the workspace, and `cd` into a subdirectory only for read-only inspection. Use exec_start/exec_poll/exec_terminate for long commands. Batch independent reads, searches, and probes into one turn. A failed tool call is evidence: change assumptions, do not retry unchanged.",
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
                "Validate proportionally: a local one-file fix needs one focused check; a cross-cutting change may need workspace-wide validation. Run the narrowest command that demonstrates the behavior and stop when it passes; on failure, fix and re-check. Never claim a change works unless validation passed; state what remains unverified.",
            ),
            fragment(
                "core.communication",
                90,
                true,
                "Open with one sentence on what you will do; then stay quiet until a load-bearing finding, a direction change, or a blocker. Do not narrate each step, announce tool calls, or repeat tool output; the transcript already shows them. Never restate the plan or your reasoning. End with a concise summary: what changed, what was verified, and any limitation; cite paths, not diffs.",
            ),
            fragment(
                "latch.kernel_truth",
                110,
                true,
                "Validation intent is yours; validation truth is the kernel's. validate takes a requirement and the proving command: the kernel runs it, records evidence, and derives completion; a failed requirement that now passes is superseded. record_evidence accepts only pending or unavailable; passed and failed are kernel-owned. Without passing validation, completion stays IMPLEMENTED, NOT VERIFIED.",
            ),
            fragment(
                "latch.context_and_staleness",
                120,
                true,
                "Observations are versioned. If an edit is rejected as stale, the file changed outside Latch since it was read: re-read and regenerate; never overwrite newer changes. On re-ground, inspect reality and name disproven assumptions.",
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
            fragment(
                "latch.agent_group",
                145,
                true,
                "In an Agent Group: use group_status and group_task when coordination matters; claim work atomically before treating it as yours; never duplicate another agent's claimed work; message concisely about dependencies or findings; mark claimed tasks completed, blocked, or released accurately.",
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
            200,
            false,
            mode_text,
        ));
        // Per-session context follows the stable coding-agent behavior above,
        // and the mode is the trailing fragment: a mode switch rewrites only
        // this tail, so the workspace and repository instructions ahead of it
        // stay in the cached prefix.
        f.push(fragment(
            "environment.workspace",
            150,
            false,
            &format!("Workspace: {}", workspace.display()),
        ));
        for (index, (name, content)) in load_repository_instructions(workspace)?
            .into_iter()
            .enumerate()
        {
            f.push(fragment(
                &format!("environment.instructions.{name}"),
                160 + index as i32,
                false,
                &format!("Repository instructions from {name}:\n{content}"),
            ));
        }
        f.sort_by_key(|x| x.priority);
        // Fragment ids and versions are compiler metadata (shown by
        // `latch debug prompt`); the model only needs the instructions, so the
        // provider-facing text carries no per-fragment headers.
        let render = |fragments: &[&PromptFragment]| -> String {
            fragments
                .iter()
                .map(|x| x.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        let all: Vec<&PromptFragment> = f.iter().collect();
        let text = render(&all);
        // `stable` is the cacheable, session-independent behavioral core; the
        // agent sends it as the provider `system` field so the `system` + tools
        // prefix is byte-identical across sessions. `session` (workspace,
        // repository instructions, mode) travels as the first message instead.
        let stable = render(&f.iter().filter(|x| x.cacheable).collect::<Vec<_>>());
        let session = render(&f.iter().filter(|x| !x.cacheable).collect::<Vec<_>>());
        Ok(CompiledPrompt {
            text,
            stable,
            session,
            fragments: f,
        })
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
                "latch.agent_group",
                "environment.workspace",
                "mode.work",
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
            "read_image",
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
    fn instruction_following_rules_are_explicit() {
        let (_d, p) = work_prompt();
        // Read economy: the model must not survey the repository or open files
        // the change does not touch (over-inspection).
        let tools = p.fragment("core.tool_use").unwrap().content.clone();
        assert!(
            tools.contains("do not open files the change does not touch"),
            "read economy must be explicit"
        );
        // Narration discipline: no per-step status, no echoing tool output.
        let communication = p.fragment("core.communication").unwrap().content.clone();
        assert!(
            communication.contains("Do not narrate each step")
                && communication.contains("announce tool calls")
                && communication.contains("repeat tool output"),
            "narration discipline must be explicit"
        );
        // Validation claims require a recorded pass, never reasoning alone.
        let validation = p.fragment("core.validation").unwrap().content.clone();
        assert!(
            validation.contains("Never claim a change works unless validation passed"),
            "validation claims must require recorded evidence"
        );
    }

    #[test]
    fn stable_behavior_precedes_per_session_context() {
        let (_d, p) = work_prompt();
        // Canonical task state is no longer part of the compiled prompt; it is
        // rendered once by the continuity engine after the stable prefix.
        assert!(p.fragment("task.state").is_none());
        // The cacheable behavioral core leads; every per-session (non-cacheable)
        // fragment follows it, so changing one cannot invalidate the prefix.
        let highest_static = p
            .fragments
            .iter()
            .filter(|f| f.cacheable)
            .map(|f| f.priority)
            .max()
            .unwrap();
        for fragment in p.fragments.iter().filter(|f| !f.cacheable) {
            assert!(
                fragment.priority > highest_static,
                "{} must follow the cacheable behavior core",
                fragment.id
            );
        }
        // The mode is the most volatile fragment and must sit last, after the
        // workspace and repository instructions it must not invalidate.
        assert_eq!(p.fragments.last().map(|f| f.id.as_str()), Some("mode.work"));
    }

    #[test]
    fn stable_prefix_is_session_independent() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("AGENTS.md"), "repo rule A").unwrap();
        let work = PromptCompiler::compile(Mode::Work, a.path()).unwrap();
        let plan = PromptCompiler::compile(Mode::Plan, b.path()).unwrap();
        // The provider `system` field is identical across mode and workspace,
        // so the `system` + tools prefix is reused by the provider cache.
        assert_eq!(
            work.stable, plan.stable,
            "the system block must not vary by session"
        );
        assert!(!work.stable.contains("Workspace:"));
        assert!(!work.stable.contains("PLAN is deep read-only"));
        assert!(!work.stable.contains("repo rule A"));
        // Every session-specific fragment lives in the message tail instead.
        assert!(work.session.contains("Workspace:"));
        assert!(work.session.contains("WORK permits policy-approved"));
        assert!(work.session.contains("repo rule A"));
        assert!(plan.session.contains("PLAN is deep read-only"));
        assert_ne!(work.session, plan.session);
        // `text` remains the full concatenation for diagnostics.
        assert_eq!(work.text, format!("{}\n\n{}", work.stable, work.session));
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
        let (directory, p) = work_prompt();
        let estimator = crate::tokens::TokenEstimator::generic();
        let static_tokens: usize = p
            .fragments
            .iter()
            .filter(|f| f.cacheable)
            .map(|f| estimator.estimate(&f.content))
            .sum();
        // Proportional-effort architecture: the always-on stable prefix is a
        // small behavioral core plus kernel semantics. The cap is deliberately
        // just above today's size so accidental growth fails loudly. It was
        // raised once for image-input guidance and once, deliberately, for the
        // short agent-group coordination policy, then lowered after the prompt
        // was tightened and per-fragment headers were dropped from the
        // provider-facing text. It was raised again for three explicit
        // instruction-following rules: read only the region the change needs,
        // do not narrate steps or tool calls, and claim success only when the
        // recorded validation passed. The mode and per-session fragments are no
        // longer counted: they are ordered last so a session change cannot
        // invalidate the cached behavioral prefix.
        assert!(
            static_tokens <= 1_329,
            "static coding prompt grew to {static_tokens} tokens"
        );
        // The workspace path has a host-dependent length (and gains a drive
        // prefix on Windows), so exclude its token cost from this budget.
        let workspace_tokens = estimator.estimate(&directory.path().display().to_string());
        assert!(
            p.approximate_tokens().saturating_sub(workspace_tokens) <= 1_371,
            "compiled prompt excluding workspace path grew to {} tokens",
            p.approximate_tokens().saturating_sub(workspace_tokens)
        );
        assert!(
            !p.text.contains(" v2]"),
            "provider-facing prompt must not carry fragment headers"
        );
        assert_eq!(p.fragments.len(), 16);
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
        assert_eq!((core, latch), (9, 5), "module split stays as designed");
    }
}
