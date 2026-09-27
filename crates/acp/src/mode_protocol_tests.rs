use super::*;
use zevria_session_api::ModeSelectionResult;
use zevria_workflow::PlanRecord;

#[tokio::test]
async fn prompt_metadata_opts_in_without_parsing_text_or_changing_mode() {
    let workspace = tempfile::tempdir().unwrap();
    let factory = FakeFactory::new();
    let commands = factory.commands.clone();
    let (client_transport, server_transport) = Channel::duplex();
    let root = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            root,
            Arc::new(factory),
            server_transport,
        )
        .await
    });
    Client
        .builder()
        .connect_with(client_transport, {
            let workspace = workspace.path().to_path_buf();
            async move |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let json = serde_json::to_value(initialized).unwrap();
                assert_eq!(
                    json["agentCapabilities"]["_meta"]["zevria.orchestration"]["version"],
                    1
                );
                let session = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                let id = session.session_id;
                assert!(
                    connection
                        .send_request(SetSessionModeRequest::new(id.clone(), "orchestrate"))
                        .block_task()
                        .await
                        .is_err()
                );
                for enabled in [None, Some(true), Some(false)] {
                    let mut prompt = PromptRequest::new(
                        id.clone(),
                        vec!["/orchestrate $orchestrate literal".into()],
                    );
                    let mut meta = serde_json::Map::from_iter([(
                        "unrelated.client".into(),
                        serde_json::json!({"keep":"opaque"}),
                    )]);
                    if let Some(enabled) = enabled {
                        meta.insert(
                            "zevria.orchestration".into(),
                            serde_json::json!({"version":1,"enabled":enabled}),
                        );
                    }
                    prompt.meta = Some(meta);
                    connection.send_request(prompt).block_task().await?;
                }
                connection
                    .send_request(SetSessionModeRequest::new(id.clone(), "plan"))
                    .block_task()
                    .await?;
                let mut prompt = PromptRequest::new(id.clone(), vec!["independent work".into()]);
                prompt.meta = Some(serde_json::Map::from_iter([(
                    "zevria.orchestration".into(),
                    serde_json::json!({"version":1,"enabled":true}),
                )]));
                assert!(
                    connection
                        .send_request(prompt)
                        .block_task()
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("root Build")
                );
                connection
                    .send_request(CloseSessionRequest::new(id))
                    .block_task()
                    .await?;
                Ok(())
            }
        })
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    let commands = commands.lock().unwrap();
    let prompts = commands
        .iter()
        .filter_map(|command| match command {
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                text,
                mode,
                behavior,
            }) => Some((text.text_projection(), *mode, *behavior)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 3);
    for (index, (text, mode, behavior)) in prompts.iter().enumerate() {
        assert_eq!(text, "/orchestrate $orchestrate literal");
        assert_eq!(*mode, SessionMode::Build);
        assert_eq!(
            *behavior,
            if index == 1 {
                zevria_foundation::RequestBehavior::Orchestrate
            } else {
                zevria_foundation::RequestBehavior::Standard
            }
        );
    }
}

#[test]
fn ordinary_modes_are_only_build_and_plan() {
    let modes = crate::agent::session_modes(SessionMode::Build, ExecutionProfile::Interactive);
    assert_eq!(modes.current_mode_id.to_string(), "build");
    assert_eq!(
        modes
            .available_modes
            .iter()
            .map(|mode| mode.id.to_string())
            .collect::<Vec<_>>(),
        ["build", "plan"]
    );
    assert!(
        modes.available_modes[0]
            .description
            .as_deref()
            .unwrap()
            .contains("explicit orchestration")
    );
}

