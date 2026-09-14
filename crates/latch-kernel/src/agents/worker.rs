use super::supervisor::{SupervisorInner, update_from_worker};
use crate::agent::{Agent, AgentEventSink};
use latch_protocol::{AgentIdentity, AgentMessage, AgentStatus, EventPayload};
use std::sync::{Arc, Weak};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) enum WorkerCommand {
    Information(AgentMessage),
    Continue(AgentMessage),
    Interrupt,
    Close,
}

pub(super) async fn run_worker(
    inner: Weak<SupervisorInner>,
    identity: AgentIdentity,
    agent: &mut Agent,
    mut receiver: mpsc::Receiver<WorkerCommand>,
    initial: Option<String>,
    restored_messages: Vec<AgentMessage>,
    lifetime: CancellationToken,
) {
    let child_mailbox = agent.child_mailbox_handle();
    for message in restored_messages {
        child_mailbox.push(message);
    }
    if let Some(brief) = initial
        && !run_turn(&inner, &identity, agent, &mut receiver, brief, &lifetime).await
    {
        return;
    }
    loop {
        let command = tokio::select! {
            () = lifetime.cancelled() => WorkerCommand::Close,
            command = receiver.recv() => match command {
                Some(command) => command,
                None => WorkerCommand::Close,
            }
        };
        match command {
            WorkerCommand::Information(message) => child_mailbox.push(message),
            WorkerCommand::Continue(message) => {
                let prompt = message.text.clone();
                child_mailbox.push(message);
                if !run_turn(&inner, &identity, agent, &mut receiver, prompt, &lifetime).await {
                    return;
                }
            }
            WorkerCommand::Interrupt => {
                let reason = "child interrupted while idle".to_owned();
                if let Some(shared) = inner.upgrade()
                    && shared
                        .store
                        .append(
                            identity.agent_id,
                            EventPayload::AgentInterrupted {
                                reason: reason.clone(),
                            },
                        )
                        .is_ok()
                {
                    update_from_worker(&inner, identity.agent_id, AgentStatus::Interrupted, None);
                }
            }
            WorkerCommand::Close => {
                close(&inner, identity.agent_id);
                return;
            }
        }
    }
}

async fn run_turn(
    inner: &Weak<SupervisorInner>,
    identity: &AgentIdentity,
    agent: &mut Agent,
    receiver: &mut mpsc::Receiver<WorkerCommand>,
    prompt: String,
    lifetime: &CancellationToken,
) -> bool {
    let Some(shared) = inner.upgrade() else {
        return false;
    };
    if shared
        .store
        .append(
            identity.agent_id,
            EventPayload::AgentStatusChanged {
                status: AgentStatus::Running,
                reason: None,
            },
        )
        .is_err()
    {
        return false;
    }
    update_from_worker(inner, identity.agent_id, AgentStatus::Running, None);
    let (outcome, interrupted, closing) = {
        let turn_cancel = lifetime.child_token();
        let child_mailbox = agent.child_mailbox_handle();
        let sink: AgentEventSink = Arc::new(|_| {});
        let run = agent.run(prompt.as_str(), turn_cancel.clone(), sink);
        tokio::pin!(run);
        let mut interrupted = false;
        let mut closing = false;
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                () = lifetime.cancelled() => {
                    closing = true;
                    turn_cancel.cancel();
                }
                command = receiver.recv() => match command {
                    Some(WorkerCommand::Information(message)) | Some(WorkerCommand::Continue(message)) => {
                        child_mailbox.push(message);
                    }
                    Some(WorkerCommand::Interrupt) => {
                        interrupted = true;
                        turn_cancel.cancel();
                    }
                    Some(WorkerCommand::Close) | None => {
                        closing = true;
                        turn_cancel.cancel();
                    }
                }
            }
        };
        (outcome, interrupted, closing)
    };
    if closing {
        close(inner, identity.agent_id);
        return false;
    }
    if interrupted {
        let reason = "current child turn interrupted by parent".to_owned();
        if shared
            .store
            .append(
                identity.agent_id,
                EventPayload::AgentInterrupted {
                    reason: reason.clone(),
                },
            )
            .is_ok()
        {
            update_from_worker(inner, identity.agent_id, AgentStatus::Interrupted, None);
        }
        return true;
    }
    let (status, summary) = match outcome {
        Ok(text) => (AgentStatus::Completed, text),
        Err(error) => (AgentStatus::Failed, error.to_string()),
    };
    let report = agent.agent_report(identity, status, summary);
    if shared
        .store
        .append(
            identity.agent_id,
            EventPayload::AgentReportCreated {
                report: report.clone(),
            },
        )
        .is_ok()
    {
        update_from_worker(inner, identity.agent_id, status, Some(report));
    }
    true
}

fn close(inner: &Weak<SupervisorInner>, agent_id: uuid::Uuid) {
    if let Some(shared) = inner.upgrade()
        && shared
            .store
            .append(agent_id, EventPayload::AgentClosed)
            .is_ok()
    {
        update_from_worker(inner, agent_id, AgentStatus::Closed, None);
    }
}
