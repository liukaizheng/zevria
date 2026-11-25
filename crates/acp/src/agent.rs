use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
        Error as AcpError, Implementation, InitializeRequest, InitializeResponse,
        ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
        McpCapabilities, McpServer, NewSessionRequest, NewSessionResponse, PromptCapabilities,
        PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
        SessionCapabilities, SessionCloseCapabilities, SessionInfo, SessionListCapabilities,
        SessionMode as AcpSessionMode, SessionModeId, SessionModeState, SessionResumeCapabilities,
        SetSessionModeRequest, SetSessionModeResponse,
    },
};
use agent_client_protocol::{Agent, ConnectTo, ConnectionTo, Stdio};
use tokio::sync::Semaphore;
use zevria_foundation::{AcpMcpServerSpec, McpServerName, SessionMode};
use zevria_transcript::transcript::TranscriptItem;

use crate::project::diagnostic_update;
use crate::replay::replay_transcript;
use crate::session::{LiveSession, mode_id};
use crate::{
    AcpConfig, ClientState, ExecutionProfile, SessionRuntimeFactory, SessionStart,
    StartSessionRequest, StartedSession,
};

const SESSION_PAGE_SIZE: usize = 32;

async fn prepare_content(
    blocks: Vec<agent_client_protocol::schema::v1::ContentBlock>,
    reservation: &crate::session::InputPreparation,
    cancellation: &agent_client_protocol::RequestCancellation,
) -> Result<zevria_content::UserPrompt, AcpError> {
    if cancellation.is_cancelled() || reservation.cancellation().is_cancelled() {
        return Err(reservation.interrupted_error());
    }
    if blocks.iter().all(|block| {
        matches!(
            block,
            agent_client_protocol::schema::v1::ContentBlock::Text(_)
        )
    }) {
        return crate::prompt::from_content(&blocks);
    }
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(reservation.interrupted_error()),
        () = reservation.cancellation().cancelled() => return Err(reservation.interrupted_error()),
        result = tokio::task::spawn_blocking(move || crate::prompt::from_content(&blocks)) => {
            result.map_err(|_| AcpError::internal_error().data("image preparation task failed"))?
        }
    };
    if cancellation.is_cancelled() || reservation.cancellation().is_cancelled() {
        return Err(reservation.interrupted_error());
    }
    result
}

/// Both prompt RPCs reserve ordering before spawning and use the same
/// cancellation fence before and after engine admission.
async fn run_input(
    live: &LiveSession,
    blocks: Vec<agent_client_protocol::schema::v1::ContentBlock>,
    reservation: crate::session::InputPreparation,
    cancellation: &agent_client_protocol::RequestCancellation,
    operation: &str,
    begin: impl FnOnce(
        crate::session::InputPreparation,
        zevria_content::UserPrompt,
    ) -> Result<crate::session::PromptWait, AcpError>,
) -> Result<agent_client_protocol::schema::v1::StopReason, AcpError> {
    use agent_client_protocol::schema::v1::{ErrorCode, StopReason};
    let prepared = prepare_content(blocks, &reservation, cancellation).await;
    let mut response = match prepared.and_then(|content| begin(reservation, content)) {
        Ok(wait) => wait.response,
        Err(error) if error.code == ErrorCode::RequestCancelled => {
            return Ok(StopReason::Cancelled);
        }
        Err(error) => return Err(error),
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            live.cancel();
            response.await.unwrap_or(Ok(StopReason::Cancelled))
        }
        result = &mut response => result.unwrap_or_else(|_| Err(AcpError::internal_error().data(format!("{operation} result channel closed")))),
    }
}

pub async fn serve_stdio(
    config: AcpConfig,
    startup_workspace: PathBuf,
    factory: Arc<dyn SessionRuntimeFactory>,
) -> anyhow::Result<()> {
    serve_on(config, startup_workspace, factory, Stdio::new()).await
}

