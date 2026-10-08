//! Serializes browser actions; the shared controller owns the running Agent.
use super::{ApiError, Host, Reply, state::View};
use crate::cli::{
    interactive::{SessionIo, run_session},
    session::{ProfileOverrides, build_agent, ingest_attachments},
};
use latch_ui::{Input, Output};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    hash::{Hash, Hasher},
    path::PathBuf,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use uuid::Uuid;

pub enum Action {
    Activate(Option<Uuid>, Vec<PathBuf>),
    Detach {
        session: Uuid,
        artifact: String,
    },
    Command {
        session: Uuid,
        command: Uuid,
        input: Input,
    },
    Upload {
        session: Uuid,
        bytes: Vec<u8>,
        name: Option<String>,
    },
}
pub struct Request {
    pub action: Action,
    pub reply: oneshot::Sender<Reply>,
}
struct Slot {
    input: mpsc::Sender<Input>,
    controller: JoinHandle<anyhow::Result<crate::cli::interactive::InteractiveOutcome>>,
    projection: JoinHandle<()>,
}

pub async fn run(
    host: Host,
    mut requests: mpsc::Receiver<Request>,
    overrides: ProfileOverrides,
    mut launch_attachments: Vec<PathBuf>,
) -> anyhow::Result<()> {
    let mut slot: Option<Slot> = None;
    let mut commands: BTreeMap<Uuid, (u64, Value, std::time::Instant)> = BTreeMap::new();
    let mut order = VecDeque::new();
    let mut deciding = BTreeSet::new();
    loop {
        let request = tokio::select! {
            _ = host.0.shutdown.cancelled() => break,
            request = requests.recv() => match request { Some(r) => r, None => break },
        };
        let result = match request.action {
            Action::Activate(selected, mut attachments) => {
                attachments.extend(launch_attachments.iter().cloned());
                let busy = {
                    let view = host.0.view.lock().await;
                    view.busy || view.starting
                };
                if busy {
                    Err(ApiError::conflict(
                        "finish or cancel the active turn before switching sessions",
                    ))
                } else {
                    if let Some(selected) = selected {
                        match host
                            .database()
                            .and_then(|s| s.resolve_session(&selected.to_string()))
                        {
                            Ok(summary)
                                if std::path::Path::new(&summary.workspace) == host.0.workspace => {
                            }
                            Ok(_) => {
                                let _ = request.reply.send(Err(ApiError::invalid(
                                    "session belongs to another workspace",
                                )));
                                continue;
                            }
                            Err(error) => {
                                let _ = request
                                    .reply
                                    .send(Err(ApiError::invalid(error.to_string())));
                                continue;
                            }
                        }
                    }
                    // Finish the old projection before replacing state. Its outputs
                    // can never be attributed to a new session.
                    if let Some(previous) = slot.take() {
                        stop(previous).await;
                    }
                    {
                        let mut view = host.0.view.lock().await;
                        let sequence = view.sequence;
                        *view = View {
                            starting: true,
                            sequence,
                            ..View::default()
                        };
                    }
                    host.changed().await;
                    let built = match latch_kernel::Config::load(host.0.config_path.as_deref()) {
                        Ok(config) => build_agent(
                            &host.0.workspace,
                            &config,
                            &overrides,
                            selected,
                            true,
                            &host.0.shutdown,
                        )
                        .await
                        .map_err(ApiError::runtime),
                        Err(error) => Err(ApiError::runtime(error)),
                    };
                    match built {
                        Err(error) => {
                            host.0.view.lock().await.starting = false;
                            host.changed().await;
                            Err(error)
                        }
                        Ok(mut built) => {
                            let id = built.agent.session_id;
                            let media = ingest_attachments(&built.context.config, id, &attachments)
                                .map_err(ApiError::runtime);
                            match media {
                                Err(error) => {
                                    let _ = built.agent.shutdown_extensions().await;
                                    host.0.view.lock().await.starting = false;
                                    host.changed().await;
                                    Err(error)
                                }
                                Ok(media) => {
                                    launch_attachments.clear();
                                    let resumed = built.restored.is_some();
                                    let mut view = host.0.view.lock().await;
                                    view.session_id = Some(id);
                                    view.sidebar = latch_ui::sidebar::SidebarState::new(
                                        latch_ui::sidebar::SidebarSession {
                                            mode: built.agent.mode(),
                                            model: built.info.profile.model.clone(),
                                            resumed,
                                            ..Default::default()
                                        },
                                    );
                                    if let Some(restored) = built.restored.take() {
                                        for event in &restored.events {
                                            view.event(event);
                                        }
                                    }
                                    view.sidebar.activity.interrupted(chrono::Utc::now());
                                    view.busy = false;
                                    view.starting = true;
                                    drop(view);
                                    let (input, receiver) = mpsc::channel(32);
                                    let (output, mut events) = mpsc::channel(512);
                                    let observer = host.clone();
                                    let projection = tokio::spawn(async move {
                                        while let Some(event) = events.recv().await {
                                            observer.0.view.lock().await.apply(event);
                                            observer.changed().await;
                                        }
                                    });
                                    let observer = host.clone();
                                    let workspace = host.0.workspace.clone();
                                    let controller = tokio::spawn(async move {
                                        let result = run_session(
                                            built,
                                            workspace,
                                            media,
                                            SessionIo {
                                                input: receiver,
                                                output,
                                                resumed,
                                            },
                                        )
                                        .await;
                                        if let Err(error) = &result {
                                            let mut view = observer.0.view.lock().await;
                                            view.starting = false;
                                            view.busy = false;
                                            view.apply(Output::Notice(format!(
                                                "error: session stopped: {error}"
                                            )));
                                            drop(view);
                                            observer.changed().await;
                                        }
                                        result
                                    });
                                    slot = Some(Slot {
                                        input,
                                        controller,
                                        projection,
                                    });
                                    deciding.clear();
                                    host.changed().await;
                                    Ok(json!({"session_id":id}))
                                }
                            }
                        }
                    }
                }
            }
            Action::Detach { session, artifact } => {
                let mut view = host.0.view.lock().await;
                if view.session_id != Some(session) {
                    Err(ApiError::conflict(
                        "session changed; reload before removing an attachment",
                    ))
                } else {
                    view.attachments.retain(|m| m.id != artifact);
                    drop(view);
                    host.changed().await;
                    Ok(json!({"removed":true}))
                }
            }
            Action::Upload {
                session,
                bytes,
                name,
            } => {
                if host.0.view.lock().await.session_id != Some(session) {
                    Err(ApiError::conflict(
                        "session changed; reload before attaching an image",
                    ))
                } else {
                    let root = host.artifacts(session);
                    match tokio::task::spawn_blocking(move || {
                        latch_kernel::media::ingest_image_bytes(&root, &bytes, name)
                    })
                    .await
                    {
                        Ok(Ok(reference)) => {
                            host.0
                                .view
                                .lock()
                                .await
                                .apply(Output::Attachment(reference.clone()));
                            host.changed().await;
                            Ok(json!(reference))
                        }
                        Ok(Err(error)) => Err(ApiError::invalid(error.to_string())),
                        Err(error) => Err(ApiError::runtime(error)),
                    }
                }
            }
            Action::Command {
                session,
                command,
                input,
            } => {
                let wire = serde_json::to_string(&input).map_err(ApiError::runtime);
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                session.hash(&mut hasher);
                match wire {
                    Err(error) => Err(error),
                    Ok(wire) => {
                        wire.hash(&mut hasher);
                        let fingerprint = hasher.finish();
                        while order.front().is_some_and(|id| {
                            commands
                                .get(id)
                                .is_some_and(|(_, _, time)| time.elapsed().as_secs() > 300)
                        }) {
                            if let Some(old) = order.pop_front() {
                                commands.remove(&old);
                            }
                        }
                        if let Some((previous, result, _)) = commands.get(&command) {
                            if *previous == fingerprint {
                                Ok(result.clone())
                            } else {
                                Err(ApiError::conflict(
                                    "command ID was already used for another action",
                                ))
                            }
                        } else if commands.len() >= 4096 {
                            Err(ApiError::conflict("too many recent commands; retry later"))
                        } else {
                            let result =
                                submit(&host, slot.as_ref(), session, input, &mut deciding).await;
                            if let Ok(value) = &result {
                                commands.insert(
                                    command,
                                    (fingerprint, value.clone(), std::time::Instant::now()),
                                );
                                order.push_back(command);
                            }
                            result
                        }
                    }
                }
            }
        };
        let _ = request.reply.send(result);
    }
    if let Some(slot) = slot {
        stop(slot).await;
    }
    Ok(())
}

