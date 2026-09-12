use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct DelegationContext {
    pub constraints: Vec<String>,
    pub decisions: Vec<String>,
}

pub fn delegation_brief(
    task_name: &str,
    message: &str,
    workspace: &Path,
    context: &DelegationContext,
) -> String {
    let mut sections = vec![
        format!("Delegated task `{task_name}`:\n{}", message.trim()),
        format!("Shared workspace: {}", workspace.display()),
        "You are an independent child Latch session. Work only on this delegated task. Your task state, evidence, and completion are isolated from the parent. Repository instructions are supplied separately in the system prompt. You cannot spawn or control other agents.".into(),
    ];
    if !context.constraints.is_empty() {
        sections.push(format!(
            "Relevant user constraints:\n- {}",
            context.constraints.join("\n- ")
        ));
    }
    if !context.decisions.is_empty() {
        sections.push(format!(
            "Relevant parent decisions:\n- {}",
            context.decisions.join("\n- ")
        ));
    }
    sections.join("\n\n")
}
