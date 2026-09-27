//! Exercise the engine's ordered live directive stream, not a reconstructed overlay.
use super::*;
use zevria_instructions::DirectiveContent;
use zevria_instructions::DirectivePayload;
use zevria_instructions::DirectivePolicy;
use zevria_instructions::InstructionSet;
use zevria_instructions::skill::SkillCatalog;
use zevria_instructions::skill::SkillDefinition;
use zevria_instructions::skill::SkillManagementRequest;
use zevria_instructions::skill::SkillManagementService;
use zevria_instructions::skill::SkillRequest;
use zevria_instructions::skill::SkillSource;
use zevria_instructions::skill::SkillsConfig;
use zevria_model::OwnedModelRequestItem;
use zevria_session_api::TurnCommand;

#[tokio::test]
async fn request_activation_correction_and_standard_reset_preserve_wire_prefix_and_reconnect() {
    use zevria_foundation::{RequestBehavior, RequestMetadata};
    use zevria_instructions::RequestDirective;
    for developer_messages in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut captured = Vec::new();
            for generation in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                for offset in 0..2 {
                    let index = generation * 2 + offset;
                    let request = receive_json(&mut socket).await;
                    send_json(
                        &mut socket,
                        completed_event(
                            &format!("r{index}"),
                            &format!("m{index}"),
                            "completed provider work",
                        ),
                    )
                    .await;
                    captured.push(request);
                }
            }
            captured
        });
        let (socket, _) = connect_async(&url).await.unwrap();
        let mut provider = test_session_with_url(&url, socket, None);
        provider.compatibility.developer_messages = developer_messages;
        provider.tools = ToolServer::new()
            .tool(PolicyCommandTool)
            .tool(PolicyWriteTool)
            .run();
        let instructions =
            test_instruction_set("Stable Build with conditional orchestration").render();
        let orchestrated = RequestMetadata::new(RequestBehavior::Orchestrate);
        let mut input = Vec::<OwnedModelRequestItem>::new();
        let mut full = Vec::new();
        for index in 0..4 {
            if index == 2 {
                input.push(OwnedModelRequestItem::RequestInstruction(
                    RequestDirective::correction(orchestrated.clone()),
                ));
                provider.ws.reconnect().await.unwrap();
            } else {
                input.push(OwnedModelRequestItem::message(Message::user(format!(
                    "prompt {index}"
                ))));
                input.push(OwnedModelRequestItem::RequestInstruction(
                    RequestDirective::boundary(if index == 1 {
                        orchestrated.clone()
                    } else {
                        RequestMetadata::new(RequestBehavior::Standard)
                    }),
                ));
            }
            let request = ModelRequest {
                instructions: &instructions,
                input: input
                    .iter()
                    .map(OwnedModelRequestItem::as_borrowed)
                    .collect(),
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            };
            let prepared = crate::turn::prepare_turn_request(&request, &mut provider)
                .await
                .unwrap();
            full.push(prepared.full_input.clone());
            let response = provider.complete(request, discard_updates()).await.unwrap();
            input.push(response.into_record().into_model_request_item());
        }
        let captured = server.await.unwrap();
        for (index, request) in captured.iter().enumerate() {
            assert_eq!(request["instructions"], captured[0]["instructions"]);
            assert_eq!(request["tools"], captured[0]["tools"]);
            assert_eq!(request["tools"].as_array().unwrap().len(), 2);
            if index == 0 || index == 2 {
                assert!(request.get("previous_response_id").is_none());
                assert_eq!(request["input"], serde_json::json!(full[index]));
            } else {
                assert_eq!(request["previous_response_id"], format!("r{}", index - 1));
                assert_eq!(request["input"].as_array().unwrap().len(), 2);
            }
            let directive = full[index].last().unwrap();
            assert_eq!(
                directive["role"],
                if developer_messages {
                    "developer"
                } else {
                    "user"
                }
            );
            assert!(
                directive["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("Request directive:")
            );
            if index > 0 {
                assert!(full[index].starts_with(&full[index - 1]));
            }
        }
        assert!(
            full[2]
                .last()
                .unwrap()
                .to_string()
                .contains("one engine correction")
        );
        assert!(
            full[3]
                .last()
                .unwrap()
                .to_string()
                .contains("Standard request")
        );
    }
}

