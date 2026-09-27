//! Session recovery, safe-mode setup, and prompt continuation.

use super::*;

pub(super) async fn recover_session(
    connection: &ConnectionTo<Agent>,
    previous_session_id: Option<String>,
    workspace: &Path,
    capabilities: &AgentCapabilities,
    log: &RunLog,
    session_gate: &Arc<Mutex<SessionBootstrapGate>>,
    session_meta: &Option<Meta>,
) -> Result<SessionSetup, AcpError> {
    let Some(previous_session_id) = previous_session_id else {
        return Ok(interrupted_setup(
            "the worker transcript has no ACP session ID to recover",
        ));
    };
    let session_id = SessionId::new(previous_session_id);
    bind_session_gate(session_gate, &session_id).map_err(acp_error)?;
    if capabilities.session_capabilities.resume.is_some() {
        match connection
            .send_request(resume_session_request(
                session_id.clone(),
                workspace,
                session_meta,
            ))
            .block_task()
            .await
        {
            Ok(response) => {
                return Ok(SessionSetup {
                    session_id,
                    modes: response.modes,
                    config_options: response.config_options,
                    recovered: true,
                    interrupted: None,
                });
            }
            Err(error) if capabilities.load_session => {
                log.emit(AgentRunEvent::Status {
                    status: AgentRunStatus::Resuming,
                    detail: Some(format!(
                        "session/resume failed ({error}); falling back to session/load"
                    )),
                })
                .await
                .map_err(acp_error)?;
            }
            Err(error) => {
                return Ok(interrupted_setup(format!(
                    "session/resume failed and session/load is unavailable: {error}"
                )));
            }
        }
    } else if !capabilities.load_session {
        return Ok(interrupted_setup(
            "agent advertises neither session/resume nor session/load",
        ));
    }

    log.emit(AgentRunEvent::ReplayBoundary)
        .await
        .map_err(acp_error)?;
    match connection
        .send_request(load_session_request(
            session_id.clone(),
            workspace,
            session_meta,
        ))
        .block_task()
        .await
    {
        Ok(response) => Ok(SessionSetup {
            session_id,
            modes: response.modes,
            config_options: response.config_options,
            recovered: true,
            interrupted: None,
        }),
        Err(error) => Ok(interrupted_setup(format!(
            "session/load failed during worker recovery: {error}"
        ))),
    }
}

pub(super) fn interrupted_setup(error: impl Into<String>) -> SessionSetup {
    SessionSetup {
        session_id: SessionId::new("interrupted"),
        modes: None,
        config_options: None,
        recovered: true,
        interrupted: Some(error.into()),
    }
}

pub(super) async fn enforce_safe_mode(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    desired: &str,
    config_options: Option<Vec<SessionConfigOption>>,
    modes: Option<SessionModeState>,
) -> Result<SafeModeBootstrap, AcpError> {
    if let Some(options) = config_options.as_ref() {
        let mode_options = options
            .iter()
            .filter(|option| option.category == Some(SessionConfigOptionCategory::Mode))
            .collect::<Vec<_>>();
        if !mode_options.is_empty() {
            let Some(option) = mode_options
                .iter()
                .copied()
                .find(|option| config_option_supports(option, desired))
            else {
                return Err(acp_error(format!(
                    "configured safe mode {desired:?} is unavailable in ACP mode config options"
                )));
            };
            let config_id = option.id.clone();
            let response = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id.clone(),
                    desired,
                ))
                .block_task()
                .await?;
            let verified = response
                .config_options
                .iter()
                .find(|candidate| candidate.id == config_id)
                .is_some_and(|candidate| config_option_current(candidate) == Some(desired));
            if !verified {
                return Err(acp_error(format!(
                    "agent did not verify configured safe mode {desired:?} after session/set_config_option"
                )));
            }
            return Ok(SafeModeBootstrap {
                expectation: SafeModeExpectation {
                    desired: desired.to_string(),
                    config_id: Some(config_id.to_string()),
                },
                config_options: Some(response.config_options),
            });
        }
    }

    let Some(modes) = modes else {
        return Err(acp_error(format!(
            "agent exposes neither a categorized mode config option nor legacy session modes; cannot enforce {desired:?}"
        )));
    };
    if !modes
        .available_modes
        .iter()
        .any(|mode| mode.id.to_string() == desired)
    {
        return Err(acp_error(format!(
            "configured safe mode {desired:?} is unavailable in legacy ACP session modes"
        )));
    }
    if modes.current_mode_id.to_string() != desired {
        connection
            .send_request(SetSessionModeRequest::new(
                session_id.clone(),
                desired.to_string(),
            ))
            .block_task()
            .await?;
    }
    Ok(SafeModeBootstrap {
        expectation: SafeModeExpectation {
            desired: desired.to_string(),
            config_id: None,
        },
        config_options,
    })
}

