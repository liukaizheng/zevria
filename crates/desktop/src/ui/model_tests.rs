use super::*;
use gpui_kit::Focusable as _;
use zevria_foundation::{ModelContextPolicy, ModelProfileRef, ModelRole, ReasoningLevel as Level};
use zevria_model::models::{
    ModelCandidate, ModelManagementRequest as Request, ModelManagementResult as Result,
    ModelSelection, ModelSelectionScope as Scope,
};
use zevria_session_api::ManagementEvent;
fn context() -> ModelContextPolicy {
    ModelContextPolicy {
        profile: ModelProfileRef::new(
            "fixture",
            "model-with-a-deliberately-long-configured-profile-name",
        ),
        context_window_tokens: 128_000,
        input_token_limit: 100_000,
        retained_user_tokens: 100,
        image_accounting: Default::default(),
    }
}
fn seed(controller: &Entity<SessionController>, cx: &mut TestAppContext) {
    controller.update(cx, |this, cx| {
        for role in ModelRole::ALL {
            this.model.root.install_model(role, &context(), None);
            this.model.root.install_reasoning(role, Level::Medium);
        }
        this.edit_draft("é draft\nkept across selection".into(), cx);
    });
    cx.run_until_parked();
}
fn list(receiver: &mut Receiver) -> (String, Scope) {
    let SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
        request_id,
        request: Request::List {
            mode: SessionMode::Build,
            scope,
        },
    }) = receiver.try_recv().unwrap()
    else {
        panic!("model list, not a submitted turn")
    };
    (request_id, scope)
}
fn catalog(
    controller: &Entity<SessionController>,
    id: &str,
    scope: Scope,
    cx: &mut TestAppContext,
) {
    event(
        controller,
        E::Manage(ManagementEvent::ModelsResult {
            request_id: id.into(),
            result: Box::new(Result::Catalog {
                mode: SessionMode::Build,
                scope,
                current: ModelSelection::new(context().profile, Level::Medium),
                profiles: vec![ModelCandidate {
                    context: context(),
                    reasoning_levels: vec![Level::Medium, Level::High],
                }],
                revision: "r".into(),
            }),
        }),
        cx,
    );
}
#[gpui_kit::test]
fn compact_status_shortcut_and_menu_open_one_session_only_lifecycle(cx: &mut TestAppContext) {
    for entry in ["click", "shortcut", "menu"] {
        let (window, view, controller, mut receiver) = setup_at_size(cx, 720., 520.);
        seed(&controller, cx);
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            let control = window.find("status-model");
            assert!(control.visible());
            assert!(control.bounds().right() <= px(720.));
            match entry {
                "click" => window.click("status-model", cx),
                "shortcut" => window.press(Shortcut::OPEN_MODELS.0, cx),
                _ => view.read(cx).composer.focus_handle(cx).dispatch_action(
                    &actions::OpenModels,
                    window,
                    cx,
                ),
            }
        })
        .unwrap();
        cx.run_until_parked();
        let (id, scope) = list(&mut receiver);
        assert_eq!(scope, Scope::SessionOnly);
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("model-picker").visible());
            let bounds = window.find("model-picker").bounds();
            assert!(bounds.origin.x >= px(0.) && bounds.right() <= px(720.));
            assert!(bounds.origin.y >= px(0.) && bounds.bottom() <= px(520.));
            window.press(Shortcut::OPEN_MODELS.0, cx);
            window.press(Shortcut::CONFIRM.0, cx); // Never a background Plan/decision action.
        })
        .unwrap();
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            controller.read_with(cx, |c, _| c.draft.clone()),
            "é draft\nkept across selection"
        );
        catalog(&controller, &id, scope, cx);
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            for _ in 0..12 {
                window.press("tab", cx);
                assert!(view.read(cx).model_picker_has_focus(window, cx));
            }
            for _ in 0..12 {
                window.press("shift-tab", cx);
                assert!(view.read(cx).model_picker_has_focus(window, cx));
            }
        })
        .unwrap();
        cx.update_window(window.into(), |_, window, cx| window.press("escape", cx))
            .unwrap();
        cx.run_until_parked();
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                request: Request::Cancel,
                ..
            })
        ));
        assert!(controller.read_with(cx, |c, _| c.model.controls.models.is_open()));
        event(
            &controller,
            E::Manage(ManagementEvent::ModelsResult {
                request_id: id,
                result: Box::new(Result::Cancelled),
            }),
            cx,
        );
        assert!(!controller.read_with(cx, |c, _| c.model.root.is_busy()));
    }
}
#[gpui_kit::test]
fn search_effort_apply_and_escape_preserve_nonempty_thinking_disclosures_and_focus(
    cx: &mut TestAppContext,
) {
    let (window, view, controller, mut receiver) = setup(cx);
    seed(&controller, cx);
    let replay = zevria_model::ProviderReplay::openai_responses(
        context().profile,
        vec![
            serde_json::json!({"type":"reasoning","id":"native","summary":[{"type":"summary_text","text":"saved visible thinking"}],"encrypted_content":"cipher"}),
            serde_json::json!({"type":"message","role":"assistant","id":"message","status":"completed","content":[{"type":"output_text","text":"saved answer","annotations":[]}]}),
        ],
    );
    controller.update(cx, |c, cx| {
        c.model.root.conversation.restore(vec![
            zevria_transcript::transcript::TranscriptItem::Message(Message::user("saved prompt")),
            zevria_transcript::transcript::TranscriptItem::provider_message(replay).unwrap(),
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press(Shortcut::EXPAND_THINKING.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let transcript = view.read_with(cx, |v, _| v.transcript.clone());
    let before = transcript.read_with(cx, |v, cx| {
        v.test_data(cx)
            .iter()
            .map(|b| (b.data.key.clone(), b.expanded))
            .collect::<Vec<_>>()
    });
    let epoch = controller.read_with(cx, |c, _| c.model.root.conversation.epoch());
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::OPEN_MODELS.0, cx)
    })
    .unwrap();
    cx.run_until_parked();
    let (id, scope) = list(&mut receiver);
    assert_eq!(
        transcript.read_with(cx, |v, cx| v
            .test_data(cx)
            .iter()
            .map(|b| (b.data.key.clone(), b.expanded))
            .collect::<Vec<_>>()),
        before,
        "management must not insert even a transient conversation activity row"
    );
    catalog(&controller, &id, scope, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        receiver.try_recv().is_err(),
        "choosing a model does not apply anything"
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("model-efforts").visible());
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionCommand::Manage(zevria_session_api::ManagementCommand::Models { request: Request::Select { target, scope: Scope::SessionOnly, .. }, .. }) if target.reasoning_level == Level::High)
    );
    cx.update_window(window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request: Request::Cancel,
            ..
        })
    ));
    // Persistence already committed; its acknowledgement wins the cancel race.
    event(
        &controller,
        E::Manage(ManagementEvent::ModelsResult {
            request_id: id,
            result: Box::new(Result::Changed {
                role: ModelRole::Build,
                scope,
                context: context(),
                reasoning_level: Level::High,
                snapshot: None,
                revision: "r".into(),
                unchanged: false,
            }),
        }),
        cx,
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.root.conversation.epoch()),
        epoch
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.draft.clone()),
        "é draft\nkept across selection"
    );
    assert_eq!(
        transcript.read_with(cx, |v, cx| v
            .test_data(cx)
            .iter()
            .map(|b| (b.data.key.clone(), b.expanded))
            .collect::<Vec<_>>()),
        before
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .composer
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
    })
    .unwrap();
}
#[gpui_kit::test]
fn native_model_effort_and_apply_buttons_keep_enter_and_space_ownership(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    seed(&controller, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::OPEN_MODELS.0, cx)
    })
    .unwrap();
    cx.run_until_parked();
    let (id, scope) = list(&mut receiver);
    catalog(&controller, &id, scope, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        for _ in 0..12 {
            if window.find(("choose-model", 0usize)).focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find(("choose-model", 0usize)).focused(), Some(true));
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        receiver.try_recv().is_err(),
        "a focused model button only selects the model"
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        for _ in 0..12 {
            if window.find(("model-effort", 1usize)).focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find(("model-effort", 1usize)).focused(), Some(true));
        window.press("space", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        receiver.try_recv().is_err(),
        "a focused effort button does not apply the request"
    );
    assert!(controller.read_with(cx, |c, _| matches!(
        c.model.controls.models.phase.step(),
        zevria_presentation::models::SelectionStep::Reasoning { selected: 1, .. }
    )));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        for _ in 0..12 {
            if window.find("model-apply").focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(window.find("model-apply").focused(), Some(true));
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionCommand::Manage(zevria_session_api::ManagementCommand::Models { request: Request::Select { target, .. }, .. }) if target.reasoning_level == Level::High)
    );
    event(
        &controller,
        E::Manage(ManagementEvent::ModelsResult {
            request_id: id,
            result: Box::new(Result::Cancelled),
        }),
        cx,
    );
}

