//! Correlated selection tests exercise the live state gate without a provider.

use super::*;
use agent_client_protocol::schema::v1::{ContentBlock, SessionNotification};
use agent_client_protocol::{Agent, Channel, Client};
use rig_core::message::Message;
use std::{future::Future, time::Duration};
use zevria_session_api::ManagementCommand;
use zevria_session_api::session_event_channel;
use zevria_workflow::PlanId;
use zevria_workflow::PlanResolution;

struct NoopLifecycle;

impl SessionRuntimeLifecycle for NoopLifecycle {
    fn shutdown(
        self: Box<Self>,
    ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Ok(()) })
    }
}

async fn exercise<F, Fut>(
    profile: ExecutionProfile,
    selected_mode: SessionMode,
    plan: PlanWorkflowState,
    run: F,
) -> Vec<AcpSessionUpdate>
where
    F: FnOnce(Arc<LiveSession>, mpsc::UnboundedReceiver<SessionCommand>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), AcpError>> + Send,
{
    let (client_transport, server_transport) = Channel::duplex();
    let (client_done, client_received) = oneshot::channel();
    let (server_done, server_received) = oneshot::channel();
    let done = Arc::new(Mutex::new(Some((client_done, server_done))));
    let updates = Arc::new(Mutex::new(Vec::new()));
    let log = updates.clone();
    let client = Client.builder().on_receive_notification(
        async move |notification: SessionNotification, _| {
            let completed = matches!(&notification.update,
                AcpSessionUpdate::AgentThoughtChunk(chunk)
                    if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "mode test complete"));
            log.lock().unwrap().push(notification.update);
            if completed {
                let (client_done, server_done) = done.lock().unwrap().take().unwrap();
                let _ = client_done.send(());
                let _ = server_done.send(());
            }
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    ).connect_with(client_transport, async move |_| {
        client_received.await.unwrap();
        Ok(())
    });
    let server = Agent
        .builder()
        .connect_with(server_transport, async move |connection| {
            let (commands, receiver) = mpsc::unbounded_channel();
            let permit = Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap();
            let live = LiveSession::new(
                "mode-test".into(),
                PathBuf::from("/mode-test"),
                commands,
                Box::new(NoopLifecycle),
                permit,
                selected_mode,
                plan,
                connection,
                ClientState {
                    form_elicitation: false,
                    plan_operations: true,
                },
                profile,
            );
            run(live.clone(), receiver).await?;
            live.send_update(diagnostic_update(
                None,
                "mode-test-complete",
                "mode test complete",
            ))?;
            server_received.await.unwrap();
            live.shutdown()
                .await
                .map_err(|error| AcpError::internal_error().data(error.to_string()))?;
            Ok(())
        });
    let (client, server) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(client, server)
    })
    .await
    .unwrap();
    client.unwrap();
    server.unwrap();
    updates.lock().unwrap().clone()
}

fn selected_updates(updates: &[AcpSessionUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|update| match update {
            AcpSessionUpdate::CurrentModeUpdate(update) => Some(update.current_mode_id.to_string()),
            _ => None,
        })
        .collect()
}

fn accepted(request_id: String, mode: SessionMode) -> SessionEvent {
    SessionEvent::ModeResult {
        request_id,
        result: ModeSelectionResult::Accepted {
            mode,
            changed: true,
        },
    }
}

fn artifact() -> PlanArtifact {
    PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 1,
        },
        title: "Retained Selection Plan".into(),
        markdown: "# Retained Selection Plan\n\n## Goal\nKeep selection.\n\n## Decisions\nReports are not selection authority.\n\n## Implementation\nPreserve explicit mode.\n\n## Validation\nTest acknowledgment and replay.\n\n## Risks\nNone.\n".into(),
        source_turn_id: TurnId::new(1),
    }
}

