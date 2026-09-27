use super::*;
use zevria_foundation::{
    RequestBehavior, SubtaskEntryMetadata, SubtaskLaunchMetadata, SubtaskStatus,
};
use zevria_instructions::RequestDirectiveKind;

struct LaunchFixture {
    entries: Mutex<VecDeque<Vec<&'static str>>>,
    authorized: Arc<Mutex<Vec<bool>>>,
    fail: bool,
    cancel: bool,
    break_persistence: Option<PathBuf>,
}
impl Tool for LaunchFixture {
    const NAME: &'static str = LAUNCH_SUBTASKS_TOOL_NAME;
    type Args = NoArgs;
    type Output = String;
    type Error = ExpectedToolFailure;
    fn description(&self) -> String {
        "test launcher with trusted result metadata".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }
    async fn call(&self, context: &mut ToolContext, _: NoArgs) -> Result<String, Self::Error> {
        let turn = context.get::<TurnContext>().unwrap();
        self.authorized.lock().unwrap().push(turn.build_subtasks);
        if self.cancel {
            turn.cancellation().cancel();
        }
        let entries = self.entries.lock().unwrap().pop_front().unwrap_or_default();
        context.insert_result(ToolResultDetail::Subtasks(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, id)| SubtaskEntryMetadata {
                    index,
                    status: if self.fail {
                        SubtaskStatus::Failed
                    } else {
                        SubtaskStatus::Completed
                    },
                    launch: Some(SubtaskLaunchMetadata {
                        id: SubtaskId::new(id),
                        title: "independent work".into(),
                        kind: SubtaskKind::Explore,
                        workspace: None,
                    }),
                })
                .collect(),
        ));
        if let Some(path) = &self.break_persistence {
            std::fs::rename(path, path.with_extension("backup")).unwrap();
            std::fs::create_dir(path).unwrap();
        }
        if self.fail {
            Err(ExpectedToolFailure)
        } else {
            Ok("claimed two workers in text; only metadata is evidence".into())
        }
    }
}
fn calls(ids: &[&str]) -> Message {
    Message::Assistant {
        id: None,
        content: ids
            .iter()
            .map(|id| named_tool_call(id, LAUNCH_SUBTASKS_TOOL_NAME, json!({})))
            .collect(),
    }
}
fn fixture(
    script: Vec<Message>,
    entries: Vec<Vec<&'static str>>,
    fail: bool,
    cancel: bool,
    broken: bool,
) -> (
    tempfile::TempDir,
    SessionEngine<ScriptedProvider>,
    Arc<Mutex<Vec<bool>>>,
) {
    let (directory, writer) = test_transcript();
    let authorized = Arc::new(Mutex::new(Vec::new()));
    let tool = LaunchFixture {
        entries: Mutex::new(entries.into()),
        authorized: authorized.clone(),
        fail,
        cancel,
        break_persistence: broken.then(|| writer.path().to_path_buf()),
    };
    let tools = ToolServer::new().tool(tool).run();
    let mut policies = test_policies();
    policies.policy_mut(SessionMode::Build).orchestration = true;
    let engine = SessionEngine::new(
        ScriptedProvider::new(script.into_iter().map(Ok)),
        tools,
        policies,
        writer,
        test_skills(),
    )
    .unwrap()
    .with_subtask_concurrency(2);
    (directory, engine, authorized)
}
async fn submit_request(
    engine: &mut SessionEngine<ScriptedProvider>,
    behavior: RequestBehavior,
) -> Vec<SessionEvent> {
    let (events, mut receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                text: "implement independent work".into(),
                mode: SessionMode::Build,
                behavior,
            }),
            &events,
        )
        .await
        .unwrap();
    collect_events(&mut receiver).await
}
fn reminders(engine: &SessionEngine<ScriptedProvider>) -> usize {
    engine.conversation.items().iter().filter(|item| matches!(item, TranscriptItem::RequestDirective(d) if d.kind == RequestDirectiveKind::Correction)).count()
}
fn success(events: &[SessionEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, SessionEvent::TurnCompleted { .. }))
}