pub(super) async fn apply_workflow_config_options(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    workflow: EnsembleWorkflow,
    configured: &BTreeMap<String, String>,
    safe_mode: &SafeModeExpectation,
    mut config_options: Option<Vec<SessionConfigOption>>,
) -> Result<(), AcpError> {
    for (id, desired) in configured {
        let Some(options) = config_options.as_ref() else {
            return Err(acp_error(format!(
                "{} requires ACP workflow configuration option {id:?} = {desired:?}, but the agent advertised no config-option list",
                workflow.slash_command()
            )));
        };
        let Some(option) = options
            .iter()
            .find(|option| option.id.to_string() == id.as_str())
        else {
            return Err(acp_error(format!(
                "{} requires ACP workflow configuration option {id:?} = {desired:?}, but that exact option ID is absent from the advertised config options",
                workflow.slash_command()
            )));
        };
        let mode_field = match workflow {
            EnsembleWorkflow::Plan => "plan_mode",
            EnsembleWorkflow::Review => "review_mode",
        };
        if safe_mode.config_id.as_deref() == Some(id.as_str())
            || option.category == Some(SessionConfigOptionCategory::Mode)
        {
            return Err(acp_error(format!(
                "{} workflow configuration option {id:?} = {desired:?} overlaps the authoritative safe-mode setting; configure mode-category options with {mode_field} instead",
                workflow.slash_command()
            )));
        }
        if !config_option_supports(option, desired) {
            let failure = if matches!(&option.kind, SessionConfigKind::Select(_)) {
                "does not advertise the desired select value"
            } else {
                "uses an unsupported non-select option kind"
            };
            return Err(acp_error(format!(
                "{} workflow configuration option {id:?} = {desired:?} {failure}",
                workflow.slash_command()
            )));
        }

        if config_option_current(option) != Some(desired.as_str()) {
            let config_id = option.id.clone();
            let response = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id,
                    desired.as_str(),
                ))
                .block_task()
                .await
                .map_err(|error| {
                    acp_error(format!(
                        "ACP rejected {} workflow configuration option {id:?} = {desired:?}: {error}",
                        workflow.slash_command()
                    ))
                })?;
            config_options = Some(response.config_options);
        }

        let options = config_options
            .as_ref()
            .expect("configured workflow options require an advertised option list");
        let Some(verified) = options
            .iter()
            .find(|option| option.id.to_string() == id.as_str())
        else {
            return Err(acp_error(format!(
                "agent failed to verify {} workflow configuration option {id:?} = {desired:?}: the session/set_config_option response omitted that exact option ID",
                workflow.slash_command()
            )));
        };
        if config_option_current(verified) != Some(desired.as_str()) {
            return Err(acp_error(format!(
                "agent failed to verify {} workflow configuration option {id:?} = {desired:?}: the reported current value does not match",
                workflow.slash_command()
            )));
        }
    }
    Ok(())
}