#[tokio::test]
async fn selection_waits_for_correlated_ack_and_locks_prompt_and_management() {
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            let mut wait = live.set_mode(SessionMode::Plan)?;
            assert_eq!(live.mode(), SessionMode::Build);
            assert!(matches!(
                wait.response.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert_eq!(
                commands.recv().await,
                Some(SessionCommand::Manage(ManagementCommand::SetMode {
                    request_id: wait.request_id.clone(),
                    mode: SessionMode::Plan,
                }))
            );
            assert!(live.set_mode(SessionMode::Plan).is_err());
            assert!(
                live.begin_prompt(zevria_content::UserPrompt::from_text("not queued"))
                    .is_err()
            );
            assert!(
                live.manage_skills(zevria_instructions::skill::SkillManagementRequest::List {
                    query: String::new(),
                })
                .await
                .is_err()
            );
            assert!(
                live.manage_skills(zevria_instructions::skill::SkillManagementRequest::Reload {
                    expected_revision: "revision".into(),
                })
                .await
                .is_err()
            );
            assert!(commands.try_recv().is_err());
            live.handle_event(accepted("unrelated".into(), SessionMode::Plan))?;
            assert_eq!(live.mode(), SessionMode::Build);
            assert!(matches!(
                wait.response.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            live.handle_event(accepted(wait.request_id.clone(), SessionMode::Plan))?;
            wait.response.await.unwrap()?;
            assert_eq!(live.mode(), SessionMode::Plan);
            // A duplicate cannot overwrite the acknowledged selection.
            live.handle_event(accepted(wait.request_id, SessionMode::Plan))?;
            assert_eq!(live.mode(), SessionMode::Plan);
            let same = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            live.handle_event(SessionEvent::ModeResult {
                request_id: same.request_id,
                result: ModeSelectionResult::Accepted {
                    mode: SessionMode::Plan,
                    changed: false,
                },
            })?;
            same.response.await.unwrap()?;
            let prompt =
                live.begin_prompt(zevria_content::UserPrompt::from_text("implement directly"))?;
            assert!(matches!(
                commands.recv().await,
                Some(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::Submit {
                        mode: SessionMode::Plan,
                        ..
                    }
                ))
            ));
            assert!(live.set_mode(SessionMode::Build).is_err());
            live.finish_prompt(Ok(StopReason::EndTurn));
            assert_eq!(prompt.response.await.unwrap()?, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;
    assert_eq!(selected_updates(&updates), ["plan", "plan"]);
}

#[tokio::test]
async fn rejection_and_stale_replies_never_change_selection() {
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            let rejected = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            live.handle_event(SessionEvent::ModeResult {
                request_id: rejected.request_id.clone(),
                result: ModeSelectionResult::Rejected {
                    code: "busy".into(),
                    message: "model management pending".into(),
                },
            })?;
            let error = rejected.response.await.unwrap().unwrap_err();
            assert!(error.to_string().contains("model management pending"));
            assert_eq!(live.mode(), SessionMode::Build);
            let mut next = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            live.handle_event(accepted(rejected.request_id.clone(), SessionMode::Plan))?;
            live.interrupt_mode(Some(&rejected.request_id), AcpError::request_cancelled());
            assert_eq!(live.mode(), SessionMode::Build);
            assert!(matches!(
                next.response.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(live.state.lock().unwrap().pending_mode.is_some());
            live.handle_event(accepted(next.request_id, SessionMode::Plan))?;
            next.response.await.unwrap()?;
            live.handle_event(accepted(rejected.request_id, SessionMode::Plan))?;
            assert_eq!(live.mode(), SessionMode::Plan);
            Ok(())
        },
    )
    .await;
    assert_eq!(selected_updates(&updates), ["plan"]);
}

#[tokio::test]
async fn abandoned_selection_keeps_admission_locked_until_accepted_or_rejected() {
    for interruption in [
        "request-cancel",
        "session-cancel",
        "timeout",
        "waiter-spawn",
    ] {
        for accepts in [false, true] {
            let updates = exercise(
                ExecutionProfile::Interactive,
                SessionMode::Build,
                PlanWorkflowState::Idle,
                async move |live, mut commands| {
                    let pending = live.set_mode(SessionMode::Plan)?;
                    commands.recv().await.unwrap();
                    match interruption {
                        "session-cancel" => live.cancel(),
                        "request-cancel" => live.interrupt_mode(
                            Some(&pending.request_id),
                            AcpError::request_cancelled(),
                        ),
                        reason => live.interrupt_mode(
                            Some(&pending.request_id),
                            AcpError::internal_error().data(reason),
                        ),
                    }
                    assert!(pending.response.await.unwrap().is_err());
                    assert_eq!(live.mode(), SessionMode::Build);
                    {
                        let state = live.state.lock().unwrap();
                        let locked = state.pending_mode.as_ref().unwrap();
                        assert_eq!(locked.request_id, pending.request_id);
                        assert!(locked.outcome.is_none());
                    }
                    // Neither cancellation nor a waiter failure cancels a
                    // durable SetMode. Nothing else may enter ahead of its ack.
                    assert!(live.set_mode(SessionMode::Plan).is_err());
                    assert!(
                        live.begin_prompt(zevria_content::UserPrompt::from_text("not queued"))
                            .is_err()
                    );
                    assert!(
                        live.manage_skills(zevria_instructions::skill::SkillManagementRequest::List {
                            query: String::new(),
                        })
                        .await
                        .is_err()
                    );
                    assert!(commands.try_recv().is_err());
                    live.handle_event(accepted("unrelated".into(), SessionMode::Plan))?;
                    assert!(live.state.lock().unwrap().pending_mode.is_some());
                    let result = if accepts {
                        ModeSelectionResult::Accepted {
                            mode: SessionMode::Plan,
                            changed: true,
                        }
                    } else {
                        ModeSelectionResult::Rejected {
                            code: "busy".into(),
                            message: "model management pending".into(),
                        }
                    };
                    live.handle_event(SessionEvent::ModeResult {
                        request_id: pending.request_id.clone(),
                        result,
                    })?;
                    let expected = if accepts {
                        SessionMode::Plan
                    } else {
                        SessionMode::Build
                    };
                    assert_eq!(live.mode(), expected);
                    assert!(live.state.lock().unwrap().pending_mode.is_none());
                    let prompt =
                        live.begin_prompt(zevria_content::UserPrompt::from_text("after ack"))?;
                    assert!(matches!(
                        commands.recv().await,
                        Some(SessionCommand::Turn(zevria_session_api::TurnCommand::Submit { mode, .. }))
                            if mode == expected
                    ));
                    live.finish_prompt(Ok(StopReason::EndTurn));
                    prompt.response.await.unwrap()?;
                    let mut next = live.set_mode(SessionMode::Plan)?;
                    commands.recv().await.unwrap();
                    // A resolved old request cannot detach the new waiter or
                    // replace its selection, even if the old caller cancelled.
                    live.interrupt_mode(Some(&pending.request_id), AcpError::request_cancelled());
                    live.handle_event(accepted(
                        pending.request_id.clone(),
                        SessionMode::Plan,
                    ))?;
                    assert_eq!(live.mode(), expected);
                    assert!(matches!(
                        next.response.try_recv(),
                        Err(oneshot::error::TryRecvError::Empty)
                    ));
                    live.handle_event(accepted(next.request_id, SessionMode::Plan))?;
                    next.response.await.unwrap()?;
                    live.handle_event(accepted(pending.request_id, SessionMode::Plan))?;
                    assert_eq!(live.mode(), SessionMode::Plan);
                    Ok(())
                },
            )
            .await;
            assert_eq!(
                selected_updates(&updates),
                if accepts {
                    vec!["plan", "plan"]
                } else {
                    vec!["plan"]
                }
            );
        }
    }
}

#[tokio::test]
async fn dropped_mode_waiter_keeps_admission_locked_until_ack() {
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            let pending = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            drop(pending.response);
            assert!(live.set_mode(SessionMode::Plan).is_err());
            assert!(
                live.begin_prompt(zevria_content::UserPrompt::from_text("not queued"))
                    .is_err()
            );
            live.handle_event(accepted(pending.request_id, SessionMode::Plan))?;
            assert_eq!(live.mode(), SessionMode::Plan);
            assert!(live.state.lock().unwrap().pending_mode.is_none());
            Ok(())
        },
    )
    .await;
    assert_eq!(selected_updates(&updates), ["plan"]);
}

#[tokio::test]
async fn failed_enqueue_releases_selection_without_committing() {
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            commands.close();
            assert!(live.set_mode(SessionMode::Plan).is_err());
            assert_eq!(live.mode(), SessionMode::Build);
            assert!(live.state.lock().unwrap().pending_mode.is_none());
            Ok(())
        },
    )
    .await;
    assert!(selected_updates(&updates).is_empty());
}