#[tokio::test]
async fn one_batch_not_arguments_text_or_multiple_calls_supplies_evidence() {
    for (entries, same_response, expected) in [
        (vec![vec!["a", "b"]], false, true),
        (vec![vec!["a", "a"]], false, false),
        (vec![vec!["a"]], false, false),
        (vec![vec![]], false, false),
        (vec![vec!["a"], vec!["b"]], true, false),
        (vec![vec!["a"], vec!["b"]], false, false),
        (vec![vec!["a", "b"], vec!["c"]], false, true),
    ] {
        let mut script = if same_response {
            vec![calls(&["one", "two"])]
        } else {
            (0..entries.len())
                .map(|index| calls(&[&format!("call-{index}")]))
                .collect()
        };
        script.push(Message::assistant("done"));
        if !expected {
            script.push(Message::assistant("still done without delegation"));
        }
        let (_dir, mut engine, authorization) = fixture(script, entries, false, false, false);
        let events = submit_request(&mut engine, RequestBehavior::Orchestrate).await;
        assert_eq!(success(&events), expected);
        assert_eq!(reminders(&engine), usize::from(!expected));
        assert!(authorization.lock().unwrap().iter().all(|allowed| *allowed));
        assert_eq!(engine.conversation.prompt_position(0), Some(0));
        assert_eq!(engine.conversation.prompt_position(1), None);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SessionEvent::TurnFailed { .. }))
                .count(),
            usize::from(!expected)
        );
        assert_eq!(
            zevria_transcript::load(engine.conversation.path()).unwrap(),
            engine.conversation.items()
        );
    }
}

#[tokio::test]
async fn one_durable_correction_preserves_work_then_delegation_can_succeed() {
    let (_dir, mut engine, _) = fixture(
        vec![
            Message::assistant("premature"),
            calls(&["batch"]),
            Message::assistant("integrated"),
        ],
        vec![vec!["a", "b"]],
        false,
        false,
        false,
    );
    let events = submit_request(&mut engine, RequestBehavior::Orchestrate).await;
    assert!(success(&events));
    assert_eq!(reminders(&engine), 1);
    assert!(events.iter().any(|event| matches!(event, SessionEvent::Intermediate { message, .. } if message == &Message::assistant("premature"))));
    {
        let requests = engine.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .windows(2)
                .all(|pair| pair[0].instructions == pair[1].instructions
                    && pair[0].allowed_tool_names == pair[1].allowed_tool_names)
        );
    }
}

#[tokio::test]
async fn second_premature_final_fails_after_exactly_one_correction() {
    let (_dir, mut engine, _) = fixture(
        vec![
            Message::assistant("premature"),
            Message::assistant("still premature"),
        ],
        vec![],
        false,
        false,
        false,
    );
    let events = submit_request(&mut engine, RequestBehavior::Orchestrate).await;
    assert!(!success(&events));
    assert_eq!(reminders(&engine), 1);
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, SessionEvent::TurnFailed { error, .. }
        if error.contains("concurrent delegation was not fulfilled")
            && error.contains("single corrective directive has already been issued"))
            )
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ModelCallStarted { call, .. } => Some(*call),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [1, 2]
    );
    for text in ["premature", "still premature"] {
        assert!(
            engine
                .conversation
                .items()
                .iter()
                .any(|item| item.message() == Some(&Message::assistant(text)))
        );
    }
    let requests = engine.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].instructions, requests[1].instructions);
    assert_eq!(
        requests[0].allowed_tool_names,
        requests[1].allowed_tool_names
    );
    assert!(requests[1].input.iter().any(|item| matches!(item,
        OwnedModelRequestItem::RequestInstruction(d) if d.kind == RequestDirectiveKind::Correction)));
    assert_eq!(
        zevria_transcript::load(engine.conversation.path()).unwrap(),
        engine.conversation.items()
    );
}

