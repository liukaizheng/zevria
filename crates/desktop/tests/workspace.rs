use rig_core::message::Message;
use zevria_desktop::session::{InputRoute, WorkspaceModel, question::QuestionForm};
use zevria_foundation::{
    QuestionAnswerValue, QuestionOption, QuestionPrompt, QuestionPromptKind, QuestionRequest,
    QuestionRequestId, QuestionResponse, RequestBehavior, SessionMode, SubtaskDescriptor,
    SubtaskId, SubtaskKind, SubtaskStatus, TurnId,
};
use zevria_session_api::{PlanEvent, QuestionEvent, SubtaskEvent, TurnEvent};
use zevria_session_api::{SessionCommand, SessionEvent as E, SessionUpdate as U, TurnCommand};
use zevria_workflow::{PlanArtifact, PlanHandoff, PlanId, PlanVersion};
const TURN: TurnId = TurnId::new(7);
fn start(workspace: &mut WorkspaceModel) {
    workspace.apply(U::Lifecycle(E::Turn(TurnEvent::Started {
        turn_id: TURN,
        mode: SessionMode::Build,
        message: Message::user("start"),
    })));
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
    workspace.apply(U::Lifecycle(E::Subtask(SubtaskEvent::Launched {
        turn_id: TURN,
        call_id: "call".into(),
        entry_index: 0,
        descriptor: Box::new(SubtaskDescriptor {
            id: id.clone(),
            parent_session_id: "parent".into(),
            title: "Inspect".into(),
            kind: SubtaskKind::Explore,
            workspace: None,
            status: SubtaskStatus::Starting,
        }),
    })));
    workspace.apply(U::Lifecycle(E::Subtask(SubtaskEvent::Session {
        id: id.clone(),
        event: Box::new(E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("child"),
        })),
    })));
    assert!(!workspace.root.is_busy());
    assert!(workspace.children[0].model.is_busy());
    assert!(workspace.children[0].model.cancel().is_none());
    workspace.apply(U::Lifecycle(E::Subtask(SubtaskEvent::Session {
        id,
        event: Box::new(E::Question(QuestionEvent::Asked {
            turn_id: TURN,
            request: question(QuestionPromptKind::SingleSelect { allow_other: true }),
        })),
    })));
    assert!(workspace.controls.question.is_none());
}
#[test]
fn question_requires_root_acceptance_and_both_close_identities() {
    let mut workspace = WorkspaceModel::default();
    let request = question(QuestionPromptKind::SingleSelect { allow_other: true });
    workspace.apply(U::Lifecycle(E::Question(QuestionEvent::Asked {
        turn_id: TURN,
        request: request.clone(),
    })));
    assert!(workspace.controls.question.is_none());
    start(&mut workspace);
    workspace.apply(U::Lifecycle(E::Question(QuestionEvent::Asked {
        turn_id: TURN,
        request,
    })));
    assert!(workspace.controls.question.is_some());
    workspace.apply(U::Lifecycle(E::Question(QuestionEvent::Closed {
        turn_id: TURN,
        request_id: QuestionRequestId::new("foreign"),
    })));
    assert!(workspace.controls.question.is_some());
    workspace.apply(U::Lifecycle(E::Turn(TurnEvent::Cancelled {
        turn_id: TurnId::new(99),
    })));
    assert!(workspace.controls.question.is_some());
    workspace.apply(U::Lifecycle(E::Turn(TurnEvent::Completed {
        turn_id: TURN,
        message: Message::assistant("done"),
        attempt_id: None,
    })));
    assert!(workspace.controls.question.is_none());
}
#[test]
fn fresh_handoff_is_typed_and_disconnect_clears_modal() {
    let mut workspace = WorkspaceModel::default();
    start(&mut workspace);
    workspace.apply(U::Lifecycle(E::Question(QuestionEvent::Asked {
        turn_id: TURN,
        request: question(QuestionPromptKind::SingleSelect { allow_other: true }),
    })));
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
    let outcome = workspace.apply(U::Lifecycle(E::Plan(PlanEvent::FreshHandoffRequested {
        handoff: Box::new(handoff.clone()),
    })));
    assert_eq!(outcome.fresh, Some(handoff));
    workspace.disconnected();
    assert!(workspace.controls.question.is_none());
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
    workspace.controls.subtasks_open = true;
    workspace.disconnected();
    assert!(!workspace.controls.subtasks_open);
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
    workspace.root.apply_selected_mode(SessionMode::Plan);
    assert!(matches!(
        workspace.route_input("/orchestrate independent work", false),
        InputRoute::Notice(notice) if notice.contains("only in Build mode")
    ));
    assert!(!workspace.root.is_busy());
}

