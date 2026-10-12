use super::supervisor::{SupervisorInner, update_from_worker};
use crate::agent::{Agent, AgentEventSink};
use latch_protocol::{AgentIdentity, AgentMessage, AgentStatus, EventPayload};
use std::future::Future;
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
    receiver: mpsc::Receiver<WorkerCommand>,
    initial: Option<String>,
    restored_messages: Vec<AgentMessage>,
    lifetime: CancellationToken,
) {
    let configs = inner
        .upgrade()
        .map(|s| {
            s.mcp_servers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
        .unwrap_or_default();
    for config in &configs {
        if let Err(error) = agent.load_mcp(config, &lifetime).await {
            let report = agent.agent_report(
                &identity,
                AgentStatus::Failed,
                format!("MCP startup: {error:#}"),
            );
            if let Some(shared) = inner.upgrade()
                && shared
                    .store
                    .append(
                        identity.agent_id,
                        EventPayload::AgentReportCreated {
                            report: report.clone(),
                        },
                    )
                    .is_ok()
            {
                update_from_worker(&inner, identity.agent_id, AgentStatus::Failed, Some(report));
            }
            let _ = agent.shutdown_extensions().await;
            return;
        }
    }
    worker_loop(
        inner,
        identity,
        agent,
        receiver,
        initial,
        restored_messages,
        lifetime,
    )
    .await;
    let _ = agent.shutdown_extensions().await;
}

async fn worker_loop(
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
    if let Some(brief) = initial {
        if !run_turn(&inner, &identity, agent, &mut receiver, brief, &lifetime).await {
            return;
        }
        if !drain_follow_ups(
            &inner,
            &identity,
            agent,
            &mut receiver,
            &child_mailbox,
            &lifetime,
        )
        .await
        {
            return;
        }
    }
    // A worker rebuilt after a process restart restores queued messages and
    // waits: a restored follow-up is consumed by the next explicit continue,
    // which carries the same durable message, exactly like the pre-restart
    // turn would have been.
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
                if !drain_follow_ups(
                    &inner,
                    &identity,
                    agent,
                    &mut receiver,
                    &child_mailbox,
                    &lifetime,
                )
                .await
                {
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

/// A continue command accepted while a turn is finishing can land in the
/// mailbox after that turn's last safe-boundary drain. It must still start a
/// turn: an accepted follow-up is never silently stranded, and information-only
/// messages continue to wait for the next boundary without waking the child.
async fn drain_follow_ups(
    inner: &Weak<SupervisorInner>,
    identity: &AgentIdentity,
    agent: &mut Agent,
    receiver: &mut mpsc::Receiver<WorkerCommand>,
    mailbox: &super::mailbox::ChildMailbox,
    lifetime: &CancellationToken,
) -> bool {
    loop {
        if !mailbox.has_follow_up() {
            return true;
        }
        // The prompt is intentionally empty: the child loop drains the mailbox
        // itself, so the queued follow-up is the turn's user input.
        if !run_turn(inner, identity, agent, receiver, String::new(), lifetime).await {
            return false;
        }
    }
}

/// Races one child turn against lifetime cancellation and worker commands.
///
/// Once closing starts, the lifetime future and the command channel are
/// disabled: an already-cancelled lifetime is permanently ready and a closed
/// command channel is permanently empty, so re-arming either branch would
/// busy-spin at full CPU until the child turn observes cancellation and
/// finishes. Returns the turn outcome, whether an explicit interrupt was
/// requested, and whether this worker is closing.
pub(super) async fn race_turn<F>(
    run: F,
    receiver: &mut mpsc::Receiver<WorkerCommand>,
    lifetime: &CancellationToken,
    turn_cancel: &CancellationToken,
    child_mailbox: &super::mailbox::ChildMailbox,
) -> (anyhow::Result<String>, bool, bool)
where
    F: Future<Output = anyhow::Result<String>>,
{
    let mut interrupted = false;
    let mut closing = false;
    tokio::pin!(run);
    let outcome = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            () = lifetime.cancelled(), if !closing => {
                closing = true;
                turn_cancel.cancel();
            }
            command = receiver.recv(), if !closing => match command {
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
        race_turn(
            agent.run(prompt.as_str(), turn_cancel.clone(), sink),
            receiver,
            lifetime,
            &turn_cancel,
            &child_mailbox,
        )
        .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use latch_protocol::AgentMessageKind;

    fn message(text: &str) -> AgentMessage {
        AgentMessage {
            message_id: uuid::Uuid::new_v4(),
            kind: AgentMessageKind::Information,
            text: text.into(),
        }
    }

    /// CR-06 regression: after lifetime cancellation sets closing, the command
    /// branch must not re-arm, so a command arriving during shutdown never
    /// reaches the child mailbox and the loop cannot spin on a permanently
    /// ready cancellation future.
    #[tokio::test]
    async fn cancelled_lifetime_stops_races_and_ignores_late_commands() {
        let mailbox = super::super::mailbox::ChildMailbox::default();
        let raced_mailbox = mailbox.clone();
        let lifetime = CancellationToken::new();
        lifetime.cancel();
        let turn_cancel = CancellationToken::new();
        let cancel_seen = turn_cancel.clone();
        let (release, run_waits) = tokio::sync::oneshot::channel::<()>();
        let run = async move {
            run_waits.await.ok();
            Ok("finished".to_owned())
        };
        let (commands, mut receiver) = mpsc::channel(4);
        let raced = tokio::spawn(async move {
            race_turn(run, &mut receiver, &lifetime, &turn_cancel, &raced_mailbox).await
        });
        // The cancellation branch fires exactly once and cancels the turn.
        cancel_seen.cancelled().await;
        commands
            .send(WorkerCommand::Information(message("late")))
            .await
            .unwrap();
        // Give a re-armed command branch a chance to misbehave before asserting.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            mailbox.drain().is_empty(),
            "commands accepted after closing must not reach the child"
        );
        release.send(()).unwrap();
        let (outcome, interrupted, closing) = raced.await.unwrap();
        assert_eq!(outcome.unwrap(), "finished");
        assert!(!interrupted);
        assert!(closing);
    }

    /// A command that arrives before closing still interrupts the turn.
    #[tokio::test]
    async fn interrupt_before_closing_still_cancels_the_turn() {
        let mailbox = super::super::mailbox::ChildMailbox::default();
        let raced_mailbox = mailbox.clone();
        let lifetime = CancellationToken::new();
        let turn_cancel = CancellationToken::new();
        let cancel_seen = turn_cancel.clone();
        let (release, run_waits) = tokio::sync::oneshot::channel::<()>();
        let run = async move {
            run_waits.await.ok();
            Ok("interrupted".to_owned())
        };
        let (commands, mut receiver) = mpsc::channel(4);
        let raced = tokio::spawn(async move {
            race_turn(run, &mut receiver, &lifetime, &turn_cancel, &raced_mailbox).await
        });
        commands.send(WorkerCommand::Interrupt).await.unwrap();
        cancel_seen.cancelled().await;
        release.send(()).unwrap();
        let (outcome, interrupted, closing) = raced.await.unwrap();
        assert!(interrupted);
        assert!(!closing);
        assert_eq!(outcome.unwrap(), "interrupted");
    }

    /// A closed command channel closes the worker once and then waits for the
    /// turn, instead of treating every empty receive as a fresh close.
    #[tokio::test]
    async fn closed_command_channel_waits_for_the_turn() {
        let mailbox = super::super::mailbox::ChildMailbox::default();
        let raced_mailbox = mailbox.clone();
        let lifetime = CancellationToken::new();
        let turn_cancel = CancellationToken::new();
        let cancel_seen = turn_cancel.clone();
        let (release, run_waits) = tokio::sync::oneshot::channel::<()>();
        let run = async move {
            run_waits.await.ok();
            Ok("closed".to_owned())
        };
        let (commands, mut receiver) = mpsc::channel::<WorkerCommand>(1);
        drop(commands);
        let raced = tokio::spawn(async move {
            race_turn(run, &mut receiver, &lifetime, &turn_cancel, &raced_mailbox).await
        });
        // The empty channel closes the worker and cancels the turn exactly once.
        cancel_seen.cancelled().await;
        release.send(()).unwrap();
        let (outcome, interrupted, closing) = raced.await.unwrap();
        assert!(closing);
        assert!(!interrupted);
        assert_eq!(outcome.unwrap(), "closed");
    }
}