pub(super) fn config_option_supports(option: &SessionConfigOption, desired: &str) -> bool {
    let SessionConfigKind::Select(select) = &option.kind else {
        return false;
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .any(|option| option.value.to_string() == desired),
        SessionConfigSelectOptions::Grouped(groups) => groups.iter().any(|group| {
            group
                .options
                .iter()
                .any(|option| option.value.to_string() == desired)
        }),
        _ => false,
    }
}

pub(super) fn config_option_current(option: &SessionConfigOption) -> Option<&str> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(select.current_value.0.as_ref())
}

pub(super) async fn run_prompt_sequence(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    prompt: zevria_content::UserPrompt,
    workflow: EnsembleWorkflow,
    log: &RunLog,
    context: PromptRunContext<'_>,
) -> Result<WorkerEnd, AcpError> {
    let cancellation = context.cancellation;
    let plan_handoff = context.plan_handoff;
    let mut deadline = context
        .turn_timeout
        .map(|timeout| tokio::time::Instant::now() + timeout);
    let mut end = dispatch_prompt_with_transient_recovery(
        connection,
        session_id.clone(),
        prompt,
        workflow,
        log,
        &context,
        &mut deadline,
    )
    .await?;
    if cancellation.is_cancelled() {
        return Ok(WorkerEnd::LocalCancelled);
    }

    if matches!(end, WorkerEnd::PromptResponse { .. })
        && let Some(error) = plan_handoff.and_then(ClaudePlanHandoff::unresolved_violation)
    {
        // Proposal-local failures are not connection-global violations. Feedback
        // may start another generation; unresolved mutation evidence survives.
        end = WorkerEnd::NativeHandoffFailed(error);
    }
    let repair = match &end {
        WorkerEnd::PromptResponse { stop_reason } => {
            semantic_repair_for_response(workflow, stop_reason, log)
        }
        WorkerEnd::NativeHandoffCompleted
        | WorkerEnd::NativeHandoffFailed(_)
        | WorkerEnd::TimedOut
        | WorkerEnd::LocalCancelled
        | WorkerEnd::Interrupted(_) => None,
    };
    if let Some(repair) = repair
        && log.repair().is_none()
    {
        let repair_prompt = semantic_repair_prompt(workflow);
        log.emit(AgentRunEvent::Prompt {
            text: repair_prompt.clone(),
            continuation: true,
            repair: Some(repair),
        })
        .await
        .map_err(acp_error)?;
        if cancellation.is_cancelled() {
            return Ok(WorkerEnd::LocalCancelled);
        }
        end = dispatch_prompt_with_transient_recovery(
            connection,
            session_id,
            repair_prompt.into(),
            workflow,
            log,
            &context,
            &mut deadline,
        )
        .await?;
        if cancellation.is_cancelled() {
            return Ok(WorkerEnd::LocalCancelled);
        }
    }
    Ok(end)
}

