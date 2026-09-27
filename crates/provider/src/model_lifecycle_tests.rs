use super::*;

#[tokio::test]
async fn explicit_a_b_a_switches_send_full_websocket_input_and_clear_all_continuations() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/responses", listener.local_addr().unwrap());
    let mut a = profile("A", &url);
    a.endpoint.supports_websockets = true;
    let mut b = profile("B", &url);
    b.endpoint.supports_websockets = true;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut a = accept_async(stream).await.unwrap();
        let first = receive_json(&mut a).await;
        assert_eq!(first["model"], "A");
        send_json(
            &mut a,
            completed_event("response-a", "message-a", "answer A"),
        )
        .await;
        let (stream, _) = listener.accept().await.unwrap();
        let mut b = accept_async(stream).await.unwrap();
        let second = receive_json(&mut b).await;
        assert_eq!(second["model"], "B");
        assert!(second.get("previous_response_id").is_none());
        assert!(second["input"].to_string().contains("answer A"));
        assert!(!second["input"].to_string().contains("message-a"));
        send_json(
            &mut b,
            completed_event("response-b", "message-b", "answer B"),
        )
        .await;
        let third = receive_json(&mut a).await;
        assert_eq!(third["model"], "A");
        assert!(third.get("previous_response_id").is_none());
        assert!(third["input"].to_string().contains("message-a"));
        assert!(!third["input"].to_string().contains("message-b"));
        assert!(third["input"].to_string().contains("answer B"));
        send_json(&mut a, completed_event("response-a2", "message-a2", "done")).await;
    });
    let settings = Arc::new(Settings::default());
    let (_directory, mut engine) = engine(a.clone(), b.clone(), vec![], settings.clone());
    let mut history = Vec::<OwnedModelRequestItem>::new();
    for (index, context) in [a.context_policy(), b.context_policy(), a.context_policy()]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            assert!(matches!(
                manage(
                    &mut engine,
                    Request::Select {
                        scope: Scope::SessionOnly,
                        mode: SessionMode::Build,
                        target: selected(context.profile.clone()),
                        revision: String::new(),
                    }
                )
                .await,
                Result::Changed {
                    unchanged: false,
                    ..
                }
            ));
            assert!(
                engine
                    .provider()
                    .continuation_response_id(&a.profile)
                    .is_none()
            );
            assert!(
                engine
                    .provider()
                    .continuation_response_id(&b.profile)
                    .is_none()
            );
        }
        history.push(OwnedModelRequestItem::message(Message::user(format!(
            "question {index}"
        ))));
        let response = engine
            .provider_mut()
            .complete(
                ModelRequest {
                    instructions: test_instructions(),
                    input: history
                        .iter()
                        .map(OwnedModelRequestItem::as_borrowed)
                        .collect(),
                    model_role: ModelRole::Build,
                    allowed_tool_names: Some(&[]),
                },
                discard_updates(),
            )
            .await
            .unwrap();
        history.push(response.into_record().into_model_request_item());
        let continuation = engine
            .provider()
            .continuation_response_id(&context.profile)
            .unwrap()
            .to_string();
        assert!(matches!(
            manage(
                &mut engine,
                Request::Select {
                    scope: Scope::SessionOnly,
                    mode: SessionMode::Build,
                    target: selected(context.profile.clone()),
                    revision: String::new(),
                }
            )
            .await,
            Result::Changed {
                unchanged: true,
                ..
            }
        ));
        assert_eq!(
            engine.provider().continuation_response_id(&context.profile),
            Some(continuation.as_str()),
            "compatible no-op must not reset continuation"
        );
    }
    assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn confirmed_capacity_conversion_uses_large_source_not_small_destination() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = profile(
            "large",
            &format!("http://{}/v1/responses", listener.local_addr().unwrap()),
        );
        let mut b = profile("small", "http://127.0.0.1:1/v1/responses");
        // Keep room for the fixed engine protocol and the portable summary,
        // while the uncompressed history still exceeds the destination limit.
        b.context_window_tokens = 1600;
        b.input_token_limit = 1400;
        b.retained_user_tokens = 50;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_json(&mut stream).await;
            assert_eq!(request.body["model"], "large");
            assert!(request.body["tools"].as_array().is_none_or(Vec::is_empty));
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("summary", "msg", "short portable summary").to_string()),
            )
            .await;
        });
        let history = vec![TranscriptItem::Message(Message::user(
            "long input ".repeat(1000),
        ))];
        let settings = Arc::new(Settings::default());
        let (_directory, mut engine) = engine(a.clone(), b.clone(), history, settings.clone());
        let Result::ConfirmationRequired(preview) = manage(
            &mut engine,
            Request::Select {
                scope,
                mode: SessionMode::Build,
                target: selected(b.profile.clone()),
                revision: String::new(),
            },
        )
        .await
        else {
            panic!("capacity confirmation")
        };
        assert_eq!(preview.source.profile, a.profile);
        assert!(preview.reason.contains("input limit"));
        assert_eq!(preview.scope, scope);
        assert!(
            preview.reason.contains("costs tokens")
                && preview.reason.contains("lose summary detail")
                && preview.reason.contains("both Build and Plan")
        );
        assert!(preview.reason.contains(if scope == Scope::SessionOnly {
            "config unchanged"
        } else {
            "global default change"
        }));
        assert!(
            matches!(manage(&mut engine,Request::Confirm {preview}).await,Result::Changed {context,..} if context.profile==b.profile)
        );
        assert_eq!(
            settings.save_calls.load(Ordering::Relaxed),
            usize::from(scope == Scope::SessionAndDefault)
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn empty_or_oversized_summary_installs_neither_checkpoint_nor_selection() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        for summary in [String::new(), "enormous summary ".repeat(1000)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = profile(
                "A",
                &format!("http://{}/v1/responses", listener.local_addr().unwrap()),
            );
            let mut b = profile("B", "http://127.0.0.1:1/v1/responses");
            b.context_window_tokens = 1000;
            b.input_token_limit = 1000;
            b.retained_user_tokens = 10;
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                receive_http_json(&mut stream).await;
                send_http_response(
                    &mut stream,
                    "200 OK",
                    "text/event-stream",
                    sse_data(completed_event("summary", "msg", &summary).to_string()),
                )
                .await;
            });
            let original = opaque_history(&a);
            let settings = Arc::new(Settings::default());
            let (_directory, mut engine) =
                engine(a.clone(), b.clone(), original.clone(), settings.clone());
            let Result::ConfirmationRequired(preview) = manage(
                &mut engine,
                Request::Select {
                    scope,
                    mode: SessionMode::Build,
                    target: selected(b.profile),
                    revision: String::new(),
                },
            )
            .await
            else {
                panic!("preview")
            };
            assert!(matches!(
                manage(&mut engine, Request::Confirm { preview }).await,
                Result::Rejected {
                    checkpoint_installed: false,
                    ..
                }
            ));
            assert_eq!(&engine.conversation().items()[1..], original);
            assert!(
                matches!(manage(&mut engine,Request::List { scope: Scope::SessionOnly,mode:SessionMode::Build}).await,Result::Catalog {current,..} if current.profile==a.profile)
            );
            assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn cancellation_racing_model_publication_is_terminal_without_hiding_commits() {
    use std::{future::Future as _, task::Poll};
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        for kind in ["catalog", "preview", "changed"] {
            let a = profile("A", "http://127.0.0.1:1/v1/responses");
            let b = profile("B", &a.endpoint.base_url);
            let settings = Arc::new(Settings::default());
            let history = if kind == "preview" {
                opaque_history(&a)
            } else {
                vec![]
            };
            let (_directory, engine) = engine(a, b.clone(), history, settings.clone());
            let path = engine.conversation().path().to_path_buf();
            let before = std::fs::read(&path).unwrap();
            let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
            let (events, mut updates) = session_event_channel(1);
            let marker_id = format!("model-publication-backpressure-{scope:?}-{kind}");
            events
                .send(SessionEvent::SkillsResult {
                    request_id: marker_id.clone(),
                    result: zevria_instructions::skill::SkillManagementResult::error(
                        "marker",
                        "publication backpressure",
                    ),
                })
                .await
                .unwrap();
            commands
                .send(SessionCommand::Manage(
                    zevria_session_api::ManagementCommand::Models {
                        request_id: "selection".into(),
                        request: if kind != "catalog" {
                            Request::Select {
                                mode: SessionMode::Build,
                                scope,
                                target: selected(b.profile.clone()),
                                revision: String::new(),
                            }
                        } else {
                            Request::List {
                                mode: SessionMode::Build,
                                scope,
                            }
                        },
                    },
                ))
                .unwrap();
            let mut run = Box::pin(engine.run(rx, events));
            // The explicit marker occupies the single lifecycle slot. Poll through
            // synchronous preparation until result publication blocks.
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            commands
                .send(SessionCommand::Manage(
                    zevria_session_api::ManagementCommand::Models {
                        request_id: "selection".into(),
                        request: Request::Cancel,
                    },
                ))
                .unwrap();
            // Consume cancellation while publication is still blocked.
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            assert!(matches!(
                updates.try_recv().unwrap(),
                SessionUpdate::Lifecycle(SessionEvent::SkillsResult { request_id, .. }) if request_id == marker_id
            ));
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            let SessionUpdate::Lifecycle(SessionEvent::ModelsResult { request_id, result }) =
                updates.try_recv().unwrap()
            else {
                panic!("model result")
            };
            assert_eq!(request_id, "selection");
            if kind == "changed" {
                assert!(
                    matches!(result, Result::Changed { scope: actual_scope, unchanged: false, .. } if actual_scope == scope),
                    "a committed selection must not be hidden by cancellation"
                );
            } else {
                assert_eq!(result, Result::Cancelled);
            }
            // A cancelled preview must not block reopening the picker.
            commands
                .send(SessionCommand::Manage(
                    zevria_session_api::ManagementCommand::Models {
                        request_id: "reopened".into(),
                        request: Request::List {
                            mode: SessionMode::Build,
                            scope,
                        },
                    },
                ))
                .unwrap();
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            assert!(
                matches!(updates.try_recv().unwrap(), SessionUpdate::Lifecycle(SessionEvent::ModelsResult { request_id, result: Result::Catalog { .. } }) if request_id == "reopened")
            );
            commands
                .send(SessionCommand::Control(
                    zevria_session_api::ControlCommand::Shutdown,
                ))
                .unwrap();
            assert!(matches!(
                std::future::poll_fn(|cx| Poll::Ready(run.as_mut().poll(cx))).await,
                Poll::Ready(Ok(()))
            ));
            if kind == "changed" {
                assert_eq!(
                    transcript::load_report(&path)
                        .unwrap()
                        .session_models()
                        .unwrap()
                        .unwrap()
                        .for_mode(SessionMode::Build)
                        .profile,
                    b.profile
                );
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), before);
            }
            assert_eq!(
                settings.save_calls.load(Ordering::Relaxed),
                usize::from(kind == "changed" && scope == Scope::SessionAndDefault)
            );
        }
    }
}

