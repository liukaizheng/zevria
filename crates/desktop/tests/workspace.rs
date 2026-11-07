use rig_core::message::Message;
use zevria_desktop::session::{InputRoute, WorkspaceModel, question::QuestionForm};
use zevria_foundation::{
    QuestionAnswerValue, QuestionOption, QuestionPrompt, QuestionPromptKind, QuestionRequest,
    QuestionRequestId, QuestionResponse, RequestBehavior, SessionMode, SubtaskDescriptor,
    SubtaskId, SubtaskKind, SubtaskStatus, TurnId,
};
use zevria_session_api::{SessionCommand, SessionEvent as E, SessionUpdate as U, TurnCommand};
use zevria_workflow::{PlanArtifact, PlanHandoff, PlanId, PlanVersion};
const TURN: TurnId = TurnId::new(7);
fn start(workspace: &mut WorkspaceModel) {
    workspace.apply(U::Lifecycle(E::TurnStarted {
        turn_id: TURN,
        mode: SessionMode::Build,
        message: Message::user("start"),
    }));
}
fn question(kind: QuestionPromptKind) -> QuestionRequest {
    QuestionRequest {
        id: QuestionRequestId::new("q"),
        source_label: None,
        dismissible: false,
        questions: vec![QuestionPrompt {
            id: "one".into(),
            header: "Choice".into(),
            question: "Pick".into(),
            kind,
            options: vec![
                QuestionOption {
                    label: "A".into(),
                    description: "First".into(),
                },
                QuestionOption {
                    label: "B".into(),
                    description: "Second".into(),
                },
            ],
            required: true,
            default: None,
        }],
    }
}
#[test]
fn child_events_never_promote_root_or_steal_questions() {
    let mut workspace = WorkspaceModel::default();
    let id = SubtaskId::new("child");
    workspace.apply(U::Lifecycle(E::SubtaskLaunched {
        turn_id: TURN,
        call_id: "call".into(),
        entry_index: 0,
        descriptor: SubtaskDescriptor {
            id: id.clone(),
            parent_session_id: "parent".into(),
            title: "Inspect".into(),
            kind: SubtaskKind::Explore,
            workspace: None,
            status: SubtaskStatus::Starting,
        },
    }));
    workspace.apply(U::Lifecycle(E::SubtaskSession {
        id: id.clone(),
        event: Box::new(E::TurnStarted {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("child"),
        }),
    }));
    assert!(!workspace.root.is_busy());
    assert!(workspace.children[0].model.is_busy());
    assert!(workspace.children[0].model.cancel().is_none());
    workspace.apply(U::Lifecycle(E::SubtaskSession {
        id,
        event: Box::new(E::QuestionAsked {
            turn_id: TURN,
            request: question(QuestionPromptKind::SingleSelect { allow_other: true }),
        }),
    }));
    assert!(workspace.question.is_none());
}
#[test]
fn question_requires_root_acceptance_and_both_close_identities() {
    let mut workspace = WorkspaceModel::default();
    let request = question(QuestionPromptKind::SingleSelect { allow_other: true });
    workspace.apply(U::Lifecycle(E::QuestionAsked {
        turn_id: TURN,
        request: request.clone(),
    }));
    assert!(workspace.question.is_none());
    start(&mut workspace);
    workspace.apply(U::Lifecycle(E::QuestionAsked {
        turn_id: TURN,
        request,
    }));
    assert!(workspace.question.is_some());
    workspace.apply(U::Lifecycle(E::QuestionClosed {
        turn_id: TURN,
        request_id: QuestionRequestId::new("foreign"),
    }));
    assert!(workspace.question.is_some());
    workspace.apply(U::Lifecycle(E::TurnCancelled {
        turn_id: TurnId::new(99),
    }));
    assert!(workspace.question.is_some());
    workspace.apply(U::Lifecycle(E::TurnCompleted {
        turn_id: TURN,
        message: Message::assistant("done"),
        attempt_id: None,
    }));
    assert!(workspace.question.is_none());
}
#[test]
fn fresh_handoff_is_typed_and_disconnect_clears_modal() {
    let mut workspace = WorkspaceModel::default();
    start(&mut workspace);
    workspace.apply(U::Lifecycle(E::QuestionAsked {
        turn_id: TURN,
        request: question(QuestionPromptKind::SingleSelect { allow_other: true }),
    }));
    let handoff = PlanHandoff::new(
        PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 3,
            },
            title: "Plan".into(),
            markdown: "# Implement".into(),
            source_turn_id: TURN,
        },
        "source",
    );
    let outcome = workspace.apply(U::Lifecycle(E::FreshPlanHandoffRequested {
        handoff: handoff.clone(),
    }));
    assert_eq!(outcome.fresh, Some(handoff));
    workspace.disconnected();
    assert!(workspace.question.is_none());
    assert!(!workspace.root.is_busy());
}
#[test]
fn question_text_single_other_multiselect_and_optional_skip_have_exact_wire_types() {
    let mut request = question(QuestionPromptKind::Text {
        min_length: Some(2),
        max_length: Some(5),
    });
    request.questions[0].options.clear();
    let mut form = QuestionForm::new(TURN, request);
    form.answers[0].text = " ".into();
    assert!(form.response().is_err());
    form.answers[0].text = "okay".into();
    assert!(form.response().is_ok());
    assert!(form.dismiss().is_err());
    let mut form = QuestionForm::new(
        TURN,
        question(QuestionPromptKind::SingleSelect { allow_other: true }),
    );
    form.answers[0].other = true;
    assert!(form.response().is_err());
    form.answers[0].text = "Other answer".into();
    let QuestionResponse::Answered { answers } = form.response().unwrap() else {
        panic!()
    };
    assert_eq!(
        answers[0].answer,
        Some(QuestionAnswerValue::String("Other answer".into()))
    );
    let mut form = QuestionForm::new(
        TURN,
        question(QuestionPromptKind::MultiSelect {
            min_selections: Some(1),
            max_selections: Some(2),
            allow_other: true,
        }),
    );
    form.answers[0].selected = vec![1, 0, 1];
    let QuestionResponse::Answered { answers } = form.response().unwrap() else {
        panic!()
    };
    assert_eq!(
        answers[0].answer,
        Some(QuestionAnswerValue::Strings(vec!["A".into(), "B".into()]))
    );
    form.request.questions[0].required = false;
    form.answers[0].skip = true;
    let QuestionResponse::Answered { answers } = form.response().unwrap() else {
        panic!()
    };
    assert_eq!(answers[0].answer, None);
}
#[test]
fn shared_grammar_routes_supported_commands_and_retains_unsupported_drafts() {
    let mut workspace = WorkspaceModel::default();
    let InputRoute::Command(command) = workspace.route_input("hello", true) else {
        panic!()
    };
    assert!(matches!(
        *command,
        SessionCommand::Turn(TurnCommand::Submit {
            behavior: RequestBehavior::Orchestrate,
            ..
        })
    ));
    assert!(matches!(
        workspace.route_input("busy", false),
        InputRoute::Retained
    ));
    workspace.disconnected();
    assert!(matches!(
        workspace.route_input("/model", false),
        InputRoute::Notice(_)
    ));
    assert!(matches!(
        workspace.route_input("/new", false),
        InputRoute::New
    ));
    assert!(matches!(
        workspace.route_input("/resume", false),
        InputRoute::Resume
    ));
    assert!(matches!(
        workspace.route_input("/orchestrate", false),
        InputRoute::Notice(_)
    ));
}
#[test]
fn terminal_subtask_status_does_not_regress_on_late_launch() {
    let mut workspace = WorkspaceModel::default();
    let id = SubtaskId::new("child");
    workspace.apply(U::Lifecycle(E::SubtaskStatus {
        turn_id: TURN,
        id: id.clone(),
        status: SubtaskStatus::Completed,
    }));
    workspace.apply(U::Lifecycle(E::SubtaskStatus {
        turn_id: TURN,
        id,
        status: SubtaskStatus::Starting,
    }));
    assert_eq!(workspace.children[0].status, Some(SubtaskStatus::Completed));
}
