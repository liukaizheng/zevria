use super::*;
use std::sync::Arc;
use zevria_instructions::skill::*;

fn skill_context(description: &str) -> SkillContext {
    let mut metadata = SkillMetadata::new(description);
    metadata.invocation_policy = SkillInvocationPolicy::ExplicitOnly;
    let definition = SkillDefinition::new(
        SkillName::parse("review").unwrap(),
        description,
        "HIDDEN MAIN INSTRUCTIONS",
        SkillSource::Programmatic("TUI fixture".into()),
    )
    .unwrap()
    .with_metadata(metadata, None)
    .unwrap();
    SkillContext {
        catalog: Arc::new(SkillCatalog::new([definition]).unwrap()),
        pins: ActiveSkills::default(),
        mode_enabled: true,
    }
}

#[test]
fn ordinary_matching_text_stays_ordinary_for_core_model_selection() {
    let context = SkillContext {
        catalog: Arc::new(
            SkillCatalog::new([SkillDefinition::new(
                "commit".parse().unwrap(),
                "Create a Git commit",
                "PRIVATE BODY",
                SkillSource::Programmatic("fixture".into()),
            )
            .unwrap()])
            .unwrap(),
        ),
        pins: ActiveSkills::default(),
        mode_enabled: true,
    };
    assert_eq!(context.prompt_catalog().entries[0].name.as_str(), "commit");
    for prompt in [
        "commit the changes",
        "Please record this patch as a commit",
        "unrelated task",
    ] {
        let mut app = App::new().with_skill_context(&context);
        enter_insert(&mut app);
        app.set_input_for_test(prompt, prompt.len());
        assert!(
            matches!(app.handle_event(ctrl_enter()), Some(UiAction::Submit { text, .. }) if text == zevria_content::UserPrompt::from_text(prompt))
        );
    }
    assert!(context.pins.is_empty());
}

#[test]
fn tui_explicit_invocation_is_name_only_and_leading_space_stays_literal() {
    let context = skill_context("Review code");
    let mut app = App::new().with_skill_context(&context);
    enter_insert(&mut app);
    app.set_input_for_test("$re", 3);
    app.handle_event(key(KeyCode::Tab));
    app.set_input_for_test("$review arguments", "$review arguments".len());
    let Some(UiAction::InvokeSkill { name, args, mode }) = app.handle_event(ctrl_enter()) else {
        panic!("bound typed skill")
    };
    assert_eq!(name.as_str(), "review");
    assert_eq!(args, zevria_content::UserPrompt::from_text("arguments"));
    assert_eq!(mode, SessionMode::Build);
    let mut literal = App::new().with_skill_context(&context);
    enter_insert(&mut literal);
    literal.set_input_for_test(" $review arguments", " $review arguments".len());
    assert!(
        matches!(literal.handle_event(ctrl_enter()), Some(UiAction::Submit { text, .. }) if text == zevria_content::UserPrompt::from_text("$review arguments"))
    );
}

#[test]
fn tui_catalog_refresh_updates_completions_without_binding_a_draft() {
    let old = skill_context("Old description");
    let new = skill_context("New description");
    let mut app = App::new().with_skill_context(&old);
    enter_insert(&mut app);
    app.set_input_for_test("$re", 3);
    app.handle_event(key(KeyCode::Tab));
    let page = new
        .management_view(&SkillManagementRequest::List {
            query: String::new(),
        })
        .unwrap();
    app.replace_skill_entries(page.completions);
    let Some(UiAction::InvokeSkill { name, .. }) = app.handle_event(ctrl_enter()) else {
        panic!("bound skill")
    };
    assert_eq!(name.as_str(), "review");
    assert_eq!(
        new.resolve(&name, SkillInvocationOrigin::Explicit)
            .unwrap()
            .description(),
        "New description"
    );
}

#[test]
fn tui_fresh_completion_after_reload_is_name_only() {
    let old = skill_context("Old");
    let new = skill_context("New");
    let mut app = App::new().with_skill_context(&old);
    enter_insert(&mut app);
    app.set_input_for_test("$re", 3);
    app.handle_event(key(KeyCode::Tab));
    app.replace_skill_entries(
        new.management_view(&SkillManagementRequest::List {
            query: String::new(),
        })
        .unwrap()
        .completions,
    );
    app.set_input_for_test("$re", 3);
    app.handle_event(key(KeyCode::Tab));
    let Some(UiAction::InvokeSkill { name, .. }) = app.handle_event(ctrl_enter()) else {
        panic!("bound skill")
    };
    assert_eq!(
        new.resolve(&name, SkillInvocationOrigin::Explicit)
            .unwrap()
            .description(),
        "New"
    );
}