async fn stop(slot: Slot) {
    let _ = slot.input.send(Input::Quit).await;
    let _ = slot.controller.await;
    let _ = slot.projection.await;
}

async fn submit(
    host: &Host,
    slot: Option<&Slot>,
    session: Uuid,
    input: Input,
    deciding: &mut BTreeSet<Uuid>,
) -> Reply {
    let slot = slot.ok_or_else(|| ApiError::conflict("create or select a session first"))?;
    let mut view = host.0.view.lock().await;
    if view.session_id != Some(session) {
        return Err(ApiError::conflict(
            "session changed; reload before submitting",
        ));
    }
    match &input {
        Input::Attach(_) | Input::Resume => {
            return Err(ApiError::invalid("use the session or attachment endpoint"));
        }
        Input::Permission { request_id, .. } => {
            deciding.retain(|id| view.pending.contains_key(id));
            if !view.pending.contains_key(request_id) || deciding.contains(request_id) {
                return Err(ApiError::conflict("approval request is no longer pending"));
            }
        }
        Input::Submit { text, media } => {
            if text.trim().is_empty() || text.len() > 256 * 1024 {
                return Err(ApiError::invalid(
                    "message must contain text and be at most 256 KiB",
                ));
            }
            if media.iter().any(|m| view.media.get(&m.id) != Some(m)) {
                return Err(ApiError::invalid(
                    "attachment does not belong to this session",
                ));
            }
            if view.busy && text.trim_start().starts_with('/') && !text.contains(['\n', '\r']) {
                return Err(ApiError::conflict(
                    "finish or cancel the active turn before running commands",
                ));
            }
        }
        Input::Cancel | Input::Quit => {}
        _ if view.busy => {
            return Err(ApiError::conflict(
                "finish or cancel the active turn before changing settings",
            ));
        }
        _ => {}
    }
    let begins_turn = matches!(&input, Input::Submit {text,..} if !(text.trim_start().starts_with('/') && !text.contains(['\n','\r'])));
    let permission = if let Input::Permission { request_id, .. } = &input {
        Some(*request_id)
    } else {
        None
    };
    if matches!(input, Input::Quit) {
        host.0.shutdown.cancel();
        return Ok(json!({"accepted":true}));
    }
    slot.input
        .try_send(input)
        .map_err(|_| ApiError::conflict("session is unavailable or its input queue is full"))?;
    if begins_turn {
        if !view.busy {
            view.sidebar.activity.preparing(chrono::Utc::now());
        }
        view.busy = true;
        view.queued_inputs += 1;
    }
    if let Some(id) = permission {
        deciding.insert(id);
    }
    drop(view);
    host.changed().await;
    Ok(json!({"accepted":true}))
}
