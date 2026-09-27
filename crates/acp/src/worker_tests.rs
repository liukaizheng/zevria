//! Scripted proposal publication and real same-session feedback continuation.
use super::*;
use agent_client_protocol::schema::v1::{Error as AcpError, PlanCapabilities};
use agent_client_protocol::{Agent, ConnectionTo};

fn artifact() -> PlanArtifact {
    PlanArtifact {
        version: PlanVersion { id: PlanId::new(), revision: 1 },
        title: "Preserve Exact Worker Markdown".into(),
        markdown: "# Preserve Exact Worker Markdown\n\n## Goal\nKeep bytes.  \n\n## Decisions\n- Native ACP.\n\n## Implementation\n1. Report only.\n\n## Validation\n- Test both orders.\n\n## Risks\n- None.\n".into(),
        source_turn_id: TurnId::new(1),
    }
}

struct WorkerFactory {
    artifact: PlanArtifact,
    initial_plan: PlanWorkflowState,
    selected_mode: SessionMode,
    commands: Arc<Mutex<Vec<SessionCommand>>>,
    shutdowns: Arc<AtomicUsize>,
}
impl WorkerFactory {
    fn new(ready: bool) -> Arc<Self> {
        let artifact = artifact();
        Arc::new(Self {
            initial_plan: if ready {
                PlanWorkflowState::Ready {
                    artifact: artifact.clone(),
                }
            } else {
                PlanWorkflowState::Idle
            },
            artifact,
            selected_mode: if ready {
                SessionMode::Plan
            } else {
                SessionMode::Build
            },
            commands: Arc::new(Mutex::new(Vec::new())),
            shutdowns: Arc::new(AtomicUsize::new(0)),
        })
    }
}
impl SessionRuntimeFactory for WorkerFactory {
    fn profile(&self) -> ExecutionProfile {
        ExecutionProfile::EnsembleWorker
    }
    fn start(
        &self,
        request: StartSessionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<StartedSession>> + Send + '_>>
    {
        Box::pin(async move {
            let (commands, mut rx) = mpsc::unbounded_channel::<SessionCommand>();
            let (tx, events) = session_event_channel(32);
            let retained_events = tx.clone();
            let (exit_tx, exit_rx) = oneshot::channel();
            let recorded = self.commands.clone();
            let mut artifact = self.artifact.clone();
            let task = tokio::spawn(async move {
                let mut turn = 0;
                while let Some(command) = rx.recv().await {
                    recorded.lock().unwrap().push(command.clone());
                    match command {
                        SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                            behavior: _,
                            text,
                            mode,
                        }) => {
                            turn += 1;
                            let turn_id = TurnId::new(turn);
                            tx.send(SessionEvent::TurnStarted {
                                turn_id,
                                mode,
                                message: text.to_message(),
                            })
                            .await
                            .unwrap();
                            if text == zevria_content::UserPrompt::from_text("provider failure") {
                                tx.send(SessionEvent::TurnFailed {
                                    turn_id,
                                    error: "provider unavailable".into(),
                                })
                                .await
                                .unwrap();
                                continue;
                            }
                            if text == zevria_content::UserPrompt::from_text("question") {
                                tx.send(SessionEvent::QuestionAsked {
                                    turn_id,
                                    request: QuestionRequest {
                                        id: QuestionRequestId::new("scope"),
                                        questions: vec![QuestionPrompt {
                                            id: "scope".into(),
                                            header: "Scope".into(),
                                            question: "Choose scope".into(),
                                            options: vec![QuestionOption {
                                                label: "Focused".into(),
                                                description: "Narrow scope".into(),
                                            }],
                                            kind: QuestionPromptKind::SingleSelect {
                                                allow_other: false,
                                            },
                                            required: true,
                                            default: None,
                                        }],
                                        source_label: None,
                                        dismissible: true,
                                    },
                                })
                                .await
                                .unwrap();
                                continue;
                            }
                            let terminal = SessionEvent::TurnCompleted {
                                display_attempt_id: None,
                                turn_id,
                                message: Message::assistant("report complete"),
                            };
                            if [
                                "plan",
                                "terminal first",
                                "submit only",
                                "revise this retained report",
                            ]
                            .contains(&text.text_projection().as_str())
                            {
                                tx.send(SessionEvent::ToolResults {
                                    turn_id,
                                    message: Message::tool_result(
                                        "submit",
                                        "submit_plan",
                                        "accepted candidate",
                                    ),
                                    metadata: vec![ToolResultMetadata {
                                        diagnostic: None,
                                        id: "submit".into(),
                                        call_id: None,
                                        tool_name: "submit_plan".into(),
                                        outcome: ToolCallOutcome::Success,
                                        detail: None,
                                    }],
                                })
                                .await
                                .unwrap();
                                if text != zevria_content::UserPrompt::from_text("plan") {
                                    tx.send(terminal.clone()).await.unwrap();
                                }
                                if text == zevria_content::UserPrompt::from_text("submit only") {
                                    continue;
                                }
                                // Separate scheduling turns exercise a terminal waiting for Ready.
                                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                                for _ in 0..2 {
                                    tx.send(SessionEvent::PlanStateChanged {
                                        state: PlanWorkflowState::Ready {
                                            artifact: artifact.clone(),
                                        },
                                    })
                                    .await
                                    .unwrap();
                                }
                                if text == zevria_content::UserPrompt::from_text("plan") {
                                    tx.send(terminal).await.unwrap();
                                }
                            } else {
                                tx.send(terminal).await.unwrap();
                            }
                        }
                        SessionCommand::Control(
                            zevria_session_api::ControlCommand::AnswerQuestion { .. },
                        ) => {
                            tx.send(SessionEvent::TurnCompleted {
                                display_attempt_id: None,
                                turn_id: TurnId::new(turn),
                                message: Message::assistant("question handled"),
                            })
                            .await
                            .unwrap();
                        }
                        SessionCommand::Control(
                            zevria_session_api::ControlCommand::CancelTurn { .. },
                        ) => {
                            tx.send(SessionEvent::TurnCancelled {
                                turn_id: TurnId::new(turn),
                            })
                            .await
                            .unwrap();
                        }
                        SessionCommand::Turn(zevria_session_api::TurnCommand::ResolvePlan {
                            expected,
                            decision: PlanDecision::Revise,
                        }) => {
                            assert_eq!(expected, artifact.version);
                            turn += 1;
                            tx.send(SessionEvent::PlanStateChanged {
                                state: PlanWorkflowState::Planning {
                                    id: artifact.version.id,
                                    previous: Some(artifact.clone()),
                                },
                            })
                            .await
                            .unwrap();
                            tx.send(SessionEvent::TurnCompleted {
                                display_attempt_id: None,
                                turn_id: TurnId::new(turn),
                                message: Message::assistant("revision opened"),
                            })
                            .await
                            .unwrap();
                            artifact.version.revision += 1;
                        }
                        SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown) => {
                            break;
                        }
                        other => panic!(
                            "worker must not dispatch management, revision or implementation: {other:?}"
                        ),
                    }
                }
                let _ = exit_tx.send(RuntimeExit::clean("worker fixture"));
            });
            Ok(StartedSession {
                session_id: "worker".into(),
                workspace: request.workspace,
                transcript_items: vec![],
                selected_mode: self.selected_mode,
                plan_state: self.initial_plan.clone(),
                startup_notices: vec![],
                commands: commands.clone(),
                events,
                background_exit: Box::pin(async move { exit_rx.await.unwrap() }),
                lifecycle: Box::new(FakeLifecycle {
                    _events: retained_events,
                    commands,
                    task: Some(task),
                    shutdowns: self.shutdowns.clone(),
                }),
            })
        })
    }
    fn list(
        &self,
        _: PathBuf,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Vec<SessionDescriptor>>> + Send + '_>,
    > {
        Box::pin(async { Ok(vec![]) })
    }
}

