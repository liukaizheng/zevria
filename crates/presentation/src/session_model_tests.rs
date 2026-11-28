use crate::conversation::HistoryEntry;
use crate::session::OperationKind;
use crate::{ConversationTail, SessionModel, Settlement};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
};
use std::time::Instant;
use zevria_foundation::{RequestBehavior, SessionMode, TurnId};
use zevria_session_api::SessionStreamState;
use zevria_session_api::{
    ControlCommand, ManagementCommand, ModeSelectionResult, SessionEvent as E, TranscriptEdit,
    TranscriptEditReplacement, TranscriptEditTarget,
};
use zevria_session_api::{JobEvent, ManagementEvent, PlanEvent, TurnEvent};
use zevria_workflow::{
    PlanArtifact, PlanDecision, PlanHandoff, PlanId, PlanVersion, PlanWorkflowState,
};
const TURN: TurnId = TurnId::new(7);

fn job_notice(
    kind: zevria_foundation::CommandJobNoticeKind,
) -> zevria_foundation::CommandJobNotice {
    use zevria_foundation::*;
    CommandJobNotice::new(
        kind,
        vec![CommandJobSnapshot {
            id: CommandJobId::new("job-notice"),
            call_id: None,
            command: "test".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Exited {
                code: Some(1),
                success: false,
                signal: None,
            },
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
            stdout_bytes: 6,
            stderr_bytes: 0,
        }],
        vec![],
        "Command job update: full output for copy".into(),
    )
}

#[test]
fn job_continuation_admission_is_mode_tagged_quiescent_and_never_binds_to_submit() {
    use zevria_foundation::CommandJobNoticeKind::Continuation;
    let mut model = SessionModel::default();
    assert!(model.continue_after_jobs().is_none());
    model.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Plan),
    }));
    assert!(model.continue_after_jobs().is_none());
    model.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Build),
    }));
    assert!(matches!(
        model.continue_after_jobs(),
        Some(zevria_session_api::TurnCommand::ContinueAfterJobs {
            mode: SessionMode::Build
        })
    ));
    assert!(model.continue_after_jobs().is_none());
    let accepted = model.reduce(E::Job(JobEvent::ContinuationStarted {
        turn_id: TURN,
        mode: SessionMode::Build,
        notice: Box::new(job_notice(Continuation)),
    }));
    assert_eq!(accepted.accepted, Some(OperationKind::JobContinuation));
    model.reduce(E::Turn(TurnEvent::Completed {
        turn_id: TURN,
        message: Message::assistant("done"),
        attempt_id: None,
    }));
    model.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Build),
    }));
    assert!(model.continue_after_jobs().is_none());
    model.reduce(E::EngineIdle);
    assert!(model.continue_after_jobs().is_some());
    let rejected = model.reduce(E::Turn(TurnEvent::Rejected {
        turn_id: TurnId::new(8),
        error: "stale".into(),
    }));
    assert!(matches!(
        rejected.settled,
        Some((OperationKind::JobContinuation, Settlement::Rejected(_)))
    ));
    assert!(
        !model
            .conversation
            .history()
            .iter()
            .any(|row| matches!(row, HistoryEntry::Error(_)))
    );

    let mut submit = pending();
    let result = submit.reduce(E::Job(JobEvent::ContinuationStarted {
        turn_id: TURN,
        mode: SessionMode::Build,
        notice: Box::new(job_notice(Continuation)),
    }));
    assert_eq!(result.accepted, None);
    assert_eq!(submit.session.operation_kind(), Some(OperationKind::Submit));
    assert!(submit.conversation.history().is_empty());
    submit.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Build),
    }));
    assert!(submit.continue_after_jobs().is_none());
    let mut management = SessionModel::default();
    management.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Build),
    }));
    assert!(management.select_mode(SessionMode::Build).is_some());
    management.session.finish_mode_management();
    assert!(
        management.continue_after_jobs().is_some(),
        "unchanged eligibility does not require another advertisement after idle management"
    );
    let mut ready = SessionModel::default();
    ready.apply_plan_snapshot(PlanWorkflowState::Ready {
        artifact: artifact(),
    });
    ready.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Plan),
    }));
    assert!(ready.continue_after_jobs().is_none());
}

