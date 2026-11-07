use super::*;

#[tokio::test]
async fn failed_confirmation_keeps_a_durable_draft_that_can_be_resubmitted_without_text() {
    let text = valid_plan_markdown("Recoverable stored plan submission", "Do not regenerate.");
    let provider = ScriptedProvider::new([
        Ok(batch(vec![create_plan_call(&text), submit_plan_call()])),
        Err(anyhow::anyhow!("confirmation provider failed")),
        Ok(submit_plan_call()),
        Ok(Message::assistant("Submitted.")),
    ]);
    let (_directory, writer) = test_transcript();
    let path = writer.path().to_owned();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    prompt(&mut engine, &events, "draft").await;
    assert_eq!(
        engine
            .plan_state()
            .unwrap()
            .working_copy()
            .unwrap()
            .markdown(),
        text
    );
    let durable = zevria_transcript::transcript::load(&path).unwrap();
    assert!(
        durable
            .iter()
            .any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::DraftSaved { .. })))
    );
    assert!(
        !durable
            .iter()
            .any(|item| matches!(item, TranscriptItem::Plan(PlanRecord::Ready { .. })))
    );
    assert!(
        collect_events(&mut receiver)
            .await
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnFailed { .. }))
    );
    prompt(&mut engine, &events, "resubmit").await;
    assert_eq!(
        engine.plan_state().unwrap().artifact().unwrap().markdown,
        text
    );
    assert_eq!(
        engine
            .conversation()
            .items()
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Plan(PlanRecord::DraftSaved { .. })))
            .count(),
        1
    );
}
#[tokio::test]
async fn revision_edits_restart_sequence_and_unchanged_resubmission_is_allowed() {
    for changed in [false, true] {
        let text = valid_plan_markdown("Revision based plan drafting", "Original.");
        let provider = ScriptedProvider::new([
            Ok(batch(vec![create_plan_call(&text), submit_plan_call()])),
            Ok(Message::assistant("Ready.")),
            Ok(if changed {
                batch(vec![
                    update_plan_call("Original.", "Revised."),
                    submit_plan_call(),
                ])
            } else {
                submit_plan_call()
            }),
            Ok(Message::assistant("Ready again.")),
        ]);
        let (_directory, writer) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            draft_tools(),
            plan_submission_policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap();
        let (events, _receiver) = session_event_channel(128);
        prompt(&mut engine, &events, "draft").await;
        let version = engine.plan_state().unwrap().version().unwrap();
        engine
            .handle_command(
                SessionCommand::Turn(TurnCommand::ResolvePlan {
                    expected: version,
                    decision: PlanDecision::Revise,
                }),
                &events,
            )
            .await
            .unwrap();
        assert!(matches!(
            engine.plan_state().unwrap().working_copy(),
            Some(zevria_workflow::PlanWorkingCopy::Submitted(_))
        ));
        prompt(&mut engine, &events, "revise").await;
        let artifact = engine.plan_state().unwrap().artifact().unwrap();
        assert_eq!(
            artifact.version,
            PlanVersion {
                revision: 2,
                ..version
            }
        );
        assert_eq!(
            artifact.markdown,
            if changed {
                text.replace("Original.", "Revised.")
            } else {
                text.clone()
            }
        );
        if changed {
            let last = engine
                .conversation()
                .items()
                .iter()
                .filter_map(|item| match item {
                    TranscriptItem::Plan(PlanRecord::DraftSaved { draft }) => Some(draft),
                    _ => None,
                })
                .last()
                .unwrap();
            assert_eq!((last.target.revision, last.sequence), (2, 1));
        }
    }
}
#[tokio::test]
async fn registered_plan_tools_fail_cleanly_in_build_even_under_an_unrestricted_policy() {
    let text = valid_plan_markdown("Scope gated plan authoring", "No Build drafts.");
    let provider = ScriptedProvider::new([
        Ok(batch(vec![
            create_plan_call(&text),
            update_plan_call("No Build drafts.", "No."),
            Message::Assistant(rig_core::message::AssistantMessage::new(vec![
                named_tool_call("read", READ_PLAN_TOOL_NAME, json!({})),
            ])),
            submit_plan_call(),
        ])),
        Ok(Message::assistant("No Plan state changed.")),
    ]);
    let policies = SessionPolicies::new(
        TurnPolicy::new("unrestricted Build", None, ModelRole::Build, false),
        plan_submission_policies().policy(SessionMode::Plan).clone(),
    );
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        policies,
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    let (events, mut receiver) = session_event_channel(128);
    engine
        .handle_command(
            SessionCommand::Turn(TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "build".into(),
                mode: SessionMode::Build,
            }),
            &events,
        )
        .await
        .unwrap();
    assert_eq!(engine.plan_state().unwrap(), &PlanWorkflowState::Idle);
    let lifecycle = collect_events(&mut receiver).await;
    let results = lifecycle
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolResults { metadata, .. } => Some(metadata),
            _ => None,
        })
        .unwrap();
    assert!(results.iter().all(|result| !result.outcome.is_success()));
}
#[tokio::test]
async fn draft_mirror_warnings_are_deduplicated_until_the_next_successful_write() {
    let text = valid_plan_markdown("Stable draft mirror warnings", "One.");
    let provider = ScriptedProvider::new([
        Ok(create_plan_call(&text)),
        Ok(Message::assistant("Draft.")),
        Ok(update_plan_call("One.", "Two.")),
        Ok(Message::assistant("Draft.")),
        Ok(update_plan_call("Two.", "Three.")),
        Ok(Message::assistant("Draft.")),
        Ok(update_plan_call("Three.", "Four.")),
        Ok(Message::assistant("Draft.")),
    ]);
    let (_directory, writer) = test_transcript();
    let workspace = tempfile::tempdir().unwrap();
    let plans = workspace.path().join("blocked-plans");
    std::fs::write(&plans, "obstruction").unwrap();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_plans_dir(plans.clone());
    let (events, mut receiver) = session_event_channel(128);
    for (index, expected_warnings) in [1, 0, 0, 1].into_iter().enumerate() {
        if index == 2 {
            std::fs::remove_file(&plans).unwrap();
        }
        if index == 3 {
            std::fs::remove_dir_all(&plans).unwrap();
            std::fs::write(&plans, "obstruction again").unwrap();
        }
        prompt(&mut engine, &events, "keep drafting").await;
        let lifecycle = collect_events(&mut receiver).await;
        let warnings = lifecycle
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    SessionEvent::PlanProjectionWarning { draft: Some(_), .. }
                )
            })
            .count();
        assert_eq!(warnings, expected_warnings, "save {}", index + 1);
    }
}
#[tokio::test]
async fn edited_prefix_compaction_preserves_its_working_copy_not_the_discarded_tail() {
    let first = valid_plan_markdown("Retained prefix draft context", "Retained goal.");
    let second = first.replace("Retained goal.", "Discarded goal.");
    let id = PlanId::new();
    let items = vec![
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::Plan(PlanRecord::Started { id }),
        saved_plan_record(id, &first),
        TranscriptItem::Message(Message::user("editable later prompt")),
        TranscriptItem::Plan(PlanRecord::DraftSaved {
            draft: PlanDraft {
                target: PlanVersion { id, revision: 1 },
                sequence: 2,
                markdown: second,
                source_turn_id: TurnId::new(2),
            },
        }),
    ];
    let provider = ScriptedProvider::new([Ok(Message::assistant("prefix summary"))]);
    let (_directory, writer) = test_transcript();
    let mut engine = SessionEngine::new(
        provider,
        draft_tools(),
        plan_submission_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap()
    .with_fixture(items)
    .unwrap();
    let index = engine
        .conversation()
        .items()
        .iter()
        .position(|item| item.message() == Some(&Message::user("editable later prompt")))
        .unwrap();
    let expected = replay_plan_state(engine.conversation().items()[..index].iter().filter_map(
        |item| match item {
            TranscriptItem::Plan(record) => Some(record),
            _ => None,
        },
    ))
    .unwrap()
    .checkpoint_context()
    .unwrap();
    let (events, _receiver) = session_event_channel(128);
    let turn = TurnContext::new(TurnId::new(3), SessionMode::Plan, CancellationToken::new());
    let placement = engine
        .automatic_checkpoint_for_prompt(
            TurnAnchor::ReplaceFrom(index),
            &plan_submission_policies().policy(SessionMode::Plan).clone(),
            true,
            &events,
            &turn,
        )
        .await
        .unwrap()
        .unwrap();
    let CheckpointPlacement::FoldedIntoCommit(checkpoint) = placement else {
        panic!("prefix checkpoint")
    };
    assert_eq!(
        checkpoint
            .replacement_history
            .last()
            .unwrap()
            .as_borrowed()
            .message_ref(),
        Some(&Message::user(expected))
    );
    assert!(
        engine
            .plan_state()
            .unwrap()
            .working_copy()
            .unwrap()
            .markdown()
            .contains("Discarded goal.")
    );
}

#[tokio::test]
async fn checkpoints_preserve_ready_and_published_but_not_idle_or_resolved_plans() {
    for phase in ["idle", "ready", "published", "resolved"] {
        let markdown = valid_plan_markdown(
            "Preserved submitted plan context",
            "Exact submitted bytes.  ",
        );
        let id = PlanId::new();
        let artifact = PlanCandidate::from_markdown(markdown.clone(), MAX_PLAN_ARTIFACT_BYTES)
            .unwrap()
            .into_artifact(PlanVersion { id, revision: 1 }, TurnId::new(1));
        let mut items = vec![TranscriptItem::Message(Message::user("original request"))];
        if phase == "published" {
            items.extend(super::super::mode::published_history(&artifact));
        } else if phase != "idle" {
            items.push(TranscriptItem::Plan(PlanRecord::Started { id }));
            items.push(TranscriptItem::Plan(PlanRecord::Ready {
                artifact: artifact.clone(),
            }));
            if phase == "resolved" {
                items.push(TranscriptItem::Plan(PlanRecord::Resolved {
                    id,
                    artifact: Some(artifact),
                    resolution: PlanResolution::ImplementedCurrent,
                }));
            }
        }
        let provider = ScriptedProvider::new([Ok(Message::assistant("summary"))]);
        let (_directory, writer) = test_transcript();
        let mut engine = SessionEngine::new(
            provider,
            draft_tools(),
            plan_submission_policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_fixture(items)
        .unwrap();
        let (events, _receiver) = session_event_channel(128);
        let turn = TurnContext::new(TurnId::new(2), SessionMode::Plan, CancellationToken::new());
        engine
            .compact_context(
                CompactionTrigger::Manual,
                &plan_submission_policies().policy(SessionMode::Plan).clone(),
                &events,
                &turn,
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
        let expected = Message::user(format!(
            "{} (revision 1):\n\n{}",
            zevria_workflow::PLAN_ARTIFACT_CONTEXT_PREFIX,
            markdown
        ));
        assert_eq!(
            checkpoint
                .replacement_history
                .last()
                .unwrap()
                .as_borrowed()
                .message_ref()
                == Some(&expected),
            matches!(phase, "ready" | "published"),
            "{phase}"
        );
    }
}

#[cfg(all(unix, feature = "test-support"))]
mod failed_persistence {
    use super::*;
    use zevria_transcript::test_support::TranscriptRewriteBlocker;
    struct BlockedCreate {
        path: PathBuf,
        blocker: Arc<Mutex<Option<TranscriptRewriteBlocker>>>,
    }
    impl Tool for BlockedCreate {
        const NAME: &'static str = CREATE_PLAN_TOOL_NAME;
        type Args = CreatePlanStubArgs;
        type Error = zevria_workflow::PlanValidationError;
        type Output = String;
        fn description(&self) -> String {
            "save with blocked persistence".into()
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type":"object"})
        }
        async fn call(
            &self,
            context: &mut ToolContext,
            args: Self::Args,
        ) -> Result<String, Self::Error> {
            let output = CreatePlanStubTool.call(context, args).await?;
            *self.blocker.lock().unwrap() =
                Some(TranscriptRewriteBlocker::new(&self.path).unwrap());
            Ok(output)
        }
    }
    #[tokio::test]
    async fn a_failed_batch_rewrite_never_projects_or_splits_results_from_drafts() {
        let text = valid_plan_markdown("Atomic durable draft storage", "All or none.");
        let provider = ScriptedProvider::new([Ok(create_plan_call(&text))]);
        let (_directory, writer) = test_transcript();
        let workspace = tempfile::tempdir().unwrap();
        let plans = workspace.path().join("plans");
        let blocker = Arc::new(Mutex::new(None));
        let tools = ToolServer::new()
            .tool(BlockedCreate {
                path: writer.path().to_owned(),
                blocker: blocker.clone(),
            })
            .run();
        let mut engine = SessionEngine::new(
            provider,
            tools,
            plan_submission_policies(),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_plans_dir(plans.clone());
        let (events, mut receiver) = session_event_channel(128);
        prompt(&mut engine, &events, "draft").await;
        assert!(!plans.exists());
        let mut blocker = blocker.lock().unwrap().take().unwrap();
        let durable = zevria_transcript::transcript::load(blocker.backup_path()).unwrap();
        assert!(!durable.iter().any(|item| matches!(
            item,
            TranscriptItem::ToolResults { .. }
                | TranscriptItem::Plan(PlanRecord::DraftSaved { .. })
        )));
        assert!(
            !collect_events(&mut receiver)
                .await
                .iter()
                .any(|event| matches!(event, SessionEvent::ToolResults { .. }))
        );
        blocker.restore().unwrap();
    }
}
