//! Synchronous admission: exact records, coherent anchors, and replay work.
use super::*;
use zevria_instructions::DirectivePayload;
use zevria_transcript::InstructionReplayState;

fn engine() -> (tempfile::TempDir, SessionEngine<ScriptedProvider>) {
    let (directory, writer) = test_transcript();
    let engine = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        test_policies(),
        writer,
        test_skills(),
    )
    .unwrap();
    (directory, engine)
}

fn skill(name: &str) -> PromptTurnInput {
    PromptTurnInput::Skill {
        name: name.parse().unwrap(),
        args: "apply here".into(),
    }
}

fn owned_input(items: &[TranscriptItem]) -> Vec<OwnedModelRequestItem> {
    zevria_transcript::model_input(items)
        .into_iter()
        .map(|item| item.to_owned_item().unwrap())
        .collect()
}

/// Test-only reference for the former two-phase, source-based preparation.
/// Intentionally replays/clones history to independently check exact ordering.
fn assert_equivalent(
    engine: &SessionEngine<ScriptedProvider>,
    anchor: TurnAnchor,
    mode: SessionMode,
    input: impl Fn() -> PromptTurnInput,
    plan_transition: Option<PlanRecord>,
) -> PreparedPrompt {
    let before = engine.conversation.items().to_vec();
    let bytes = std::fs::read(engine.conversation.path()).unwrap();
    let before_pins = engine.active_skills().unwrap().clone();
    let before_directives = engine.directive_state().unwrap().clone();
    let before_plan = engine.plan_state().unwrap().clone();
    let admitted = engine.prepare_test_prompt(anchor, input(), mode).unwrap();
    let source = match anchor {
        TurnAnchor::Append => engine.conversation.items(),
        TurnAnchor::ReplaceFrom(index) => &engine.conversation.items()[..index],
    };
    let pins = replay_active_skills(source).unwrap();
    let policy = engine.policies.policy(mode);
    let mut expected = engine.instruction_updates(source, policy, &pins).unwrap();
    // Keep the one fresh request identity and invocation, but independently
    // derive both reconciliations from source replay rather than admission state.
    expected.extend(
        admitted
            .records
            .iter()
            .filter(|item| !matches!(item, TranscriptItem::Directive(_)))
            .cloned(),
    );
    if matches!(admitted.kind, PromptTurnKind::Skill) {
        let mut prospective = source.to_vec();
        prospective.extend(expected.clone());
        expected.extend(
            engine
                .instruction_updates(
                    &prospective,
                    policy,
                    &replay_active_skills(&prospective).unwrap(),
                )
                .unwrap(),
        );
    }
    assert_eq!(admitted.records, expected);
    assert_eq!(admitted.plan_transition, plan_transition);
    let mut actual_full = source.to_vec();
    actual_full.extend(admitted.records.clone());
    let mut expected_full = source.to_vec();
    expected_full.extend(expected);
    assert_eq!(owned_input(&actual_full), owned_input(&expected_full));
    assert!(owned_input(&actual_full).starts_with(&owned_input(source)));
    InstructionReplayState::replay(&actual_full).unwrap();
    assert_eq!(engine.conversation.items(), before);
    assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), bytes);
    assert_eq!(engine.active_skills().unwrap(), &before_pins);
    assert_eq!(engine.directive_state().unwrap(), &before_directives);
    assert_eq!(engine.plan_state().unwrap(), &before_plan);
    admitted
}

