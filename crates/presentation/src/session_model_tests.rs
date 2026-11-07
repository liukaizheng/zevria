use crate::conversation::HistoryEntry;
use crate::session::OperationKind;
use crate::{ConversationTail, SessionModel, Settlement};
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
};
use std::time::Instant;
use zevria_foundation::{RequestBehavior, SessionMode, TurnId};
use zevria_session_api::{
    ControlCommand, ManagementCommand, ModeSelectionResult, SessionEvent as E, TranscriptEdit,
    TranscriptEditReplacement, TranscriptEditTarget,
};
use zevria_workflow::{PlanArtifact, PlanDecision, PlanId, PlanVersion, PlanWorkflowState};
const TURN: TurnId = TurnId::new(7);
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
    model.reduce(E::TurnStarted {
        turn_id: TURN,
        message: Message::user("hello"),
        mode: SessionMode::Build,
    })
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
    model.reduce(E::ModelCallStarted {
        turn_id: TURN,
        call: 1,
    });
    model.reduce(E::AssistantStreamUpdated {
        turn_id: TURN,
        snapshot: (Message::assistant("partial")).into(),
    });
    assert!(matches!(
        model.tail(Instant::now()),
        ConversationTail::Streaming { .. }
    ));
    let reduced = model.reduce(E::TurnCompleted {
        turn_id: TURN,
        message: Message::assistant("done"),
        attempt_id: None,
    });
    assert_eq!(
        reduced.settled,
        Some((OperationKind::Submit, Settlement::Completed))
    );
    assert!(!model.is_busy());
    assert_eq!(model.conversation.history().len(), 2);
    assert!(
        model
            .reduce(E::TurnCancelled { turn_id: TURN })
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
            .reduce(E::TurnFailed {
                turn_id: foreign,
                error: "foreign".into()
            })
            .settled
            .is_none()
    );
    model.reduce(E::Intermediate {
        turn_id: foreign,
        message: Message::assistant("foreign"),
        attempt_id: None,
    });
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
    let reduced = model.reduce(E::TurnRejected {
        turn_id: TURN,
        error: "not admitted".into(),
    });
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
    let reduced = model.reduce(E::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: artifact(),
        },
    });
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
                .reduce(E::ModeResult {
                    request_id: id,
                    result: ModeSelectionResult::Accepted {
                        mode,
                        changed: true
                    }
                })
                .mode_selection
                .is_none()
        );
        assert!(model.is_busy());
    }
    assert!(
        model
            .reduce(E::ModeResult {
                request_id,
                result: ModeSelectionResult::Accepted {
                    mode: SessionMode::Plan,
                    changed: true
                }
            })
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
        .try_send(E::Intermediate {
            turn_id: TURN,
            message: Message::assistant("committed"),
            attempt_id: None,
        })
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
    model.reduce(E::TurnCancelled { turn_id: TURN });
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
    model.reduce(E::Intermediate {
        turn_id: TURN,
        message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
            call,
        )])),
        attempt_id: None,
    });
    let HistoryEntry::Conversation(entry) = &model.conversation.history()[1] else {
        panic!("conversation")
    };
    let before = (entry.blocks[0].id, entry.blocks[0].revision);
    model.reduce(E::ToolResults {
        turn_id: TURN,
        message: Message::tool_result(
            CallId::from_wire("c"),
            ToolName::new("command").unwrap(),
            "hi",
        ),
        metadata: Vec::new(),
    });
    let HistoryEntry::Conversation(entry) = &model.conversation.history()[1] else {
        panic!("conversation")
    };
    assert_eq!(entry.blocks[0].id, before.0);
    assert!(entry.blocks[0].revision > before.1);
}
#[test]
fn disconnect_rejects_a_pending_mode_without_a_synthetic_turn() {
    let mut model = SessionModel::default();
    model.select_mode(SessionMode::Plan).unwrap();
    assert!(model.disconnected().mode_selection.is_some());
    assert!(!model.is_busy());
    assert!(model.disconnected().mode_selection.is_none());
}
