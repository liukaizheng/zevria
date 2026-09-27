use super::*;
use crate::viewport::RowRange;
use ModelSelectionScope::{SessionAndDefault, SessionOnly};
use zevria_foundation::{ModelContextPolicy, ModelProfileRef, ReasoningLevel as Level};

fn context(index: usize) -> ModelContextPolicy {
    ModelContextPolicy {
        profile: ModelProfileRef::new("provider/with:separator", format!("model/{index}")),
        context_window_tokens: 10000,
        input_token_limit: 9000,
        retained_user_tokens: 100,
    }
}
fn selection(index: usize) -> ModelSelection {
    ModelSelection::new(context(index).profile, Level::Medium)
}
fn candidate(index: usize) -> ModelCandidate {
    ModelCandidate {
        context: context(index),
        reasoning_levels: vec![Level::Low, Level::Medium, Level::High],
    }
}
fn catalog(picker: &mut ModelPicker) {
    let id = picker.request_id.clone();
    assert!(picker.accept(
        &id,
        &Result::Catalog {
            mode: picker.mode.unwrap(),
            scope: picker.scope.unwrap(),
            current: selection(0),
            profiles: (0..40).map(candidate).collect(),
            revision: "revision".into()
        }
    ));
}
fn preview(picker: &ModelPicker) -> ModelSelectionPreview {
    ModelSelectionPreview {
        request_id: picker.request_id.clone(),
        generation: 3,
        session_generation: "runtime".into(),
        mode: picker.mode.unwrap(),
        scope: picker.scope.unwrap(),
        target: selection(1),
        source: selection(0),
        revision: "revision".into(),
        reason: "Costs tokens, can lose detail, and changes shared root context".into(),
    }
}

#[test]
fn picker_captures_mode_and_scope_and_waits_for_authoritative_results() {
    for scope in [SessionOnly, SessionAndDefault] {
        for mode in SessionMode::ALL {
            let mut picker = ModelPicker::default();
            picker.show(mode, scope);
            assert!(picker.pending());
            assert!(
                matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                request: Request::List { mode: actual_mode, scope: actual_scope }, ..
            })) if actual_mode == mode && actual_scope == scope)
            );
            picker.handle_key(KeyCode::Enter);
            assert!(picker.commands.is_empty());
            catalog(&mut picker);
            for ch in "model/39".chars() {
                picker.handle_key(KeyCode::Char(ch));
            }
            assert_eq!(picker.filtered(), vec![39]);
            picker.handle_key(KeyCode::Enter);
            assert!(!picker.pending() && picker.commands.is_empty());
            picker.handle_key(KeyCode::Enter);
            assert!(picker.pending() && picker.is_open());
            assert_eq!(picker.current, Some(selection(0)));
            assert!(
                matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
                request: Request::Select { target, mode: actual_mode, scope: actual_scope, .. }, ..
            })) if target == selection(39) && actual_mode == mode && actual_scope == scope)
            );
            let changed = Result::Changed {
                role: mode_role(mode),
                scope,
                context: context(39),
                snapshot: None,
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
                revision: "next".into(),
                unchanged: false,
            };
            assert!(!picker.accept("old-session", &changed));
            assert!(picker.is_open() && picker.pending());
            let id = picker.request_id.clone();
            assert!(picker.accept(&id, &changed));
            assert!(!picker.is_open());
        }
    }
}

#[test]
fn partial_save_rejection_only_adopts_a_global_revision_for_global_scope() {
    for scope in [SessionOnly, SessionAndDefault] {
        let mut picker = ModelPicker::default();
        picker.show(SessionMode::Build, scope);
        catalog(&mut picker);
        picker.commands.clear();
        let id = picker.request_id.clone();
        picker.accept(
            &id,
            &Result::Rejected {
                code: "session_save_failed".into(),
                message: "Session unchanged".into(),
                checkpoint_installed: true,
                current_revision: Some("committed-global-revision".into()),
            },
        );
        assert!(picker.is_open());
        assert_eq!(picker.current, Some(selection(0)));
        picker.handle_key(KeyCode::Enter);
        picker.handle_key(KeyCode::Enter);
        let expected = if scope == SessionOnly {
            "revision"
        } else {
            "committed-global-revision"
        };
        assert!(
            matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request: Request::Select { revision, .. }, ..
        })) if revision == expected)
        );
    }
}