#[test]
fn completion_refresh_preserves_the_highlighted_name() {
    let mut context = skill_context("Review code");
    context.catalog = Arc::new(
        SkillCatalog::new(["alpha", "review"].map(|name| {
            SkillDefinition::new(
                name.parse().unwrap(),
                name,
                "Body",
                SkillSource::Programmatic(name.into()),
            )
            .unwrap()
        }))
        .unwrap(),
    );
    let mut app = App::new().with_skill_context(&context);
    enter_insert(&mut app);
    app.set_input_for_test("$", 1);
    app.handle_event(key(KeyCode::Down));
    app.replace_skill_entries(
        ["aardvark", "alpha", "review"]
            .map(|name| SkillMeta {
                name: name.parse().unwrap(),
                description: name.into(),
            })
            .into(),
    );
    app.handle_event(key(KeyCode::Tab));
    assert!(
        matches!(app.handle_event(ctrl_enter()), Some(UiAction::InvokeSkill { name, .. }) if name.as_str() == "review")
    );
}

#[test]
fn manager_ignores_stale_browser_views_and_coalesces_completion_refreshes() {
    let old = skill_context("Old description");
    let new = skill_context("New description");
    let pop = |manager: &mut crate::skills::SkillManager| {
        let Some(zevria_session_api::SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Skills {
                request_id,
                request,
            },
        )) = manager.commands.pop_front()
        else {
            panic!("management request")
        };
        (request_id, request)
    };
    let response = |request_id, context: &SkillContext, request: &SkillManagementRequest| {
        SessionEvent::SkillsResult {
            request_id,
            result: SkillManagementResult::View {
                view: context.management_view(request).unwrap(),
            },
        }
    };
    let mut manager = crate::skills::SkillManager::default();
    manager.show();
    let (stale_id, stale_request) = pop(&mut manager);
    manager.key(KeyCode::Char('/'));
    for ch in "New".chars() {
        manager.key(KeyCode::Char(ch));
    }
    manager.key(KeyCode::Enter);
    let (current_id, current_request) = pop(&mut manager);
    manager.key(KeyCode::Char(' '));
    assert!(
        manager.commands.is_empty(),
        "loading data is not actionable"
    );
    manager.event(&response(current_id, &new, &current_request));
    manager.event(&response(stale_id, &old, &stale_request));
    manager.key(KeyCode::Char(' '));
    let (_, mutation) = pop(&mut manager);
    assert!(
        matches!(mutation, SkillManagementRequest::SetEnabled { expected_revision, .. } if expected_revision == new.catalog.revision())
    );

    let mut manager = crate::skills::SkillManager::default();
    manager.refresh();
    let (stale_id, stale_request) = pop(&mut manager);
    manager.event(&SessionEvent::SkillsChanged {
        revision: new.catalog.revision().into(),
        counts: new.management_counts(),
    });
    assert!(
        manager
            .event(&response(stale_id, &old, &stale_request))
            .is_none()
    );
    let (current_id, current_request) = pop(&mut manager);
    assert_eq!(
        manager.event(&response(current_id, &new, &current_request)),
        Some(new.completions())
    );
    assert!(manager.commands.is_empty());
    manager.event(&SessionEvent::ModeChanged {
        mode: SessionMode::Plan,
    });
    assert_eq!(
        manager.commands.len(),
        1,
        "mode capability changes refresh completion eligibility"
    );
}

#[test]
fn tui_recalled_skill_edit_sends_name_and_leaves_snapshot_choice_to_engine() {
    let mut context = skill_context("Pinned description");
    let snapshot = context.catalog.iter().next().unwrap().snapshot();
    context.pins = ActiveSkills::from_snapshots([snapshot.clone()]).unwrap();
    context.catalog = skill_context("Changed installed description").catalog;
    let invocation = SkillInvocation::new(
        snapshot.name().clone(),
        "original",
        SkillApplication::Activate(snapshot.clone()),
    );
    let mut app = App::new().with_skill_context(&context);
    app.restore(vec![TranscriptItem::SkillInvocation(invocation)]);
    app.select_for_test(cursor(0, 0));
    ctrl_e(&mut app);
    app.set_input_for_test("$review changed args", "$review changed args".len());
    let Some(UiAction::EditTranscript(edit)) = app.handle_event(ctrl_enter()) else {
        panic!("typed tail edit")
    };
    assert!(
        matches!(edit.replacement, TranscriptEditReplacement::Skill { name, args, .. } if name == *snapshot.name() && args == zevria_content::UserPrompt::from_text("changed args"))
    );
}