#[tokio::test]
async fn set_mode_replies_after_ack_and_keeps_terminal_mode_commands_literal() {
    let workspace = tempfile::tempdir().unwrap();
    let mut factory = FakeFactory::new();
    let (gates, mut requests) = mpsc::unbounded_channel::<ModeAcknowledgment>();
    factory.mode_requests = Some(gates);
    let recorded = factory.commands.clone();
    let notifications = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
    let log = notifications.clone();
    let (selections, mut selected) = mpsc::unbounded_channel();
    let (client_transport, server_transport) = Channel::duplex();
    let server_workspace = workspace.path().to_path_buf();
    let server = tokio::spawn(async move {
        serve_on(
            AcpConfig::default(),
            server_workspace,
            Arc::new(factory),
            server_transport,
        )
        .await
    });
    let client = Client.builder().on_receive_notification(
        async move |notification: SessionNotification, _| {
            if let AcpSessionUpdate::CurrentModeUpdate(update) = &notification.update {
                selections.send(update.current_mode_id.to_string()).unwrap();
            }
            log.lock().unwrap().push(notification);
            Ok(())
        },
        agent_client_protocol::on_receive_notification!(),
    ).connect_with(client_transport, {
        let workspace = workspace.path().to_path_buf();
        let notifications = notifications.clone();
        async move |connection| {
            connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            let created = connection.send_request(NewSessionRequest::new(workspace)).block_task().await?;
            let id = created.session_id;
            assert_eq!(created.modes.unwrap().current_mode_id.to_string(), "build");
            let mut selecting = Box::pin(connection.send_request(SetSessionModeRequest::new(id.clone(), "plan")).block_task());
            let (_, mode, ack) = requests.recv().await.unwrap();
            assert_eq!(mode, SessionMode::Plan);
            assert!(!notifications.lock().unwrap().iter().any(|notification| matches!(notification.update, AcpSessionUpdate::CurrentModeUpdate(_))));
            tokio::select! {
                biased;
                response = &mut selecting => panic!("mode response preceded engine acknowledgment: {response:?}"),
                response = connection.send_request(PromptRequest::new(id.clone(), vec![ContentBlock::from("must not be queued")])).block_task() => {
                    assert!(response.unwrap_err().to_string().contains("management is pending"));
                }
            }
            assert!(connection.send_request(SetSessionModeRequest::new(id.clone(), "plan")).block_task().await.is_err());
            ack.send(ModeSelectionResult::Accepted { mode, changed: true }).unwrap();
            selecting.await?;
            assert_eq!(selected.recv().await.as_deref(), Some("plan"));
            for text in ["/build", "/orchestrate", "/plan", "/mode plan"] {
                connection.send_request(PromptRequest::new(id.clone(), vec![ContentBlock::from(text)])).block_task().await?;
            }
            let selecting = connection.send_request(SetSessionModeRequest::new(id.clone(), "plan")).block_task();
            let (_, mode, ack) = requests.recv().await.unwrap();
            assert_eq!(mode, SessionMode::Plan);
            ack.send(ModeSelectionResult::Rejected { code: "busy".into(), message: "model management pending".into() }).unwrap();
            assert!(selecting.await.unwrap_err().to_string().contains("model management pending"));
            // Cancellation resolves only the caller, not the durable command.
            // Keep admission locked and publish its eventual accepted result.
            for (request_cancellation, selected_mode) in [(false, "build"), (true, "plan")] {
                let selecting = connection.send_request(SetSessionModeRequest::new(id.clone(), selected_mode));
                let (_, mode, ack) = requests.recv().await.unwrap();
                if request_cancellation {
                    selecting.cancel()?;
                } else {
                    connection.send_notification(CancelNotification::new(id.clone()))?;
                }
                assert!(selecting.block_task().await.is_err());
                assert!(connection.send_request(PromptRequest::new(id.clone(), vec![ContentBlock::from("cancelled selection still pending")])).block_task().await.unwrap_err().to_string().contains("management is pending"));
                assert!(connection.send_request(SetSessionModeRequest::new(id.clone(), "build")).block_task().await.is_err());
                ack.send(ModeSelectionResult::Accepted { mode, changed: true }).unwrap();
                assert_eq!(selected.recv().await.as_deref(), Some(selected_mode));
            }
            let selecting = connection.send_request(SetSessionModeRequest::new(id.clone(), "build")).block_task();
            let (_, mode, ack) = requests.recv().await.unwrap();
            ack.send(ModeSelectionResult::Accepted { mode, changed: true }).unwrap();
            selecting.await?;
            assert_eq!(selected.recv().await.as_deref(), Some("build"));
            connection.send_request(CloseSessionRequest::new(id)).block_task().await?;
            Ok(())
        }
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
    let mode_updates = notifications
        .lock()
        .unwrap()
        .iter()
        .filter_map(|notification| match &notification.update {
            AcpSessionUpdate::CurrentModeUpdate(update) => Some(update.current_mode_id.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(mode_updates, ["plan", "build", "plan", "build"]);
    let prompts = recorded
        .lock()
        .unwrap()
        .iter()
        .filter_map(|command| match command {
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                behavior: _,
                text,
                mode,
            }) => Some((text.text_projection(), *mode)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        prompts,
        ["/build", "/orchestrate", "/plan", "/mode plan"]
            .map(|text| (text.to_string(), SessionMode::Plan))
    );
}

#[tokio::test]
async fn new_load_and_resume_use_composed_selection_except_ready_lock() {
    let artifact = PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 1,
        },
        title: "Retained Selection Plan".into(),
        markdown: "# Retained Selection Plan\n\n## Goal\nKeep selection.\n\n## Decisions\nReports are not selection authority.\n\n## Implementation\nPreserve explicit mode.\n\n## Validation\nTest acknowledgment and replay.\n\n## Risks\nNone.\n".into(),
        source_turn_id: TurnId::new(1),
    };
    for (records, expected_mode) in [
        (vec![], "build"),
        (
            vec![PlanRecord::Started {
                id: artifact.version.id,
            }],
            "build",
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Published {
                    artifact: artifact.clone(),
                    provenance: zevria_workflow::PlanPublicationProvenance::Synthesized,
                },
            ],
            "build",
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
                PlanRecord::RevisionRequested {
                    artifact: artifact.clone(),
                },
            ],
            "build",
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
                PlanRecord::Resolved {
                    id: artifact.version.id,
                    artifact: Some(artifact.clone()),
                    resolution: PlanResolution::ImplementedCurrent,
                },
            ],
            "build",
        ),
        (
            vec![
                PlanRecord::Started {
                    id: artifact.version.id,
                },
                PlanRecord::Ready {
                    artifact: artifact.clone(),
                },
            ],
            "plan",
        ),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let mut factory = FakeFactory::new();
        factory.selected_mode = SessionMode::Build;
        // Deliberately leave older mode metadata in the transcript: the
        // composition root's selected_mode is the authoritative replay result.
        Arc::make_mut(&mut factory.persisted).extend(records.into_iter().map(TranscriptItem::Plan));
        let commands = factory.commands.clone();
        let notifications = Arc::new(Mutex::new(Vec::<SessionNotification>::new()));
        let log = notifications.clone();
        let (client_transport, server_transport) = Channel::duplex();
        let server_workspace = workspace.path().to_path_buf();
        let server = tokio::spawn(async move {
            serve_on(
                AcpConfig::default(),
                server_workspace,
                Arc::new(factory),
                server_transport,
            )
            .await
        });
        let client = Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _| {
                    log.lock().unwrap().push(notification);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, {
                let workspace = workspace.path().to_path_buf();
                async move |connection| {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let new = connection
                        .send_request(NewSessionRequest::new(workspace.clone()))
                        .block_task()
                        .await?;
                    assert_eq!(new.modes.unwrap().current_mode_id.to_string(), "build");
                    connection
                        .send_request(CloseSessionRequest::new(new.session_id))
                        .block_task()
                        .await?;
                    let loaded = connection
                        .send_request(LoadSessionRequest::new("persisted", workspace.clone()))
                        .block_task()
                        .await?;
                    assert_eq!(
                        loaded.modes.unwrap().current_mode_id.to_string(),
                        expected_mode
                    );
                    if expected_mode == "plan" {
                        assert!(
                            connection
                                .send_request(SetSessionModeRequest::new("persisted", "build"))
                                .block_task()
                                .await
                                .is_err()
                        );
                    }
                    connection
                        .send_request(CloseSessionRequest::new("persisted"))
                        .block_task()
                        .await?;
                    let resumed = connection
                        .send_request(ResumeSessionRequest::new("persisted", workspace))
                        .block_task()
                        .await?;
                    assert_eq!(
                        resumed.modes.unwrap().current_mode_id.to_string(),
                        expected_mode
                    );
                    connection
                        .send_request(CloseSessionRequest::new("persisted"))
                        .block_task()
                        .await?;
                    Ok(())
                }
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
        assert!(
            commands.lock().unwrap().iter().all(|command| matches!(
                command,
                SessionCommand::Control(zevria_session_api::ControlCommand::Shutdown)
            )),
            "replay must not allocate turns or management requests"
        );
        assert!(
            !notifications
                .lock()
                .unwrap()
                .iter()
                .any(|notification| matches!(
                    notification.update,
                    AcpSessionUpdate::CurrentModeUpdate(_)
                )),
            "metadata replay remains inert"
        );
    }
}