#[test]
fn turn_mode_changes_and_completion_request_skill_completions() {
    use zevria_instructions::skill::SkillManagementRequest;
    use zevria_session_api::ManagementCommand;

    for event in [
        TurnEvent::ModeChanged {
            mode: SessionMode::Plan,
        },
        TurnEvent::Completed {
            turn_id: TURN,
            message: Message::assistant("done"),
            attempt_id: None,
        },
    ] {
        let mut workspace = WorkspaceModel::default();
        if matches!(&event, TurnEvent::Completed { .. }) {
            start(&mut workspace);
        }
        let reduction = workspace.apply(U::Lifecycle(E::Turn(event)));
        assert!(reduction.root.is_some());
        assert!(matches!(
            workspace.controls.skills.commands.pop_front(),
            Some(SessionCommand::Manage(ManagementCommand::Skills {
                request: SkillManagementRequest::List { query }, ..
            })) if query.is_empty()
        ));
    }
}

#[test]
fn terminal_subtask_status_does_not_regress_on_late_launch() {
    let mut workspace = WorkspaceModel::default();
    let id = SubtaskId::new("child");
    workspace.apply(U::Lifecycle(E::Subtask(SubtaskEvent::Status {
        turn_id: TURN,
        id: id.clone(),
        status: SubtaskStatus::Completed,
    })));
    workspace.apply(U::Lifecycle(E::Subtask(SubtaskEvent::Status {
        turn_id: TURN,
        id,
        status: SubtaskStatus::Starting,
    })));
    assert_eq!(workspace.children[0].status, Some(SubtaskStatus::Completed));
}

mod transcript_edits {
    use super::*;
    use rig_core::message::{AssistantContent, ToolCall, ToolFunction, ToolName, UserContent};
    use zevria_content::{PromptBlock, PromptImage, UserPrompt};
    use zevria_desktop::session::DraftContent;
    use zevria_presentation::{
        blocks::{NativeToolCallStatus as ToolCallStatus, PresentationBlockKind},
        conversation::HistoryEntry,
    };
    use zevria_session_api::{TranscriptEdit, TranscriptEditReplacement, TranscriptEditTarget};
    use zevria_workflow::{
        AgentRunStatus, EnsembleRunId, EnsembleStart, EnsembleWorkflow, PlanWorkflowState,
    };