#[test]
fn skill_navigation_reveals_selected_rows_and_keeps_detail_scroll_local() {
    let context = SkillContext {
        catalog: Arc::new(
            SkillCatalog::new((0..20).map(|index| {
                SkillDefinition::new(
                    format!("skill-{index:02}").parse().unwrap(),
                    "A description long enough to wrap on a narrow surface",
                    "PRIVATE BODY",
                    SkillSource::Programmatic("navigation fixture".into()),
                )
                .unwrap()
            }))
            .unwrap(),
        ),
        pins: ActiveSkills::default(),
        mode_enabled: true,
    };
    let mut manager = crate::skills::SkillManager::default();
    manager.show();
    let Some(zevria_session_api::SessionCommand::Manage(
        zevria_session_api::ManagementCommand::Skills {
            request_id,
            request,
        },
    )) = manager.commands.pop_front()
    else {
        panic!("metadata request")
    };
    manager.event(&SessionEvent::SkillsResult {
        request_id,
        result: SkillManagementResult::View {
            view: context.management_view(&request).unwrap(),
        },
    });
    let mut terminal = Terminal::new(TestBackend::new(45, 7)).unwrap();
    for index in 0..20 {
        if index > 0 {
            manager.key(KeyCode::Down);
        }
        terminal
            .draw(|frame| manager.render(frame, frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            text.contains(&format!("> skill-{index:02}")),
            "selected {index}: {text}"
        );
    }
    manager.key(KeyCode::Home);
    terminal
        .draw(|frame| manager.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("> skill-00"), "{text}");
    manager.key(KeyCode::End);
    terminal
        .draw(|frame| manager.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("> skill-19"), "{text}");
    manager.invalidate_geometry();
    manager.key(KeyCode::PageUp);
    terminal
        .draw(|frame| manager.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        text.contains("> skill-18"),
        "unmeasured paging falls back to one entry: {text}"
    );
    assert!(manager.commands.is_empty());
}

#[test]
fn tui_skills_manager_is_bodyless_and_sends_correlated_revision_bound_mutations() {
    let context = skill_context("Review code");
    let mut manager = crate::skills::SkillManager::default();
    manager.show();
    let Some(zevria_session_api::SessionCommand::Manage(
        zevria_session_api::ManagementCommand::Skills {
            request_id,
            request,
        },
    )) = manager.commands.pop_front()
    else {
        panic!("metadata request")
    };
    let mut page = context.management_view(&request).unwrap();
    page.global_location = Some("/temporary-home/.zevria/skills".into());
    page.project_location = Some("/temporary-workspace/.zevria/skills".into());
    manager.event(&SessionEvent::SkillsResult {
        request_id,
        result: SkillManagementResult::View { view: page },
    });
    let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
    terminal
        .draw(|frame| manager.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("/temporary-home/.zevria/skills"));
    assert!(text.contains("/temporary-workspace/.zevria/skills"));
    assert!(!text.contains("HIDDEN MAIN INSTRUCTIONS"));
    manager.key(KeyCode::Char(' '));
    let Some(zevria_session_api::SessionCommand::Manage(
        zevria_session_api::ManagementCommand::Skills {
            request_id,
            request,
        },
    )) = manager.commands.pop_front()
    else {
        panic!("toggle request")
    };
    assert!(
        matches!(request, SkillManagementRequest::SetEnabled { expected_revision, enabled: false, .. } if expected_revision == context.catalog.revision())
    );
    manager.event(&SessionEvent::SkillsResult {
        request_id,
        result: SkillManagementResult::error("busy", "request was not queued"),
    });
    assert!(
        manager.commands.is_empty(),
        "busy mutations are not automatically retried"
    );
    manager.key(KeyCode::Esc);
    assert!(!manager.is_open());
}

#[test]
fn skill_vim_and_control_pages_use_the_shared_list_selection() {
    let context = SkillContext {
        catalog: Arc::new(
            SkillCatalog::new((0..20).map(|index| {
                SkillDefinition::new(
                    format!("skill-{index:02}").parse().unwrap(),
                    "metadata",
                    "PRIVATE BODY",
                    SkillSource::Programmatic("navigation fixture".into()),
                )
                .unwrap()
            }))
            .unwrap(),
        ),
        pins: ActiveSkills::default(),
        mode_enabled: true,
    };
    let mut manager = crate::skills::SkillManager::default();
    manager.show();
    let Some(zevria_session_api::SessionCommand::Manage(
        zevria_session_api::ManagementCommand::Skills {
            request_id,
            request,
        },
    )) = manager.commands.pop_front()
    else {
        panic!("list request")
    };
    manager.event(&SessionEvent::SkillsResult {
        request_id,
        result: SkillManagementResult::View {
            view: context.management_view(&request).unwrap(),
        },
    });
    let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
    terminal
        .draw(|frame| manager.render(frame, frame.area()))
        .unwrap();
    for (event, index) in [
        (KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), 19),
        (KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), 18),
        (KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), 0),
        (KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), 1),
        (KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL), 9),
        (KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 5),
    ] {
        manager.handle_input(event);
        manager.key(KeyCode::Char(' '));
        let Some(zevria_session_api::SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Skills {
                request: SkillManagementRequest::SetEnabled { name, .. },
                ..
            },
        )) = manager.commands.pop_front()
        else {
            panic!("toggle selected skill")
        };
        assert_eq!(name.as_str(), format!("skill-{index:02}"));
    }
    manager.key(KeyCode::Char('q'));
    assert!(!manager.is_open());
}
