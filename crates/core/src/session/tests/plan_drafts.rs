use super::*;

#[path = "plan_draft_lifecycle.rs"]
mod lifecycle;
#[path = "plan_draft_validation.rs"]
mod validation;

fn draft_tools() -> ToolServerHandle {
    ToolServer::new()
        .tool(CreatePlanStubTool)
        .tool(UpdatePlanStubTool)
        .tool(ReadPlanStubTool)
        .tool(SubmitPlanStubTool)
        .run()
}
async fn prompt<P: ModelProvider>(
    engine: &mut SessionEngine<P>,
    events: &SessionEventSender,
    text: &str,
) {
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: text.into(),
                mode: SessionMode::Plan,
            }),
            events,
        )
        .await
        .unwrap();
}
fn batch(messages: Vec<Message>) -> Message {
    Message::Assistant(rig_core::message::AssistantMessage::new(
        messages
            .iter()
            .flat_map(|message| assistant_tool_calls(message).into_iter())
            .enumerate()
            .map(|(index, mut call)| {
                call.id = rig_core::message::CallId::from_wire(format!("{}-{index}", call.id));
                AssistantContent::ToolCall(call)
            })
            .collect(),
    ))
}
#[tokio::test]
async fn drafts_survive_turns_restart_and_submit_only_at_finalization() {
    let text = valid_plan_markdown("Iterative stored plan draft", "First goal.");
    let provider = ScriptedProvider::new([
        Ok(create_plan_call(&text)),
        Ok(Message::assistant("Still investigating.")),
    ]);
    let (directory, transcript) = test_transcript();
    let transcript_path = transcript.path().to_owned();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    prompt(&mut engine, &events, "plan it").await;
    let draft = match engine.plan_state().unwrap() {
        PlanWorkflowState::Planning {
            draft: Some(draft), ..
        } => draft.clone(),
        state => panic!("not a draft: {state:?}"),
    };
    assert_eq!(draft.sequence, 1);
    let items = engine.conversation().items().to_vec();
    let saved = items
        .iter()
        .position(|item| matches!(item, TranscriptItem::Plan(PlanRecord::DraftSaved { .. })))
        .unwrap();
    assert!(matches!(
        items[saved - 1],
        TranscriptItem::ToolResults { .. }
    ));
    let lifecycle = collect_events(&mut receiver).await;
    let results = lifecycle
        .iter()
        .position(|event| matches!(event, SessionEvent::ToolResults { .. }))
        .unwrap();
    assert!(matches!(
        lifecycle[results + 1],
        SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Planning { draft: Some(_), .. }
        }
    ));
    assert!(!lifecycle.iter().any(|event| matches!(
        event,
        SessionEvent::PlanStateChanged {
            state: PlanWorkflowState::Ready { .. }
        }
    )));
    drop(engine);
    let provider = ScriptedProvider::new([
        Ok(batch(vec![
            update_plan_call("First goal.", "Final goal."),
            submit_plan_call(),
        ])),
        Ok(Message::assistant("Submitted.")),
    ]);
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        TranscriptWriter::append_to(transcript_path.clone()).unwrap(),
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_transcript_items(items)
    .unwrap();
    prompt(&mut engine, &events, "finish the plan").await;
    assert!(
        matches!(engine.plan_state().unwrap(), PlanWorkflowState::Ready { artifact } if artifact.markdown == text.replace("First goal.", "Final goal."))
    );
    let loaded = zevria_transcript::transcript::load(&transcript_path).unwrap();
    assert_eq!(
        replay_plan_state(loaded.iter().filter_map(|item| match item {
            TranscriptItem::Plan(record) => Some(record),
            _ => None,
        }))
        .unwrap(),
        *engine.plan_state().unwrap()
    );
    drop(directory);
}
#[tokio::test]
async fn serialized_draft_batches_are_atomic_and_lock_after_submission() {
    for fail in [true, false] {
        let text = valid_plan_markdown("Atomic iterative plan draft", "Original.");
        let provider = ScriptedProvider::new([
            Ok(create_plan_call(&text)),
            Ok(batch(if fail {
                vec![update_plan_call("not present", "No."), submit_plan_call()]
            } else {
                vec![
                    update_plan_call("Original.", "Updated."),
                    submit_plan_call(),
                    update_plan_call("Updated.", "Locked."),
                    submit_plan_call(),
                ]
            })),
            Ok(Message::assistant("Finished.")),
        ]);
        let (_directory, transcript) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            draft_tools(),
            plan_submission_policies(),
            transcript,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let (events, mut receiver) = session_event_channel(128);
        prompt(&mut engine, &events, "draft").await;
        let lifecycle = collect_events(&mut receiver).await;
        let results = lifecycle
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ToolResults { metadata, .. } => Some(metadata),
                _ => None,
            })
            .last()
            .unwrap();
        let outcomes = results.iter().map(|meta| meta.outcome).collect::<Vec<_>>();
        if fail {
            assert_eq!(outcomes, [ToolCallOutcome::Error, ToolCallOutcome::Denied]);
            assert_eq!(
                engine
                    .plan_state()
                    .unwrap()
                    .working_copy()
                    .unwrap()
                    .markdown(),
                text
            );
        } else {
            assert_eq!(
                outcomes,
                [
                    ToolCallOutcome::Success,
                    ToolCallOutcome::Success,
                    ToolCallOutcome::Denied,
                    ToolCallOutcome::Denied
                ]
            );
            assert_eq!(
                engine.plan_state().unwrap().artifact().unwrap().markdown,
                text.replace("Original.", "Updated.")
            );
        }
    }
}
#[tokio::test]
async fn draft_requests_extend_the_same_cacheable_prefix() {
    let text = valid_plan_markdown("Stable plan authoring prefix", "Original.");
    let provider = ScriptedProvider::new([
        Ok(create_plan_call(&text)),
        Ok(update_plan_call("Original.", "Updated.")),
        Ok(Message::Assistant(
            rig_core::message::AssistantMessage::new(vec![named_tool_call(
                "read-plan",
                READ_PLAN_TOOL_NAME,
                json!({}),
            )]),
        )),
        Ok(submit_plan_call()),
        Ok(Message::assistant("Submitted.")),
    ]);
    let requests = provider.requests.clone();
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, _receiver) = session_event_channel(128);
    prompt(&mut engine, &events, "plan").await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    for pair in requests.windows(2) {
        assert_eq!(pair[0].instructions, pair[1].instructions);
        assert_eq!(pair[0].allowed_tool_names, pair[1].allowed_tool_names);
        assert!(pair[1].input.starts_with(&pair[0].input));
    }
}
#[tokio::test]
async fn latest_draft_is_mirrored_once_per_batch_with_a_stable_first_title() {
    let text = valid_plan_markdown("First durable plan title", "Original.");
    let provider = ScriptedProvider::new([
        Ok(batch(vec![
            create_plan_call(&text),
            update_plan_call("First durable plan title", "Changed durable plan title"),
        ])),
        Ok(Message::assistant("Still drafting.")),
    ]);
    let (_directory, transcript) = test_transcript();
    let workspace = tempfile::tempdir().unwrap();
    let plans = workspace.path().join("plans");
    let session = transcript.session_id().to_owned();
    let transcript_path = transcript.path().to_owned();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_plans_dir(plans.clone());
    let (events, _receiver) = session_event_channel(128);
    prompt(&mut engine, &events, "draft").await;
    let copy = engine.plan_state().unwrap().working_copy().unwrap();
    assert!(copy.markdown().contains("Changed durable plan title"));
    let id = engine
        .plan_state()
        .unwrap()
        .draft_scope(TurnId::new(1))
        .unwrap()
        .target
        .id;
    let path = plans
        .join(session)
        .join(format!("{id}-first-durable-plan-title.md"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{}\n", copy.markdown())
    );
    std::fs::write(&path, "manual edits are not input").unwrap();
    let items = engine.conversation().items().to_vec();
    drop(engine);
    let provider =
        ScriptedProvider::new([Ok(submit_plan_call()), Ok(Message::assistant("Submitted."))]);
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        TranscriptWriter::append_to(transcript_path).unwrap(),
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_transcript_items(items)
    .unwrap()
    .with_plans_dir(plans);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "manual edits are not input",
        "restore never repairs the mirror"
    );
    prompt(&mut engine, &events, "submit").await;
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        format!(
            "{}\n",
            engine.plan_state().unwrap().artifact().unwrap().markdown
        )
    );
}
#[tokio::test]
async fn compaction_appends_exact_plan_working_copy_after_summary() {
    let text = valid_plan_markdown("Preserved iterative plan draft", "Exact UTF-8: 你好.\r\n");
    let id = PlanId::new();
    let items = vec![
        TranscriptItem::Message(Message::user("plan")),
        TranscriptItem::Plan(PlanRecord::Started { id }),
        saved_plan_record(id, &text),
    ];
    let provider = ScriptedProvider::new([Ok(Message::assistant("summary"))]);
    let (_directory, transcript) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_fixture(items)
    .unwrap();
    let expected = engine.plan_state().unwrap().checkpoint_context().unwrap();
    let (events, _receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Compact {
                mode: SessionMode::Plan,
            }),
            &events,
        )
        .await
        .unwrap();
    let checkpoint = engine
        .conversation()
        .items()
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Compaction(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .unwrap();
    assert!(format!("{checkpoint:?}").contains("Engine-preserved Plan working copy"));
    let projected = snapshot_model_input(engine.conversation().model_input()).unwrap();
    let expected = Message::user(expected);
    assert!(
        projected
            .iter()
            .any(|item| item.as_borrowed().message_ref() == Some(&expected))
    );
}