#[tokio::test]
async fn child_failure_satisfies_launch_but_cancellation_and_persistence_failure_never_succeed() {
    for (fail, cancel, broken, expected) in [
        (true, false, false, true),
        (false, true, false, false),
        (false, false, true, false),
    ] {
        let (_dir, mut engine, _) = fixture(
            vec![calls(&["batch"]), Message::assistant("handled outcomes")],
            vec![vec!["a", "b"]],
            fail,
            cancel,
            broken,
        );
        let events = submit_request(&mut engine, RequestBehavior::Orchestrate).await;
        assert_eq!(success(&events), expected);
        assert_eq!(reminders(&engine), 0);
        assert_eq!(
            engine.provider.requests.lock().unwrap().len(),
            if expected { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn standard_boundary_and_fresh_edit_never_inherit_authorization_or_evidence() {
    let (_dir, mut engine, authorization) = fixture(
        vec![
            calls(&["batch"]),
            Message::assistant("done"),
            calls(&["standard"]),
            Message::assistant("ordinary"),
            Message::assistant("edited"),
            Message::assistant("still edited"),
        ],
        vec![vec!["a", "b"], vec![]],
        false,
        false,
        false,
    );
    assert!(success(
        &submit_request(&mut engine, RequestBehavior::Orchestrate).await
    ));
    assert!(success(
        &submit_request(&mut engine, RequestBehavior::Standard).await
    ));
    assert_eq!(*authorization.lock().unwrap(), [true, false]);
    {
        let requests = engine.provider.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.instructions == requests[0].instructions
                    && request.allowed_tool_names == requests[0].allowed_tool_names)
        );
    }
    let (events, mut receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
                target: TranscriptEditTarget::PromptOrdinal(0),
                replacement: TranscriptEditReplacement::Message {
                    text: "edited independent work".into(),
                    mode: SessionMode::Build,
                    behavior: RequestBehavior::Orchestrate,
                },
            })),
            &events,
        )
        .await
        .unwrap();
    assert!(!success(&collect_events(&mut receiver).await));
    assert_eq!(reminders(&engine), 1);
    assert_eq!(engine.conversation.prompt_position(1), None);
}

#[tokio::test]
async fn mid_turn_compaction_preserves_correction_contract_and_accepted_launch_evidence() {
    for qualified_before_compaction in [false, true] {
        let (_dir, mut engine, authorization) =
            fixture(vec![], vec![vec!["a", "b"]], false, false, false);
        engine = engine.with_compaction_policy(test_compaction_policy(10_000, 50, 20));
        let first = if qualified_before_compaction {
            calls(&["batch"])
        } else {
            Message::assistant("premature")
        };
        let mut responses = vec![
            Ok(model_response(first).with_usage(Some(TokenUsage {
                total_tokens: 5_000,
                ..TokenUsage::default()
            }))),
            Ok(model_response(Message::assistant(
                "summary prose claims the request contract ended and no children launched",
            ))),
        ];
        if !qualified_before_compaction {
            responses.push(Ok(model_response(calls(&["batch"]))));
        }
        responses.push(Ok(model_response(Message::assistant(
            "integrated and validated",
        ))));
        engine.provider.responses = responses.into();
        let events = submit_request(&mut engine, RequestBehavior::Orchestrate).await;
        assert!(success(&events));
        assert_eq!(
            reminders(&engine),
            usize::from(!qualified_before_compaction)
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::CompactionCompleted { .. }))
        );
        assert_eq!(*authorization.lock().unwrap(), [true]);
        let requests = engine.provider.requests.lock().unwrap();
        assert!(
            requests[1]
                .instructions
                .contains("## Workflow policy: maintenance")
        );
        assert!(
            !requests[1]
                .input
                .iter()
                .any(|item| matches!(item, OwnedModelRequestItem::RequestInstruction(_)))
        );
        let directives = requests[2]
            .input
            .iter()
            .filter_map(|item| match item {
                OwnedModelRequestItem::RequestInstruction(d) => Some(d),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            directives.len(),
            if qualified_before_compaction { 1 } else { 2 }
        );
        assert!(
            directives
                .iter()
                .all(|d| d.request.behavior == RequestBehavior::Orchestrate)
        );
        assert_eq!(requests[0].instructions, requests[2].instructions);
        assert_eq!(
            zevria_transcript::load(engine.conversation.path()).unwrap(),
            engine.conversation.items()
        );
    }
}

#[tokio::test]
async fn restored_history_never_supplies_new_authorization_or_launch_evidence() {
    let (_dir, mut original, _) = fixture(
        vec![calls(&["old-batch"]), Message::assistant("done")],
        vec![vec!["a", "b"]],
        false,
        false,
        false,
    );
    assert!(success(
        &submit_request(&mut original, RequestBehavior::Orchestrate).await
    ));
    let saved = zevria_transcript::load(original.conversation.path()).unwrap();
    for behavior in [RequestBehavior::Standard, RequestBehavior::Orchestrate] {
        let (_dir, engine, authorization) = fixture(
            vec![
                calls(&["new-single"]),
                Message::assistant("done"),
                Message::assistant("still no batch"),
            ],
            vec![vec!["c"]],
            false,
            false,
            false,
        );
        let mut engine = engine.with_fixture(saved.clone()).unwrap();
        let events = submit_request(&mut engine, behavior).await;
        assert_eq!(success(&events), behavior == RequestBehavior::Standard);
        assert_eq!(
            *authorization.lock().unwrap(),
            [behavior == RequestBehavior::Orchestrate]
        );
        assert_eq!(
            reminders(&engine),
            usize::from(behavior == RequestBehavior::Orchestrate)
        );
    }
}