async fn exercise<F, Fut>(
    factory: Arc<WorkerFactory>,
    plans: bool,
    forms: bool,
    answer: Option<CreateElicitationResponse>,
    run: F,
) -> (Vec<SessionNotification>, usize)
where
    F: FnOnce(ConnectionTo<Agent>, PathBuf) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), AcpError>> + Send,
{
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().to_path_buf();
    let (client_transport, server_transport) = Channel::duplex();
    let server_path = path.clone();
    let server = tokio::spawn(async move {
        serve_on(AcpConfig::default(), server_path, factory, server_transport).await
    });
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let log = notifications.clone();
    let forms_seen = Arc::new(AtomicUsize::new(0));
    let count = forms_seen.clone();
    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _| {
                log.lock().unwrap().push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_request: CreateElicitationRequest, responder, connection| {
                count.fetch_add(1, Ordering::SeqCst);
                if let Some(answer) = &answer {
                    return responder.respond(answer.clone());
                }
                connection.spawn(async move {
                    responder.cancellation().cancelled().await;
                    responder.respond_with_error(AcpError::request_cancelled())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_transport, async move |connection| {
            let mut capabilities = ClientCapabilities::new();
            if plans {
                capabilities = capabilities.plan(PlanCapabilities::new());
            }
            if forms {
                capabilities = capabilities.elicitation(
                    ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
                );
            }
            let initialized = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities),
                )
                .block_task()
                .await?;
            assert!(
                !serde_json::to_string(&initialized)
                    .unwrap()
                    .contains("zevria.skills")
            );
            run(connection, path).await
        });
    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let updates = notifications.lock().unwrap().clone();
    (updates, forms_seen.load(Ordering::SeqCst))
}

#[tokio::test]
async fn workers_reject_explicit_orchestration_metadata() {
    for ready in [false, true] {
        let factory = WorkerFactory::new(ready);
        let commands = factory.commands.clone();
        exercise(
            factory,
            true,
            false,
            None,
            |connection, workspace| async move {
                let session = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                let mut request = PromptRequest::new(session.session_id, vec!["delegate".into()]);
                request.meta = Some(serde_json::Map::from_iter([(
                    "zevria.orchestration".into(),
                    serde_json::json!({"version":1,"enabled":true}),
                )]));
                let error = connection
                    .send_request(request)
                    .block_task()
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("root Build"));
                Ok(())
            },
        )
        .await;
        assert!(
            !commands
                .lock()
                .unwrap()
                .iter()
                .any(|command| matches!(command, SessionCommand::Turn(_)))
        );
    }
}