#[tokio::test]
async fn runtime_failure_and_shutdown_never_reopen_unknown_selection() {
    for runtime_failure in [false, true] {
        let updates = exercise(
            ExecutionProfile::Interactive,
            SessionMode::Build,
            PlanWorkflowState::Idle,
            async move |live, mut commands| {
                let pending = live.set_mode(SessionMode::Plan)?;
                commands.recv().await.unwrap();
                if runtime_failure {
                    live.runtime_unavailable("original mode failure".into());
                } else {
                    live.shutdown().await.unwrap();
                }
                let error = pending.response.await.unwrap().unwrap_err();
                if runtime_failure {
                    assert!(error.to_string().contains("original mode failure"));
                    assert!(live.state.lock().unwrap().pending_mode.is_some());
                }
                assert!(live.set_mode(SessionMode::Plan).is_err());
                assert!(
                    live.begin_prompt(zevria_content::UserPrompt::from_text("not queued"))
                        .is_err()
                );
                live.shutdown().await.unwrap();
                assert!(live.is_closed());
                assert!(live.state.lock().unwrap().pending_mode.is_none());
                // Only a permanently closed session may discard an unresolved
                // commit: it can no longer submit work with stale selection.
                live.handle_event(accepted(pending.request_id, SessionMode::Plan))?;
                assert_eq!(live.mode(), SessionMode::Build);
                Ok(())
            },
        )
        .await;
        assert!(selected_updates(&updates).is_empty());
    }
}

