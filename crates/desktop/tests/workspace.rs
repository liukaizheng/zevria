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
        allow_choice_notes: false,
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
                    detail: None,
                },
                QuestionOption {
                    label: "B".into(),
                    description: "Second".into(),
                    detail: None,
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
fn question_preview_defaults_to_detail_and_tracks_choices_without_selecting_on_preview() {
    let mut request = question(QuestionPromptKind::SingleSelect { allow_other: true });
    let prompt = &mut request.questions[0];
    prompt.options[0].detail = Some("Preview A".into());
    prompt.default = Some("B".to_string().into());
    let mut form = QuestionForm::new(TURN, request.clone());
    assert_eq!(form.preview(0), Some(0), "the default has no detail");
    assert_eq!(form.answers[0].selected, [1]);
    assert!(form.set_preview(0, 0));
    assert_eq!(form.answers[0].selected, [1]);
    assert!(form.choose(0, 1, true));
    assert_eq!(form.preview(0), Some(1));

    request.questions[0].options[1].detail = Some("Preview B".into());
    let mut form = QuestionForm::new(TURN, request);
    assert_eq!(
        form.preview(0),
        Some(1),
        "preview the default when it has detail"
    );
    form.answers[0].other = true;
    form.answers[0].skip = true;
    form.error = Some("Earlier error".into());
    assert!(form.choose(0, 0, true));
    assert_eq!(form.preview(0), Some(0));
    assert_eq!(form.answers[0].selected, [0]);
    assert!(!form.answers[0].other && !form.answers[0].skip);
    assert!(form.error.is_none());
    assert!(!form.set_preview(0, 2));
    form.pending = true;
    assert!(!form.choose(0, 1, true));
    assert_eq!(form.preview(0), Some(0));
}

#[test]
fn multi_select_needs_one_choice_and_deduplicates_other() {
    let request = question(QuestionPromptKind::MultiSelect {
        min_selections: Some(1),
        max_selections: None,
        allow_other: true,
    });
    let mut form = QuestionForm::new(TURN, request);
    assert!(form.response().is_err());
    assert!(form.choose(0, 1, true));
    assert!(form.choose(0, 0, true));
    assert!(form.choose(0, 0, true));
    assert_eq!(form.answers[0].selected, [1, 0]);
    assert_eq!(form.preview(0), Some(0));
    form.answers[0].other = true;
    form.answers[0].text = " B ".into();
    let QuestionResponse::Answered { answers } = form.response().unwrap() else {
        panic!()
    };
    assert_eq!(
        answers[0].answer,
        Some(QuestionAnswerValue::Strings(vec!["A".into(), "B".into()]))
    );
    assert!(answers[0].note.is_none());
    form.answers[0].other = false;
    assert!(form.choose(0, 1, false));
    assert!(form.choose(0, 0, false));
    assert!(form.response().is_err());
}

#[test]
fn choice_notes_are_scoped_to_the_selected_radio_and_omitted_for_other_and_skip() {
    let mut request = question(QuestionPromptKind::SingleSelect { allow_other: true });
    request.allow_choice_notes = true;
    request.questions[0].required = false;
    let mut form = QuestionForm::new(TURN, request);
    assert_eq!(form.choice_note_draft(0, 0), None);
    form.answers[0].selected = vec![0];
    assert!(form.set_choice_note_draft(
        0,
        0,
        " Only for the first release \nNext step later ".into()
    ));
    form.answers[0].selected = vec![1];
    assert!(!form.set_choice_note_draft(0, 0, "stale edit".into()));
    assert!(form.set_choice_note_draft(0, 1, " \u{2003} ".into()));
    let first = |form: &QuestionForm| {
        let QuestionResponse::Answered { mut answers } = form.response().unwrap() else {
            panic!()
        };
        answers.remove(0)
    };
    assert_eq!(first(&form).note, None);
    form.answers[0].selected = vec![0];
    assert_eq!(
        first(&form).note.as_deref(),
        Some(" Only for the first release \nNext step later ")
    );
    form.pending = true;
    assert!(!form.set_choice_note_draft(0, 0, "late edit".into()));
    form.pending = false;
    form.answers[0].other = true;
    form.answers[0].text = "A".into();
    assert_eq!(first(&form).answer, Some("A".to_string().into()));
    assert_eq!(first(&form).note, None);
    form.answers[0].other = false;
    form.answers[0].skip = true;
    assert_eq!(first(&form).answer, None);
    assert_eq!(first(&form).note, None);

    form.answers[0].skip = false;
    form.request.allow_choice_notes = false;
    assert_eq!(form.choice_note_draft(0, 0), None);
    assert_eq!(first(&form).note, None);
    form.request.allow_choice_notes = true;
    form.request.questions[0].kind = QuestionPromptKind::MultiSelect {
        min_selections: None,
        max_selections: None,
        allow_other: true,
    };
    assert_eq!(form.choice_note_draft(0, 0), None);
    assert_eq!(first(&form).note, None);
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
    assert!(matches!(
        workspace.route_input("/subtasks", false),
        InputRoute::Subtasks
    ));
    workspace.subtasks_open = true;
    workspace.disconnected();
    assert!(!workspace.subtasks_open);
    assert!(matches!(
        workspace.route_input("/model", false),
        InputRoute::Models(zevria_model::models::ModelSelectionScope::SessionAndDefault)
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