#[tokio::test]
async fn worker_orchestration_text_remains_literal_without_metadata() {
    let factory = WorkerFactory::new(false);
    let commands = factory.commands.clone();
    exercise(
        factory,
        true,
        false,
        None,
        |connection, workspace| async move {
            let session = connection
                .send_request(NewSessionRequest::new(workspace))
                .block_task()
                .await?;
            connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec!["/orchestrate literal data".into()],
                ))
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await;
    assert!(commands.lock().unwrap().iter().any(|command| matches!(command, SessionCommand::Turn(zevria_session_api::TurnCommand::Submit { behavior: zevria_foundation::RequestBehavior::Standard, text, .. }) if text.text_projection() == "/orchestrate literal data")));
}

#[tokio::test]
async fn worker_plan_selection_requires_capability_without_a_plan_snapshot() {
    let mut factory = WorkerFactory::new(false);
    Arc::get_mut(&mut factory).unwrap().selected_mode = SessionMode::Plan;
    let shutdowns = factory.shutdowns.clone();
    exercise(
        factory,
        false,
        false,
        None,
        |connection, workspace| async move {
            let error = connection
                .send_request(NewSessionRequest::new(workspace))
                .block_task()
                .await
                .unwrap_err();
            assert!(error.to_string().contains("Plan-operation capability"));
            Ok(())
        },
    )
    .await;
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn worker_proposals_precede_end_turn_and_feedback_runs_real_turns_without_approval() {
    for prompt in ["plan", "terminal first"] {
        let factory = WorkerFactory::new(false);
        let expected = factory.artifact.clone();
        let commands = factory.commands.clone();
        let (updates, forms) = exercise(
            factory,
            true,
            true,
            None,
            move |connection, workspace| async move {
                let created = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                let modes = created.modes.unwrap();
                assert_eq!(modes.current_mode_id.to_string(), "review");
                assert_eq!(
                    modes
                        .available_modes
                        .iter()
                        .map(|mode| mode.id.to_string())
                        .collect::<Vec<_>>(),
                    ["plan", "review"]
                );
                for unsupported in ["build", "orchestrate"] {
                    assert!(
                        connection
                            .send_request(SetSessionModeRequest::new("worker", unsupported))
                            .block_task()
                            .await
                            .is_err()
                    );
                }
                connection
                    .send_request(SetSessionModeRequest::new("worker", "plan"))
                    .block_task()
                    .await?;
                for text in [prompt, "continue", "revise this retained report"] {
                    assert_eq!(
                        connection
                            .send_request(PromptRequest::new(
                                "worker",
                                vec![ContentBlock::from(text)]
                            ))
                            .block_task()
                            .await?
                            .stop_reason,
                        StopReason::EndTurn
                    );
                }
                assert!(
                    connection
                        .send_request(PromptRequest::new(
                            "worker",
                            vec![ContentBlock::from("/implement")]
                        ))
                        .block_task()
                        .await
                        .is_err()
                );
                connection
                    .send_request(SetSessionModeRequest::new("worker", "plan"))
                    .block_task()
                    .await?;
                assert!(
                    connection
                        .send_request(SetSessionModeRequest::new("worker", "review"))
                        .block_task()
                        .await
                        .is_err()
                );
                connection
                    .send_request(CloseSessionRequest::new("worker"))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await;
        assert_eq!(forms, 0);
        let proofs: Vec<_> = updates
            .iter()
            .filter_map(|update| match &update.update {
                AcpSessionUpdate::PlanUpdate(plan) => Some(serde_json::to_value(plan).unwrap()),
                _ => None,
            })
            .collect();
        assert_eq!(
            proofs.len(),
            2,
            "live Ready duplicates are suppressed; prose does not republish; revised submission publishes"
        );
        for proof in proofs {
            assert_eq!(proof["plan"]["type"], "markdown");
            assert_eq!(proof["plan"]["content"], expected.markdown);
            assert_eq!(
                proof["plan"]["planId"],
                format!("zevria-plan-{}", expected.version.id)
            );
        }
        for update in updates {
            let wire = serde_json::to_string(&update).unwrap();
            assert!(
                !wire.contains("instructions")
                    && !wire.contains("implemented plan")
                    && !wire.contains("\"build\""),
                "{wire}"
            );
        }
        assert_eq!(
            commands
                .lock()
                .unwrap()
                .iter()
                .filter(|command| matches!(
                    command,
                    SessionCommand::Turn(zevria_session_api::TurnCommand::Submit { .. })
                ))
                .count(),
            3
        );
    }
}

#[tokio::test]
async fn worker_plan_capability_is_required_for_negotiation_and_restoration_with_cleanup() {
    let factory = WorkerFactory::new(false);
    exercise(
        factory,
        false,
        false,
        None,
        |connection, workspace| async move {
            connection
                .send_request(NewSessionRequest::new(workspace))
                .block_task()
                .await?;
            assert!(
                connection
                    .send_request(SetSessionModeRequest::new("worker", "plan"))
                    .block_task()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Plan-operation")
            );
            assert_eq!(
                connection
                    .send_request(PromptRequest::new(
                        "worker",
                        vec![ContentBlock::from("review")]
                    ))
                    .block_task()
                    .await?
                    .stop_reason,
                StopReason::EndTurn
            );
            connection
                .send_request(CloseSessionRequest::new("worker"))
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await;
    let factory = WorkerFactory::new(true);
    let shutdowns = factory.shutdowns.clone();
    exercise(
        factory,
        false,
        false,
        None,
        |connection, workspace| async move {
            assert!(
                connection
                    .send_request(LoadSessionRequest::new("worker", workspace.clone()))
                    .block_task()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Plan-operation")
            );
            assert!(
                connection
                    .send_request(ResumeSessionRequest::new("worker", workspace))
                    .block_task()
                    .await
                    .is_err()
            );
            Ok(())
        },
    )
    .await;
    assert_eq!(shutdowns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ready_worker_load_resume_replays_but_continue_dispatches_real_feedback() {
    let factory = WorkerFactory::new(true);
    let commands = factory.commands.clone();
    let (updates, forms) = exercise(
        factory,
        true,
        true,
        None,
        |connection, workspace| async move {
            let loaded = connection
                .send_request(LoadSessionRequest::new("worker", workspace.clone()))
                .block_task()
                .await?;
            assert_eq!(loaded.modes.unwrap().current_mode_id.to_string(), "plan");
            connection
                .send_request(CloseSessionRequest::new("worker"))
                .block_task()
                .await?;
            let resumed = connection
                .send_request(ResumeSessionRequest::new("worker", workspace))
                .block_task()
                .await?;
            assert_eq!(resumed.modes.unwrap().current_mode_id.to_string(), "plan");
            connection
                .send_request(PromptRequest::new(
                    "worker",
                    vec![ContentBlock::from("continue")],
                ))
                .block_task()
                .await?;
            connection
                .send_request(CloseSessionRequest::new("worker"))
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await;
    assert_eq!(forms, 0);
    assert_eq!(
        updates
            .iter()
            .filter(|update| matches!(update.update, AcpSessionUpdate::PlanUpdate(_)))
            .count(),
        2
    );
    assert_eq!(commands.lock().unwrap().iter().filter(|command| matches!(
        command,
        SessionCommand::Turn(zevria_session_api::TurnCommand::Submit { behavior: _, text, mode: SessionMode::Plan }) if text == &zevria_content::UserPrompt::from_text("continue")
    )).count(), 1);
}

#[tokio::test]
async fn worker_proofless_turn_failure_and_cancelled_pending_ready_do_not_fabricate_proof() {
    let (updates, _) = exercise(
        WorkerFactory::new(false),
        true,
        false,
        None,
        |connection, workspace| async move {
            connection
                .send_request(NewSessionRequest::new(workspace))
                .block_task()
                .await?;
            connection
                .send_request(SetSessionModeRequest::new("worker", "plan"))
                .block_task()
                .await?;
            connection
                .send_request(PromptRequest::new(
                    "worker",
                    vec![ContentBlock::from("proofless")],
                ))
                .block_task()
                .await?;
            assert!(
                connection
                    .send_request(PromptRequest::new(
                        "worker",
                        vec![ContentBlock::from("provider failure")]
                    ))
                    .block_task()
                    .await
                    .is_err()
            );
            let pending = connection.send_request(PromptRequest::new(
                "worker",
                vec![ContentBlock::from("submit only")],
            ));
            let mut pending = Box::pin(pending.block_task());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), pending.as_mut())
                    .await
                    .is_err(),
                "tool success must not complete without Ready"
            );
            connection.send_notification(CancelNotification::new("worker"))?;
            assert_eq!(pending.await?.stop_reason, StopReason::Cancelled);
            connection
                .send_request(CloseSessionRequest::new("worker"))
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await;
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update.update, AcpSessionUpdate::PlanUpdate(_)))
    );
}

#[tokio::test]
async fn worker_questions_answer_dismiss_and_cancel_without_approval() {
    for forms in [false, true] {
        let factory = WorkerFactory::new(false);
        let commands = factory.commands.clone();
        let answer = CreateElicitationResponse::new(ElicitationAction::Accept(
            ElicitationAcceptAction::new().content(BTreeMap::from([(
                "question_0".into(),
                ElicitationContentValue::String("option_0".into()),
            )])),
        ));
        exercise(
            factory,
            true,
            forms,
            Some(answer),
            |connection, workspace| async move {
                connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        "worker",
                        vec![ContentBlock::from("question")],
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(CloseSessionRequest::new("worker"))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await;
        assert!(
            commands
                .lock()
                .unwrap()
                .iter()
                .any(|command| match command {
                    SessionCommand::Control(
                        zevria_session_api::ControlCommand::AnswerQuestion { response, .. },
                    ) => matches!(response, QuestionResponse::Answered { .. }) == forms,
                    _ => false,
                })
        );
    }
    let (_, forms) = exercise(
        WorkerFactory::new(false),
        true,
        true,
        None,
        |connection, workspace| async move {
            connection
                .send_request(NewSessionRequest::new(workspace))
                .block_task()
                .await?;
            let pending = connection.send_request(PromptRequest::new(
                "worker",
                vec![ContentBlock::from("question")],
            ));
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            connection.send_notification(CancelNotification::new("worker"))?;
            assert_eq!(
                pending.block_task().await?.stop_reason,
                StopReason::Cancelled
            );
            connection
                .send_request(CloseSessionRequest::new("worker"))
                .block_task()
                .await?;
            Ok(())
        },
    )
    .await;
    assert_eq!(forms, 1);
}