// Only session/prompt errors enter this helper. Startup, authentication, session
// bootstrap, and arbitrary connection failures retain their existing paths.
pub(super) async fn dispatch_prompt_with_transient_recovery(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    prompt: zevria_content::UserPrompt,
    workflow: EnsembleWorkflow,
    log: &RunLog,
    context: &PromptRunContext<'_>,
    deadline: &mut Option<tokio::time::Instant>,
) -> Result<WorkerEnd, AcpError> {
    let result = run_prompt_once(connection, session_id.clone(), prompt, *deadline, context).await;
    let error = match result {
        Err(error) => error,
        Ok(end) => return Ok(end),
    };
    let Some(kind) = transient_prompt_error_kind(&error) else {
        return Err(error);
    };
    context
        .transient_recovery
        .lock()
        .expect("transient recovery lock poisoned")
        .observe(kind, &error);
    if let Some(end) = prompt_recovery_end(log, context).await? {
        return Ok(end);
    }
    if !context
        .transient_recovery
        .lock()
        .expect("transient recovery lock poisoned")
        .reserve(kind, &error)
    {
        return Err(error);
    }
    log.emit(AgentRunEvent::Status {
        status: AgentRunStatus::Resuming,
        detail: Some(format!(
            "worker prompt failed transiently ({kind}); continuing the same ACP session after a two-second backoff (automatic transient continuation attempt 1 of {MAX_LIVE_TRANSIENT_CONTINUATIONS})"
        )),
    })
    .await
    .map_err(acp_error)?;
    tokio::select! {
        biased;
        () = context.cancellation.cancelled() => return Ok(WorkerEnd::LocalCancelled),
        () = async {
            match context.plan_handoff {
                Some(handoff) => handoff.wait_for_capture().await,
                None => std::future::pending::<()>().await,
            }
        } => {
            return prompt_recovery_end(log, context).await.map(|end| {
                end.expect("native completion triggered recovery wakeup")
            });
        }
        () = tokio::time::sleep(TRANSIENT_CONTINUATION_BACKOFF) => {}
    }
    if let Some(end) = prompt_recovery_end(log, context).await? {
        return Ok(end);
    }
    let prompt = log_transient_continuation(workflow, log).await?;
    if let Some(end) = prompt_recovery_end(log, context).await? {
        return Ok(end);
    }
    context
        .transient_recovery
        .lock()
        .expect("transient recovery lock poisoned")
        .original
        .as_mut()
        .expect("continuation reserved above")
        .dispatched = true;
    // Ordinary semantic repair shares this refreshed window; it does not get
    // another deadline reset. A second transient error propagates directly.
    *deadline = context
        .turn_timeout
        .map(|timeout| tokio::time::Instant::now() + timeout);
    run_prompt_once(connection, session_id, prompt.into(), *deadline, context).await
}

pub(super) async fn log_transient_continuation(
    workflow: EnsembleWorkflow,
    log: &RunLog,
) -> Result<String, AcpError> {
    let prompt = live_continuation_prompt(workflow, log.repair().as_ref());
    // Prompt is a sync boundary. Never send a continuation whose intent could
    // not be durably recorded, and never consume another semantic repair.
    log.emit(AgentRunEvent::Prompt {
        text: prompt.clone(),
        continuation: true,
        repair: None,
    })
    .await
    .map_err(acp_error)?;
    log.emit(AgentRunEvent::Status {
        status: AgentRunStatus::Running,
        detail: None,
    })
    .await
    .map_err(acp_error)?;
    Ok(prompt)
}

pub(super) async fn prompt_recovery_end(
    log: &RunLog,
    context: &PromptRunContext<'_>,
) -> Result<Option<WorkerEnd>, AcpError> {
    if let Some(error) = log.failure() {
        return Err(acp_error(error));
    }
    if context.cancellation.is_cancelled() {
        return Ok(Some(WorkerEnd::LocalCancelled));
    }
    if let Some(handoff) = context.plan_handoff {
        if handoff.is_settling() {
            // A transient prompt error does not retire its native permission
            // handlers. Never overlap a continuation with outstanding capture.
            tokio::select! {
                () = context.cancellation.cancelled() => return Ok(Some(WorkerEnd::LocalCancelled)),
                () = handoff.wait_for_capture() => {}
            }
        }
        if let Some(error) = log.failure() {
            return Err(acp_error(error));
        }
        if let Some(error) = handoff.capture_error() {
            return Ok(Some(WorkerEnd::NativeHandoffFailed(error)));
        }
        if handoff.is_completed() {
            return Ok(Some(WorkerEnd::NativeHandoffCompleted));
        }
    }
    Ok(None)
}

pub(super) fn semantic_repair_for_response(
    _workflow: EnsembleWorkflow,
    stop_reason: &str,
    log: &RunLog,
) -> Option<AgentRunRepair> {
    match stop_reason {
        "refusal" | "max_tokens" | "max_turn_requests" => Some(AgentRunRepair::EarlyStop {
            stop_reason: stop_reason.to_string(),
        }),
        "cancelled" => log.denied_permission_repair(),
        _ => None,
    }
}