#[test]
fn job_notice_rows_restore_copy_full_text_and_do_not_enter_recall_or_prompt_ordinals() {
    use zevria_foundation::CommandJobNoticeKind::{Continuation, Update};
    use zevria_transcript::TranscriptItem;
    let mut model = SessionModel::default();
    let continuation = job_notice(Continuation);
    model.restore_transcript(vec![
        TranscriptItem::Message(Message::user("first")),
        TranscriptItem::CommandJobNotice(Box::new(continuation.clone())),
        TranscriptItem::CommandJobNotice(Box::new(job_notice(Update))),
        TranscriptItem::Message(Message::user("second")),
    ]);
    let rows = model.conversation.history();
    assert!(matches!(rows[1], HistoryEntry::JobNotice(_, Some(_))));
    assert!(matches!(rows[2], HistoryEntry::JobNotice(_, None)));
    let selection = crate::conversation::Selection {
        history_index: 1,
        content_index: 0,
    };
    assert_eq!(
        model.conversation.selected_plain_text(selection).as_deref(),
        Some(continuation.text())
    );
    assert!(model.conversation.recallable_selection(selection).is_none());
    let selection = crate::conversation::Selection {
        history_index: 3,
        content_index: 0,
    };
    assert!(matches!(
        model.conversation.recallable_selection(selection),
        Some((_, TranscriptEditTarget::PromptOrdinal(1)))
    ));
    model.conversation.commit_edit(0, false);
    assert!(model.conversation.history().is_empty());
    model.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Build),
    }));
    model.restore_transcript(vec![]);
    assert!(model.continue_after_jobs().is_none());
}

#[test]
fn quiescence_requires_a_post_terminal_barrier_and_restore_resets_it() {
    use crate::session::SessionActivity;
    let mut model = SessionModel::default();
    assert!(model.session.is_quiescent());
    assert!(!model.session.has_turn_work());
    assert!(model.select_mode(SessionMode::Build).is_some());
    assert!(model.session.is_quiescent());
    assert!(!model.session.has_turn_work());
    model.session.finish_mode_management();
    model
        .submit("hello".into(), RequestBehavior::Standard)
        .unwrap();
    assert!(!model.session.is_quiescent());
    assert!(model.session.has_turn_work());
    model.reduce(E::EngineIdle);
    assert!(!model.session.is_quiescent());
    start(&mut model);
    model.reduce(E::EngineIdle);
    assert!(!model.session.is_quiescent());
    model.reduce(E::Turn(TurnEvent::Completed {
        turn_id: TURN,
        message: Message::assistant("done"),
        attempt_id: None,
    }));
    assert!(!model.session.has_turn_work());
    assert!(!model.session.is_quiescent());
    model.reduce(E::EngineIdle);
    assert!(model.session.is_quiescent());
    start(&mut model);
    assert!(!model.session.is_quiescent());
    assert!(matches!(
        model.session.activity(),
        SessionActivity::ActiveTurn(_)
    ));
    model.session.reset_for_restore();
    assert!(model.session.is_quiescent());
    model.reduce(E::Turn(TurnEvent::CompactionStarted {
        turn_id: TURN,
        trigger: zevria_model::CompactionTrigger::Manual,
    }));
    assert!(!model.session.is_quiescent());
    model.session.reset_for_restore();
    assert!(model.session.is_quiescent());
}