#[gpui_kit::test]
fn escape_before_catalog_keeps_cancellation_owned_until_acknowledged(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    seed(&controller, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.press(Shortcut::OPEN_MODELS.0, cx)
    })
    .unwrap();
    cx.run_until_parked();
    let (id, scope) = list(&mut receiver);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request: Request::Cancel,
            ..
        })
    ));
    catalog(&controller, &id, scope, cx);
    assert!(controller.read_with(cx, |c, _| matches!(
        c.model.controls.models.phase,
        zevria_presentation::models::ModelPhase::Cancelling { .. }
    )));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(view.read(cx).model_picker_has_focus(window, cx));
    })
    .unwrap();
    event(
        &controller,
        E::Manage(ManagementEvent::ModelsResult {
            request_id: id,
            result: Box::new(Result::Cancelled),
        }),
        cx,
    );
    assert!(!controller.read_with(cx, |c, _| c.model.controls.models.is_open()));
}

#[gpui_kit::test]
fn conversion_and_partial_save_errors_keep_native_controls_visible_at_minimum_size(
    cx: &mut TestAppContext,
) {
    for (cancel_focused, tokens) in [
        (true, zevria_theme::ZEVRIA_GRAPHITE),
        (false, zevria_theme::ZEVRIA_GRAPHITE_LIGHT),
    ] {
        let (window, view, controller, mut receiver) = setup_at_size(cx, 720., 520.);
        cx.update(|cx| theme::apply(tokens, cx));
        seed(&controller, cx);
        controller.update(cx, |c, cx| {
            c.open_model_picker(Scope::SessionAndDefault, cx);
        });
        cx.run_until_parked();
        let (id, scope) = list(&mut receiver);
        let mut destination = context();
        destination.profile =
            ModelProfileRef::new("other-provider", "other-model-with-a-long-configured-name");
        event(
            &controller,
            E::Manage(ManagementEvent::ModelsResult {
                request_id: id.clone(),
                result: Box::new(Result::Catalog {
                    mode: SessionMode::Build,
                    scope,
                    current: ModelSelection::new(context().profile, Level::Medium),
                    profiles: vec![
                        ModelCandidate {
                            context: context(),
                            reasoning_levels: vec![Level::Medium],
                        },
                        ModelCandidate {
                            context: destination.clone(),
                            reasoning_levels: vec![Level::Medium, Level::High],
                        },
                    ],
                    revision: "r".into(),
                }),
            }),
            cx,
        );
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request: Request::Select { target, .. },
            ..
        }) = receiver.try_recv().unwrap()
        else {
            panic!("explicit effort selection")
        };
        let preview = zevria_model::models::ModelSelectionPreview { request_id: id.clone(), session_generation: "native-runtime".into(), generation: 19, mode: SessionMode::Build, scope, target, source: ModelSelection::new(context().profile, Level::Medium), revision: "r".into(), reason: "The effective checkpoint contains source-model-only opaque context. Conversion makes a source-model call and can lose detail. ".repeat(8) };
        event(
            &controller,
            E::Manage(ManagementEvent::ModelsResult {
                request_id: id.clone(),
                result: Box::new(Result::ConfirmationRequired(preview.clone())),
            }),
            cx,
        );
        assert!(
            receiver.try_recv().is_err(),
            "a preview never confirms itself"
        );
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            for button in ["model-apply", "model-cancel"] {
                assert!(window.find(button).visible());
                assert!(window.find(button).bounds().bottom() <= px(520.));
            }
            window.press("alt-left", cx);
            assert!(
                view.read(cx).model_picker_has_focus(window, cx),
                "Back during a preview must not focus the unmounted search input"
            );
            window.press("tab", cx);
            assert!(view.read(cx).model_picker_has_focus(window, cx));
            if cancel_focused {
                for _ in 0..8 {
                    if window.find("model-cancel").focused() == Some(true) {
                        break;
                    }
                    window.press("tab", cx);
                }
                assert_eq!(window.find("model-cancel").focused(), Some(true));
                window.press("enter", cx);
            } else {
                window.click("model-apply", cx);
            }
        })
        .unwrap();
        cx.run_until_parked();
        if cancel_focused {
            assert!(
                matches!(
                    receiver.try_recv().unwrap(),
                    SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                        request: Request::Cancel,
                        ..
                    })
                ),
                "Enter on Cancel must never initiate a paid conversion"
            );
            event(
                &controller,
                E::Manage(ManagementEvent::ModelsResult {
                    request_id: id,
                    result: Box::new(Result::Cancelled),
                }),
                cx,
            );
            continue;
        } else {
            assert!(
                matches!(receiver.try_recv().unwrap(), SessionCommand::Manage(zevria_session_api::ManagementCommand::Models { request: Request::Confirm { preview: actual }, .. }) if actual == preview)
            );
        }
        event(&controller, E::Manage(ManagementEvent::ModelsResult { request_id: id.clone(), result: Box::new(Result::Rejected { code: "session_save_failed".into(), message: "The global default was saved, but the session header could not be written. Fix transcript persistence and retry.".into(), checkpoint_installed: true, current_revision: Some("new-revision".into()) }) }), cx);
        assert_eq!(
            controller.read_with(cx, |c, _| c.model.root.session.reasoning(ModelRole::Build)),
            Some(Level::Medium)
        );
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("model-error").visible());
            window.click("model-cancel", cx);
        })
        .unwrap();
        event(
            &controller,
            E::Manage(ManagementEvent::ModelsResult {
                request_id: id,
                result: Box::new(Result::Cancelled),
            }),
            cx,
        );
    }
}