#[tokio::test]
async fn admission_records_match_source_replay_for_message_activation_reapplication_and_edit() {
    let (_dir, mut engine) = engine();
    let initial = assert_equivalent(
        &engine,
        TurnAnchor::Append,
        SessionMode::Build,
        || PromptTurnInput::Message {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "initial".into(),
        },
        None,
    );
    engine.record_required_items(initial.records).unwrap();
    let first = assert_equivalent(
        &engine,
        TurnAnchor::Append,
        SessionMode::Build,
        || skill("commit"),
        None,
    );
    assert!(matches!(
        first.records[0],
        TranscriptItem::SkillInvocation(_)
    ));
    assert_eq!(first.records.len(), 2);
    assert!(matches!(&first.records[1], TranscriptItem::Directive(d)
        if matches!(d.payload, DirectivePayload::SkillBody { .. })));
    engine.record_required_items(first.records).unwrap();
    let again = assert_equivalent(
        &engine,
        TurnAnchor::Append,
        SessionMode::Build,
        || skill("commit"),
        None,
    );
    assert_eq!(
        again.records.len(),
        1,
        "no duplicate directives on reapplication"
    );
    engine.record_required_items(again.records).unwrap();
    let first_use = engine
        .conversation
        .items()
        .iter()
        .position(|item| matches!(item, TranscriptItem::SkillInvocation(_)))
        .unwrap();
    assert_equivalent(
        &engine,
        TurnAnchor::ReplaceFrom(first_use),
        SessionMode::Build,
        || skill("commit"),
        None,
    );
    assert_equivalent(
        &engine,
        TurnAnchor::ReplaceFrom(first_use),
        SessionMode::Build,
        || PromptTurnInput::Message {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: "replace activation".into(),
        },
        None,
    );
}

#[tokio::test]
async fn edits_reuse_only_the_replaced_first_use_not_discarded_future_pins() {
    let (_dir, mut engine) = engine();
    let first = engine
        .prepare_test_prompt(TurnAnchor::Append, skill("commit"), SessionMode::Build)
        .unwrap();
    engine.record_required_items(first.records).unwrap();
    let later = engine
        .prepare_test_prompt(TurnAnchor::Append, skill("review"), SessionMode::Build)
        .unwrap();
    engine.record_required_items(later.records).unwrap();
    let old_commit = engine
        .active_skills()
        .unwrap()
        .get(&"commit".parse().unwrap())
        .unwrap()
        .clone();
    let index = engine
        .conversation
        .items()
        .iter()
        .position(|item| matches!(item, TranscriptItem::SkillInvocation(_)))
        .unwrap();
    let catalog = SkillCatalog::new(["commit", "review"].map(|name| {
        zevria_instructions::SkillDefinition::new(
            name.parse().unwrap(),
            "Changed metadata",
            "New installed body",
            zevria_instructions::SkillSource::Programmatic("changed".into()),
        )
        .unwrap()
    }))
    .unwrap();
    let new_review = catalog.get("review").unwrap().snapshot();
    engine = engine.with_skill_catalog(Arc::new(catalog)).unwrap();
    for name in ["commit", "review"] {
        let admitted = assert_equivalent(
            &engine,
            TurnAnchor::ReplaceFrom(index),
            SessionMode::Build,
            || skill(name),
            None,
        );
        let prefix = InstructionReplayState::replay(&engine.conversation.items()[..index]).unwrap();
        assert!(prefix.skills().is_empty());
        let prospective = prefix.apply_suffix(&admitted.records).unwrap();
        assert_eq!(prospective.skills().len(), 1);
        assert_eq!(
            prospective.skills().get(&name.parse().unwrap()).unwrap(),
            if name == "commit" {
                &old_commit
            } else {
                &new_review
            }
        );
    }
}

