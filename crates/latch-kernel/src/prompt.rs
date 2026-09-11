use anyhow::Result;
use latch_protocol::{Mode, TaskState};
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
    #[must_use]
    pub fn approximate_tokens(&self) -> usize {
        self.text.len().div_ceil(4)
    }
    pub fn fragment(&self, id: &str) -> Option<&PromptFragment> {
        self.fragments.iter().find(|f| f.id == id)
    }
}

pub struct PromptCompiler;
impl PromptCompiler {
    pub fn compile(mode: Mode, state: &TaskState, workspace: &Path) -> Result<CompiledPrompt> {
        let mut f = vec![
            fragment(
                "core.identity",
                10,
                true,
                "You are Latch, a quiet terminal coding agent. Reason about reality; the kernel owns and records reality.",
            ),
            fragment(
                "core.communication",
                20,
                true,
                "Communicate concisely. Explain findings, actions, and evidence. Stop when the requested task is satisfied.",
            ),
            fragment(
                "core.tool_semantics",
                30,
                true,
                "Prefer the dedicated read_file, search, git_status, and git_diff tools for inspection; they preserve provenance and version hashes. Use shell only for checks those tools cannot express, and prefer a single dedicated tool over a compound shell pipeline. Read a file before editing and pass its observed hash. Tool failures are evidence; reconsider assumptions rather than inventing success.",
            ),
            fragment(
                "policy.evidence",
                40,
                true,
                "Do not report the task complete until required validation has produced evidence. If validation is unavailable, report IMPLEMENTED, NOT VERIFIED.",
            ),
            fragment(
                "policy.failure",
                50,
                true,
                "When re-ground is requested, inspect current reality, name disproven assumptions, and choose a materially different strategy before further mutation.",
            ),
            fragment(
                "policy.scope",
                60,
                true,
                "Keep changes within the requested scope. Justify substantial growth in files, lines, dependencies, or modules before continuing.",
            ),
            fragment(
                "policy.stale_context",
                70,
                true,
                "Treat observations as versioned. If an edit is stale, re-read and regenerate it; never overwrite newer changes.",
            ),
        ];
        let mode_text = match mode {
            Mode::Ask => {
                "ASK is read-only. Inspect with read_file, search, git_status, and git_diff. Do not run tests, builds, or package managers, and do not use shell to modify the workspace; shell is limited to conservative read-only commands."
            }
            Mode::Plan => {
                "PLAN is deep read-only exploration. Produce an implementation plan; no workspace mutation is permitted. Prefer dedicated inspection tools over shell, which is limited to conservative read-only commands."
            }
            Mode::Work => {
                "WORK permits policy-approved changes. Investigate, modify, verify, review, and stop naturally; a formal plan is optional."
            }
        };
        f.push(fragment(
            &format!("mode.{}", mode.to_string().to_ascii_lowercase()),
            80,
            true,
            mode_text,
        ));
        f.push(fragment(
            "task.state",
            90,
            false,
            &format!(
                "Current canonical task state:\n{}",
                serde_json::to_string_pretty(state)?
            ),
        ));
        f.push(fragment(
            "environment.workspace",
            100,
            false,
            &format!("Workspace: {}", workspace.display()),
        ));
        for (index, (name, content)) in load_repository_instructions(workspace)?
            .into_iter()
            .enumerate()
        {
            f.push(fragment(
                &format!("environment.instructions.{name}"),
                110 + index as i32,
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
    #[test]
    fn assembles_fragments_in_order() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "rule").unwrap();
        let p = PromptCompiler::compile(Mode::Plan, &TaskState::default(), d.path()).unwrap();
        assert!(p.text.contains("PLAN is deep read-only") && p.text.contains("rule"));
        assert!(
            p.fragments
                .windows(2)
                .all(|w| w[0].priority <= w[1].priority)
        );
    }
}