#[test]
fn instruction_set_bytes_are_an_independent_wire_fixture() {
    let instructions = test_instruction_set("Compatibility instructions").render();
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/instructions.txt");
    if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
        std::fs::write(&path, &instructions).unwrap();
    }
    assert_eq!(instructions, std::fs::read_to_string(path).unwrap());
}

#[tokio::test]
async fn second_prompt_reconnect_preserves_prefix_in_both_developer_compatibility_modes() {
    for developer_messages in [true, false] {
        let requests = incident_prefix_flow(developer_messages, [1000, 1000, 0, 1000]).await;
        let expected_role = if developer_messages {
            "developer"
        } else {
            "user"
        };
        let mut directives = 0;
        for item in requests[2]["input"].as_array().unwrap() {
            if item["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("Skill directive:"))
            {
                assert_eq!(item["role"], expected_role);
                directives += 1;
            }
        }
        assert_eq!(directives, 0, "ordinary prompts need no ordered directives");
        assert_eq!(requests[0]["instructions"], requests[2]["instructions"]);
        assert_eq!(requests[0]["tools"], requests[2]["tools"]);
    }
}

pub(super) struct ApplySkill;
impl Tool for ApplySkill {
    const NAME: &'static str = "skill";
    type Error = std::convert::Infallible;
    type Args = SkillRequest;
    type Output = String;
    fn description(&self) -> String {
        "Activate a pinned skill".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object", "properties":{"skill":{"type":"string"}}, "required":["skill"]})
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        request: SkillRequest,
    ) -> Result<String, Self::Error> {
        let application = if request.arguments().is_empty() {
            "the current request"
        } else {
            request.arguments()
        };
        Ok(format!(
            "status: activated\nskill: {}\napplication: {application}",
            request.skill
        ))
    }
}

struct ToggleSkills;
impl SkillManagementService for ToggleSkills {
    fn update<'a>(
        &'a self,
        request: SkillManagementRequest,
        installed: Arc<SkillCatalog>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Arc<SkillCatalog>>> + Send + 'a>,
    > {
        Box::pin(async move {
            let SkillManagementRequest::SetEnabled { enabled, .. } = request else {
                anyhow::bail!("expected enablement mutation")
            };
            Ok(Arc::new(installed.as_ref().clone().with_config(
                SkillsConfig {
                    enabled,
                    ..Default::default()
                },
            )?))
        })
    }
}

async fn run_command(engine: &mut SessionEngine<OpenAiProvider>, command: SessionCommand) {
    let (events, mut receiver) = session_event_channel(128);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.handle_command(command, &events),
    )
    .await
    .unwrap()
    .unwrap();
    while let Ok(update) = receiver.try_recv() {
        if let SessionUpdate::Lifecycle(event) = update {
            assert!(
                !matches!(
                    event,
                    SessionEvent::TurnFailed { .. } | SessionEvent::TurnRejected { .. }
                ),
                "{event:?}"
            );
        }
    }
}
fn submit(text: &str) -> SessionCommand {
    SessionCommand::Turn(TurnCommand::Submit {
        behavior: zevria_foundation::RequestBehavior::Standard,
        text: text.into(),
        mode: SessionMode::Build,
    })
}
fn invoke() -> SessionCommand {
    SessionCommand::Turn(TurnCommand::InvokeSkill {
        name: "review".parse().unwrap(),
        args: "inspect".into(),
        mode: SessionMode::Build,
    })
}