#[test]
fn wrong_scope_mode_and_preview_identity_do_not_change_pending_state() {
    let mut picker = ModelPicker::default();
    picker.show(SessionMode::Plan, SessionOnly);
    catalog(&mut picker);
    picker.handle_key(KeyCode::Enter);
    picker.handle_key(KeyCode::Enter);
    let id = picker.request_id.clone();
    let original = preview(&picker);
    let mut wrong_scope = original.clone();
    wrong_scope.scope = SessionAndDefault;
    let mut wrong_mode = original.clone();
    wrong_mode.mode = SessionMode::Build;
    let mut wrong_id = original.clone();
    wrong_id.request_id = "old".into();
    let mut wrong_revision = original;
    wrong_revision.revision = "stale".into();
    for invalid in [wrong_scope, wrong_mode, wrong_id, wrong_revision] {
        assert!(!picker.accept(&id, &Result::ConfirmationRequired(invalid)));
        assert!(picker.pending() && picker.is_open() && picker.preview().is_none());
    }
    for (mode, scope) in [
        (SessionMode::Build, SessionOnly),
        (SessionMode::Plan, SessionAndDefault),
    ] {
        for invalid in [
            Result::Catalog {
                mode,
                scope,
                current: selection(1),
                profiles: vec![],
                revision: "wrong".into(),
            },
            Result::Changed {
                role: mode_role(mode),
                scope,
                context: context(1),
                snapshot: None,
                reasoning_level: zevria_foundation::ReasoningLevel::Medium,
                revision: "wrong".into(),
                unchanged: false,
            },
        ] {
            assert!(!picker.accept(&id, &invalid));
            assert!(picker.pending() && picker.is_open());
            assert_eq!(picker.current, Some(selection(0)));
            assert_eq!(picker.revision, "revision");
        }
    }
}

#[test]
fn picker_cancel_and_confirmation_are_correlated() {
    for scope in [SessionOnly, SessionAndDefault] {
        let mut picker = ModelPicker::default();
        picker.show(SessionMode::Build, scope);
        catalog(&mut picker);
        picker.commands.clear();
        let id = picker.request_id.clone();
        let preview = preview(&picker);
        picker.phase = ModelPhase::PendingChange {
            target: preview.target.clone(),
            back: SelectionStep::Model,
        };
        assert!(picker.accept(&id, &Result::ConfirmationRequired(preview.clone())));
        picker.handle_key(KeyCode::Enter);
        assert!(
            matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request: Request::Confirm { preview: p }, ..
        })) if p == preview)
        );
        picker.handle_key(KeyCode::Esc);
        picker.handle_key(KeyCode::Esc);
        assert!(picker.pending() && picker.is_open());
        assert!(
            matches!(picker.commands.pop_front(), Some(SessionCommand::Manage(zevria_session_api::ManagementCommand::Models {
            request: Request::Cancel, request_id
        })) if request_id == id)
        );
        assert!(picker.commands.is_empty());
        // A late preview cannot undo a pending cancellation.
        picker.accept(&id, &Result::ConfirmationRequired(preview));
        assert!(picker.pending());
        picker.accept(&id, &Result::Cancelled);
        assert!(!picker.is_open());
        picker.show(SessionMode::Build, scope);
        assert!(!picker.accept(&id, &Result::Cancelled));
        assert!(picker.is_open());
    }
}