#[tokio::test]
async fn ready_revision_preserves_exact_records_and_checks_before_staging() {
    let (_dir, mut engine) = engine();
    let (artifact, items) = ready_plan_fixture();
    engine.record_required_items(items).unwrap();
    let policy = engine.policies.policy_mut(SessionMode::Plan);
    policy.skills_enabled = true;
    policy.allowed_tool_names = Some(vec!["command".into(), "skill".into()]);
    let input = || PromptTurnInput::RevisionSkill {
        expected: artifact.version,
        name: "review".parse().unwrap(),
        args: "revise".into(),
    };
    let admitted = assert_equivalent(
        &engine,
        TurnAnchor::Append,
        SessionMode::Plan,
        input,
        Some(PlanRecord::RevisionRequested {
            artifact: artifact.clone(),
        }),
    );
    assert!(
        admitted
            .records
            .iter()
            .any(|item| matches!(item, TranscriptItem::SkillInvocation(_)))
    );
    let before = engine.conversation.items().to_vec();
    for (anchor, mode) in [
        (TurnAnchor::ReplaceFrom(0), SessionMode::Plan),
        (TurnAnchor::Append, SessionMode::Build),
    ] {
        assert!(matches!(
            engine.prepare_test_prompt(anchor, input(), mode),
            Err(Rejection::Rejected(_))
        ));
    }
    let mut stale = artifact.version;
    stale.revision += 1;
    assert!(matches!(
        engine.prepare_test_prompt(
            TurnAnchor::Append,
            PromptTurnInput::RevisionSkill {
                expected: stale,
                name: "review".parse().unwrap(),
                args: "revise".into(),
            },
            SessionMode::Plan
        ),
        Err(Rejection::Rejected(_))
    ));
    assert_eq!(engine.conversation.items(), before);
    assert!(engine.active_skills().unwrap().is_empty());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn admission_replays_zero_append_history_once_for_edits_and_only_the_proposed_suffix() {
    use zevria_transcript::replay_probe::measure_instruction_replay;
    for (count, bytes) in [(4, 128), (1000, 8192)] {
        for reapply in [false, true] {
            let (_dir, mut engine) = engine();
            if reapply {
                let first = engine
                    .prepare_test_prompt(TurnAnchor::Append, skill("commit"), SessionMode::Build)
                    .unwrap();
                engine.record_required_items(first.records).unwrap();
            }
            let policy = engine.policies.policy(SessionMode::Build).clone();
            engine.reconcile_before_dispatch(&policy).unwrap();
            engine
                .record_required_items(
                    (0..count)
                        .map(|i| {
                            TranscriptItem::Message(if i % 2 == 0 {
                                Message::user("x".repeat(bytes))
                            } else {
                                Message::assistant("x".repeat(bytes))
                            })
                        })
                        .collect(),
                )
                .unwrap();
            let index = engine.conversation.items().len() - 2;
            for anchor in [TurnAnchor::Append, TurnAnchor::ReplaceFrom(index)] {
                for is_skill in [false, true] {
                    let input = if is_skill {
                        skill("commit")
                    } else {
                        PromptTurnInput::Message {
                            behavior: zevria_foundation::RequestBehavior::Standard,
                            text: "next".into(),
                        }
                    };
                    let plan_before = PROMPT_PLAN_PREFIX_REPLAYS.get();
                    let (result, counts) = measure_instruction_replay(|| {
                        engine.prepare_test_prompt(anchor, input, SessionMode::Build)
                    });
                    let admitted = result.unwrap();
                    let edit = usize::from(matches!(anchor, TurnAnchor::ReplaceFrom(_)));
                    assert_eq!(counts.full_replays, edit);
                    assert_eq!(counts.full_records, edit * index);
                    assert_eq!(counts.pin_replays, 0);
                    assert_eq!(counts.pin_records, 0);
                    assert_eq!(counts.suffix_applications, 1);
                    assert_eq!(counts.suffix_records, admitted.records.len());
                    assert_eq!(PROMPT_PLAN_PREFIX_REPLAYS.get() - plan_before, edit);
                }
            }
            let captured = engine.prompt_anchor_state(TurnAnchor::Append).unwrap();
            let SessionReplayState::Valid { instructions, .. } = &engine.replay else {
                panic!()
            };
            assert!(std::ptr::eq(
                captured.instructions.as_ref(),
                instructions.as_ref()
            ));
        }
    }
}

#[tokio::test]
async fn admission_keeps_pending_tool_context_and_does_not_leak_rejected_pins() {
    let (_dir, mut engine) = engine();
    let policy = engine.policies.policy(SessionMode::Build).clone();
    engine.reconcile_before_dispatch(&policy).unwrap();
    engine
        .record_required(TranscriptItem::Message(Message::Assistant {
            id: None,
            content: vec![named_tool_call("pending", "command", json!({}))],
        }))
        .unwrap();
    let before = engine.conversation.items().to_vec();
    let bytes = std::fs::read(engine.conversation.path()).unwrap();
    let Err(Rejection::Rejected(error)) =
        engine.prepare_test_prompt(TurnAnchor::Append, skill("commit"), SessionMode::Build)
    else {
        panic!("directives must not split the cached pending batch");
    };
    assert!(error.contains("tool call/result batch"));
    assert!(engine.active_skills().unwrap().is_empty());
    assert_eq!(engine.conversation.items(), before);
    assert_eq!(std::fs::read(engine.conversation.path()).unwrap(), bytes);
    engine
        .record_required(TranscriptItem::ToolResults {
            message: Message::tool_result("pending", "command", "done"),
            metadata: vec![],
            skill_applications: vec![],
        })
        .unwrap();
    engine
        .prepare_test_prompt(TurnAnchor::Append, skill("commit"), SessionMode::Build)
        .unwrap();
    assert!(
        engine.active_skills().unwrap().is_empty(),
        "successful staging also stays local"
    );
}

#[tokio::test]
async fn second_prompt_after_metadata_bearing_results_keeps_the_committed_prefix() {
    for activates_skill in [false, true] {
        let (_dir, mut engine) = engine();
        let first = engine
            .prepare_test_prompt(
                TurnAnchor::Append,
                PromptTurnInput::Message {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "first ordinary prompt".into(),
                },
                SessionMode::Build,
            )
            .unwrap();
        engine.record_required_items(first.records).unwrap();
        let assistant = Message::Assistant {
            id: None,
            content: vec![named_tool_call(
                "correlated",
                if activates_skill { "skill" } else { "command" },
                if activates_skill {
                    json!({"skill":"commit"})
                } else {
                    json!({})
                },
            )],
        };
        let result = if activates_skill {
            let policy = engine.policies.policy(SessionMode::Build).clone();
            let turn =
                TurnContext::new(TurnId::new(1), SessionMode::Build, CancellationToken::new());
            let gate = PlanSubmissionGate::inert(None);
            let scope = ToolExecutionScope::new(&engine, SessionMode::Build, &policy, &turn, &gate);
            let batch = execute_tool_calls(
                &scope,
                &assistant_tool_calls(&assistant),
                ActiveSkills::default(),
                &gate,
            )
            .await;
            assert_eq!(batch.metadata.len(), 1);
            assert_eq!(
                batch.skill_applications.len(),
                1,
                "metadata and skill applications can coexist"
            );
            TranscriptItem::ToolResults {
                message: batch.message,
                metadata: batch.metadata,
                skill_applications: batch.skill_applications,
            }
        } else {
            TranscriptItem::ToolResults {
                message: Message::tool_result("correlated", "command", "stable model result"),
                metadata: vec![ToolResultMetadata {
                    diagnostic: None,
                    id: "correlated".into(),
                    call_id: None,
                    tool_name: "command".into(),
                    outcome: ToolCallOutcome::Success,
                    detail: None,
                }],
                skill_applications: vec![],
            }
        };
        engine
            .record_required_items(vec![TranscriptItem::Message(assistant), result])
            .unwrap();
        let before_directives = owned_input(engine.conversation.items());
        let mut without_metadata = engine.conversation.items().to_vec();
        for item in &mut without_metadata {
            if let TranscriptItem::ToolResults { metadata, .. } = item {
                metadata.clear();
            }
        }
        assert_eq!(
            owned_input(&without_metadata),
            before_directives,
            "metadata is not model input"
        );
        let policy = engine.policies.policy(SessionMode::Build).clone();
        engine.reconcile_before_dispatch(&policy).unwrap();
        let after_directives = owned_input(engine.conversation.items());
        assert!(after_directives.starts_with(&before_directives));
        if activates_skill {
            assert!(
                after_directives.len() > before_directives.len(),
                "new skill directives append, never rewrite"
            );
        }
        engine
            .record_required(TranscriptItem::Message(Message::assistant(
                "final assistant response",
            )))
            .unwrap();
        let completed = owned_input(engine.conversation.items());
        let second = assert_equivalent(
            &engine,
            TurnAnchor::Append,
            SessionMode::Build,
            || PromptTurnInput::Message {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "second ordinary prompt".into(),
            },
            None,
        );
        engine.record_required_items(second.records).unwrap();
        let next = owned_input(engine.conversation.items());
        assert!(next.starts_with(&completed));
        assert_eq!(next.len(), completed.len() + 1);
        let loaded = zevria_transcript::transcript::load(engine.conversation.path()).unwrap();
        assert_eq!(loaded, engine.conversation.items());
        assert_eq!(
            owned_input(&loaded),
            owned_input(engine.conversation.items()),
            "model records and ordered directives stay exact after loading"
        );
    }
}