#[tokio::test]
async fn maintenance_handles_cancellation_shutdown_and_never_queues_competing_management() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = profile(
            "A",
            &format!("http://{}/v1/responses", listener.local_addr().unwrap()),
        );
        let b = profile("B", "http://127.0.0.1:1/v1/responses");
        let (started, request_started) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            receive_http_json(&mut stream).await;
            started.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let original = opaque_history(&a);
        let settings = Arc::new(Settings::default());
        let (_directory, mut engine) = engine(a, b.clone(), original.clone(), settings.clone());
        let path = engine.conversation().path().to_path_buf();
        let Result::ConfirmationRequired(preview) = manage(
            &mut engine,
            Request::Select {
                scope,
                mode: SessionMode::Build,
                target: selected(b.profile),
                revision: String::new(),
            },
        )
        .await
        else {
            panic!("preview")
        };
        let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut updates) = session_event_channel(32);
        let task = tokio::spawn(engine.run(rx, events));
        commands
            .send(SessionCommand::Manage(
                zevria_session_api::ManagementCommand::Models {
                    request_id: "selection".into(),
                    request: Request::Confirm { preview },
                },
            ))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), request_started)
            .await
            .unwrap()
            .unwrap();
        commands
            .send(SessionCommand::Manage(
                zevria_session_api::ManagementCommand::Models {
                    request_id: "competitor".into(),
                    request: Request::List {
                        scope,
                        mode: SessionMode::Build,
                    },
                },
            ))
            .unwrap();
        commands
            .send(SessionCommand::Manage(
                zevria_session_api::ManagementCommand::Models {
                    request_id: "selection".into(),
                    request: Request::Cancel,
                },
            ))
            .unwrap();
        commands
            .send(SessionCommand::Control(
                zevria_session_api::ControlCommand::Shutdown,
            ))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .expect("valid replay");
        let mut cancelled = false;
        let mut busy = false;
        while let Ok(update) = updates.try_recv() {
            match update {
                SessionUpdate::Lifecycle(SessionEvent::ModelsResult { request_id, result })
                    if request_id == "competitor" =>
                {
                    busy = matches!(result,Result::Rejected {code,..} if code=="busy")
                }
                SessionUpdate::Lifecycle(SessionEvent::ModelsResult { request_id, result })
                    if request_id == "selection" =>
                {
                    cancelled = result == Result::Cancelled
                }
                SessionUpdate::Lifecycle(SessionEvent::TurnStarted { .. }) => {
                    panic!("management is not a turn")
                }
                _ => {}
            }
        }
        assert!(busy && cancelled);
        assert_eq!(
            &transcript::load_report(&path).unwrap().items[1..],
            original
        );
        server.abort();
        assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn unassigned_selectable_profile_forces_portable_root_compaction_but_not_children() {
    let source = r#"
base_url = "http://127.0.0.1:1/v1/responses"
api_key = "secret"
supports_websockets = false
[models.A]
context_window_tokens = 100000
retained_user_tokens = 100
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
[models.B]
context_window_tokens = 100000
retained_user_tokens = 100
reasoning_levels = ["low", "medium", "high"]
reasoning_summary_level = "detailed"
"#;
    let provider: ProviderConfig = toml::from_str(source).unwrap();
    let assignment = ModelAssignment {
        provider: "p".into(),
        model: "A".into(),
        reasoning_level: zevria_foundation::ReasoningLevel::Medium,
    };
    let modes = ModeAssignments {
        build: assignment.clone(),
        plan: assignment.clone(),
        review: assignment.clone(),
        explore: assignment.clone(),
        builder: assignment,
    };
    let routing =
        ModelRouting::resolve(&BTreeMap::from([("p".into(), provider)]), &modes, 90).unwrap();
    let tools = ToolServer::new().run();
    let mut root = ResponsesRouter::root(&routing, "", tools.clone(), "root").unwrap();
    assert_eq!(root.model_catalog().len(), 2);
    assert!(root.requires_portable_compaction());
    assert_eq!(root.initialized_profile_count(), 0);
    for role in [ModelRole::Explore, ModelRole::Builder] {
        let error = root
            .complete(
                ModelRequest {
                    instructions: test_instructions(),
                    input: Vec::new(),
                    model_role: role,
                    allowed_tool_names: None,
                },
                discard_updates(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not available in this session router")
        );
        assert_eq!(root.initialized_profile_count(), 0);
        let child = ResponsesRouterFactory::new(
            role,
            routing.for_role(role).clone(),
            routing.selection_for_role(role).reasoning_level,
            "",
        )
        .create("child", tools.clone())
        .unwrap();
        assert_eq!(child.model_catalog().len(), 1);
        assert!(!child.requires_portable_compaction());
    }
}