#[test]
fn picker_viewport_and_scope_text_render_on_small_terminals() {
    for scope in [SessionOnly, SessionAndDefault] {
        for (width, height) in [(1, 1), (10, 3), (30, 8), (100, 24)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut picker = ModelPicker::default();
            picker.show(SessionMode::Build, scope);
            terminal
                .draw(|frame| picker.render(frame, frame.area()))
                .unwrap();
            catalog(&mut picker);
            picker.handle_key(KeyCode::End);
            terminal
                .draw(|frame| picker.render(frame, frame.area()))
                .unwrap();
            assert_eq!(picker.nav.selected, 39);
            picker.handle_key(KeyCode::PageUp);
            assert_eq!(
                picker.nav.selected,
                39usize.saturating_sub(picker.nav.viewport.visible_rows().max(1))
            );
            if width == 100 {
                let buffer = terminal.backend().buffer();
                let text = buffer
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains(picker.scope_label()), "{text}");
                assert!(text.contains(scope.command()));
            }
            picker.phase = ModelPhase::Conversion {
                preview: preview(&picker),
                back: SelectionStep::Model,
            };
            picker.details.viewport.set_top(usize::MAX);
            terminal
                .draw(|frame| picker.render(frame, frame.area()))
                .unwrap();
        }
    }
}

#[test]
fn build_and_orchestrate_picker_titles_explain_the_shared_build_role() {
    for mode in [SessionMode::Build] {
        for width in [1, 8, 24, 100] {
            let mut picker = ModelPicker::default();
            picker.show(mode, SessionOnly);
            catalog(&mut picker);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 18)).unwrap();
            terminal
                .draw(|frame| picker.render(frame, frame.area()))
                .unwrap();
            assert_eq!(mode_role(mode), zevria_foundation::ModelRole::Build);
            if width == 100 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains("Build role"), "{text}");
            }
        }
    }
}

#[test]
fn model_commands_are_bare_builtins_with_completion_and_literal_escaping() {
    use crate::command::{ClassifiedInput, CommandRegistry, SlashCommand};
    let registry = CommandRegistry::default();
    for (text, command) in [
        ("/model", SlashCommand::Model),
        ("/model-session", SlashCommand::ModelSession),
    ] {
        assert_eq!(
            registry.classify(text).unwrap(),
            ClassifiedInput::Builtin(command)
        );
        assert!(
            registry
                .classify(&format!("{text} provider/model"))
                .is_err()
        );
        assert_eq!(
            registry.classify(&format!(" {text}")).unwrap(),
            ClassifiedInput::Message(text.into())
        );
    }
    for removed in ["/reasoning", "/reasoning-session", "/model-reasoning"] {
        assert!(registry.classify(removed).is_err());
        assert!(registry.matches(removed, removed.len()).is_empty());
    }
    assert_eq!(registry.matches("/model", 6).len(), 2);
    assert_eq!(registry.matches("/model-s", 8).len(), 1);
}

#[test]
fn reasoning_stage_is_explicit_supported_and_back_cancel_are_nonmutating() {
    for mode in SessionMode::ALL {
        for scope in [SessionOnly, SessionAndDefault] {
            let mut picker = ModelPicker::default();
            picker.show(mode, scope);
            catalog(&mut picker);
            picker.commands.clear();
            picker.current.as_mut().unwrap().reasoning_level = Level::High;
            picker.handle_key(KeyCode::Enter);
            assert_eq!(
                picker.phase.step(),
                SelectionStep::Reasoning {
                    profile: 0,
                    selected: 2
                }
            );
            assert!(picker.commands.is_empty() && !picker.pending());
            picker.handle_key(KeyCode::Backspace);
            assert_eq!(picker.phase.step(), SelectionStep::Model);
            assert!(picker.commands.is_empty());
            picker.profiles[0].reasoning_levels = vec![Level::Low, Level::Medium];
            picker.handle_key(KeyCode::Enter);
            assert_eq!(
                picker.phase.step(),
                SelectionStep::Reasoning {
                    profile: 0,
                    selected: 0
                },
                "unsupported current level highlights first option, not an inferred default"
            );
            picker.handle_key(KeyCode::End);
            picker.handle_key(KeyCode::Down);
            assert_eq!(
                picker.phase.step(),
                SelectionStep::Reasoning {
                    profile: 0,
                    selected: 1
                }
            );
            picker.handle_key(KeyCode::Home);
            picker.handle_key(KeyCode::Enter);
            assert!(matches!(
                picker.commands.pop_front(),
                Some(SessionCommand::Manage(
                    zevria_session_api::ManagementCommand::Models {
                        request: Request::Select {
                            target: ModelSelection {
                                reasoning_level: Level::Low,
                                ..
                            },
                            ..
                        },
                        ..
                    }
                ))
            ));
            let id = picker.request_id.clone();
            let changed = Result::Changed {
                role: mode_role(mode),
                scope,
                context: context(0),
                reasoning_level: Level::High,
                snapshot: None,
                revision: "r".into(),
                unchanged: false,
            };
            assert!(
                !picker.accept(&id, &changed),
                "a different reasoning selection is stale"
            );
            picker.accept(&id, &Result::rejected("save_failed", "retry"));
            picker.handle_key(KeyCode::Left);
            picker.handle_key(KeyCode::Esc);
            picker.handle_key(KeyCode::Esc);
            assert!(matches!(
                picker.commands.pop_front(),
                Some(SessionCommand::Manage(
                    zevria_session_api::ManagementCommand::Models {
                        request: Request::Cancel,
                        ..
                    }
                ))
            ));
            assert!(picker.commands.is_empty());
        }
    }
}