#[gpui_kit::test]
fn slash_scopes_consume_only_accepted_commands_and_channel_failure_does_not_strand_picker(
    cx: &mut TestAppContext,
) {
    let (_, _, controller, mut receiver) = setup(cx);
    seed(&controller, cx);
    for (command, expected) in [
        ("/model", Scope::SessionAndDefault),
        ("/model-session", Scope::SessionOnly),
    ] {
        controller.update(cx, |c, cx| {
            c.edit_draft(command.into(), cx);
            c.submit(cx);
        });
        cx.run_until_parked();
        let (id, scope) = list(&mut receiver);
        assert_eq!(scope, expected);
        assert!(controller.read_with(cx, |c, _| c.draft.is_empty()));
        event(
            &controller,
            E::Manage(ManagementEvent::ModelsResult {
                request_id: id,
                result: Box::new(Result::Cancelled),
            }),
            cx,
        );
        controller.update(cx, |c, cx| {
            c.capturing_dialog = true;
            c.edit_draft(command.into(), cx);
            c.submit(cx);
        });
        assert_eq!(controller.read_with(cx, |c, _| c.draft.clone()), command);
        assert!(receiver.try_recv().is_err());
        controller.update(cx, |c, _| c.capturing_dialog = false);
    }
    // /resume only labels the noncapturing Sessions sidebar; it must not
    // permanently disable model selection when the user stays in this session.
    controller.update(cx, |c, cx| {
        c.show_sessions = true;
        assert!(c.open_model_picker(Scope::SessionOnly, cx));
    });
    let (id, _) = list(&mut receiver);
    assert_eq!(
        controller.read_with(cx, |c, _| c.draft.clone()),
        "/model-session"
    );
    event(
        &controller,
        E::Manage(ManagementEvent::ModelsResult {
            request_id: id,
            result: Box::new(Result::Cancelled),
        }),
        cx,
    );
    drop(receiver);
    controller.update(cx, |c, cx| {
        c.open_model_picker(Scope::SessionOnly, cx);
    });
    cx.run_until_parked();
    assert!(
        !controller.read_with(cx, |c, _| c.model.controls.models.is_open()
            || c.model.root.is_busy())
    );
}