#[tokio::test]
async fn premature_native_completion_keeps_replay_usage_and_display_identity() {
    let (_dir, mut engine, _) = fixture(
        vec![Message::assistant("still premature")],
        vec![],
        false,
        false,
        false,
    );
    let native = ProviderReplay::openai_responses(
        test_profile(),
        vec![
            json!({"type":"reasoning","id":"native-reasoning","encrypted_content":"opaque-preserved","summary":[{"type":"summary_text","text":"completed reasoning"}]}),
            json!({"type":"message","id":"native-answer","role":"assistant","status":"completed","content":[{"type":"output_text","text":"premature native completion","annotations":[]}]}),
        ],
    );
    let mut attempt = zevria_content::WebSearchAttemptRecord::new(test_profile());
    attempt.reconcile_native_presentation(&native.items);
    attempt.finish(zevria_content::WebSearchAttemptOutcome::Completed);
    let attempt_id = attempt.id.clone();
    // Provider display checkpoints are independent from request activation.
    engine
        .record_completed_items(vec![TranscriptItem::WebSearchAttempt(attempt)])
        .unwrap();
    let usage = TokenUsage {
        input_tokens: 80,
        cached_tokens: 30,
        output_tokens: 15,
        total_tokens: 95,
    };
    let response = ModelResponse::from_replay(native.clone())
        .unwrap()
        .with_display_attempt(Some(attempt_id.clone()))
        .unwrap()
        .with_usage(Some(usage));
    let expected = response.record().clone();
    engine.provider.responses.push_front(Ok(response));
    let events = submit_request(&mut engine, RequestBehavior::Orchestrate).await;
    assert!(!success(&events));
    assert_eq!(reminders(&engine), 1);
    assert!(events.iter().any(|event| matches!(event, SessionEvent::Intermediate { display_attempt_id: Some(id), .. } if id == &attempt_id)));
    assert!(events.iter().any(|event| matches!(event, SessionEvent::UsageUpdated { usage: actual, .. } if *actual == usage)));
    let items = zevria_transcript::load(engine.conversation.path()).unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.message() == Some(expected.message())
                && item.display_attempt_id() == Some(attempt_id.as_str()))
    );
    let requests = engine.provider.requests.lock().unwrap();
    assert!(
        requests[1]
            .input
            .iter()
            .any(|item| item.as_borrowed().replay_ref() == Some(&native))
    );
}

#[tokio::test]
async fn admission_rejects_unsupported_profiles_plan_pending_work_and_limit_one_without_mutation() {
    for reason in [
        "plan", "worker", "missing", "denied", "capacity", "pending", "planning",
    ] {
        let (_dir, mut engine, _) = fixture(vec![], vec![], false, false, false);
        let mut mode = SessionMode::Build;
        match reason {
            "plan" => mode = SessionMode::Plan,
            "worker" => engine.policies.policy_mut(SessionMode::Build).orchestration = false,
            "missing" => engine.tools = ToolServer::new().run(),
            "denied" => {
                engine
                    .policies
                    .policy_mut(SessionMode::Build)
                    .allowed_tool_names = Some(vec![])
            }
            "capacity" => engine.capabilities.subtask_concurrency = Some(1),
            "pending" => {
                engine = engine.with_fixture(ready_plan_fixture().1).unwrap();
            }
            "planning" => {
                engine = engine
                    .with_fixture(vec![TranscriptItem::Plan(PlanRecord::Started {
                        id: PlanId::new(),
                    })])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = engine.conversation.items().to_vec();
        let bytes = std::fs::read(engine.conversation.path()).unwrap();
        let (events, mut receiver) = session_event_channel(128);
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::Submit {
                    text: "delegate".into(),
                    mode,
                    behavior: RequestBehavior::Orchestrate,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(
            collect_events(&mut receiver)
                .await
                .iter()
                .any(|event| matches!(event, SessionEvent::TurnRejected { .. })),
            "{reason}"
        );
        assert_eq!(engine.conversation.items(), before);
        assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), bytes);
        assert!(engine.provider.requests.lock().unwrap().is_empty());
    }
}