#[tokio::test]
async fn mismatched_ack_fails_closed_instead_of_admitting_unknown_selection() {
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            let (events, receiver) = session_event_channel(8);
            live.start_event_loop(receiver, Box::pin(std::future::pending()));
            let pending = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            events
                .send(accepted(pending.request_id, SessionMode::Build))
                .await
                .unwrap();
            let error = pending.response.await.unwrap().unwrap_err();
            assert!(error.to_string().contains("different session mode"));
            assert_eq!(live.mode(), SessionMode::Build);
            assert!(live.set_mode(SessionMode::Plan).is_err());
            assert!(
                live.begin_prompt(zevria_content::UserPrompt::from_text("not queued"))
                    .is_err()
            );
            assert!(live.state.lock().unwrap().unavailable.is_some());
            Ok(())
        },
    )
    .await;
    assert!(selected_updates(&updates).is_empty());
}

#[tokio::test]
async fn background_exit_fails_a_waiting_mode_ack_with_original_diagnostic() {
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            let (_events, receiver) = session_event_channel(8);
            let (exit, exited) = oneshot::channel();
            live.start_event_loop(receiver, Box::pin(async move { exited.await.unwrap() }));
            let pending = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            exit.send(RuntimeExit::failed(
                "mode-runtime",
                "original background failure",
            ))
            .unwrap();
            let error = pending.response.await.unwrap().unwrap_err();
            assert!(error.to_string().contains("original background failure"));
            assert_eq!(live.mode(), SessionMode::Build);
            assert!(live.set_mode(SessionMode::Plan).is_err());
            Ok(())
        },
    )
    .await;
    assert!(selected_updates(&updates).is_empty());
}

#[tokio::test]
async fn pending_skill_mutation_blocks_mode_selection_and_root_prompts() {
    exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            let skills = live.clone();
            let request = tokio::spawn(async move {
                skills
                    .manage_skills(zevria_instructions::skill::SkillManagementRequest::Reload {
                        expected_revision: "revision".into(),
                    })
                    .await
            });
            let Some(SessionCommand::Manage(ManagementCommand::Skills { request_id, .. })) =
                commands.recv().await
            else {
                panic!("skill request must be queued")
            };
            assert!(live.set_mode(SessionMode::Plan).is_err());
            assert!(
                live.begin_prompt(zevria_content::UserPrompt::from_text("not queued"))
                    .is_err()
            );
            assert!(commands.try_recv().is_err());
            live.handle_event(SessionEvent::SkillsResult {
                request_id,
                result: zevria_instructions::skill::SkillManagementResult::error(
                    "test", "resolved",
                ),
            })?;
            request.await.unwrap()?;
            let mode = live.set_mode(SessionMode::Plan)?;
            commands.recv().await.unwrap();
            live.handle_event(accepted(mode.request_id, SessionMode::Plan))?;
            mode.response.await.unwrap()?;
            Ok(())
        },
    )
    .await;
}