#[test]
fn reasoning_list_pages_by_measured_rows_and_reveals_selection() {
    for height in [3, 5, 6, 10] {
        let mut picker = ModelPicker::default();
        picker.show(SessionMode::Build, SessionOnly);
        catalog(&mut picker);
        picker.commands.clear();
        picker.handle_key(KeyCode::Enter);
        picker.handle_key(KeyCode::Home);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, height)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        let page = picker.reasoning.viewport.visible_rows().max(1);
        picker.handle_key(KeyCode::PageDown);
        let selected = page.min(2);
        assert_eq!(
            picker.phase.step(),
            SelectionStep::Reasoning {
                profile: 0,
                selected
            }
        );
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            text.contains(&picker.profiles[0].reasoning_levels[selected].to_string()),
            "{height}: {text}"
        );
        picker.handle_key(KeyCode::End);
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        assert!(
            picker
                .reasoning
                .viewport
                .visible_range()
                .intersects(RowRange::from_start_len(2, 1))
        );
        picker.invalidate_geometry();
        picker.handle_key(KeyCode::PageUp);
        assert_eq!(
            picker.phase.step(),
            SelectionStep::Reasoning {
                profile: 0,
                selected: 1
            }
        );
        assert!(picker.commands.is_empty());
    }
}

#[test]
fn late_catalog_cannot_replace_a_reasoning_choice_or_pending_change() {
    let mut picker = ModelPicker::default();
    picker.show(SessionMode::Build, SessionOnly);
    catalog(&mut picker);
    picker.commands.clear();
    picker.handle_key(KeyCode::Enter);
    picker.handle_key(KeyCode::End);
    let id = picker.request_id.clone();
    let late_catalog = Result::Catalog {
        mode: SessionMode::Build,
        scope: SessionOnly,
        current: selection(0),
        profiles: vec![],
        revision: "late".into(),
    };
    assert!(!picker.accept(&id, &late_catalog));
    assert_eq!(
        picker.phase.step(),
        SelectionStep::Reasoning {
            profile: 0,
            selected: 2
        }
    );
    picker.handle_key(KeyCode::Enter);
    assert!(!picker.accept(&id, &late_catalog));
    assert!(picker.pending());
    assert_eq!(picker.revision, "revision");
    picker.cancel();
    picker.cancel();
    assert!(matches!(picker.phase, ModelPhase::Cancelling { .. }));
    assert!(picker.accept(&id, &late_catalog));
    assert!(matches!(picker.phase, ModelPhase::Cancelling { .. }));
    picker.accept(&id, &Result::Cancelled);
    assert!(!picker.is_open());
    assert!(!picker.accept(&id, &late_catalog));
}

