use super::*;

#[tokio::test]
async fn target_counting_uses_portable_input_and_exact_or_conservative_admission() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        for status in ["200 OK", "404 Not Found", "503 Service Unavailable"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/input_tokens", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = receive_http_json(&mut stream).await;
                assert_eq!(request.body["model"], "B");
                assert_eq!(
                    request.body["reasoning"]["effort"], "high",
                    "destination counting uses the requested level, not the source role's medium"
                );
                assert!(request.body.get("previous_response_id").is_none());
                let wire = request.body["input"].to_string();
                assert!(wire.contains("completed output"));
                for forbidden in [
                    "private-cipher",
                    "private-meta",
                    "private-signature",
                    "item_count-call",
                    "count-call",
                ] {
                    assert!(!wire.contains(forbidden), "{wire}");
                }
                send_http_response(
                    &mut stream,
                    status,
                    "application/json",
                    json!({"input_tokens":100}).to_string(),
                )
                .await;
            });
            let a = profile("A", "http://127.0.0.1:1/v1/responses");
            let mut b = profile("B", &a.endpoint.base_url);
            b.context_window_tokens = 300;
            b.input_token_limit = 300;
            b.retained_user_tokens = 10;
            b.endpoint.input_token_count = InputTokenCountConfig {
                enabled: true,
                derive_url: false,
                url: Some(url),
                request_timeout_seconds: 1,
            };
            let replay = ledger("A", "count-call");
            let result = tool_result(&replay);
            let history = vec![
                TranscriptItem::Message(Message::user("input ".repeat(400))),
                TranscriptItem::provider_message(replay).unwrap(),
                TranscriptItem::ToolResults {
                    skill_applications: Vec::new(),
                    message: result,
                    metadata: vec![],
                },
            ];
            let settings = Arc::new(Settings::default());
            let (_directory, mut engine) = engine(a, b.clone(), history.clone(), settings.clone());
            let result = manage(
                &mut engine,
                Request::Select {
                    scope,
                    mode: SessionMode::Build,
                    target: ModelSelection::new(b.profile, zevria_foundation::ReasoningLevel::High),
                    revision: String::new(),
                },
            )
            .await;
            if status == "200 OK" {
                assert!(matches!(result, Result::Changed { .. }));
            } else {
                assert!(matches!(result, Result::ConfirmationRequired(_)));
            }
            assert_eq!(&engine.conversation().items()[1..], history);
            assert_eq!(engine.provider().initialized_profile_count(), 1);
            assert_eq!(
                settings.save_calls.load(Ordering::Relaxed),
                usize::from(scope == Scope::SessionAndDefault && status == "200 OK")
            );
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn pending_plan_and_readonly_history_reject_model_management() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        use zevria_foundation::TurnId;
        use zevria_workflow::PlanArtifact;
        use zevria_workflow::PlanId;
        use zevria_workflow::PlanRecord;
        use zevria_workflow::PlanVersion;
        let a = profile("A", "http://127.0.0.1:1/v1/responses");
        let b = profile("B", &a.endpoint.base_url);
        let id = PlanId::new();
        let history=vec![TranscriptItem::Plan(PlanRecord::Started {id}),TranscriptItem::Plan(PlanRecord::Ready {artifact:PlanArtifact {version:PlanVersion {id,revision:1},title:"Pending Model Selection".into(),markdown:"# Pending Model Selection\n\n## Goal\nDone\n\n## Decisions\nDone\n\n## Implementation\nDone\n\n## Validation\nDone\n\n## Risks\nNone".into(),source_turn_id:TurnId::new(1)}})];
        let settings = Arc::new(Settings::default());
        let (_directory, mut ready) = engine(a.clone(), b.clone(), history, settings.clone());
        assert!(
            matches!(manage(&mut ready,Request::Select { scope,mode:SessionMode::Build,target: selected(b.profile.clone()),revision:String::new()}).await,Result::Rejected {message,..} if message.contains("Plan approval"))
        );
        let (_directory, mut blocked) =
            engine_with_access(a, b.clone(), vec![], settings.clone(), true);
        assert!(
            matches!(manage(&mut blocked,Request::Select { scope,mode:SessionMode::Build,target: selected(b.profile),revision:String::new()}).await,Result::Rejected {message,..} if message.contains("read-only"))
        );
        assert_eq!(ready.provider().initialized_profile_count(), 0);
        assert_eq!(blocked.provider().initialized_profile_count(), 0);
        assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn unavailable_or_multiple_opaque_sources_fail_without_inference() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let a = profile("A", "http://127.0.0.1:1/v1/responses");
        let b = profile("B", &a.endpoint.base_url);
        for multiple in [false, true] {
            let mut history = opaque_history(&a);
            let TranscriptItem::Compaction(checkpoint) = history.last_mut().unwrap() else {
                unreachable!()
            };
            let missing = OwnedModelRequestItem::replay_only(ProviderReplay::openai_responses(
                ModelProfileRef::new("missing", "C"),
                vec![json!({"type":"compaction","encrypted_content":"unavailable"})],
            ))
            .unwrap();
            if multiple {
                checkpoint.replacement_history.push(missing);
            } else {
                checkpoint.replacement_history = vec![missing];
            }
            let settings = Arc::new(Settings::default());
            let (_directory, mut engine) =
                engine(a.clone(), b.clone(), history.clone(), settings.clone());
            assert!(matches!(
                manage(
                    &mut engine,
                    Request::Select {
                        scope,
                        mode: SessionMode::Build,
                        target: selected(b.profile.clone()),
                        revision: String::new()
                    }
                )
                .await,
                Result::Rejected {
                    checkpoint_installed: false,
                    ..
                }
            ));
            assert_eq!(&engine.conversation().items()[1..], history);
            assert_eq!(engine.provider().initialized_profile_count(), 0);
            assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
        }
    }
}