#[tokio::test]
async fn engine_skill_lifecycle_is_incremental_with_both_explicit_wire_roles() {
    for developer_messages in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut requests = Vec::<Value>::new();
            for index in 0..10 {
                let request = receive_json(&mut socket).await;
                assert!(
                    request["instructions"]
                        .as_str()
                        .unwrap()
                        .starts_with("Zevria engine instructions.")
                );
                if (1..=4).contains(&index) {
                    assert_eq!(request["instructions"], requests[0]["instructions"]);
                }
                assert_eq!(request["prompt_cache_key"], "test-session");
                if index == 0 || index >= 5 {
                    assert!(request.get("previous_response_id").is_none());
                } else {
                    assert_eq!(
                        request["previous_response_id"],
                        format!("resp_{previous}", previous = index - 1)
                    );
                    assert_eq!(request["tools"], requests[0]["tools"]);
                }
                let output = if index == 3 {
                    completed_output_event(
                        "resp_3",
                        vec![
                            json!({"type":"function_call", "id":"logical_skill", "call_id":"native_skill", "name":"skill", "arguments":"{\"skill\":\"commit\"}", "status":"completed"}),
                        ],
                    )
                } else {
                    completed_event(&format!("resp_{index}"), &format!("msg_{index}"), "done")
                };
                send_json(&mut socket, output).await;
                requests.push(request);
            }
            requests
        });
        let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
        let registry = Arc::new(
            SkillCatalog::new([
                SkillDefinition::new(
                    "review".parse().unwrap(),
                    "Review",
                    "EXACT_REVIEW_BODY",
                    SkillSource::Programmatic("fixture".into()),
                )
                .unwrap(),
                SkillDefinition::new(
                    "commit".parse().unwrap(),
                    "Commit",
                    "EXACT_COMMIT_BODY",
                    SkillSource::Programmatic("fixture".into()),
                )
                .unwrap(),
            ])
            .unwrap(),
        );
        let tools = ToolServer::new().tool(ApplySkill).run();
        let mut provider = test_session(socket, None);
        provider.tools = tools.clone();
        provider.compatibility.developer_messages = developer_messages;
        provider.compatibility.send_prompt_cache_key = true;
        provider.responses_parameters = Some(json!({"prompt_cache_key":"test-session"}));
        let policies = SessionPolicies::new(
            TurnPolicy::new(
                "Stable Build policy",
                Some(vec!["skill".into()]),
                ModelRole::Build,
                true,
            ),
            TurnPolicy::new("Plan", Some(vec![]), ModelRole::Plan, false),
        );
        let directory = tempfile::tempdir().unwrap();
        let writer = TranscriptWriter::create(directory.path()).unwrap();
        let mut engine = SessionEngine::new(provider, tools, policies, writer, registry.clone())
            .unwrap()
            .with_skill_management(Arc::new(ToggleSkills), [true, false])
            .unwrap();
        let mut previous = Vec::<OwnedModelRequestItem>::new();
        for command in [
            submit("begin"),
            invoke(),
            invoke(),
            submit("commit the changes"),
        ] {
            run_command(&mut engine, command).await;
            let input = engine
                .model_input()
                .into_iter()
                .map(ModelRequestItem::to_owned_item)
                .collect::<anyhow::Result<Vec<_>>>()
                .unwrap();
            assert!(input.starts_with(&previous));
            previous = input;
        }
        let initial_catalog = (registry.clone()).as_ref().clone();
        run_command(
            &mut engine,
            SessionCommand::Manage(zevria_session_api::ManagementCommand::Skills {
                request_id: "disable".into(),
                request: SkillManagementRequest::SetEnabled {
                    expected_revision: initial_catalog.revision().into(),
                    name: "review".parse().unwrap(),
                    enabled: false,
                },
            }),
        )
        .await;
        run_command(&mut engine, submit("disabled")).await;
        let disabled = (Arc::new(
            registry
                .as_ref()
                .clone()
                .with_config(SkillsConfig {
                    enabled: false,
                    ..Default::default()
                })
                .unwrap(),
        ))
        .as_ref()
        .clone();
        run_command(
            &mut engine,
            SessionCommand::Manage(zevria_session_api::ManagementCommand::Skills {
                request_id: "enable".into(),
                request: SkillManagementRequest::SetEnabled {
                    expected_revision: disabled.revision().into(),
                    name: "review".parse().unwrap(),
                    enabled: true,
                },
            }),
        )
        .await;
        run_command(&mut engine, submit("enabled")).await;
        // Change effective policy under the same mode identifier and exact tools.
        engine = engine
            .with_skill_management(Arc::new(ToggleSkills), [false, false])
            .unwrap();
        run_command(&mut engine, submit("skill-disabled scope")).await;
        engine = engine
            .with_skill_management(Arc::new(ToggleSkills), [true, false])
            .unwrap();
        run_command(&mut engine, submit("restore scope")).await;
        assert_eq!(
            transcript::load(engine.conversation().path()).unwrap(),
            engine.conversation().items()
        );
        assert_eq!(engine.active_skills().unwrap().len(), 2);
        run_command(
            &mut engine,
            SessionCommand::Turn(TurnCommand::EditTranscript(
                zevria_session_api::TranscriptEdit {
                    target: zevria_session_api::TranscriptEditTarget::PromptOrdinal(0),
                    replacement: zevria_session_api::TranscriptEditReplacement::Message {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: "replacement history".into(),
                        mode: SessionMode::Build,
                    },
                },
            )),
        )
        .await;
        assert!(engine.active_skills().unwrap().is_empty());
        let requests = server.await.unwrap();
        assert!(
            requests[9]["instructions"]
                .to_string()
                .contains("## Eligible skills")
        );
        assert!(
            !requests[9]["input"]
                .to_string()
                .contains("EXACT_COMMIT_BODY")
        );
        assert!(
            !requests[9]["input"]
                .to_string()
                .contains("EXACT_REVIEW_BODY")
        );
        let expected_role = if developer_messages {
            "developer"
        } else {
            "user"
        };
        for request in &requests {
            for item in request["input"].as_array().unwrap() {
                if item["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Skill directive:"))
                {
                    assert_eq!(item["role"], expected_role);
                }
            }
        }
        let first = requests[0]["instructions"].to_string();
        assert!(first.contains("## Eligible skills"));
        assert!(first.contains("Commit"));
        assert!(!first.contains("EXACT_COMMIT_BODY") && !first.contains("EXACT_REVIEW_BODY"));
        assert_eq!(
            requests[1]["input"].as_array().unwrap().len(),
            2,
            "direct invocation then body only"
        );
        assert!(
            requests[1]["input"][1]
                .to_string()
                .contains("EXACT_REVIEW_BODY")
        );
        assert_eq!(
            requests[2]["input"].as_array().unwrap().len(),
            1,
            "repeated invocation is bodyless"
        );
        assert_eq!(
            requests[4]["input"].as_array().unwrap().len(),
            2,
            "complete result then new body only"
        );
        assert_eq!(requests[4]["input"][0]["type"], "function_call_output");
        assert_eq!(requests[4]["input"][0]["call_id"], "native_skill");
        assert!(
            requests[4]["input"][1]
                .to_string()
                .contains("EXACT_COMMIT_BODY")
        );
        assert!(
            requests[5]["instructions"]
                .to_string()
                .contains("Skill selection is unavailable")
        );
        assert!(
            requests[5]["input"]
                .to_string()
                .contains("Skill directive: revoke")
        );
        assert!(
            requests[6]["input"]
                .to_string()
                .contains("EXACT_REVIEW_BODY")
        );
        assert!(
            requests[7]["instructions"]
                .as_str()
                .unwrap()
                .contains("\"skills\":false")
        );
        assert!(
            requests[8]["instructions"]
                .as_str()
                .unwrap()
                .contains("\"skills\":true")
        );
    }
}