#[tokio::test]
async fn retained_plan_snapshots_and_turn_modes_do_not_erase_explicit_selection() {
    let plan = artifact();
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Build,
        PlanWorkflowState::Planning {
            id: plan.version.id,
            previous: None,
        },
        async move |live, _commands| {
            assert_eq!(live.mode(), SessionMode::Build);
            for state in [
                PlanWorkflowState::Planning {
                    id: plan.version.id,
                    previous: Some(plan.clone()),
                },
                PlanWorkflowState::Published {
                    artifact: plan.clone(),
                },
                PlanWorkflowState::Resolved {
                    artifact: plan.clone(),
                    resolution: PlanResolution::ImplementedCurrent,
                },
                PlanWorkflowState::Idle,
            ] {
                live.handle_event(SessionEvent::PlanStateChanged { state })?;
                assert_eq!(live.mode(), SessionMode::Build);
            }
            live.handle_event(SessionEvent::TurnStarted {
                turn_id: TurnId::new(2),
                mode: SessionMode::Plan,
                message: Message::user("retained turn"),
            })?;
            assert_eq!(live.mode(), SessionMode::Build);
            live.handle_event(SessionEvent::PlanStateChanged {
                state: PlanWorkflowState::Ready {
                    artifact: plan.clone(),
                },
            })?;
            assert_eq!(live.mode(), SessionMode::Plan);
            assert!(live.set_mode(SessionMode::Build).is_err());
            assert!(live.set_mode(SessionMode::Plan).is_err());
            live.handle_event(SessionEvent::ModeChanged {
                mode: SessionMode::Build,
            })?;
            assert_eq!(live.mode(), SessionMode::Plan, "Ready remains modal");
            live.handle_event(SessionEvent::PlanStateChanged {
                state: PlanWorkflowState::Resolved {
                    artifact: plan,
                    resolution: PlanResolution::ImplementedCurrent,
                },
            })?;
            live.handle_event(SessionEvent::ModeChanged {
                mode: SessionMode::Build,
            })?;
            assert_eq!(live.mode(), SessionMode::Build);
            Ok(())
        },
    )
    .await;
    assert_eq!(selected_updates(&updates), ["plan", "build"]);
}

#[tokio::test]
async fn implementing_a_published_plan_from_plan_still_selects_build() {
    let plan = artifact();
    let updates = exercise(
        ExecutionProfile::Interactive,
        SessionMode::Plan,
        PlanWorkflowState::Published {
            artifact: plan.clone(),
        },
        async move |live, mut commands| {
            let pending = live.begin_prompt(zevria_content::UserPrompt::from_text("/implement"))?;
            assert_eq!(
                commands.recv().await,
                Some(SessionCommand::Turn(
                    zevria_session_api::TurnCommand::ResolvePlan {
                        expected: plan.version,
                        decision: PlanDecision::ImplementCurrent,
                    }
                ))
            );
            assert_eq!(
                live.state.lock().unwrap().pending.as_ref().unwrap().mode,
                SessionMode::Build
            );
            live.handle_event(SessionEvent::PlanHandoffStarted {
                turn_id: TurnId::new(2),
                handoff: zevria_workflow::PlanHandoff::new(plan, "mode-test"),
            })?;
            assert_eq!(live.mode(), SessionMode::Build);
            live.finish_prompt(Ok(StopReason::EndTurn));
            pending.response.await.unwrap()?;
            Ok(())
        },
    )
    .await;
    assert_eq!(selected_updates(&updates), ["build"]);
}

#[tokio::test]
async fn workers_reject_request_orchestration_and_never_send_root_management() {
    exercise(
        ExecutionProfile::EnsembleWorker,
        SessionMode::Build,
        PlanWorkflowState::Idle,
        async move |live, mut commands| {
            assert!(
                live.begin_prompt_with_behavior(
                    "delegate".into(),
                    zevria_foundation::RequestBehavior::Orchestrate
                )
                .is_err()
            );
            assert_eq!(live.mode(), SessionMode::Build);
            live.set_mode(SessionMode::Plan)?.response.await.unwrap()?;
            assert_eq!(live.mode(), SessionMode::Plan);
            live.handle_event(SessionEvent::PlanStateChanged {
                state: PlanWorkflowState::Ready {
                    artifact: artifact(),
                },
            })?;
            assert!(live.set_mode(SessionMode::Build).is_err());
            assert!(
                live.begin_prompt_with_behavior(
                    "delegate".into(),
                    zevria_foundation::RequestBehavior::Orchestrate
                )
                .is_err()
            );
            live.set_mode(SessionMode::Plan)?.response.await.unwrap()?;
            assert!(commands.try_recv().is_err());
            Ok(())
        },
    )
    .await;
}
