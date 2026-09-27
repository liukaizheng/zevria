//! A real engine/tool/JSONL/new-router resume, exercised in both feature builds.
use super::*;
use zevria_instructions::skill::{SkillDefinition, SkillSource};

pub(super) async fn flow() -> Vec<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut requests = Vec::<String>::new();
        let mut prefix = Vec::<Value>::new();
        let mut previous_output = Vec::new();
        for index in 0..4 {
            if index == 3 {
                let (stream, _) = listener.accept().await.unwrap();
                socket = accept_async(stream).await.unwrap();
            }
            let text = socket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .to_string();
            let request: Value = serde_json::from_str(&text).unwrap();
            if index == 0 {
                prefix = request["input"].as_array().unwrap().clone();
            } else {
                prefix.extend(previous_output);
                let input = request["input"].as_array().unwrap();
                if index == 3 {
                    assert_eq!(
                        prefix.len(),
                        46,
                        "44 previous request items + two native outputs"
                    );
                    assert!(input.starts_with(&prefix));
                    assert_eq!(input.len(), 47);
                    assert_eq!(input[46]["content"][0]["text"], "ok");
                    assert!(request.get("previous_response_id").is_none());
                    prefix = input.clone();
                } else {
                    assert_eq!(
                        request["previous_response_id"],
                        format!("resp_resume_{}", index - 1)
                    );
                    prefix.extend(input.iter().cloned());
                }
                let first: Value = serde_json::from_str(&requests[0]).unwrap();
                assert_eq!(
                    wire_request_properties(&request),
                    wire_request_properties(&first)
                );
            }
            if index >= 1 {
                let positions = prefix
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        item["content"][0]["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("Skill directive:"))
                    })
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                assert_eq!(
                    positions,
                    vec![40],
                    "activation directive cannot move on resume"
                );
            }
            let output = match index {
                0 => vec![function_call(
                    "fc_skill",
                    "call_skill",
                    "skill",
                    json!({"skill":"review"}),
                )],
                1 => vec![
                    json!({"type":"reasoning", "id":"rs_tool", "summary":[], "encrypted_content":"PRIVATE_ENCRYPTED_TOOL"}),
                    function_call("fc_count", "call_count", "count_once", json!({})),
                ],
                _ => vec![
                    json!({"type":"reasoning", "id":format!("rs_final_{index}"), "summary":[], "encrypted_content":"PRIVATE_ENCRYPTED_FINAL", "future":{"array":[2,1]}}),
                    json!({"type":"message", "id":format!("msg_final_{index}"), "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":"PRIVATE_FINAL_TEXT"}]}),
                ],
            };
            previous_output = output.clone();
            let mut event = completed_output_event(&format!("resp_resume_{index}"), output);
            event["response"]["usage"] = json!({"input_tokens":if index == 3 {15979} else {15820}, "input_tokens_details":{"cached_tokens":if index == 3 {0} else {14848}}, "output_tokens":100, "total_tokens":16079});
            send_json(&mut socket, event).await;
            requests.push(text);
        }
        requests
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let tools = ToolServer::new()
        .tool(directive_wire::ApplySkill)
        .tool(CountTool {
            executions: executions.clone(),
        })
        .run();
    let catalog = Arc::new(
        SkillCatalog::new([SkillDefinition::new(
            "review".parse().unwrap(),
            "Review",
            "PRIVATE_SKILL_BODY",
            SkillSource::Programmatic("fixture".into()),
        )
        .unwrap()])
        .unwrap(),
    );
    let policies = SessionPolicies::new(
        TurnPolicy::new(
            "Stable Build",
            Some(vec!["skill".into(), "count_once".into()]),
            ModelRole::Build,
            true,
        ),
        TurnPolicy::new("Plan", Some(vec![]), ModelRole::Plan, false),
    );
    let directory = tempfile::tempdir().unwrap();
    let mut writer =
        TranscriptWriter::create_with_id(directory.path(), "fixed-resume-session").unwrap();
    let path = writer.path().to_path_buf();
    writer
        .append(&TranscriptItem::SessionModels(
            zevria_model::models::SessionModels::new(
                zevria_model::models::ModelSelection::new(
                    test_profile_ref(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                zevria_model::models::ModelSelection::new(
                    test_profile_ref(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
            )
            .unwrap(),
        ))
        .unwrap();
    // A fixed, content-free synthetic scale matching the reported item counts.
    for i in 0..37 {
        writer
            .append(&TranscriptItem::Message(if i % 2 == 0 {
                Message::user(format!("PRIVATE_SEED_{i}"))
            } else {
                Message::assistant(format!("PRIVATE_SEED_{i}"))
            }))
            .unwrap();
    }
    let mut profile = resolved_profile(
        "test-provider",
        "gpt-test",
        url,
        "PRIVATE_CREDENTIAL",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        100_000,
    );
    profile.endpoint.input_token_count.enabled = false;
    profile.endpoint.session_id_header = Some("X-Session-ID".into());
    let compaction = zevria_model::CompactionPolicy::new(
        Default::default(),
        std::array::from_fn(|_| profile.context_policy()),
    )
    .unwrap();
    let router = || {
        let router = ResponsesRouter::from_routes(
            [(
                ModelRole::Build,
                profile.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            )],
            "Stable application",
            tools.clone(),
            "fixed-resume-session",
        )
        .unwrap();
        #[cfg(feature = "cache-diagnostics")]
        let router = router.with_cache_diagnostics(crate::CacheDiagnosticContext::new(
            &path,
            "fixed-resume-session",
        ));
        router
    };
    let mut engine = SessionEngine::new(
        router(),
        tools.clone(),
        policies.clone(),
        writer,
        catalog.clone(),
    )
    .unwrap()
    .with_compaction_policy(compaction.clone());
    submit(&mut engine, "activate review and run the tool").await;
    let persisted = transcript::load(&path).unwrap();
    let directive = persisted
        .iter()
        .position(|item| matches!(item, TranscriptItem::Directive(_)))
        .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    drop(engine);
    let mut engine = SessionEngine::new(
        router(),
        tools,
        policies,
        TranscriptWriter::append_to(path.clone()).unwrap(),
        catalog,
    )
    .unwrap()
    .with_transcript_items(transcript::load(&path).unwrap())
    .unwrap()
    .with_compaction_policy(compaction);
    assert_eq!(engine.conversation().items(), persisted);
    submit(&mut engine, "ok").await;
    assert_eq!(
        engine.conversation().items()[directive],
        persisted[directive]
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "resume cannot re-execute completed tools"
    );
    #[cfg(all(feature = "cache-diagnostics", unix))]
    {
        let files = std::fs::read_dir(directory.path().join(".cache-diagnostics"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            files.len(),
            1,
            "one latest snapshot for this session/profile"
        );
        let snapshot = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(!snapshot.contains("PRIVATE_") && !snapshot.contains("fixed-resume-session"));
    }
    server.await.unwrap()
}
async fn submit(engine: &mut SessionEngine<ResponsesRouter>, text: &str) {
    let (events, mut receiver) = session_event_channel(256);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.handle_command(
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: text.into(),
                mode: SessionMode::Build,
            }),
            &events,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let mut completed = false;
    while let Ok(update) = receiver.try_recv() {
        if let SessionUpdate::Lifecycle(event) = update {
            assert!(
                !matches!(
                    event,
                    SessionEvent::TurnFailed { .. } | SessionEvent::TurnRejected { .. }
                ),
                "{event:?}"
            );
            completed |= matches!(event, SessionEvent::TurnCompleted { .. });
        }
    }
    assert!(completed);
}
#[tokio::test]
async fn skill_tool_native_prefix_survives_jsonl_and_new_router() {
    flow().await;
}