pub(super) async fn run_prompt_once(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    prompt: zevria_content::UserPrompt,
    deadline: Option<tokio::time::Instant>,
    context: &PromptRunContext<'_>,
) -> Result<WorkerEnd, AcpError> {
    let PromptRunContext {
        cancellation,
        elicitation_lifetime,
        plan_handoff,
        prompt_dispatched,
        semantic_end,
        cancel_grace,
        ..
    } = *context;
    let request = connection
        .send_request(PromptRequest::new(
            session_id.clone(),
            zevria_acp::prompt::to_content(&prompt),
        ))
        .block_task();
    prompt_dispatched.store(true, AtomicOrdering::Release);
    tokio::pin!(request);
    let timeout = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout);
    let handoff_completed = async {
        match plan_handoff {
            Some(handoff) => handoff.wait_for_capture().await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(handoff_completed);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            elicitation_lifetime.cancel();
            // Cancellation/completion/timeout is already authoritative even if
            // the transport closes while sending the best-effort cancel.
            let _ = connection.send_notification(CancelNotification::new(session_id));
            let settled = tokio::time::timeout(cancel_grace, &mut request).await;
            if context.keep_alive {
                match settled {
                    Err(_) => return Err(acp_error("cancelled prompt did not settle; close and recover the same ACP session before another prompt")),
                    Ok(Err(error)) if is_unexpected_process_termination(&error) => return Err(error),
                    _ => {}
                }
            }
            Ok(WorkerEnd::LocalCancelled)
        }
        () = &mut handoff_completed => {
            if cancellation.is_cancelled() {
                return Ok(WorkerEnd::LocalCancelled);
            }
            if !context.keep_alive { elicitation_lifetime.cancel(); }
            // Cancellation/completion/timeout is already authoritative even if
            // the transport closes while sending the best-effort cancel.
            let _ = connection.send_notification(CancelNotification::new(session_id));
            if tokio::time::timeout(cancel_grace, &mut request).await.is_err() && context.keep_alive {
                return Err(acp_error("native proposal stop did not settle; close and recover the same ACP session before another prompt"));
            }
            Ok(plan_handoff.and_then(ClaudePlanHandoff::capture_error).map_or(WorkerEnd::NativeHandoffCompleted, WorkerEnd::NativeHandoffFailed))
        }
        response = &mut request => {
            if let Some(handoff) = plan_handoff {
                if handoff.is_settling() {
                    // end_turn does not imply the host snapshot has been synced.
                    // The independent settlement task owns its bounded deadline.
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok(WorkerEnd::LocalCancelled),
                        () = handoff.wait_for_capture() => {}
                    }
                }
                if let Some(error) = handoff.capture_error() {
                    return Ok(WorkerEnd::NativeHandoffFailed(error));
                }
                if handoff.is_completed() {
                    return Ok(WorkerEnd::NativeHandoffCompleted);
                }
            }
            if cancellation.is_cancelled() {
                return Ok(WorkerEnd::LocalCancelled);
            }
            let response = response?;
            Ok(WorkerEnd::PromptResponse {
                stop_reason: wire_name(&response.stop_reason),
            })
        }
        () = &mut timeout => {
            // The connection may terminate during cancellation grace. Retain
            // the observed timeout so that transport cleanup cannot relaunch
            // already-ended work or replace its authoritative status.
            *semantic_end.lock().expect("worker attempt progress lock poisoned") =
                Some(WorkerEnd::TimedOut);
            elicitation_lifetime.cancel();
            // Cancellation/completion/timeout is already authoritative even if
            // the transport closes while sending the best-effort cancel.
            let _ = connection.send_notification(CancelNotification::new(session_id));
            let _ = tokio::time::timeout(cancel_grace, &mut request).await;
            Ok(WorkerEnd::TimedOut)
        }
    }
}