pub(crate) async fn serve_on(
    config: AcpConfig,
    startup_workspace: PathBuf,
    factory: Arc<dyn SessionRuntimeFactory>,
    transport: impl ConnectTo<Agent>,
) -> anyhow::Result<()> {
    config.validate()?;
    let startup_workspace = canonical_workspace(&startup_workspace).map_err(anyhow::Error::from)?;
    let state = Arc::new(ServerState {
        profile: factory.profile(),
        config,
        startup_workspace,
        factory,
        semaphore: Arc::new(Semaphore::new(config.max_sessions)),
        sessions: Mutex::new(HashMap::new()),
        activating: Arc::new(Mutex::new(HashSet::new())),
        client: Mutex::new(None),
        closing: AtomicBool::new(false),
    });

    let initialize_state = Arc::clone(&state);
    let new_state = Arc::clone(&state);
    let list_state = Arc::clone(&state);
    let load_state = Arc::clone(&state);
    let resume_state = Arc::clone(&state);
    let close_state = Arc::clone(&state);
    let mode_state = Arc::clone(&state);
    let prompt_state = Arc::clone(&state);
    let cancel_state = Arc::clone(&state);
    let eof_state = Arc::clone(&state);
    let skills_list_state = Arc::clone(&state);
    let skills_inspect_state = Arc::clone(&state);
    let skills_reload_state = Arc::clone(&state);
    let skills_write_state = Arc::clone(&state);
    let skills_invoke_state = Arc::clone(&state);

    Agent
        .builder()
        .name("zevria-acp")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                if request.protocol_version != ProtocolVersion::V1 {
                    return responder.respond_with_error(
                        AcpError::invalid_request().data(format!(
                            "Zevria supports ACP protocol V1; requested {}",
                            request.protocol_version
                        )),
                    );
                }
                let client = ClientState {
                    plan_operations: request.client_capabilities.plan.is_some(),
                    form_elicitation: request
                        .client_capabilities
                        .elicitation
                        .as_ref()
                        .and_then(|elicitation| elicitation.form.as_ref())
                        .is_some(),
                };
                *initialize_state
                    .client
                    .lock()
                    .expect("ACP client state poisoned") = Some(client);
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(agent_capabilities(initialize_state.config, initialize_state.profile))
                        .agent_info(Implementation::new(
                            "zevria",
                            env!("CARGO_PKG_VERSION"),
                        )),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, connection| {
                let state = Arc::clone(&new_state);
                let task_connection = connection.clone();
                connection.spawn(async move {
                    let result = async {
                        let live = state.open_session(
                            task_connection, &request.cwd, &request.additional_directories,
                            request.mcp_servers, SessionStart::New, false,
                        ).await?;
                        Ok(NewSessionResponse::new(live.id().clone()).modes(session_modes(live.mode(), state.profile)))
                    }
                    .await;
                    responder.respond_with_result(result)
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest, responder, connection| {
                let state = Arc::clone(&list_state);
                connection.spawn(async move {
                    let result = state.list(request).await;
                    responder.respond_with_result(result)
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, connection| {
                let state = Arc::clone(&load_state);
                let task_connection = connection.clone();
                connection.spawn(async move {
                    let result = async {
                        let live = state.open_session(
                            task_connection, &request.cwd, &request.additional_directories,
                            request.mcp_servers, SessionStart::Existing { session_id: request.session_id.to_string() }, true,
                        ).await?;
                        Ok(LoadSessionResponse::new().modes(session_modes(live.mode(), state.profile)))
                    }
                    .await;
                    responder.respond_with_result(result)
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, connection| {
                let state = Arc::clone(&resume_state);
                let task_connection = connection.clone();
                connection.spawn(async move {
                    let result = async {
                        let live = state.open_session(
                            task_connection, &request.cwd, &request.additional_directories,
                            request.mcp_servers, SessionStart::Existing { session_id: request.session_id.to_string() }, false,
                        ).await?;
                        Ok(ResumeSessionResponse::new().modes(session_modes(live.mode(), state.profile)))
                    }
                    .await;
                    responder.respond_with_result(result)
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionModeRequest, responder, connection| {
                let live = mode_state.session(&request.session_id)?;
                let mode = match request.mode_id.to_string().as_str() {
                    "build" if !mode_state.profile.is_worker() => SessionMode::Build,
                    "review" if mode_state.profile.is_worker() => SessionMode::Build,
                    "plan" => SessionMode::Plan,
                    other => {
                        return responder.respond_with_error(AcpError::invalid_params().data(
                            format!("unsupported session mode {other:?}; expected {}", if mode_state.profile.is_worker() { "plan or review" } else { "build or plan; orchestration is an explicit session/prompt metadata option" }),
                        ));
                    }
                };
                let wait = live.set_mode(mode)?;
                let cancellation = responder.cancellation();
                let cleanup = Arc::clone(&live);
                let request_id = wait.request_id.clone();
                if let Err(error) = connection.spawn(async move {
                    let outcome = tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {
                            let error = AcpError::request_cancelled();
                            live.interrupt_mode(Some(&wait.request_id), error.clone());
                            Err(error)
                        }
                        outcome = wait.response => outcome.unwrap_or_else(|_| {
                            Err(AcpError::internal_error().data("mode selection result channel closed"))
                        }),
                        () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                            let error = AcpError::internal_error().data("mode selection acknowledgment timed out; session management remains locked until the engine reports its result");
                            live.interrupt_mode(Some(&wait.request_id), error.clone());
                            Err(error)
                        }
                    };
                    responder.respond_with_result(outcome.map(|()| SetSessionModeResponse::new()))
                }) {
                    cleanup.interrupt_mode(Some(&request_id), error.clone());
                    return Err(error);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let live = prompt_state.session(&request.session_id)?;
                let behavior = crate::prompt::request_behavior(request.meta.as_ref().and_then(|meta| meta.get(crate::prompt::ORCHESTRATION_EXTENSION)))?;
                // Reserve on the receive path, but never decode/resample there.
                let reservation = live.reserve_input_preparation()?;
                let cancellation = responder.cancellation();
                connection.spawn(async move {
                    let result = run_input(&live, request.prompt, reservation, &cancellation,
                        "prompt", |reservation, text| reservation.begin_prompt(text, behavior)).await;
                    responder.respond_with_result(result.map(PromptResponse::new))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: crate::skills::SkillsListRequest, responder, connection| {
                crate::skills::check_version(request.version)?;
                let live = skills_list_state.session(&request.session_id)?;
                connection.spawn(async move {
                    responder.respond_with_result(live.manage_skills(zevria_instructions::skill::SkillManagementRequest::List { query: request.query }).await)
                })?;
                Ok(())
            }, agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: crate::skills::SkillsInspectRequest, responder, connection| {
                crate::skills::check_version(request.version)?;
                let live = skills_inspect_state.session(&request.session_id)?;
                connection.spawn(async move {
                    responder.respond_with_result(live.manage_skills(zevria_instructions::skill::SkillManagementRequest::Inspect { name: request.name }).await)
                })?;
                Ok(())
            }, agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: crate::skills::SkillsReloadRequest, responder, connection| {
                crate::skills::check_version(request.version)?;
                let live = skills_reload_state.session(&request.session_id)?;
                connection.spawn(async move {
                    responder.respond_with_result(live.manage_skills(zevria_instructions::skill::SkillManagementRequest::Reload { expected_revision: request.expected_revision }).await)
                })?;
                Ok(())
            }, agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: crate::skills::SkillsConfigWriteRequest, responder, connection| {
                crate::skills::check_version(request.version)?;
                let live = skills_write_state.session(&request.session_id)?;
                connection.spawn(async move {
                    responder.respond_with_result(live.manage_skills(zevria_instructions::skill::SkillManagementRequest::SetEnabled { expected_revision: request.expected_revision, name: request.name, enabled: request.enabled }).await)
                })?;
                Ok(())
            }, agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: crate::skills::SkillInvokeRequest, responder, connection| {
                crate::skills::check_version(request.version)?;
                let live = skills_invoke_state.session(&request.session_id)?;
                let reservation = live.reserve_input_preparation()?;
                let cancellation = responder.cancellation();
                connection.spawn(async move {
                    let result = run_input(&live, request.args, reservation, &cancellation,
                        "skill invocation", |reservation, args| reservation.begin_skill(request.name, args)).await;
                    responder.respond_with_result(result.map(|stop_reason| crate::skills::SkillInvokeResponse { version: crate::skills::SKILLS_EXTENSION_VERSION, stop_reason }))
                })?;
                Ok(())
            }, agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, connection| {
                let state = Arc::clone(&close_state);
                connection.spawn(async move {
                    let result = state.close(&request.session_id).await.map(|()| CloseSessionResponse::new());
                    responder.respond_with_result(result)
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                if let Ok(live) = cancel_state.session(&notification.session_id) {
                    live.cancel();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_close(async move |_connection| {
            eof_state
                .shutdown_all()
                .await
                .map_err(|error| AcpError::internal_error().data(error.to_string()))
        })
        .connect_to(transport)
        .await
        .map_err(anyhow::Error::from)
}

struct ServerState {
    profile: ExecutionProfile,
    config: AcpConfig,
    startup_workspace: PathBuf,
    factory: Arc<dyn SessionRuntimeFactory>,
    semaphore: Arc<Semaphore>,
    sessions: Mutex<HashMap<String, Arc<LiveSession>>>,
    activating: Arc<Mutex<HashSet<String>>>,
    client: Mutex<Option<ClientState>>,
    closing: AtomicBool,
}

struct ActivatedSession {
    live: Arc<LiveSession>,
    transcript_items: Vec<TranscriptItem>,
    startup_notices: Vec<String>,
    events: zevria_session_api::SessionEventReceiver,
    background_exit:
        std::pin::Pin<Box<dyn std::future::Future<Output = crate::RuntimeExit> + Send>>,
}

impl ServerState {
    async fn open_session(
        self: &Arc<Self>,
        connection: ConnectionTo<agent_client_protocol::Client>,
        cwd: &Path,
        additional_directories: &[PathBuf],
        servers: Vec<McpServer>,
        start: SessionStart,
        replay: bool,
    ) -> Result<Arc<LiveSession>, AcpError> {
        let mcp_servers =
            validate_roots_and_parse_mcp(cwd, additional_directories, servers, self.profile)?;
        let workspace = canonical_workspace(cwd)?;
        let activated = self
            .activate(connection, workspace, start, mcp_servers)
            .await?;
        if replay {
            activated.live.send_updates(replay_transcript(
                &activated.transcript_items,
                activated.live.workspace(),
            ))?;
        }
        publish_startup(&activated.live, &activated.startup_notices)?;
        activated.live.emit_initial_plan()?;
        activated
            .live
            .start_event_loop(activated.events, activated.background_exit);
        Ok(activated.live)
    }

    async fn activate(
        self: &Arc<Self>,
        connection: ConnectionTo<agent_client_protocol::Client>,
        workspace: PathBuf,
        start: SessionStart,
        mcp_servers: Vec<AcpMcpServerSpec>,
    ) -> Result<ActivatedSession, AcpError> {
        if self.closing.load(Ordering::Acquire) {
            return Err(AcpError::request_cancelled().data("ACP connection is closing"));
        }
        let client = self
            .client
            .lock()
            .expect("ACP client state poisoned")
            .clone()
            .ok_or_else(|| AcpError::invalid_request().data("initialize must complete first"))?;
        let activation_key = match &start {
            SessionStart::New => None,
            SessionStart::Existing { session_id } => Some(session_id.clone()),
        };
        let _guard = activation_key
            .as_ref()
            .map(|key| ActivationGuard::reserve(Arc::clone(&self.activating), key.clone()))
            .transpose()?;
        if let Some(key) = &activation_key {
            let duplicate = {
                let mut sessions = self.sessions.lock().expect("ACP session registry poisoned");
                sessions.retain(|_, session| !session.is_closed());
                sessions.contains_key(key)
            };
            if duplicate {
                return Err(
                    AcpError::invalid_params().data(format!("session {key:?} is already active"))
                );
            }
        }
        let permit = self.semaphore.clone().try_acquire_owned().map_err(|_| {
            AcpError::invalid_request().data(format!(
                "the ACP session limit of {} is already in use",
                self.config.max_sessions
            ))
        })?;
        let started = self
            .factory
            .start(StartSessionRequest {
                workspace: workspace.clone(),
                start,
                mcp_servers,
            })
            .await
            .map_err(internal_error)?;
        let StartedSession {
            session_id,
            workspace: runtime_workspace,
            transcript_items,
            selected_mode,
            plan_state,
            startup_notices,
            commands,
            events,
            background_exit,
            lifecycle,
        } = started;
        if runtime_workspace != workspace {
            lifecycle.shutdown().await.map_err(internal_error)?;
            return Err(AcpError::internal_error()
                .data("runtime factory returned a different canonical workspace"));
        }
        if session_id.trim().is_empty() {
            lifecycle.shutdown().await.map_err(internal_error)?;
            return Err(AcpError::internal_error().data("runtime returned an empty session ID"));
        }
        if self.profile.is_worker()
            && !client.plan_operations
            && (selected_mode == SessionMode::Plan
                || matches!(
                    plan_state,
                    zevria_workflow::PlanWorkflowState::Planning { .. }
                        | zevria_workflow::PlanWorkflowState::Ready { .. }
                ))
        {
            // Drop frontend endpoints before awaiting runtime shutdown, just
            // as normal close does. Activation must not leak a worker lease.
            drop(commands);
            drop(events);
            drop(background_exit);
            lifecycle.shutdown().await.map_err(internal_error)?;
            return Err(AcpError::invalid_params()
                .data("ensemble Plan workers require the ACP client Plan-operation capability"));
        }
        let live = LiveSession::new(
            session_id.clone(),
            runtime_workspace,
            commands,
            lifecycle,
            permit,
            selected_mode,
            plan_state,
            connection,
            client,
            self.profile,
        );
        let duplicate = {
            let mut sessions = self.sessions.lock().expect("ACP session registry poisoned");
            sessions.retain(|_, session| !session.is_closed());
            if sessions.contains_key(&session_id) {
                true
            } else {
                sessions.insert(session_id.clone(), Arc::clone(&live));
                false
            }
        };
        if duplicate {
            live.shutdown().await.map_err(internal_error)?;
            return Err(AcpError::invalid_params()
                .data(format!("session {session_id:?} is already active")));
        }
        Ok(ActivatedSession {
            live,
            transcript_items,
            startup_notices,
            events,
            background_exit,
        })
    }

    fn session(
        &self,
        id: &agent_client_protocol::schema::v1::SessionId,
    ) -> Result<Arc<LiveSession>, AcpError> {
        let mut sessions = self.sessions.lock().expect("ACP session registry poisoned");
        sessions.retain(|_, session| !session.is_closed());
        sessions
            .get(&id.to_string())
            .map(Arc::clone)
            .ok_or_else(|| {
                AcpError::invalid_params().data(format!("unknown or closed session {id}"))
            })
    }

    async fn close(
        &self,
        id: &agent_client_protocol::schema::v1::SessionId,
    ) -> Result<(), AcpError> {
        let active = self
            .sessions
            .lock()
            .expect("ACP session registry poisoned")
            .remove(&id.to_string())
            .ok_or_else(|| {
                AcpError::invalid_params().data(format!("unknown or closed session {id}"))
            })?;
        active.shutdown().await.map_err(internal_error)
    }

    async fn list(&self, request: ListSessionsRequest) -> Result<ListSessionsResponse, AcpError> {
        if !self.config.expose_session_list {
            return Err(
                AcpError::method_not_found().data("session/list is disabled by configuration")
            );
        }
        let workspace = match request.cwd {
            Some(cwd) => canonical_workspace(&cwd)?,
            None => self.startup_workspace.clone(),
        };
        let offset = request
            .cursor
            .as_deref()
            .map(parse_cursor)
            .transpose()?
            .unwrap_or(0);
        let sessions = self
            .factory
            .list(workspace.clone())
            .await
            .map_err(internal_error)?;
        if offset > sessions.len() {
            return Err(AcpError::invalid_params().data("session/list cursor is out of range"));
        }
        let end = offset.saturating_add(SESSION_PAGE_SIZE).min(sessions.len());
        let page = sessions[offset..end]
            .iter()
            .map(|session| {
                SessionInfo::new(session.id.clone(), session.workspace.clone())
                    .title(session.preview.clone())
            })
            .collect();
        let next = (end < sessions.len()).then(|| format!("zevria:{end}"));
        Ok(ListSessionsResponse::new(page).next_cursor(next))
    }

    async fn shutdown_all(&self) -> anyhow::Result<()> {
        if self.closing.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let sessions = self
            .sessions
            .lock()
            .expect("ACP session registry poisoned")
            .drain()
            .map(|(_, active)| active)
            .collect::<Vec<_>>();
        let mut first_error = None;
        for session in sessions {
            if let Err(error) = session.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

struct ActivationGuard {
    activating: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl ActivationGuard {
    fn reserve(activating: Arc<Mutex<HashSet<String>>>, key: String) -> Result<Self, AcpError> {
        if !activating
            .lock()
            .expect("ACP activation registry poisoned")
            .insert(key.clone())
        {
            return Err(AcpError::invalid_params()
                .data(format!("session {key:?} is already being activated")));
        }
        Ok(Self { activating, key })
    }
}

impl Drop for ActivationGuard {
    fn drop(&mut self) {
        self.activating
            .lock()
            .expect("ACP activation registry poisoned")
            .remove(&self.key);
    }
}

fn agent_capabilities(config: AcpConfig, profile: ExecutionProfile) -> AgentCapabilities {
    let sessions = SessionCapabilities::new()
        .list(
            config
                .expose_session_list
                .then(SessionListCapabilities::new),
        )
        .resume(SessionResumeCapabilities::new())
        .close(SessionCloseCapabilities::new());
    let capabilities = AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new().image(true))
        .session_capabilities(sessions);
    if profile.is_worker() {
        return capabilities;
    }
    capabilities.mcp_capabilities(McpCapabilities::new().http(true)).meta(serde_json::Map::from_iter([("zevria.skills".to_string(), serde_json::json!({
            "version": crate::skills::SKILLS_EXTENSION_VERSION,
            "requests": ["_zevria/skills/list", "_zevria/skills/inspect", "_zevria/skills/invoke", "_zevria/skills/reload", "_zevria/skills/config/write"],
            "notifications": ["_zevria/skills/changed"],
            "fixedRoots": true, "completeResults": true
        })), (crate::prompt::ORCHESTRATION_EXTENSION.to_string(), serde_json::json!({"version": 1, "request": "session/prompt", "buildOnly": true, "minimumBatchSize": 2}))]))
}

pub(crate) fn session_modes(current: SessionMode, profile: ExecutionProfile) -> SessionModeState {
    if profile.is_worker() {
        return SessionModeState::new(
            SessionModeId::new(mode_id(current, profile)),
            vec![
                AcpSessionMode::new("plan", "Plan").description("Read-only analysis with structured report-only Plan completion; no implementation approval."),
                AcpSessionMode::new("review", "Review").description("Read-only findings-first analysis using the Review model role."),
            ],
        );
    }
    SessionModeState::new(
        SessionModeId::new(mode_id(current, profile)),
        vec![
            AcpSessionMode::new("build", "Build").description(
                "Implement directly with full Build tools and optional read-only Explore subtasks; explicit orchestration metadata authorizes Builders for one request.",
            ),
            AcpSessionMode::new("plan", "Plan").description(
                "Investigate and produce a durable Plan artifact without implementation tools.",
            ),
        ],
    )
}

fn validate_roots_and_parse_mcp(
    cwd: &Path,
    additional_directories: &[PathBuf],
    servers: Vec<McpServer>,
    profile: ExecutionProfile,
) -> Result<Vec<AcpMcpServerSpec>, AcpError> {
    let invalid = |message: &str| AcpError::invalid_params().data(message.to_owned());
    if !cwd.is_absolute() {
        return Err(invalid("session cwd must be an absolute path"));
    }
    if !additional_directories.is_empty() {
        return Err(invalid(
            "additional workspace directories are not supported by Zevria ACP v1",
        ));
    }
    if profile.is_worker() && !servers.is_empty() {
        return Err(invalid(
            "MCP servers are not supported by Zevria Ensemble workers",
        ));
    }
    let mut names = BTreeSet::new();
    let mut parse_name = |name: &str| -> Result<McpServerName, AcpError> {
        let name: McpServerName = name.parse().map_err(invalid)?;
        if !names.insert(name.clone()) {
            return Err(invalid("duplicate MCP server name"));
        }
        Ok(name)
    };
    servers.into_iter().map(|server| match server {
        McpServer::Stdio(server) => {
            let name = parse_name(&server.name)?;
            let command = server.command.into_os_string().into_string().map_err(|_| invalid("invalid MCP stdio command"))?;
            if command.trim().is_empty() || command.contains('\0') || server.args.iter().any(|arg| arg.contains('\0')) {
                return Err(invalid("invalid MCP stdio command or arguments"));
            }
            let mut env = BTreeMap::new();
            for pair in server.env {
                if pair.name.is_empty() || pair.name.contains(['=', '\0']) || pair.value.contains('\0') || env.insert(pair.name, pair.value).is_some() {
                    return Err(invalid("invalid or repeated MCP environment variable"));
                }
            }
            Ok(AcpMcpServerSpec::Stdio { name, command, args: server.args, env })
        }
        McpServer::Http(server) => {
            let name = parse_name(&server.name)?;
            let url = url::Url::parse(&server.url).ok().filter(|url| {
                matches!(url.scheme(), "http" | "https") && url.host().is_some()
                    && url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
            }).ok_or_else(|| invalid("MCP HTTP servers require an http(s) URL without URL credentials or a fragment"))?;
            let mut headers = BTreeMap::new();
            for header in server.headers {
                if header.name.trim().is_empty() || header.value.contains(['\n', '\r']) || headers.insert(header.name.to_ascii_lowercase(), header.value).is_some() {
                    return Err(invalid("invalid or repeated MCP HTTP header"));
                }
            }
            Ok(AcpMcpServerSpec::Http { name, url: url.into(), headers })
        }
        _ => Err(invalid("unsupported MCP transport; use stdio or Streamable HTTP (not legacy SSE)")),
    }).collect()
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, AcpError> {
    if !path.is_absolute() {
        return Err(AcpError::invalid_params().data("workspace must be an absolute path"));
    }
    #[cfg(windows)]
    let canonical =
        zevria_foundation::windows_io::checked_directory_path(path).map_err(|error| {
            AcpError::invalid_params().data(format!(
                "unsupported protected Windows workspace {}: {error}",
                path.display()
            ))
        })?;
    #[cfg(not(windows))]
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        AcpError::invalid_params().data(format!(
            "failed to resolve workspace {}: {error}",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(AcpError::invalid_params().data(format!(
            "workspace {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn publish_startup(live: &LiveSession, notices: &[String]) -> Result<(), AcpError> {
    for (index, notice) in notices.iter().enumerate() {
        live.send_update(diagnostic_update(None, &format!("startup-{index}"), notice))?;
    }
    Ok(())
}

fn parse_cursor(cursor: &str) -> Result<usize, AcpError> {
    cursor
        .strip_prefix("zevria:")
        .and_then(|offset| offset.parse().ok())
        .ok_or_else(|| AcpError::invalid_params().data("invalid session/list cursor"))
}

fn internal_error(error: anyhow::Error) -> AcpError {
    AcpError::internal_error().data(error.to_string())
}