    #[test]
    fn edit_requires_an_idle_root_without_capturing_controls_or_executing_tools() {
        let mut workspace = WorkspaceModel::default();
        assert!(workspace.can_edit_transcript());
        start(&mut workspace);
        assert!(!workspace.can_edit_transcript());
        workspace.disconnected();

        let artifact = PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "Implement later".into(),
            source_turn_id: TURN,
        };
        workspace
            .root
            .workflow
            .apply_snapshot(PlanWorkflowState::Ready {
                artifact: artifact.clone(),
            });
        workspace.root.workflow.hide();
        assert!(!workspace.can_edit_transcript(), "hidden Ready Plan");
        workspace
            .root
            .workflow
            .apply_snapshot(PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact),
                draft: None,
            });
        assert!(workspace.can_edit_transcript());
        assert!(workspace.root.workflow.open_review());
        assert!(!workspace.can_edit_transcript(), "recovery Plan dialog");
        workspace.root.workflow.reset();

        workspace.controls.question = Some(QuestionForm::new(
            TURN,
            question(QuestionPromptKind::SingleSelect { allow_other: true }),
        ));
        assert!(!workspace.can_edit_transcript());
        workspace.controls.question = None;
        assert!(workspace.controls.models.show(
            SessionMode::Build,
            zevria_model::models::ModelSelectionScope::SessionOnly
        ));
        assert!(!workspace.can_edit_transcript());
        workspace.controls.models.reset();
        workspace.controls.selected_child = Some(SubtaskId::new("read-only"));
        assert!(!workspace.can_edit_transcript());
        workspace.controls.selected_child = None;

        assert!(workspace.can_edit_transcript());
        workspace.root.conversation.push_message(
            Message::from(vec![AssistantContent::ToolCall(ToolCall::from_wire(
                "call-id",
                ToolFunction::new(ToolName::new("read").unwrap(), serde_json::json!({})),
            ))]),
            ToolCallStatus::Executing,
        );
        assert!(!workspace.root.is_busy());
        assert!(!workspace.can_edit_transcript());
    }

    fn image(red: u8) -> PromptImage {
        PromptImage::from_rgba(1, 1, &[red, 0, 0, 255]).unwrap()
    }

    #[test]
    fn recall_resolves_editable_prompt_blocks_not_placeholders_or_ensembles() {
        let mut workspace = WorkspaceModel::default();
        let ordered = UserPrompt::new(vec![
            PromptBlock::Text("before ".into()),
            PromptBlock::Image(image(1)),
            PromptBlock::Text(" after".into()),
        ])
        .unwrap();
        let prompts = [
            "ordinary".into(),
            "/orchestrate parallel work".into(),
            "$review changes".into(),
            " /new".into(),
            " $review literal".into(),
            ordered,
        ];
        let mut first_block = None;
        for (ordinal, prompt) in prompts.into_iter().enumerate() {
            let conversation = &mut workspace.root.conversation;
            conversation.push_message(Message::assistant("separator"), ToolCallStatus::Finished);
            let index = conversation.history().len();
            conversation.push_message(prompt.to_message(), ToolCallStatus::Finished);
            let selection = conversation.message_recall_selection(index).unwrap();
            let block = conversation.selection_identity(selection).unwrap();
            first_block.get_or_insert(block);
            assert_eq!(
                workspace.recallable_prompt(index, block),
                Some((prompt, TranscriptEditTarget::PromptOrdinal(ordinal), index))
            );
        }
        let conversation = &mut workspace.root.conversation;
        let unsupported = conversation.history().len();
        conversation.push_message(
            Message::User {
                content: vec![UserContent::image_base64("unsupported", None, None)],
            },
            ToolCallStatus::Finished,
        );
        let HistoryEntry::Conversation(entry) = &conversation.history()[unsupported] else {
            panic!()
        };
        let block = entry.blocks.first().unwrap();
        assert!(matches!(block.kind, PresentationBlockKind::Placeholder(_)));
        let unsupported_block = block.id;
        let blank = conversation.history().len();
        conversation.push_message(Message::user("  "), ToolCallStatus::Finished);
        let ensemble = conversation.history().len();
        conversation.add_ensemble(
            EnsembleStart {
                run_id: EnsembleRunId::from_string("ensemble"),
                workflow: EnsembleWorkflow::Review,
                prompt: "review".into(),
                agents: vec![],
            },
            AgentRunStatus::Completed,
            None,
        );
        assert!(
            workspace
                .recallable_prompt(unsupported, unsupported_block)
                .is_none()
        );
        assert!(
            workspace
                .recallable_prompt(blank, first_block.unwrap())
                .is_none()
        );
        assert!(
            workspace
                .recallable_prompt(ensemble, first_block.unwrap())
                .is_none()
        );
    }

    fn route_edit(workspace: &mut WorkspaceModel, prompt: impl Into<UserPrompt>) -> InputRoute {
        workspace.route_edit_input(&prompt.into(), TranscriptEditTarget::PromptOrdinal(0), 0)
    }

    #[test]
    fn edit_grammar_routes_messages_and_skills_without_consuming_unsupported_drafts() {
        for (input, mode, behavior, expected) in [
            (
                "a message",
                SessionMode::Plan,
                RequestBehavior::Standard,
                "a message",
            ),
            (
                " /new",
                SessionMode::Build,
                RequestBehavior::Standard,
                "/new",
            ),
            (
                " $review literal",
                SessionMode::Build,
                RequestBehavior::Standard,
                "$review literal",
            ),
            (
                "/orchestrate independent work",
                SessionMode::Build,
                RequestBehavior::Orchestrate,
                "independent work",
            ),
        ] {
            let mut workspace = WorkspaceModel::default();
            workspace.root.apply_selected_mode(mode);
            let InputRoute::Command(command) = route_edit(&mut workspace, input) else {
                panic!("{input}")
            };
            assert!(
                matches!(*command, SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
                target: TranscriptEditTarget::PromptOrdinal(0),
                replacement: TranscriptEditReplacement::Message { text, mode: actual, behavior: actual_behavior },
            })) if text == UserPrompt::from_text(expected) && actual == mode && actual_behavior == behavior)
            );
            assert!(matches!(
                route_edit(&mut workspace, "busy"),
                InputRoute::Retained
            ));
        }
        let mut workspace = WorkspaceModel::default();
        workspace.root.apply_selected_mode(SessionMode::Plan);
        assert!(
            matches!(route_edit(&mut workspace, "/orchestrate independent work"), InputRoute::Notice(notice) if notice.contains("only in Build mode"))
        );
        assert!(matches!(
            route_edit(&mut workspace, "\n "),
            InputRoute::Retained
        ));
        for builtin in [
            "/new",
            "/resume",
            "/compact",
            "/build",
            "/plan",
            "/model",
            "/jobs",
            "/subtasks",
            "/mcp",
            "/prompt docs:summary",
        ] {
            assert!(
                matches!(route_edit(&mut workspace, builtin), InputRoute::Notice(notice) if notice.contains("cancel the recall")),
                "{builtin}"
            );
        }
        for ensemble in [
            "/ensemble-plan propose options",
            "/ensemble-review check options",
        ] {
            assert!(
                matches!(route_edit(&mut workspace, ensemble), InputRoute::Notice(notice) if notice.contains("not available in this frontend"))
            );
        }
        assert!(matches!(
            route_edit(&mut workspace, "/orchestrate"),
            InputRoute::Notice(_)
        ));
        assert!(!workspace.root.is_busy());
        workspace
            .controls
            .registry
            .set_skills(vec![zevria_instructions::skill::SkillMeta {
                name: "review".parse().unwrap(),
                description: "Review my changes".into(),
            }]);
        let InputRoute::Command(command) = route_edit(&mut workspace, "$review changes") else {
            panic!()
        };
        assert!(
            matches!(*command, SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
            replacement: TranscriptEditReplacement::Skill { name, args, mode: SessionMode::Plan }, ..
        })) if name.as_str() == "review" && args == UserPrompt::from_text("changes"))
        );
    }

    #[test]
    fn image_first_sigils_stay_literal_after_text_or_attachment_edits() {
        for literal in ["/new", "$review"] {
            let attachment = image(1);
            let source = UserPrompt::new(vec![
                PromptBlock::Image(attachment.clone()),
                PromptBlock::Text(literal.into()),
            ])
            .unwrap();
            let (draft, origin) = DraftContent::from_user_prompt(&source);
            assert_eq!(draft.text, format!(" {literal}"));
            assert_eq!(draft.to_user_prompt(Some(&origin)), source);

            for (remove_image, suffix) in [(true, ""), (false, " edited")] {
                let mut draft = draft.clone();
                if remove_image {
                    draft.images.clear();
                }
                draft.text.push_str(suffix);
                let mut workspace = WorkspaceModel::default();
                workspace.controls.registry.set_skills(vec![
                    zevria_instructions::skill::SkillMeta {
                        name: "review".parse().unwrap(),
                        description: "Review my changes".into(),
                    },
                ]);
                let InputRoute::Command(command) =
                    route_edit(&mut workspace, draft.to_user_prompt(Some(&origin)))
                else {
                    panic!("{literal}{suffix} should still route as a literal prompt");
                };
                let mut expected = vec![PromptBlock::Text(format!("{literal}{suffix}"))];
                if !remove_image {
                    expected.push(PromptBlock::Image(attachment.clone()));
                }
                assert!(matches!(
                    *command,
                    SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
                        replacement: TranscriptEditReplacement::Message { text, behavior: RequestBehavior::Standard, .. }, ..
                    })) if text.blocks() == expected
                ));
            }
        }
    }

    #[test]
    fn draft_retains_source_order_until_edits_and_routes_images_on_both_paths() {
        let first = image(1);
        let second = image(2);
        let prompt = UserPrompt::new(vec![
            PromptBlock::Image(first.clone()),
            PromptBlock::Text("/literal text because image comes first".into()),
            PromptBlock::Image(second.clone()),
            PromptBlock::Text(" plus more".into()),
        ])
        .unwrap();
        let (mut draft, origin) = DraftContent::from_user_prompt(&prompt);
        assert!(!draft.is_blank() && !draft.is_empty());
        assert_eq!(draft.images, [first, second.clone()]);
        assert_eq!(draft.to_user_prompt(Some(&origin)), prompt);
        let InputRoute::Command(command) = route_edit(
            &mut WorkspaceModel::default(),
            draft.to_user_prompt(Some(&origin)),
        ) else {
            panic!()
        };
        assert!(
            matches!(*command, SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
            replacement: TranscriptEditReplacement::Message { text, .. }, ..
        })) if text == prompt)
        );

        draft.text.clear();
        draft.images.remove(0);
        assert!(!draft.is_blank() && !draft.is_empty());
        let recovered = draft.to_user_prompt(Some(&origin));
        assert_eq!(recovered.blocks(), &[PromptBlock::Image(second)]);
        let InputRoute::Command(command) =
            WorkspaceModel::default().route_prompt(&recovered, false)
        else {
            panic!()
        };
        assert!(
            matches!(*command, SessionCommand::Turn(TurnCommand::Submit { text, .. }) if text == recovered)
        );
        draft.images.clear();
        draft.text.push(' ');
        assert!(draft.is_blank() && !draft.is_empty());
    }
}
