//! ACP session identity and startup configuration expectations.

use agent_client_protocol::schema::v1::{
    ClientCapabilities, ClientSessionCapabilities, ElicitationCapabilities,
    ElicitationFormCapabilities, LoadSessionRequest, Meta, NewSessionRequest, PlanCapabilities,
    ResumeSessionRequest, SessionConfigOption, SessionConfigOptionsCapabilities, SessionId,
    SessionModeState,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use zevria_workflow::EnsembleWorkflow;

pub(super) struct SessionSetup {
    pub(super) session_id: SessionId,
    pub(super) modes: Option<SessionModeState>,
    pub(super) config_options: Option<Vec<SessionConfigOption>>,
    pub(super) recovered: bool,
    pub(super) interrupted: Option<String>,
}

#[derive(Clone)]
pub(super) struct SessionConfigurationExpectation {
    pub(super) safe_mode: SafeModeExpectation,
    pub(super) workflow: EnsembleWorkflow,
    pub(super) workflow_options: BTreeMap<String, String>,
}

#[derive(Clone)]
pub(super) struct SafeModeExpectation {
    pub(super) desired: String,
    pub(super) config_id: Option<String>,
}

pub(super) struct SafeModeBootstrap {
    pub(super) expectation: SafeModeExpectation,
    pub(super) config_options: Option<Vec<SessionConfigOption>>,
}

#[derive(Debug, Default)]
pub(super) struct SessionBootstrapGate {
    state: SessionBootstrapState,
}

#[derive(Debug, Default)]
enum SessionBootstrapState {
    #[default]
    Bootstrapping,
    Provisional(String),
    Bound(String),
}

impl SessionBootstrapGate {
    pub(super) fn observe_notification(&mut self, session_id: &str) -> Result<(), String> {
        match &self.state {
            SessionBootstrapState::Bootstrapping => {
                self.state = SessionBootstrapState::Provisional(session_id.to_string());
                Ok(())
            }
            SessionBootstrapState::Provisional(provisional) if provisional == session_id => Ok(()),
            SessionBootstrapState::Bound(expected) if expected == session_id => Ok(()),
            SessionBootstrapState::Provisional(provisional) => Err(format!(
                "agent sent an update for conflicting provisional ACP session {session_id:?}; first provisional session was {provisional:?}"
            )),
            SessionBootstrapState::Bound(expected) => Err(format!(
                "agent sent an update for mismatched ACP session {session_id:?}; expected {expected:?}"
            )),
        }
    }

    pub(super) fn bind(&mut self, session_id: &str) -> Result<(), String> {
        match &self.state {
            SessionBootstrapState::Bootstrapping => {
                self.state = SessionBootstrapState::Bound(session_id.to_string());
                Ok(())
            }
            SessionBootstrapState::Provisional(provisional) if provisional == session_id => {
                self.state = SessionBootstrapState::Bound(session_id.to_string());
                Ok(())
            }
            SessionBootstrapState::Bound(expected) if expected == session_id => Ok(()),
            SessionBootstrapState::Provisional(provisional) => Err(format!(
                "session/new returned ACP session {session_id:?}, which conflicts with provisional session {provisional:?}"
            )),
            SessionBootstrapState::Bound(expected) => Err(format!(
                "cannot rebind ACP session from {expected:?} to {session_id:?}"
            )),
        }
    }

    pub(super) fn bound_id(&self) -> Option<&str> {
        match &self.state {
            SessionBootstrapState::Bound(session_id) => Some(session_id),
            SessionBootstrapState::Bootstrapping | SessionBootstrapState::Provisional(_) => None,
        }
    }
}

pub(super) fn client_capabilities(workflow: EnsembleWorkflow) -> ClientCapabilities {
    let capabilities = ClientCapabilities::new()
        .session(
            ClientSessionCapabilities::new()
                .config_options(SessionConfigOptionsCapabilities::new()),
        )
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    match workflow {
        EnsembleWorkflow::Plan => capabilities.plan(PlanCapabilities::new()),
        EnsembleWorkflow::Review => capabilities,
    }
}

pub(super) fn bind_session_gate(
    gate: &Mutex<SessionBootstrapGate>,
    session_id: &SessionId,
) -> Result<(), String> {
    gate.lock()
        .expect("ACP session gate lock poisoned")
        .bind(&session_id.to_string())
}

pub(super) fn new_session_request(workspace: &Path, meta: &Option<Meta>) -> NewSessionRequest {
    NewSessionRequest::new(workspace.to_path_buf()).meta(meta.clone())
}

pub(super) fn resume_session_request(
    session_id: SessionId,
    workspace: &Path,
    meta: &Option<Meta>,
) -> ResumeSessionRequest {
    ResumeSessionRequest::new(session_id, workspace.to_path_buf()).meta(meta.clone())
}

pub(super) fn load_session_request(
    session_id: SessionId,
    workspace: &Path,
    meta: &Option<Meta>,
) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, workspace.to_path_buf()).meta(meta.clone())
}
