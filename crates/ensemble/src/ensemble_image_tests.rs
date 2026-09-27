#[tokio::test]
async fn external_image_capability_gates_transport_without_text_only_downgrades() {
    for supported in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let script = directory.path().join("image_agent.py");
        let trace = directory.path().join("prompt.json");
        std::fs::write(&script, r#"import json, os, sys
def send(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method, rid = request.get('method'), request.get('id')
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':1,'agentCapabilities':{'promptCapabilities':{'image':os.environ['IMAGES']=='true'}}}})
    elif method == 'session/new':
        send({'jsonrpc':'2.0','id':rid,'result':{'sessionId':'image-session','modes':{'currentModeId':'read-only','availableModes':[{'id':'read-only','name':'Read only'}]}}})
    elif method == 'session/set_mode':
        send({'jsonrpc':'2.0','id':rid,'result':{}})
    elif method == 'session/prompt':
        prompt = request['params']['prompt']
        with open(os.environ['TRACE'], 'w') as file:
            json.dump(prompt, file)
        print(json.dumps(prompt), file=sys.stderr, flush=True)
        for block in prompt:
            send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'image-session','update':{'sessionUpdate':'user_message_chunk','content':block,'messageId':'image-echo'}}})
        send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'image-session','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'Image review complete'},'messageId':'report'}}})
        send({'jsonrpc':'2.0','id':rid,'result':{'stopReason':'end_turn'}})
"#).unwrap();
        let logs = directory.path().join("logs");
        let supervisor = EnsembleSupervisor::new(
            single_agent_config(fake_stdio_agent(
                &script,
                BTreeMap::from([
                    ("IMAGES".into(), supported.to_string()),
                    ("TRACE".into(), trace.display().to_string()),
                ]),
            )),
            &workspace,
            logs.clone(),
            test_questions(),
        )
        .unwrap();
        let image = zevria_content::PromptImage::from_rgba(1, 1, &[9, 8, 7, 255]).unwrap();
        let prompt = zevria_content::UserPrompt::new(vec![
            zevria_content::PromptBlock::Text("before ".into()),
            zevria_content::PromptBlock::Image(image.clone()),
            zevria_content::PromptBlock::Text(" after".into()),
        ])
        .unwrap();
        let start = zevria_workflow::EnsembleStart {
            run_id: EnsembleRunId::new(),
            workflow: EnsembleWorkflow::Review,
            prompt: prompt.clone(),
            agents: supervisor.workers(EnsembleWorkflow::Review).unwrap(),
        };
        let (events, _receiver) = session_event_channel(128);
        let outcomes = supervisor
            .observe_review_rounds(
                EnsembleLaunchRequest {
                    start: start.clone(),
                    resume: false,
                },
                events,
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new()),
            )
            .await
            .unwrap();
        if supported {
            assert_eq!(outcomes[0].status, AgentRunStatus::Completed);
            let blocks: Vec<ContentBlock> =
                serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
            let delivered = zevria_acp::prompt::from_content(&blocks).unwrap();
            assert_eq!(delivered.images().collect::<Vec<_>>(), vec![&image]);
            assert!(
                matches!(delivered.blocks(), [zevria_content::PromptBlock::Text(before), zevria_content::PromptBlock::Image(_), zevria_content::PromptBlock::Text(after)] if before.ends_with("before ") && after == " after")
            );
        } else {
            assert_eq!(outcomes[0].status, AgentRunStatus::Failed);
            assert!(outcomes[0].failure.as_deref().unwrap().contains("image"));
            assert!(
                !trace.exists(),
                "unsupported agent must receive no downgraded prompt"
            );
        }
        let records =
            load_agent_run(&agent_run_path(&logs, &start.run_id, &start.agents[0].id)).unwrap();
        assert!(
            matches!(&records[0], AgentRunTranscriptRecord::Header { header } if header.prompt == prompt)
        );
        for record in &records {
            if let AgentRunTranscriptRecord::Event {
                event: AgentRunEvent::Protocol { json, .. } | AgentRunEvent::Stderr { text: json },
            } = record
            {
                assert!(!json.contains(&image.base64()));
            }
        }
        if supported {
            assert!(records.iter().any(|record| matches!(record, AgentRunTranscriptRecord::Event { event: AgentRunEvent::UserImage { image: echoed, .. } } if echoed == &image)));
        }
    }
}

#[test]
fn normalized_unsupported_tool_image_content_is_payload_free() {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    let block = serde_json::json!({"type":"image", "mimeType":"image/png", "data":image.base64()});
    let update = acp_update(
        serde_json::json!({"sessionUpdate":"tool_call", "toolCallId":"image-output", "title":"Image output", "kind":"read", "status":"failed", "content":[{"type":"content", "content":block}], "rawInput":block, "rawOutput":block}),
    );
    let events = normalize_update(update);
    let diagnostic = serde_json::to_string(&events).unwrap();
    assert!(!diagnostic.contains(&image.base64()));
    assert!(diagnostic.contains("unsupported tool image output"));
}