#[test]
fn reasoning_stage_renders_at_all_sizes_without_submitting() {
    for (width, height) in [(1, 1), (10, 3), (30, 8), (110, 24)] {
        let mut picker = ModelPicker::default();
        picker.show(SessionMode::Plan, SessionOnly);
        catalog(&mut picker);
        picker.commands.clear();
        picker.handle_key(KeyCode::Enter);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        assert!(picker.commands.is_empty());
        if width == 110 {
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            for expected in [
                "Step 2/2",
                "Plan role",
                "config unchanged",
                "low",
                "medium",
                "high",
                "back",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
        }
    }
}

#[test]
fn every_model_step_uses_vim_list_navigation_and_filter_owns_printable_keys() {
    let mut picker = ModelPicker::default();
    picker.show(SessionMode::Build, SessionOnly);
    catalog(&mut picker);
    picker.commands.clear();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).unwrap();
    terminal
        .draw(|frame| picker.render(frame, frame.area()))
        .unwrap();
    let page = picker.nav.viewport.visible_rows().max(1);
    for (key, expected) in [
        (KeyCode::Char('j'), 1),
        (KeyCode::Char('k'), 0),
        (KeyCode::Char('G'), 39),
        (KeyCode::Char('g'), 0),
    ] {
        picker.handle_key(key);
        assert_eq!(picker.nav.selected, expected);
    }
    picker.handle_input(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(picker.nav.selected, page);
    picker.handle_input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(picker.nav.selected, page.saturating_sub((page / 2).max(1)));
    picker.handle_key(KeyCode::Char('g'));
    picker.handle_key(KeyCode::Enter);
    picker.handle_key(KeyCode::Char('G'));
    assert_eq!(
        picker.phase.step(),
        SelectionStep::Reasoning {
            profile: 0,
            selected: 2
        }
    );
    picker.handle_key(KeyCode::Char('k'));
    assert_eq!(
        picker.phase.step(),
        SelectionStep::Reasoning {
            profile: 0,
            selected: 1
        }
    );
    picker.handle_key(KeyCode::Char('g'));
    assert_eq!(
        picker.phase.step(),
        SelectionStep::Reasoning {
            profile: 0,
            selected: 0
        }
    );
    picker.handle_key(KeyCode::Left);
    picker.handle_key(KeyCode::Char('/'));
    for ch in "jkq?".chars() {
        picker.handle_key(KeyCode::Char(ch));
    }
    assert_eq!(picker.filter.text(), "jkq?");
    assert_eq!(picker.context(), KeyContext::TextEntry);
    assert!(picker.commands.is_empty());
    picker.handle_key(KeyCode::Esc);
    picker.handle_key(KeyCode::Char('q'));
    assert!(matches!(
        picker.commands.pop_front(),
        Some(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Models {
                request: Request::Cancel,
                ..
            }
        ))
    ));
}

#[test]
fn conversion_details_share_scroll_keys_without_changing_the_target() {
    let mut picker = ModelPicker::default();
    picker.show(SessionMode::Plan, SessionOnly);
    catalog(&mut picker);
    let mut preview = preview(&picker);
    preview.reason = "A long conversion detail line.\n".repeat(60);
    picker.phase = ModelPhase::Conversion {
        preview: preview.clone(),
        back: SelectionStep::Model,
    };
    picker.commands.clear();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
    terminal
        .draw(|frame| picker.render(frame, frame.area()))
        .unwrap();
    picker.handle_key(KeyCode::Char('G'));
    assert_eq!(
        picker.details.viewport.top(),
        picker.details.viewport.max_top()
    );
    picker.handle_key(KeyCode::Char('g'));
    picker.handle_key(KeyCode::Char('j'));
    assert_eq!(picker.details.viewport.top(), 1);
    picker.handle_key(KeyCode::Char('k'));
    assert_eq!(picker.details.viewport.top(), 0);
    picker.handle_input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(
        picker.details.viewport.top(),
        (picker.details.viewport.visible_rows() / 2).max(1)
    );
    assert_eq!(picker.preview(), Some(&preview));
    assert!(picker.commands.is_empty());
}
