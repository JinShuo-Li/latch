//! Disposable browser projection; durable events remain the source of truth.
use latch_protocol::{Event, EventPayload, MediaRef};
use latch_ui::{
    Output,
    presentation::PresentationModel,
    sidebar::{SidebarSession, SidebarState},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

pub struct View {
    pub session_id: Option<Uuid>,
    pub busy: bool,
    pub starting: bool,
    pub queued_inputs: usize,
    pub sequence: u64,
    pub metadata: BTreeMap<String, Value>,
    pub pending: BTreeMap<Uuid, Value>,
    pub media: BTreeMap<String, MediaRef>,
    pub streaming: String,
    pub attachments: Vec<MediaRef>,
    pub presentation: PresentationModel,
    pub sidebar: SidebarState,
    pub history: Vec<String>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            session_id: None,
            busy: false,
            starting: false,
            queued_inputs: 0,
            sequence: 0,
            metadata: BTreeMap::new(),
            pending: BTreeMap::new(),
            media: BTreeMap::new(),
            streaming: String::new(),
            attachments: Vec::new(),
            presentation: PresentationModel::default(),
            sidebar: SidebarState::new(SidebarSession::default()),
            history: Vec::new(),
        }
    }
}

impl View {
    pub fn event(&mut self, event: &Event) {
        self.presentation.apply_event(event);
        self.sidebar.apply_event(event);
        match &event.payload {
            EventPayload::UserMessage { text, media } => {
                self.history.push(text.clone());
                self.attachments
                    .retain(|a| !media.iter().any(|m| m.id == a.id));
                for reference in media {
                    self.media.insert(reference.id.clone(), reference.clone());
                }
            }
            EventPayload::ToolCompleted { result } | EventPayload::ToolFailed { result } => {
                for reference in &result.media {
                    self.media.insert(reference.id.clone(), reference.clone());
                }
            }
            EventPayload::AssistantMessageCompleted { .. } => self.streaming.clear(),
            EventPayload::RunStarted { .. } => self.busy = true,
            EventPayload::RunCompleted { .. } => {
                self.streaming.clear();
            }
            EventPayload::PermissionRequested {
                request_id,
                tool,
                arguments,
                reason,
                capabilities,
            } => {
                self.pending.insert(*request_id, json!({"request_id":request_id,"tool":tool,"arguments":arguments,"reason":reason,"capabilities":capabilities}));
            }
            EventPayload::PermissionResolved { request_id, .. } => {
                self.pending.remove(request_id);
            }
            _ => {}
        }
    }

    pub fn apply(&mut self, output: Output) {
        match output {
            Output::Ready => {
                self.starting = false;
                self.busy = self.queued_inputs > 0;
            }
            Output::InputReceived => {
                self.queued_inputs = self.queued_inputs.saturating_sub(1);
            }
            Output::Event(event) => self.event(&event),
            Output::AssistantDelta(text) => self.streaming.push_str(&text),
            Output::AssistantDone => {
                self.streaming.clear();
            }
            Output::ToolResult(result) => {
                for reference in &result.media {
                    self.media.insert(reference.id.clone(), reference.clone());
                }
                self.presentation.apply_tool_result(&result);
            }
            Output::Notice(text) => {
                if text.starts_with("error:") {
                    self.presentation.push_error(text);
                } else {
                    self.presentation.push_notice(text);
                }
            }
            Output::Attachment(reference) => {
                if !self.attachments.iter().any(|m| m.id == reference.id) {
                    self.attachments.push(reference.clone());
                }
                self.media.insert(reference.id.clone(), reference);
            }
            Output::History(history) => self.history = history,
            output => {
                let wire = serde_json::to_value(&output).expect("UI output is serializable");
                let kind = wire["type"].as_str().unwrap_or("unknown").to_owned();
                self.metadata
                    .insert(kind, wire.get("data").cloned().unwrap_or(Value::Bool(true)));
                if let Output::Header {
                    model,
                    branch,
                    resumed,
                    pricing,
                    ..
                } = output
                {
                    self.metadata.remove("setup_required");
                    let mut session = self.sidebar.session().clone();
                    session.model = model;
                    session.branch = branch;
                    session.resumed = resumed;
                    session.pricing = pricing;
                    self.sidebar.set_session(session);
                } else if let Output::Mode(mode) = output {
                    let mut session = self.sidebar.session().clone();
                    session.mode = mode;
                    self.sidebar.set_session(session);
                }
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "session_id":self.session_id,"busy":self.busy,"starting":self.starting,
            "sequence":self.sequence,"metadata":self.metadata,"pending_permissions":self.pending.values().collect::<Vec<_>>(),
            "cells":self.presentation.cells(),"streaming":self.streaming,"history":self.history,
            "pending_attachments":self.attachments,
            "media":self.media.values().collect::<Vec<_>>(),"sidebar":self.sidebar,
            "validation_status":self.sidebar.validation_status(),"estimated_cost":self.sidebar.estimated_cost(),
        })
    }
}