#[tokio::test]
async fn resumed_current_destination_can_request_conversion_without_a_temporary_route_change() {
    for scope in [Scope::SessionOnly, Scope::SessionAndDefault] {
        let a = profile("A", "http://127.0.0.1:1/v1/responses");
        let b = profile("B", &a.endpoint.base_url);
        let history = opaque_history(&a);
        let (_directory, mut engine) =
            engine(a.clone(), b.clone(), history, Arc::new(Settings::default()));
        // Plan's saved selection is already B, but the shared checkpoint is A.
        let Result::ConfirmationRequired(preview) = manage(
            &mut engine,
            Request::Select {
                scope,
                mode: SessionMode::Plan,
                target: selected(b.profile.clone()),
                revision: String::new(),
            },
        )
        .await
        else {
            panic!("same incompatible default must offer conversion")
        };
        assert_eq!(preview.scope, scope);
        assert_eq!(preview.source.profile, a.profile);
        assert_eq!(preview.target.profile, b.profile);
        assert_eq!(engine.provider().initialized_profile_count(), 0);
    }
}

#[tokio::test]
async fn session_only_revalidates_configuration_after_async_counting_and_conversion() {
    for conversion in [false, true] {
        let settings = Arc::new(Settings::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let a = profile("A", &url);
        let mut b = profile("B", &url);
        let history = if conversion {
            opaque_history(&a)
        } else {
            b.context_window_tokens = 300;
            b.input_token_limit = 300;
            b.retained_user_tokens = 10;
            b.endpoint.input_token_count = InputTokenCountConfig {
                enabled: true,
                derive_url: false,
                url: Some(url.clone()),
                request_timeout_seconds: 2,
            };
            vec![TranscriptItem::Message(Message::user(
                "large input ".repeat(400),
            ))]
        };
        let server_settings = settings.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = receive_http_json(&mut stream).await;
            assert_eq!(request.body["model"], if conversion { "A" } else { "B" });
            // An external edit happens while the engine is awaiting the provider.
            *server_settings.revision.lock().unwrap() = "external-edit".into();
            let (content_type, response) = if conversion {
                (
                    "text/event-stream",
                    sse_data(completed_event("summary", "msg", "portable summary").to_string()),
                )
            } else {
                ("application/json", json!({"input_tokens": 1}).to_string())
            };
            send_http_response(&mut stream, "200 OK", content_type, response).await;
        });
        let (_directory, mut engine) =
            engine(a.clone(), b.clone(), history.clone(), settings.clone());
        let path = engine.conversation().path().to_path_buf();
        let bytes = std::fs::read(&path).unwrap();
        let mut result = manage(
            &mut engine,
            Request::Select {
                scope: Scope::SessionOnly,
                mode: SessionMode::Build,
                target: selected(b.profile),
                revision: String::new(),
            },
        )
        .await;
        if conversion {
            let Result::ConfirmationRequired(preview) = result else {
                panic!("preview: {result:?}")
            };
            result = manage(&mut engine, Request::Confirm { preview }).await;
        }
        assert!(
            matches!(result, Result::Rejected { checkpoint_installed: false, current_revision: None, message, .. } if message.contains("configuration conflict"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(&engine.conversation().items()[1..], history);
        assert_eq!(
            &engine
                .conversation()
                .session_models()
                .unwrap()
                .for_mode(SessionMode::Build)
                .profile,
            &a.profile
        );
        assert_eq!(settings.save_calls.load(Ordering::Relaxed), 0);
        assert_eq!(*settings.revision.lock().unwrap(), "external-edit");
        server.await.unwrap();
    }
}

#[test]
fn portable_ids_cannot_collide_with_later_native_ids() {
    let foreign = ledger("A", "foreign");
    let native = ledger("B", "zevria_call_000000");
    let fm = zevria_model::ReplayMessage::new(foreign.clone()).unwrap();
    let nm = zevria_model::ReplayMessage::new(native.clone()).unwrap();
    let fr = tool_result(&foreign);
    let nr = tool_result(&native);
    let input = [
        ModelRequestItem::replay_backed(&fm),
        ModelRequestItem::message(&fr),
        ModelRequestItem::replay_backed(&nm),
        ModelRequestItem::message(&nr),
    ];
    let projection = zevria_responses::replay::project(&input, &native.source_profile)
        .unwrap()
        .0;
    let calls = projection
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect::<Vec<_>>();
    assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
}