#[test]
fn explicit_mode_admission_captures_mode_and_role_without_changing_selection() {
    use crate::session::SessionActivity;
    use zevria_foundation::ModelRole;
    use zevria_session_api::TurnCommand;
    for skill in [false, true] {
        let mut model = SessionModel::default();
        let command = if skill {
            model.invoke_skill_in_mode(
                zevria_instructions::SkillName::parse("review").unwrap(),
                "args".into(),
                SessionMode::Plan,
            )
        } else {
            model.submit_in_mode(
                "prompt".into(),
                RequestBehavior::Orchestrate,
                SessionMode::Plan,
            )
        }
        .unwrap();
        assert!(matches!(
            command,
            TurnCommand::Submit {
                mode: SessionMode::Plan,
                behavior: RequestBehavior::Orchestrate,
                ..
            } | TurnCommand::InvokeSkill {
                mode: SessionMode::Plan,
                ..
            }
        ));
        let SessionActivity::PendingTurn(pending) = model.session.activity() else {
            panic!("pending")
        };
        assert_eq!(
            (pending.mode, pending.role),
            (SessionMode::Plan, ModelRole::Plan)
        );
        assert_eq!(model.next_mode(), SessionMode::Build);
    }
    let mut model = SessionModel::default();
    model.apply_selected_mode(SessionMode::Plan);
    assert!(matches!(
        model.submit("hello".into(), RequestBehavior::Standard),
        Some(TurnCommand::Submit {
            mode: SessionMode::Plan,
            ..
        })
    ));
}
fn pending() -> SessionModel {
    let mut model = SessionModel::default();
    assert!(
        model
            .submit("hello".into(), RequestBehavior::Standard)
            .is_some()
    );
    model
}
fn start(model: &mut SessionModel) -> crate::Reduction {
    model.reduce(E::Turn(TurnEvent::Started {
        turn_id: TURN,
        message: Message::user("hello"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }))
}
fn artifact() -> PlanArtifact {
    PlanArtifact {
        version: PlanVersion {
            id: PlanId::new(),
            revision: 2,
        },
        title: "Plan".into(),
        markdown: "# Plan".into(),
        source_turn_id: TURN,
    }
}
#[test]
fn full_lifecycle_reports_hooks_once_and_freezes_headers() {
    let mut model = pending();
    assert_eq!(start(&mut model).accepted, Some(OperationKind::Submit));
    assert_eq!(start(&mut model).accepted, None);
    model.reduce(E::Turn(TurnEvent::ModelCallStarted {
        turn_id: TURN,
        call: 1,
    }));
    model.apply_stream(SessionStreamState::new(
        TURN,
        (Message::assistant("partial")).into(),
    ));
    assert!(matches!(
        model.tail(Instant::now()),
        ConversationTail::Streaming { .. }
    ));
    let reduced = model.reduce(E::Turn(TurnEvent::Completed {
        turn_id: TURN,
        message: Message::assistant("done"),
        attempt_id: None,
    }));
    assert_eq!(
        reduced.settled,
        Some((OperationKind::Submit, Settlement::Completed))
    );
    assert!(!model.is_busy());
    assert_eq!(model.conversation.history().len(), 2);
    assert!(
        model
            .reduce(E::Turn(TurnEvent::Cancelled { turn_id: TURN }))
            .settled
            .is_none()
    );
}
#[test]
fn stale_and_foreign_ids_are_inert() {
    let mut model = pending();
    start(&mut model);
    let foreign = TurnId::new(99);
    assert!(
        model
            .reduce(E::Turn(TurnEvent::Failed {
                turn_id: foreign,
                error: "foreign".into()
            }))
            .settled
            .is_none()
    );
    model.reduce(E::Turn(TurnEvent::Intermediate {
        turn_id: foreign,
        message: Message::assistant("foreign"),
        attempt_id: None,
    }));
    assert_eq!(model.conversation.history().len(), 1);
    assert!(model.is_busy());
}
#[test]
fn early_rejection_unlocks_and_restores_plan_dialog() {
    let mut model = SessionModel::default();
    let artifact = artifact();
    model.apply_plan_snapshot(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    model
        .resolve_plan(artifact.version, PlanDecision::ImplementCurrent)
        .unwrap();
    model.workflow.hide();
    // A hidden dialog is intentionally not reopened; a visible pending review is.
    model.workflow.open_review();
    let reduced = model.reduce(E::Turn(TurnEvent::Rejected {
        turn_id: TURN,
        error: "not admitted".into(),
    }));
    assert!(matches!(
        reduced.settled,
        Some((_, Settlement::Rejected(_)))
    ));
    assert!(!model.is_busy());
    assert!(model.workflow.dialog().is_some());
}
#[test]
fn ready_forces_plan_mode_even_if_restore_metadata_disagrees() {
    let mut model = SessionModel::default();
    let reduced = model.reduce(E::Plan(PlanEvent::StateChanged {
        state: Box::new(PlanWorkflowState::Ready {
            artifact: artifact(),
        }),
    }));
    assert!(reduced.plan_ready);
    model.apply_selected_mode(SessionMode::Build);
    assert_eq!(model.next_mode(), SessionMode::Plan);
    assert!(
        model
            .submit("blocked".into(), RequestBehavior::Standard)
            .is_none()
    );
}
#[test]
fn hidden_ready_admits_only_direct_versioned_revision_feedback() {
    use crate::PaneScope;
    let mut model = SessionModel::default();
    let artifact = artifact();
    model.apply_plan_snapshot(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    assert_eq!(model.ready_revision_target(), None);
    assert!(
        model
            .submit("feedback".into(), RequestBehavior::Standard)
            .is_none()
    );
    assert!(
        model
            .invoke_skill("review".parse().unwrap(), "feedback".into())
            .is_none()
    );
    model.workflow.hide();
    assert_eq!(model.ready_revision_target(), Some(artifact.version));
    assert!(
        model
            .submit(" ".into(), RequestBehavior::Standard)
            .is_none()
    );
    assert!(
        model
            .submit("feedback".into(), RequestBehavior::Orchestrate)
            .is_none()
    );
    for mode in SessionMode::ALL {
        assert!(
            model
                .submit_in_mode("queued".into(), RequestBehavior::Standard, mode)
                .is_none()
        );
        assert!(
            model
                .invoke_skill_in_mode("review".parse().unwrap(), "queued".into(), mode)
                .is_none()
        );
        assert!(model.select_mode(mode).is_none());
    }
    assert!(model.compact().is_none());
    assert!(
        model
            .run_ensemble(zevria_workflow::EnsembleWorkflow::Plan, "ensemble".into())
            .is_none()
    );
    assert!(
        model
            .edit_transcript(
                TranscriptEdit {
                    target: TranscriptEditTarget::PromptOrdinal(0),
                    replacement: TranscriptEditReplacement::Message {
                        text: "edit".into(),
                        mode: SessionMode::Plan,
                        behavior: RequestBehavior::Standard,
                        references: Vec::new(),
                    },
                },
                0
            )
            .is_none()
    );
    model.reduce(E::Job(JobEvent::ContinuationChanged {
        mode: Some(SessionMode::Plan),
    }));
    assert!(model.continue_after_jobs().is_none());
    model.set_scope(PaneScope::Inspect { role: None });
    assert_eq!(model.ready_revision_target(), None);
    assert!(
        model
            .submit("feedback".into(), RequestBehavior::Standard)
            .is_none()
    );
    assert!(
        model
            .invoke_skill("review".parse().unwrap(), "feedback".into())
            .is_none()
    );
    model.set_scope(PaneScope::Root);
    assert!(model.begin_operation(
        OperationKind::Submit,
        SessionMode::Plan,
        zevria_foundation::ModelRole::Plan
    ));
    assert_eq!(model.ready_revision_target(), None);
    assert!(
        model
            .submit("feedback".into(), RequestBehavior::Standard)
            .is_none()
    );
    assert!(
        model
            .invoke_skill("review".parse().unwrap(), "feedback".into())
            .is_none()
    );
}

#[test]
fn hidden_ready_revision_uses_prompt_operations_without_restoring_review() {
    use crate::session::SessionActivity;
    use zevria_foundation::ModelRole;
    use zevria_session_api::TurnCommand;
    for skill in [false, true] {
        for rejected in [false, true] {
            let mut model = SessionModel::default();
            let artifact = artifact();
            let ready = PlanWorkflowState::Ready {
                artifact: artifact.clone(),
            };
            model.apply_plan_snapshot(ready.clone());
            model.workflow.hide();
            let kind = if skill {
                OperationKind::Skill
            } else {
                OperationKind::Submit
            };
            let command = if skill {
                model
                    .invoke_skill("review".parse().unwrap(), "feedback".into())
                    .unwrap()
            } else {
                model
                    .submit("feedback".into(), RequestBehavior::Standard)
                    .unwrap()
            };
            if skill {
                assert!(
                    matches!(command, TurnCommand::RevisePlanWithSkill { expected, name, args }
                    if expected == artifact.version && name.as_str() == "review" && args.display_projection() == "feedback")
                );
            } else {
                assert!(
                    matches!(command, TurnCommand::RevisePlanWithMessage { expected, text }
                    if expected == artifact.version && text.display_projection() == "feedback")
                );
            }
            assert_eq!(model.session.operation_kind(), Some(kind));
            assert_eq!(model.pending_prompt().is_some(), !skill);
            let SessionActivity::PendingTurn(pending) = model.session.activity() else {
                panic!("pending")
            };
            assert_eq!(
                (pending.mode, pending.role),
                (SessionMode::Plan, ModelRole::Plan)
            );
            if rejected {
                let reduction = model.reduce(E::Turn(TurnEvent::Rejected {
                    turn_id: TURN,
                    error: "stale".into(),
                }));
                let (settled, settlement) = reduction.settled.unwrap();
                assert_eq!(settled, kind);
                assert!(settlement.restores_draft());
                assert_eq!(model.workflow.snapshot(), &ready);
                assert!(!model.is_busy());
            } else {
                let reduction = model.reduce(E::Turn(TurnEvent::Started {
                    turn_id: TURN,
                    message: Message::user("feedback"),
                    mode: SessionMode::Plan,
                    references: Vec::new(),
                }));
                assert_eq!(reduction.accepted, Some(kind));
                model.reduce(E::Plan(PlanEvent::StateChanged {
                    state: Box::new(PlanWorkflowState::Planning {
                        id: artifact.version.id,
                        previous: Some(artifact),
                        draft: None,
                    }),
                }));
                assert!(model.is_busy());
                assert_eq!(model.next_mode(), SessionMode::Plan);
                assert!(matches!(
                    model.workflow.snapshot(),
                    PlanWorkflowState::Planning {
                        previous: Some(_),
                        draft: None,
                        ..
                    }
                ));
            }
            assert!(model.workflow.dialog().is_none());
            assert!(model.pending_prompt().is_none());
        }
    }
}

#[test]
fn pending_append_survives_preturn_compaction_until_the_matching_turn_starts() {
    use zevria_model::{CompactionBackend, CompactionTrigger};
    let mut model = SessionModel::default();
    let trigger = CompactionTrigger::AutomaticPreTurn;
    model
        .submit("preview".into(), RequestBehavior::Standard)
        .unwrap();
    let pending_id = model.pending_prompt().unwrap().id();
    assert!(model.conversation.history().is_empty());
    assert!(model.conversation.turn_starts(false).is_empty());
    model.reduce(E::EngineIdle);
    model.reduce(E::Turn(TurnEvent::ModeChanged {
        mode: SessionMode::Build,
    }));
    let reduced = model.reduce(E::Turn(TurnEvent::CompactionStarted {
        turn_id: TURN,
        trigger,
    }));
    assert_eq!(reduced.accepted, None);
    model.reduce(E::Turn(TurnEvent::CompactionCompleted {
        turn_id: TURN,
        trigger,
        backend: CompactionBackend::LocalSummary,
    }));
    assert_eq!(model.pending_prompt().unwrap().id(), pending_id);
    assert!(matches!(
        model.conversation.history(),
        [HistoryEntry::CompactionDivider]
    ));
    let stale = TurnId::new(TURN.get() - 1);
    assert!(
        model
            .reduce(E::Turn(TurnEvent::Cancelled { turn_id: stale }))
            .settled
            .is_none()
    );
    assert!(
        model
            .reduce(E::Turn(TurnEvent::Started {
                turn_id: stale,
                message: Message::user("wrong"),
                mode: SessionMode::Build,
                references: Vec::new(),
            }))
            .accepted
            .is_none()
    );
    let reduced = model.reduce(E::Turn(TurnEvent::Started {
        turn_id: TURN,
        message: Message::user("canonical"),
        mode: SessionMode::Build,
        references: Vec::new(),
    }));
    assert_eq!(reduced.accepted, Some(OperationKind::Submit));
    assert!(model.pending_prompt().is_none());
    assert_eq!(model.conversation.turn_starts(false).len(), 1);
    model.reduce(E::Turn(TurnEvent::Failed {
        turn_id: TURN,
        error: "after acceptance".into(),
    }));
    let [
        HistoryEntry::CompactionDivider,
        HistoryEntry::Conversation(committed),
        HistoryEntry::Error(_),
    ] = model.conversation.history()
    else {
        panic!("committed prompt retained after failure");
    };
    assert_eq!(committed.blocks[0].primary_copy(), "canonical");
}

#[test]
fn manual_compaction_start_reports_acceptance() {
    let mut model = SessionModel::default();
    assert!(model.compact().is_some());
    let reduced = model.reduce(E::Turn(TurnEvent::CompactionStarted {
        turn_id: TURN,
        trigger: zevria_model::CompactionTrigger::Manual,
    }));
    assert_eq!(reduced.accepted, Some(OperationKind::ManualCompaction));
    assert!(model.session.is_compacting());
}

#[test]
fn stray_manual_compaction_does_not_accept_or_bind_a_pending_mcp_prompt() {
    let mut model = SessionModel::default();
    assert!(model.begin_mcp_prompt(SessionMode::Build));
    assert!(model.pending_prompt().is_none());

    let reduced = model.reduce(E::Turn(TurnEvent::CompactionStarted {
        turn_id: TurnId::new(TURN.get() + 1),
        trigger: zevria_model::CompactionTrigger::Manual,
    }));
    assert_eq!(reduced.accepted, None);
    assert_eq!(model.session.cancel_target(), Some(None));
    assert_eq!(start(&mut model).accepted, Some(OperationKind::Submit));
}

#[test]
fn plan_handoff_start_reports_acceptance() {
    let mut model = SessionModel::default();
    let artifact = artifact();
    let expected = artifact.version;
    model.apply_plan_snapshot(PlanWorkflowState::Ready {
        artifact: artifact.clone(),
    });
    assert!(
        model
            .resolve_plan(expected, PlanDecision::ImplementCurrent)
            .is_some()
    );
    let reduced = model.reduce(E::Plan(PlanEvent::HandoffStarted {
        turn_id: TURN,
        handoff: Box::new(PlanHandoff::new(artifact, "source")),
    }));
    assert_eq!(
        reduced.accepted,
        Some(OperationKind::PlanImplementCurrent { expected })
    );
}

#[test]
fn only_rejection_and_failure_recover_unaccepted_prompts() {
    for settlement in [
        Settlement::Rejected("stale".into()),
        Settlement::Failed("offline".into()),
    ] {
        assert!(settlement.restores_unaccepted_on_error());
    }
    for settlement in [
        Settlement::Completed,
        Settlement::Recovered,
        Settlement::Cancelled,
    ] {
        assert!(!settlement.restores_unaccepted_on_error());
    }
    assert!(Settlement::Cancelled.restores_draft());
}

#[test]
fn identical_submits_have_distinct_previews_and_commit_once_each() {
    use crate::blocks::PresentationBlockKind;
    use zevria_content::{PromptBlock, PromptImage, UserPrompt};
    let prompt = UserPrompt::new(vec![
        PromptBlock::Text("same\u{1b}[31m draft\u{202e}".into()),
        PromptBlock::Image(PromptImage::from_rgba(1, 1, &[0, 0, 0, 255]).unwrap()),
    ])
    .unwrap();
    let mut model = SessionModel::default();
    let mut previous = None;
    for id in 1..=2 {
        model
            .submit(prompt.clone(), RequestBehavior::Standard)
            .unwrap();
        let preview = model.pending_prompt().unwrap();
        assert_ne!(Some(preview.id()), previous);
        previous = Some(preview.id());
        let blocks = preview.blocks();
        assert!(
            matches!(&blocks[0].kind, PresentationBlockKind::Text {text, editable: false, ..} if text == "same draft")
        );
        assert!(
            matches!(&blocks[1].kind, PresentationBlockKind::Placeholder(label) if label == "[image 1 · png · 1×1]")
        );
        assert_eq!(model.conversation.history().len(), id - 1);
        let turn_id = TurnId::new(id as u64);
        for _ in 0..2 {
            model.reduce(E::Turn(TurnEvent::Started {
                turn_id,
                message: prompt.to_message(),
                mode: SessionMode::Build,
                references: Vec::new(),
            }));
        }
        assert!(model.pending_prompt().is_none());
        assert_eq!(model.conversation.history().len(), id);
        let HistoryEntry::Conversation(committed) = &model.conversation.history()[id - 1] else {
            panic!("canonical user row");
        };
        assert!(matches!(
            &committed.blocks[1].kind,
            PresentationBlockKind::Image { ordinal: 1, .. }
        ));
        model.reduce(E::Turn(TurnEvent::Recovered { turn_id }));
    }
}

#[test]
fn accepted_edit_replaces_tail_before_appending_the_new_prompt() {
    let mut model = SessionModel::default();
    model.conversation.restore(vec![
        zevria_transcript::transcript::TranscriptItem::Message(Message::user("old")),
        zevria_transcript::transcript::TranscriptItem::Message(Message::assistant("old answer")),
    ]);
    model
        .edit_transcript(
            TranscriptEdit {
                target: TranscriptEditTarget::PromptOrdinal(0),
                replacement: TranscriptEditReplacement::Message {
                    text: "hello".into(),
                    mode: SessionMode::Build,
                    behavior: RequestBehavior::Standard,
                    references: Vec::new(),
                },
            },
            0,
        )
        .unwrap();
    assert_eq!(model.conversation.history().len(), 2);
    let reduced = start(&mut model);
    assert_eq!(reduced.change.invalidate_from(), Some(0));
    assert_eq!(model.conversation.history().len(), 1);
    assert!(matches!(
        &model.conversation.history()[0],
        HistoryEntry::Conversation(_)
    ));
}
#[test]
fn mode_selection_requires_both_request_id_and_requested_mode() {
    let mut model = SessionModel::default();
    let ManagementCommand::SetMode { request_id, .. } =
        model.select_mode(SessionMode::Plan).unwrap()
    else {
        panic!("mode")
    };
    for (id, mode) in [
        ("foreign".to_string(), SessionMode::Plan),
        (request_id.clone(), SessionMode::Build),
    ] {
        assert!(
            model
                .reduce(E::Manage(ManagementEvent::ModeResult {
                    request_id: id,
                    result: ModeSelectionResult::Accepted {
                        mode,
                        changed: true
                    }
                }))
                .mode_selection
                .is_none()
        );
        assert!(model.is_busy());
    }
    assert!(
        model
            .reduce(E::Manage(ManagementEvent::ModeResult {
                request_id,
                result: ModeSelectionResult::Accepted {
                    mode: SessionMode::Plan,
                    changed: true
                }
            }))
            .mode_selection
            .is_some()
    );
    assert_eq!(model.next_mode(), SessionMode::Plan);
    assert!(!model.is_busy());
}
#[test]
fn unread_stream_is_fenced_after_intermediate_without_blocking_future_previews() {
    let mut model = pending();
    start(&mut model);
    let (sender, mut receiver) = zevria_session_api::session_event_channel(8);
    sender.stream_updated(TURN, Message::assistant("stale"));
    sender
        .try_send(E::Turn(TurnEvent::Intermediate {
            turn_id: TURN,
            message: Message::assistant("committed"),
            attempt_id: None,
        }))
        .unwrap();
    let zevria_session_api::SessionUpdate::Lifecycle(event) = receiver.try_recv().unwrap() else {
        panic!("lifecycle first")
    };
    model.reduce(event);
    assert!(receiver.try_recv().is_err());
    assert!(!matches!(
        model.tail(Instant::now()),
        ConversationTail::Streaming { .. }
    ));
}
#[test]
fn cancel_learns_the_turn_id_only_after_promotion() {
    let mut model = pending();
    assert_eq!(
        model.cancel(),
        Some(ControlCommand::CancelTurn { turn_id: None })
    );
    start(&mut model);
    assert_eq!(
        model.cancel(),
        Some(ControlCommand::CancelTurn {
            turn_id: Some(TURN)
        })
    );
    model.reduce(E::Turn(TurnEvent::Cancelled { turn_id: TURN }));
    assert_eq!(model.cancel(), None);
}
#[test]
fn tool_completion_bumps_the_same_block_revision() {
    let mut model = pending();
    start(&mut model);
    let call = ToolCall::new(
        CallId::from_wire("c"),
        ToolFunction::new(
            ToolName::new("command").unwrap(),
            serde_json::json!({"command":"echo hi"}),
        ),
    );
    model.reduce(E::Turn(TurnEvent::Intermediate {
        turn_id: TURN,
        message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
            call,
        )])),
        attempt_id: None,
    }));
    let HistoryEntry::Conversation(entry) = &model.conversation.history()[1] else {
        panic!("conversation")
    };
    let before = (entry.blocks[0].id, entry.blocks[0].revision);
    model.reduce(E::Turn(TurnEvent::ToolResults {
        turn_id: TURN,
        message: Message::tool_result(
            CallId::from_wire("c"),
            ToolName::new("command").unwrap(),
            "hi",
        ),
        metadata: Vec::new(),
    }));
    let HistoryEntry::Conversation(entry) = &model.conversation.history()[1] else {
        panic!("conversation")
    };
    assert_eq!(entry.blocks[0].id, before.0);
    assert!(entry.blocks[0].revision > before.1);
}
#[test]
fn tool_results_attach_only_the_batchs_live_command_snapshots() {
    use zevria_foundation::{
        CommandJobDetail, CommandJobDisposition, CommandJobId, CommandJobMode, CommandJobSnapshot,
        CommandJobStatus, ToolCallOutcome, ToolResultDetail, ToolResultMetadata,
    };
    fn block<'a>(model: &'a SessionModel, id: &str) -> &'a crate::blocks::PresentationBlock {
        model
            .conversation
            .history()
            .iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Conversation(entry) => Some(entry),
                _ => None,
            })
            .flat_map(|entry| &entry.blocks)
            .find(|block| {
                block
                    .native_tool()
                    .is_some_and(|(call, _)| call.id.wire().as_ref() == id)
            })
            .unwrap()
    }
    for match_by_job_id in [false, true] {
        let mut model = pending();
        start(&mut model);
        let older = CommandJobSnapshot {
            id: CommandJobId::new("job-older"),
            call_id: Some("older".into()),
            command: "older command".into(),
            mode: CommandJobMode::Background,
            status: CommandJobStatus::Running,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
        };
        let current = CommandJobSnapshot {
            id: CommandJobId::new("job-batch"),
            call_id: (!match_by_job_id).then(|| "batch".into()),
            command: "batch command".into(),
            ..older.clone()
        };
        for job in [&older, &current] {
            model.reduce(E::Job(JobEvent::Updated {
                job: Box::new(job.clone()),
            }));
        }
        model.reduce(E::Turn(TurnEvent::Intermediate {
            turn_id: TURN,
            attempt_id: None,
            message: Message::Assistant(AssistantMessage::new(
                ["older", "batch"]
                    .into_iter()
                    .map(|id| {
                        AssistantContent::ToolCall(ToolCall::new(
                            CallId::from_wire(id),
                            ToolFunction::new(
                                ToolName::new("command").unwrap(),
                                serde_json::json!({"command":id}),
                            ),
                        ))
                    })
                    .collect(),
            )),
        }));
        // A deliberately distinct cached snapshot proves an unrelated block is
        // not revisited by this batch's attachment pass.
        let mut cached = older;
        cached.stdout_bytes = 42;
        model.conversation.update_command_job(&cached);
        let old_revision = block(&model, "older").revision;
        model.reduce(E::Turn(TurnEvent::ToolResults {
            turn_id: TURN,
            message: Message::tool_result(
                CallId::from_wire("batch"),
                ToolName::new("command").unwrap(),
                "done",
            ),
            metadata: vec![ToolResultMetadata {
                id: "batch".into(),
                call_id: None,
                tool_name: "command".into(),
                outcome: ToolCallOutcome::Success,
                diagnostic: None,
                detail: match_by_job_id.then(|| {
                    ToolResultDetail::CommandJob(CommandJobDetail {
                        id: current.id.clone(),
                        disposition: CommandJobDisposition::StartedInBackground,
                    })
                }),
            }],
        }));
        assert_eq!(
            block(&model, "batch")
                .native_tool()
                .unwrap()
                .1
                .command_job
                .as_ref(),
            Some(&current)
        );
        assert_eq!(
            block(&model, "older")
                .native_tool()
                .unwrap()
                .1
                .command_job
                .as_ref(),
            Some(&cached)
        );
        assert_eq!(block(&model, "older").revision, old_revision);
    }
}

#[test]
fn disconnect_rejects_a_pending_mode_without_a_synthetic_turn() {
    let mut model = SessionModel::default();
    model.select_mode(SessionMode::Plan).unwrap();
    assert!(model.disconnected().mode_selection.is_some());
    assert!(!model.is_busy());
    assert!(model.disconnected().mode_selection.is_none());
}