#[tokio::test]
async fn directive_only_preflight_count_and_http_use_identical_input_without_rig_fallback() {
    for developer_messages in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut count_stream, _) = listener.accept().await.unwrap();
            let count = receive_http_json(&mut count_stream).await;
            send_http_response(
                &mut count_stream,
                "200 OK",
                "application/json",
                json!({"input_tokens":321}).to_string(),
            )
            .await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let completion = receive_http_json(&mut stream).await;
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(completed_event("response", "message", "done").to_string()),
            )
            .await;
            (count, completion)
        });
        let registry = Arc::new(
            SkillCatalog::new([SkillDefinition::new(
                "commit".parse().unwrap(),
                "Commit matching metadata",
                "NEVER_DISCLOSE_THIS_BODY",
                SkillSource::Programmatic("fixture".into()),
            )
            .unwrap()])
            .unwrap(),
        );
        let context = zevria_instructions::skill::SkillContext {
            catalog: registry.clone(),
            pins: Default::default(),
            mode_enabled: true,
        };
        let catalog = context.prompt_catalog();
        let tools = ToolServer::new().tool(ApplySkill).run();
        let compatibility = ResponsesCompatibilityConfig {
            developer_messages,
            ..Default::default()
        };
        let profile = resolved_profile(
            "p",
            "model",
            format!("http://{address}/v1/responses"),
            "key",
            false,
            ReasoningSummaryLevel::Auto,
            compatibility,
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            10_000,
        );
        let target = profile.profile.clone();
        let mut router = ResponsesRouter::from_routes(
            [(
                ModelRole::Build,
                profile,
                zevria_foundation::ReasoningLevel::Medium,
            )],
            "not wire instructions",
            tools,
            "stable-session",
        )
        .unwrap();
        let instructions = InstructionSet {
            application: "Current app".into(),
            system: vec![
                ("guidance:global".into(), "GLOBAL_GUIDANCE_DEFAULTS".into()),
                (
                    "guidance:project".into(),
                    "PROJECT_GUIDANCE_OVERRIDES".into(),
                ),
            ],
            workflow: DirectivePolicy::new(
                "build",
                &TurnPolicy::new("Build policy", None, ModelRole::Build, true),
            ),
            catalog: Some(catalog),
        }
        .render();
        let body = test_skill("EXACT_PINNED_BODY");
        let revocation = DirectiveContent::new(DirectivePayload::SkillRevocation {
            name: "review".parse().unwrap(),
            reason: "disabled".into(),
        })
        .unwrap();
        let request = ModelRequest {
            instructions: &instructions,
            input: vec![
                ModelRequestItem::DeveloperInstruction(body),
                ModelRequestItem::DeveloperInstruction(&revocation),
            ],
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        };
        router.preflight_input(&target, &request.input).unwrap();
        assert_eq!(
            router
                .count_profile(
                    &zevria_model::models::ModelSelection::new(
                        target.clone(),
                        zevria_foundation::ReasoningLevel::Medium
                    ),
                    request.clone()
                )
                .await
                .unwrap(),
            InputTokenCount::Exact(321)
        );
        router.complete(request, discard_updates()).await.unwrap();
        let (count, completion) = server.await.unwrap();
        for field in ["input", "instructions", "tools"] {
            assert_eq!(count.body[field], completion.body[field], "{field}");
        }
        assert!(
            count.body.get("prompt_cache_key").is_none(),
            "count endpoint omits non-prompt cache metadata"
        );
        assert!(completion.body["prompt_cache_key"].is_string());
        assert_eq!(completion.body["instructions"], instructions);
        assert_eq!(completion.body["input"].as_array().unwrap().len(), 2);
        assert!(
            !completion
                .body
                .to_string()
                .contains("NEVER_DISCLOSE_THIS_BODY")
        );
        assert!(
            !completion.body["tools"]
                .to_string()
                .contains("Commit matching metadata")
        );
        for (index, directive) in [body, &revocation].into_iter().enumerate() {
            assert_eq!(
                completion.body["input"][index]["role"],
                if developer_messages {
                    "developer"
                } else {
                    "user"
                }
            );
            assert_eq!(
                completion.body["input"][index]["content"][0]["text"],
                directive.text
            );
        }
    }
}

#[tokio::test]
async fn unsupported_developer_role_does_not_silently_retry_as_user() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = receive_http_json(&mut stream).await;
        assert_eq!(request.body["input"][0]["role"], "developer");
        send_http_response(&mut stream, "400 Bad Request", "application/json", json!({"error":{"type":"invalid_request_error", "message":"developer role unsupported"}}).to_string()).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "role-changing retry is forbidden"
        );
    });
    let mut provider = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let instruction = test_skill("Pinned body");
    let request = ModelRequest {
        instructions: test_instructions(),
        input: vec![ModelRequestItem::DeveloperInstruction(instruction)],
        model_role: ModelRole::Build,
        allowed_tool_names: Some(&[]),
    };
    assert!(provider.complete(request, discard_updates()).await.is_err());
    assert!(provider.compatibility.developer_messages);
    server.await.unwrap();
}
