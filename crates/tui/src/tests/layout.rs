//! Layout behavior and presentation tests.
use super::*;

#[test]
fn narrow_composer_wraps_words_without_mutating_separator_bytes() {
    let input = "hello  world";
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test(input, 6);

    let rows = rendered_rows(&mut app, 16, 12);
    assert_eq!(app.composer_width(), 8);
    assert_eq!(app.input(), input);
    let hello_row = rows
        .iter()
        .position(|row| row.contains("hello"))
        .expect("first wrapped composer row");
    let world_row = rows
        .iter()
        .position(|row| row.contains("world"))
        .expect("second wrapped composer row");
    assert_eq!(world_row, hello_row + 1);
    assert_eq!(
        rows[world_row].chars().nth(5),
        Some('w'),
        "the hidden separator does not indent the wrapped word"
    );
    assert!(!rows[hello_row].contains("wo"));
    assert_eq!(
        cursor_position_after_render(&mut app, 16, 12),
        Position::new(10, hello_row as u16),
        "an interior hidden-space caret aliases the visible end of hello"
    );
}

#[test]
fn focused_vertical_arrows_follow_soft_wraps_without_scrolling_the_transcript() {
    let mut app = App::new();
    enter_insert(&mut app);
    for ch in "abcdefghi".chars() {
        app.handle_event(key(KeyCode::Char(ch)));
    }
    // Fourteen terminal columns leave six display cells after gutters,
    // rounded chrome padding, and the prompt prefix.
    let _ = rendered_text(&mut app, 14, 12);
    assert_eq!(app.composer_width(), 6);
    app.set_view_for_test(7, false);

    assert_eq!(app.handle_event(key(KeyCode::Up)), None);
    assert_eq!(app.input_cursor(), 3);
    assert_eq!(app.view_scroll(), 7, "Up is owned by the focused composer");
    assert_eq!(app.handle_event(key(KeyCode::Down)), None);
    assert_eq!(app.input_cursor(), app.input().len());
    assert_eq!(
        app.view_scroll(),
        7,
        "Down is owned by the focused composer"
    );
}

#[test]
fn multiline_composer_grows_to_six_rows_then_scrolls_with_the_caret() {
    let mut app = App::new();
    enter_insert(&mut app);
    for index in 0..8 {
        for ch in format!("row{index}").chars() {
            app.handle_event(key(KeyCode::Char(ch)));
        }
        if index < 7 {
            app.handle_event(key(KeyCode::Enter));
        }
    }

    let rendered = rendered_text(&mut app, 40, 20);
    assert_eq!(app.composer_scroll(), 2, "the last six rows stay visible");
    assert!(
        rendered.contains('█'),
        "overflowing composer has a scrollbar"
    );
    assert!(!rendered.contains("row0"));
    assert!(!rendered.contains("row1"));
    assert!(rendered.contains("row2"));
    assert!(rendered.contains("row7"));
    assert_eq!(cursor_position_after_render(&mut app, 40, 20).y, 16);

    for _ in 0..7 {
        app.handle_event(key(KeyCode::Up));
    }
    let rendered = rendered_text(&mut app, 40, 20);
    assert_eq!(app.composer_scroll(), 0);
    assert!(rendered.contains("row0"));
    assert!(!rendered.contains("row7"));
    assert_eq!(cursor_position_after_render(&mut app, 40, 20).y, 11);
}

#[test]
fn fitting_and_empty_conversation_and_composer_viewports_have_no_scrollbar() {
    let mut empty = App::new();
    assert!(!rendered_text(&mut empty, 80, 20).contains('█'));

    let mut fitting = App::new();
    fitting.seed_history_entry(history_message(Message::user("short message")));
    enter_insert(&mut fitting);
    for character in "short draft".chars() {
        fitting.handle_event(key(KeyCode::Char(character)));
    }
    assert!(!rendered_text(&mut fitting, 80, 20).contains('█'));

    let mut overflowing = App::new();
    for index in 0..14 {
        overflowing.seed_history_entry(history_message(Message::user(format!(
            "scrollbar row {index}"
        ))));
    }
    let rendered = rendered_text(&mut overflowing, 36, 9);
    assert!(
        rendered.contains('█'),
        "overflowing transcript has a scrollbar"
    );
}

#[test]
fn followed_long_conversation_scrollbar_reaches_bottom_endpoint() {
    const WIDTH: u16 = 64;
    const HEIGHT: u16 = 24;
    const FINAL_ITEM: &str = "final transcript endpoint sentinel";

    let mut app = App::new();
    for index in 0..14 {
        let content = if index == 13 {
            FINAL_ITEM.to_string()
        } else {
            format!("conversation history item {index:02}")
        };
        app.seed_history_entry(history_message(Message::user(content)));
    }
    assert!(app.view_follow(), "the default view should follow the tail");

    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("render long conversation");
    let buffer = terminal.backend().buffer();
    let rendered = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<String>();

    assert!(
        app.view_follow(),
        "rendering should remain pinned to the tail"
    );
    assert!(
        app.view_scroll() > 0,
        "the fixture must overflow the viewport"
    );
    assert!(
        rendered.contains(FINAL_ITEM),
        "the logical viewport should render the final transcript item"
    );

    let scrollbar_x = buffer.area.width - 1;
    let down_y = (0..buffer.area.height)
        .find(|&y| buffer[(scrollbar_x, y)].symbol() == "↓")
        .expect("overflowing conversation scrollbar should render an end marker");
    let final_track_y = down_y
        .checked_sub(1)
        .expect("the end marker should have a track cell above it");
    assert_eq!(
        buffer[(scrollbar_x, final_track_y)].symbol(),
        "█",
        "the followed-bottom thumb should end immediately above the down marker"
    );
}

#[test]
fn input_titles_and_cursor_reflect_the_input_mode() {
    let mut app = App::new();
    let normal = rendered_text(&mut app, 100, 20);
    assert!(normal.contains("Build · Normal"));
    assert!(normal.contains("i input"));
    assert!(!cursor_visible_after_render(&mut app, 100, 20));

    enter_insert(&mut app);
    let insert = rendered_text(&mut app, 100, 20);
    assert!(insert.contains("Build · Insert"));
    assert!(insert.contains("Ctrl+Enter send"));
    assert!(cursor_visible_after_render(&mut app, 100, 20));

    toggle_mode_with_ack(&mut app);
    app.handle_event(key(KeyCode::Esc));
    let plan_normal = rendered_text(&mut app, 100, 20);
    assert!(plan_normal.contains("Plan · Normal"));
    assert!(!cursor_visible_after_render(&mut app, 100, 20));
}

#[test]
fn footer_owns_state_and_accounting_while_composer_chrome_is_action_only() {
    let mut app = App::new();
    let rows = rendered_rows(&mut app, 100, 20);
    let footer = rows.last().expect("footer");
    let body = rows[..rows.len() - 1].concat();
    assert!(footer.contains("Build · Normal"));
    assert!(!body.contains("Build · Normal"));
    assert!(body.contains("i input"));

    start_empty_turn(&mut app, TEST_TURN_ID, SessionMode::Build);
    app.reduce_without_effects(SessionEvent::UsageUpdated {
        turn_id: TEST_TURN_ID,
        usage: TokenUsage {
            input_tokens: 12_300,
            cached_tokens: 10_200,
            output_tokens: 1_400,
            total_tokens: 13_700,
        },
        profile: ModelProfileRef::new("test", "model"),
        model_role: ModelRole::Build,
        input_token_limit: 100_000,
        context_window_tokens: 128_000,
    });
    app.reduce_without_effects(SessionEvent::TurnFailed {
        turn_id: TEST_TURN_ID,
        error: "failed after usage".to_string(),
    });
    let rows = rendered_rows(&mut app, 140, 20);
    let footer = rows.last().expect("footer");
    let body = rows[..rows.len() - 1].concat();
    assert!(footer.contains("test/model"));
    assert!(footer.contains("last in 12.3k"));
    assert!(!body.contains("test/model"));
    assert!(!body.contains("last in 12.3k"));

    let artifact = test_plan_artifact();
    let version = artifact.version;
    app.restore_plan_state(PlanWorkflowState::Ready { artifact });
    let rows = rendered_rows(&mut app, 120, 20);
    let footer = rows.last().expect("footer");
    let body = rows[..rows.len() - 1].concat();
    assert!(footer.contains("Plan ready"));
    assert!(!footer.contains(&version.to_string()));
    assert!(!body.contains("Plan ready"));
    assert!(body.contains(&version.to_string()));
    assert!(body.contains("j next"));
}

#[test]
fn spacious_and_compact_frames_use_borderless_transcript_gutters_and_gaps() {
    let mut spacious = App::new();
    spacious.seed_history_entry(history_message(Message::user("spacious geometry")));
    let mut terminal = Terminal::new(TestBackend::new(80, 21)).expect("terminal");
    terminal
        .draw(|frame| spacious.render(frame))
        .expect("render spacious frame");
    let buffer = terminal.backend().buffer();
    assert!((0..80).all(|x| buffer[(x, 0)].symbol() == " "));
    assert_eq!(buffer[(2, 1)].symbol(), "┃");
    assert_eq!(buffer[(5, 1)].symbol(), "●");
    assert_eq!(buffer[(2, 14)].symbol(), "╭");
    assert_eq!(buffer[(77, 14)].symbol(), "╮");
    assert_eq!(buffer[(2, 20)].symbol(), "B");
    assert_eq!(buffer[(2, 20)].fg, ZEVRIA_DARK.workflow.build);
    for x in [0, 1, 78, 79] {
        assert_eq!(buffer[(x, 20)].symbol(), " ");
        assert_eq!(buffer[(x, 20)].fg, ZEVRIA_DARK.text.primary);
        assert_eq!(buffer[(x, 20)].bg, ZEVRIA_DARK.surfaces.canvas);
    }
    assert!((0..80).all(|x| buffer[(x, 13)].symbol() == " "));
    assert!((0..80).all(|x| buffer[(x, 17)].symbol() == " "));
    assert!(!rendered_text(&mut spacious, 80, 21).contains("Conversation"));

    let mut compact = App::new();
    compact.seed_history_entry(history_message(Message::user("compact geometry")));
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal
        .draw(|frame| compact.render(frame))
        .expect("render compact frame");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(1, 0)].symbol(), "┃");
    assert_eq!(buffer[(4, 0)].symbol(), "●");
    assert_eq!(buffer[(1, 15)].symbol(), "╭");
    assert_eq!(buffer[(78, 15)].symbol(), "╮");
    assert_eq!(buffer[(1, 19)].symbol(), "B");
    assert_eq!(buffer[(1, 19)].fg, ZEVRIA_DARK.workflow.build);
    for x in [0, 79] {
        assert_eq!(buffer[(x, 19)].symbol(), " ");
        assert_eq!(buffer[(x, 19)].fg, ZEVRIA_DARK.text.primary);
        assert_eq!(buffer[(x, 19)].bg, ZEVRIA_DARK.surfaces.canvas);
    }
}

#[test]
fn startup_frame_owns_the_same_zevria_canvas() {
    let mut terminal = Terminal::new(TestBackend::new(32, 5)).expect("terminal");
    terminal
        .draw(|frame| render_startup_frame(frame, "Connecting…"))
        .expect("startup frame");
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| cell.bg == ZEVRIA_DARK.surfaces.canvas)
    );
    let status = buffer
        .content()
        .iter()
        .find(|cell| cell.symbol() == "C")
        .expect("status text");
    assert_eq!(status.fg, ZEVRIA_DARK.text.primary);
    assert_eq!(buffer[(0, 0)].symbol(), "╭");
    assert_eq!(buffer[(0, 0)].fg, ZEVRIA_DARK.surfaces.border_strong);
}

#[test]
fn interactive_frame_never_falls_back_to_terminal_default_colors() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.handle_event(key(KeyCode::Char('/')));
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("render");
    for cell in terminal.backend().buffer().content() {
        assert!(matches!(cell.fg, Color::Rgb(_, _, _)), "fg: {cell:?}");
        assert!(matches!(cell.bg, Color::Rgb(_, _, _)), "bg: {cell:?}");
    }
}

#[test]
fn transcript_entry_and_tail_accents_follow_semantic_colors() {
    fn gutter_cell(app: &mut App, needle: &str) -> ratatui::buffer::Cell {
        let buffer = rendered_buffer(app, 100, 20);
        let y = (0..buffer.area.height)
            .find(|&y| buffer_row_text(&buffer, y).contains(needle))
            .unwrap_or_else(|| panic!("missing row containing {needle:?}"));
        buffer[(1, y)].clone()
    }

    fn accent_color(app: &mut App, needle: &str) -> Color {
        let cell = gutter_cell(app, needle);
        assert_eq!(cell.symbol(), "┃", "message accent for {needle:?}");
        cell.fg
    }

    let mut user = App::new();
    user.seed_history_entry(history_message(Message::user("user accent")));
    assert_eq!(
        accent_color(&mut user, "user accent"),
        ZEVRIA_DARK.roles.you
    );

    let mut assistant = App::new();
    assistant.seed_history_entry(history_message(Message::assistant("assistant accent")));
    assert_eq!(
        accent_color(&mut assistant, "assistant accent"),
        ZEVRIA_DARK.roles.assistant
    );

    let mut system = App::new();
    system.seed_history_entry(history_message(Message::System {
        content: "system accent".to_string(),
    }));
    assert_eq!(
        accent_color(&mut system, "system accent"),
        ZEVRIA_DARK.roles.system
    );

    let mut subtask = App::new();
    subtask.seed_history_entry(history_message(launch_assistant(
        "accent-subtask",
        "subtask accent",
        "prompt",
    )));
    assert_eq!(
        accent_color(&mut subtask, "subtask launch ?"),
        ZEVRIA_DARK.roles.assistant
    );

    let mut artifact = test_plan_artifact();
    artifact.title = "plan accent title".to_string();
    artifact.markdown = "plan accent body".to_string();
    let mut plan = App::new();
    plan.seed_history_entry(HistoryEntry::PlanArtifact(artifact.clone()));
    assert_eq!(
        accent_color(&mut plan, "plan accent body"),
        ZEVRIA_DARK.workflow.plan
    );

    let mut handoff = App::new();
    handoff.seed_history_entry(HistoryEntry::PlanHandoff(
        PlanHandoff::new(artifact, "accent-source"),
        None,
    ));
    assert_eq!(
        accent_color(&mut handoff, "Approved Plan handoff"),
        ZEVRIA_DARK.workflow.plan
    );

    let start = editable_ensemble_start(
        "accent-ensemble",
        EnsembleWorkflow::Review,
        "ensemble accent",
    );
    let prompt = start.prompt.clone();
    let mut ensemble = idle_app_with_ensemble(start);
    assert_eq!(
        accent_color(&mut ensemble, &prompt.display_projection()),
        ZEVRIA_DARK.workflow.review
    );

    let mut error = App::new();
    error.seed_history_entry(HistoryEntry::Error("error accent".to_string()));
    assert_eq!(
        accent_color(&mut error, "error accent"),
        ZEVRIA_DARK.feedback.error
    );

    let mut divider = App::new();
    divider.seed_history_entry(HistoryEntry::CompactionDivider);
    assert_eq!(
        accent_color(&mut divider, "Context compacted"),
        ZEVRIA_DARK.text.muted
    );

    let mut waiting = App::new();
    start_empty_turn(&mut waiting, TEST_TURN_ID, SessionMode::Build);
    assert_eq!(gutter_cell(&mut waiting, "running…").symbol(), " ");

    let mut streaming = App::new();
    start_empty_turn(&mut streaming, TEST_TURN_ID, SessionMode::Build);
    streaming.reduce_without_effects(SessionEvent::AssistantStreamUpdated {
        turn_id: TEST_TURN_ID,
        snapshot: (Message::assistant("stream accent")).into(),
    });
    assert_eq!(
        accent_color(&mut streaming, "stream accent"),
        ZEVRIA_DARK.roles.assistant
    );
    assert_eq!(gutter_cell(&mut streaming, "streaming ·").symbol(), " ");

    let mut retrying = App::new();
    start_empty_turn(&mut retrying, TEST_TURN_ID, SessionMode::Build);
    retrying.reduce_without_effects(SessionEvent::TurnRetrying {
        call: 1,
        turn_id: TEST_TURN_ID,
        attempt: 2,
        max_attempts: 4,
        retry_after: std::time::Duration::from_millis(500),
        error: "offline".to_string(),
    });
    assert_eq!(gutter_cell(&mut retrying, "reconnecting").symbol(), " ");
    assert_eq!(
        gutter_cell(&mut retrying, "request interrupted:").symbol(),
        " "
    );

    let mut compacting = App::new();
    compacting.reduce_without_effects(SessionEvent::CompactionStarted {
        turn_id: TEST_TURN_ID,
        trigger: CompactionTrigger::Manual,
    });
    assert_eq!(
        gutter_cell(&mut compacting, "Compacting context").symbol(),
        " "
    );
}

#[test]
fn prompt_caption_prefix_flags_multiline_and_tiny_collapse_are_rendered() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("first\nsecond", "first\nsecond".len());
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("render Build prompt");
    let buffer = terminal.backend().buffer();
    let top = (0..80)
        .map(|x| buffer[(x, 14)].symbol())
        .collect::<String>();
    let bottom = (0..80)
        .map(|x| buffer[(x, 17)].symbol())
        .collect::<String>();
    assert!(top.contains("Build"));
    assert!(bottom.contains("multiline"));
    assert_eq!(buffer[(3, 15)].symbol(), "❯");
    assert_eq!(buffer[(3, 15)].fg, ZEVRIA_DARK.workflow.build);
    assert_eq!(buffer[(3, 15)].bg, ZEVRIA_DARK.surfaces.panel);
    assert_eq!(buffer[(5, 15)].symbol(), "f");
    assert_eq!(buffer[(5, 15)].fg, ZEVRIA_DARK.text.primary);
    assert_eq!(buffer[(5, 15)].bg, ZEVRIA_DARK.surfaces.panel);
    assert_eq!(buffer[(5, 16)].symbol(), "s");

    toggle_mode_with_ack(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("render Plan prompt");
    let buffer = terminal.backend().buffer();
    let top = (0..80)
        .map(|x| buffer[(x, 14)].symbol())
        .collect::<String>();
    assert!(top.contains("Plan"));
    assert_eq!(buffer[(3, 15)].fg, ZEVRIA_DARK.workflow.plan);
    assert_eq!(buffer[(3, 15)].bg, ZEVRIA_DARK.surfaces.panel);

    let artifact = test_plan_artifact();
    let mut retained = App::new();
    retained.restore_plan_state(PlanWorkflowState::Planning {
        id: artifact.version.id,
        previous: Some(artifact),
    });
    let rendered = rendered_text(&mut retained, 100, 20);
    assert!(rendered.contains("plan retained"));
    assert!(
        !rendered.contains("locked"),
        "Normal focus is not an editing prohibition"
    );

    let mut tiny = App::new();
    enter_insert(&mut tiny);
    tiny.set_input_for_test("hidden at eight rows", "hidden at eight rows".len());
    let rows = rendered_rows(&mut tiny, 80, 8);
    assert_eq!(rows[5].chars().nth(1), Some('╭'));
    assert_eq!(rows[6].chars().nth(1), Some('╰'));
    assert!(!rows.concat().contains("hidden at eight rows"));
    assert!(!rows.concat().contains('❯'));
    assert!(!cursor_visible_after_render(&mut tiny, 80, 8));
}

#[test]
fn transcript_scrollbar_uses_the_terminal_right_edge() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::assistant(
        (0..40)
            .map(|index| format!("scroll line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    app.set_view_for_test(0, false);
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    terminal
        .draw(|frame| app.render(frame))
        .expect("render scrollbar");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(79, 0)].symbol(), "↑");
    assert_eq!(buffer[(79, 14)].symbol(), "↓");
    assert!((1..14).any(|y| buffer[(79, y)].symbol() == "█"));
    assert!((0..15).all(|y| buffer[(78, y)].symbol() != "█"));
}

#[test]
fn oversized_selection_fills_viewport_edges_without_markers_or_bleed() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::assistant(
        (0..30)
            .map(|index| format!("selected line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    app.select_for_test(cursor(0, 0));

    fn assert_visible_selection_surface(app: &mut App) {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        terminal
            .draw(|frame| app.render(frame))
            .expect("render oversized selection");
        let buffer = terminal.backend().buffer();

        for y in 0..15 {
            assert!((0..79).all(|x| buffer[(x, y)].bg == SELECTION_BG));
            assert_ne!(buffer[(79, y)].bg, SELECTION_BG);
            assert!((0..79).all(|x| {
                !matches!(buffer[(x, y)].symbol(), "┆" | "│" | "┌" | "┐" | "└" | "┘")
            }));
        }
        assert!((0..80).all(|x| buffer[(x, 0)].bg == SELECTION_BG || x == 79));
        assert!((0..80).all(|x| buffer[(x, 14)].bg == SELECTION_BG || x == 79));
        assert!((15..20).all(|y| (0..80).all(|x| buffer[(x, y)].bg != SELECTION_BG)));
    }

    app.set_view_for_test(0, false);
    assert_visible_selection_surface(&mut app);

    app.set_view_for_test(usize::MAX, false);
    assert_visible_selection_surface(&mut app);
}

#[test]
fn footer_thresholds_preserve_tiny_interactive_and_inspect_surfaces() {
    let mut root = App::new();
    let tiny = rendered_rows(&mut root, 40, 4);
    assert!(!tiny.concat().contains("Build · Normal"));
    let threshold = rendered_rows(&mut root, 40, 5);
    assert!(threshold.last().expect("footer").contains("Build · Normal"));

    let mut inspect = App::subtask_inspect("◆ explore · tiny ◐");
    inspect.seed_history_entry(history_message(Message::assistant("only content")));
    let tiny = rendered_rows(&mut inspect, 40, 3);
    assert!(!tiny.concat().contains("◆ explore · tiny ◐"));
    assert!(tiny.concat().contains("only content"));
    assert!(!tiny.concat().contains("Conversation"));
    let threshold = rendered_rows(&mut inspect, 40, 4);
    assert!(
        threshold
            .last()
            .expect("inspect footer")
            .contains("◆ explore · tiny ◐")
    );
}

#[test]
fn inspect_panes_use_the_complete_body_without_a_composer() {
    let mut inspect = App::subtask_inspect("◆ explore · child ✓");
    inspect.seed_history_entry(history_message(Message::assistant("finding")));
    let rows = rendered_rows(&mut inspect, 120, 8);
    assert!(!rows[..7].concat().contains("Conversation"));
    assert!(rows[..7].concat().contains("Assistant"));
    assert!(rows[..7].concat().contains("finding"));
    assert!(rows[7].contains("◆ explore · child ✓"));
    assert!(
        !rows[7].contains("Ctrl+O back"),
        "optional controls are omitted as one compression stage"
    );
    assert!(!rows[..7].concat().contains("Ctrl+O back"));
    assert!(!cursor_visible_after_render(&mut inspect, 120, 8));
}

#[test]
fn status_line_uses_insets_default_background_and_semantic_accents() {
    fn assert_primary(app: &mut App, expected_text: &str, expected_color: Color) {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        terminal
            .draw(|frame| app.render(frame))
            .expect("render semantic status");
        let buffer = terminal.backend().buffer();
        let status_y = buffer.area.height - 1;
        let row = (0..buffer.area.width)
            .map(|x| buffer[(x, status_y)].symbol())
            .collect::<String>();
        assert!(row.contains(expected_text), "missing {expected_text:?}");
        assert_eq!(buffer[(1, status_y)].fg, expected_color);
        assert_eq!(buffer[(1, status_y)].bg, ZEVRIA_DARK.surfaces.canvas);
        assert!(buffer[(1, status_y)].modifier.contains(Modifier::BOLD));
        assert!((0..buffer.area.width).all(|x| {
            let cell = &buffer[(x, status_y)];
            cell.bg == ZEVRIA_DARK.surfaces.canvas && !cell.modifier.contains(Modifier::REVERSED)
        }));
        for x in [0, buffer.area.width - 1] {
            let cell = &buffer[(x, status_y)];
            assert_eq!(cell.symbol(), " ");
            assert_eq!(cell.fg, ZEVRIA_DARK.text.primary);
            assert_eq!(cell.bg, ZEVRIA_DARK.surfaces.canvas);
            assert_eq!(cell.modifier, Modifier::empty());
        }
    }

    let mut build = App::new();
    assert_primary(&mut build, "Build · Normal", ZEVRIA_DARK.workflow.build);

    let mut plan = App::new();
    toggle_mode_with_ack(&mut plan);
    assert_primary(&mut plan, "Plan · Normal", ZEVRIA_DARK.workflow.plan);

    let mut review = configured_app();
    review.reduce_without_effects(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: editable_ensemble_start(
            "semantic-review-status",
            EnsembleWorkflow::Review,
            "review status",
        ),
        resumed: false,
    });
    assert_primary(&mut review, "Review · Waiting", ZEVRIA_DARK.workflow.review);

    let mut inspect = App::subtask_inspect("◆ explore · child ◐");
    assert_primary(&mut inspect, "◆ explore · child ◐", ZEVRIA_DARK.roles.tools);

    let mut warning = App::new();
    warning.reduce_without_effects(SessionEvent::PersistenceChanged {
        path: PathBuf::from("/tmp/session.jsonl"),
        error: Some("read only".to_string()),
    });
    assert_primary(
        &mut warning,
        "Persistence degraded",
        ZEVRIA_DARK.feedback.warning,
    );

    let mut terminal = Terminal::new(TestBackend::new(64, 6)).expect("terminal");
    terminal
        .draw(|frame| warning.render(frame))
        .expect("render warning metadata");
    let buffer = terminal.backend().buffer();
    let status_y = buffer.area.height - 1;
    let detail_x = 1 + u16::try_from("Persistence degraded".len()).expect("detail offset");
    assert_eq!(buffer[(detail_x, status_y)].fg, ZEVRIA_DARK.text.muted);
    assert_eq!(
        buffer[(buffer.area.width - 2, status_y)].fg,
        ZEVRIA_DARK.text.primary
    );
}

#[test]
fn plan_footer_renders_at_threshold_but_height_four_keeps_all_choice_rows_available() {
    let mut app = App::new();
    app.reduce_without_effects(SessionEvent::PlanStateChanged {
        state: PlanWorkflowState::Ready {
            artifact: test_plan_artifact(),
        },
    });
    let tiny = rendered_rows(&mut app, 100, 4);
    assert!(!tiny.concat().contains("Plan ready"));
    assert!(tiny.concat().contains("3. Revise the plan"));

    let threshold = rendered_rows(&mut app, 100, 5);
    assert!(threshold.last().expect("footer").contains("Plan ready"));
    assert!(threshold[..4].concat().contains("3. Revise the plan"));
}

#[test]
fn modal_bounds_protect_then_reveal_the_unchanged_footer() {
    let mut root = App::new();
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    let mut views = test_session_views(root);
    let expected_status = rendered_views_status_cells(&mut views, 50, 5);
    views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request: question_request("footer-overlay"),
    });
    let mut covered = Terminal::new(TestBackend::new(50, 5)).expect("terminal");
    covered
        .draw(|frame| views.render(frame))
        .expect("render question");
    let buffer = covered.backend().buffer();
    assert_eq!(
        bottom_row_cells(buffer),
        expected_status,
        "the question popup must remain above the protected status line"
    );

    assert!(matches!(
        views.handle_event(key(KeyCode::Esc)),
        Some(UiAction::AnswerQuestion { .. })
    ));
    let rows = rendered_views_rows(&mut views, 50, 5);
    assert!(
        rows.last()
            .expect("restored footer")
            .contains("Build · Waiting")
    );
}

#[test]
fn only_selected_content_receives_the_full_width_background_band() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![
            AssistantContent::text("plain item"),
            AssistantContent::text(
                "chosen-start-abcdefghijklmnopqrstuvwxyz-0123456789-ABCDEFGHIJKLMNOPQRSTUVWXYZ-chosen-end",
            ),
        ],
    }));
    app.select_for_test(cursor(0, 1));

    let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let row_text = |y| {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let header_y = (0..buffer.area.height)
        .find(|&y| row_text(y).contains("Assistant"))
        .expect("role header");
    let plain_y = (0..buffer.area.height)
        .find(|&y| row_text(y).contains("plain item"))
        .expect("plain row");
    let selected_rows = (0..buffer.area.height)
        .filter(|&y| (0..39).all(|x| buffer[(x, y)].bg == SELECTION_BG))
        .collect::<Vec<_>>();

    assert!(
        selected_rows.len() >= 2,
        "long selected content should wrap"
    );
    assert!(
        selected_rows
            .iter()
            .any(|&y| row_text(y).contains("chosen-start"))
    );
    for y in selected_rows {
        assert_eq!(buffer[(0, y)].bg, SELECTION_BG);
        assert_eq!(buffer[(38, y)].bg, SELECTION_BG);
        assert_ne!(buffer[(39, y)].bg, SELECTION_BG);
    }
    assert!((0..39).all(|x| buffer[(x, header_y)].bg != SELECTION_BG));
    assert!((0..39).all(|x| buffer[(x, plain_y)].bg != SELECTION_BG));
}

#[test]
fn selected_content_is_scrolled_into_view_without_repinning_follow() {
    let mut app = App::new();
    let tall_content = (0..20)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![
            AssistantContent::text(tall_content),
            AssistantContent::text("chosen tail"),
        ],
    }));
    app.set_view_for_test(0, false);
    app.select_for_test(cursor(0, 1));

    let rendered = rendered_text(&mut app, 30, 8);

    assert!(rendered.contains("chosen tail"));
    assert!(app.view_scroll() > 0);
    assert!(
        !app.view_follow(),
        "selection-driven bottom clamping must not restore follow"
    );
}

#[test]
fn empty_content_still_has_a_selectable_rendered_row() {
    let message = Message::assistant("");
    let mut lines = Vec::new();

    let range = layout_native_message(&message, None, &mut lines, 20, Some(0));

    assert_eq!(range, Some(RowRange::new(1, 2)));
    assert!(lines.len() >= 2);
    assert_eq!(
        lines[1].style.bg,
        Some(ZEVRIA_DARK.surfaces.selection_background)
    );
}

#[test]
fn native_chat_boundaries_render_one_full_width_semantic_separator() {
    let pairs = [
        (
            "user to assistant",
            Message::user("first user"),
            Message::assistant("second assistant"),
        ),
        (
            "assistant to system",
            Message::assistant("first assistant"),
            Message::System {
                content: "second system".to_string(),
            },
        ),
        (
            "system to user",
            Message::System {
                content: "first system".to_string(),
            },
            Message::user("second user"),
        ),
    ];

    for (label, first, second) in pairs {
        let mut app = App::new();
        app.seed_history_entry(history_message(first));
        app.seed_history_entry(history_message(second));

        let buffer = rendered_buffer(&mut app, 60, 14);
        let content = conversation_content_area(&buffer, false);
        let separators = full_width_message_separator_rows(&buffer, false);
        assert_eq!(separators.len(), 1, "{label}");
        let separator_y = separators[0];
        for x in content.x..content.right() {
            let cell = &buffer[(x, separator_y)];
            assert_eq!(cell.symbol(), "─", "{label} at x={x}");
            assert_eq!(cell.fg, ZEVRIA_DARK.surfaces.border, "{label} at x={x}");
        }
    }
}

#[test]
fn user_message_surfaces_cover_wrapping_blank_rows_and_attachments() {
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[1, 2, 3, 255]).unwrap();
    for inspect in [false, true] {
        for width in [28, 80] {
            let mut app = if inspect {
                App::subtask_inspect("user surfaces")
            } else {
                App::new()
            };
            let messages = [
                (
                    Message::User {
                        content: vec![
                            UserContent::text(
                                "a user message with enough words to wrap across several rows in a narrow conversation pane\n\nlast user line",
                            ),
                            image.to_user_content(),
                            UserContent::Document(Document {
                                data: DocumentSourceKind::String("payload".into()),
                                media_type: None,
                                additional_params: None,
                            }),
                        ],
                    },
                    true,
                ),
                (Message::assistant("assistant body"), false),
                (Message::system("system body"), false),
                (Message::user(""), true),
                (
                    Message::User {
                        content: vec![
                            UserContent::text("next user block"),
                            UserContent::text("another user block"),
                        ],
                    },
                    true,
                ),
                (
                    assistant_message(vec![tool_call(
                        "surface-tool",
                        None,
                        "command",
                        json!({"command":"pwd"}),
                    )]),
                    false,
                ),
            ];
            for (message, _) in &messages {
                app.seed_history_entry(history_message(message.clone()));
            }
            let buffer = rendered_buffer(&mut app, width, 80);
            let content = conversation_content_area(&buffer, inspect);
            assert_eq!(app.view_scroll(), 0);
            let entries = app.view_cache().entries();
            let mut y = content.y;
            for (entry, (_, user)) in entries.iter().zip(&messages) {
                assert!(y + (entry.height as u16) < content.bottom());
                for row in y..y + entry.height as u16 {
                    assert_conversation_row_background(
                        &buffer,
                        content,
                        row,
                        if *user {
                            ZEVRIA_DARK.surfaces.panel
                        } else {
                            ZEVRIA_DARK.surfaces.canvas
                        },
                    );
                }
                y += entry.height as u16;
                assert_conversation_row_background(
                    &buffer,
                    content,
                    y,
                    ZEVRIA_DARK.surfaces.canvas,
                );
                y += 1;
            }
            for row in y..content.bottom() {
                assert_blank_conversation_row(&buffer, row);
            }
            let first = &entries[0];
            let text = first.items[0].1;
            assert!(text.len() > 3, "fixture must wrap at width {width}");
            let blank_y = content.y + text.end() as u16 - 2;
            assert!((content.x..content.right()).all(|x| buffer[(x, blank_y)].symbol() == " "));
            assert_eq!(
                entries[3].height, 2,
                "empty user content retains its body row"
            );
            assert_eq!(buffer[(content.x, content.y)].fg, ZEVRIA_DARK.roles.you);
            assert_eq!(
                buffer[(content.x, content.y + text.start() as u16)].fg,
                ZEVRIA_DARK.text.primary
            );
            for (_, attachment) in &first.items[1..] {
                assert_eq!(
                    buffer[(content.x, content.y + attachment.start() as u16)].fg,
                    ZEVRIA_DARK.text.muted
                );
            }
            assert_eq!(full_width_message_separator_rows(&buffer, inspect).len(), 5);
        }
    }
}

#[test]
fn user_message_surfaces_exclude_roleless_diagnostics_and_mixed_role_separators() {
    use crate::presentation::{ConversationEntry, DiagnosticTone, PresentedDiagnostic};
    use selection_tests::plain_block;

    let mut diagnostic = plain_block(1, PresentationRole::User, "");
    diagnostic.role = None;
    diagnostic.kind = PresentationBlockKind::Diagnostic(PresentedDiagnostic {
        label: "trace".into(),
        text: "roleless diagnostic".into(),
        tone: DiagnosticTone::Muted,
    });
    let mut app = App::new();
    app.seed_history_entry(HistoryEntry::Conversation(ConversationEntry {
        header: None,
        blocks: vec![
            plain_block(0, PresentationRole::User, "first user"),
            diagnostic,
            plain_block(2, PresentationRole::User, "second user"),
            plain_block(3, PresentationRole::Assistant, "assistant body"),
            plain_block(4, PresentationRole::System, "system body"),
            plain_block(5, PresentationRole::User, "third user"),
        ],
    }));
    let buffer = rendered_buffer(&mut app, 80, 30);
    let content = conversation_content_area(&buffer, false);
    assert_eq!(app.view_cache().entries()[0].height, 15);
    assert!(buffer_row_text(&buffer, content.y + 3).contains("roleless diagnostic"));
    assert!(buffer_row_text(&buffer, content.y + 5).contains("second user"));
    assert!(buffer_row_text(&buffer, content.y + 13).contains("● You"));
    for y in content.y..content.bottom() {
        assert_conversation_row_background(
            &buffer,
            content,
            y,
            if [0, 1, 4, 5, 13, 14].contains(&(y - content.y)) {
                ZEVRIA_DARK.surfaces.panel
            } else {
                ZEVRIA_DARK.surfaces.canvas
            },
        );
    }
    assert_eq!(
        full_width_message_separator_rows(&buffer, false),
        [6, 9, 12].map(|row| content.y + row)
    );
}

#[test]
fn user_message_surfaces_restore_glyphs_and_styles_after_selection() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::User {
        content: vec![
            UserContent::text("user 界 e\u{301} text\n\nsecond line"),
            UserContent::Document(Document {
                data: DocumentSourceKind::String("payload".into()),
                media_type: None,
                additional_params: None,
            }),
        ],
    }));
    app.seed_history_entry(history_message(Message::assistant("unchanged **answer**")));
    // Reuse the backend to exercise actual selection/deselection redraws.
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let original = terminal.backend().buffer().clone();
    let content = conversation_content_area(&original, false);
    let layout = crate::frame_layout::FrameLayout::compute(
        original.area,
        false,
        crate::frame_layout::LowerSurface::Composer {
            requested_height: 3,
        },
        false,
    );
    for row in content.y..content.y + app.view_cache().entries()[0].height as u16 {
        assert_conversation_row_background(&original, content, row, ZEVRIA_DARK.surfaces.panel);
    }
    for scope in [SelectionScope::Message, SelectionScope::Block] {
        for index in [0, 1] {
            match scope {
                SelectionScope::Message => app.select_message_for_test(cursor(0, index)),
                SelectionScope::Block => app.select_for_test(cursor(0, index)),
            }
            terminal.draw(|frame| app.render(frame)).unwrap();
            let selected = terminal.backend().buffer();
            let rows = app.view_cache().entries()[0].selection.unwrap();
            for y in content.y..content.bottom() {
                let mut x = 0;
                while x < original.area.width {
                    let mut expected = original[(x, y)].clone();
                    if (rows.start()..rows.end()).contains(&usize::from(y - content.y))
                        && (layout.selection_band.x..layout.selection_band.right()).contains(&x)
                    {
                        expected
                            .set_fg(ZEVRIA_DARK.surfaces.selection_foreground)
                            .set_bg(ZEVRIA_DARK.surfaces.selection_background);
                    }
                    assert_eq!(selected[(x, y)], expected, "{scope:?} at ({x}, {y})");
                    x += crate::text::display_width(expected.symbol()).max(1) as u16;
                }
            }
            app.select_for_test(None);
            terminal.draw(|frame| app.render(frame)).unwrap();
            assert_eq!(terminal.backend().buffer(), &original);
        }
    }
}

#[test]
fn user_message_surfaces_clip_when_scrolling_resizing_and_content_width_is_zero() {
    let mut app = App::new();
    for message in [
        Message::assistant("leading answer"),
        Message::user("long user line that wraps in a narrow pane\n\n".repeat(12)),
        Message::assistant("answer row\n".repeat(6)),
        Message::user("another user\nlast line"),
        Message::assistant("last answer"),
    ] {
        app.seed_history_entry(history_message(message));
    }
    for (width, height) in [
        (80, 20),
        (28, 12),
        (12, 8),
        (6, 8),
        (2, 8),
        (0, 8),
        (80, 0),
        (100, 30),
    ] {
        app.handle_event(Event::Resize(width, height));
        app.set_view_for_test(0, false);
        rendered_buffer(&mut app, width, height);
        // Independent of decorations: these fixtures each have a single role.
        let mut user_rows = Vec::new();
        let mut starts = Vec::new();
        for (entry, user) in app
            .view_cache()
            .entries()
            .iter()
            .zip([false, true, false, true, false])
        {
            starts.push(user_rows.len());
            user_rows.extend(std::iter::repeat_n(user, entry.height));
            user_rows.push(false);
        }
        for top in [0, starts[1] + 2, starts[2] - 2, starts[3] + 1, usize::MAX] {
            app.set_view_for_test(top, false);
            let buffer = rendered_buffer(&mut app, width, height);
            let content = conversation_content_area(&buffer, false);
            if width <= 6 {
                assert_eq!(content.width, 0);
            }
            for y in content.y..content.bottom() {
                let row = app.view_scroll() + usize::from(y - content.y);
                assert_conversation_row_background(
                    &buffer,
                    content,
                    y,
                    if user_rows.get(row) == Some(&true) {
                        ZEVRIA_DARK.surfaces.panel
                    } else {
                        ZEVRIA_DARK.surfaces.canvas
                    },
                );
            }
        }
    }
}

#[test]
fn user_message_surfaces_include_body_folds_but_not_aggregate_fold_summaries() {
    for scope in [SelectionScope::Message, SelectionScope::Block] {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user(
            "first line\nsecond line\nthird line",
        )));
        app.seed_history_entry(history_message(Message::assistant("answer")));
        let original = rendered_buffer(&mut app, 80, 20);
        match scope {
            SelectionScope::Message => app.select_message_for_test(cursor(0, 0)),
            SelectionScope::Block => app.select_for_test(cursor(0, 0)),
        }
        app.handle_event(key(KeyCode::Char('z')));
        app.handle_event(key(KeyCode::Char('c')));
        app.select_for_test(None);
        let folded = rendered_buffer(&mut app, 80, 20);
        let content = conversation_content_area(&folded, false);
        assert_eq!(app.view_cache().entries()[0].height, 2);
        assert!(buffer_row_text(&folded, content.y + 1).contains("▸ first line · 2 more rows"));
        for row in content.y..content.y + 2 {
            assert_conversation_row_background(&folded, content, row, ZEVRIA_DARK.surfaces.panel);
        }
        assert_conversation_row_background(
            &folded,
            content,
            content.y + 2,
            ZEVRIA_DARK.surfaces.canvas,
        );
        app.handle_event(key(KeyCode::Char('z')));
        app.handle_event(key(KeyCode::Char('R')));
        assert_eq!(rendered_buffer(&mut app, 80, 20), original);
    }

    // A collapsed run may start at a user entry, but is not a user message.
    let mut app = App::new();
    for message in [
        Message::user("first user"),
        Message::user("second user"),
        Message::assistant("answer"),
    ] {
        app.seed_history_entry(history_message(message));
    }
    app.handle_event(key(KeyCode::Char('z')));
    app.handle_event(key(KeyCode::Char('m')));
    let buffer = rendered_buffer(&mut app, 80, 20);
    let content = conversation_content_area(&buffer, false);
    assert!(buffer_row_text(&buffer, content.y).contains("▸ 2 earlier messages"));
    assert!(app.view_cache().entries()[0].decorations.is_empty());
    for y in content.y..content.bottom() {
        assert_conversation_row_background(&buffer, content, y, ZEVRIA_DARK.surfaces.canvas);
    }
}

#[test]
fn non_chat_artifacts_keep_their_surrounding_gap_rows_blank() {
    let mut artifact = test_plan_artifact();
    artifact.title = "compact plan".to_string();
    artifact.markdown = "plan body".to_string();
    let handoff = PlanHandoff::new(artifact.clone(), "artifact-boundary");
    let mut app = App::subtask_inspect("artifact separator test");
    app.seed_history_entry(history_message(Message::user("before plan")));
    app.seed_history_entry(HistoryEntry::PlanArtifact(artifact));
    app.seed_history_entry(history_message(Message::System {
        content: "before error".to_string(),
    }));
    app.seed_history_entry(HistoryEntry::Error("error boundary".to_string()));
    app.seed_history_entry(history_message(Message::user("before compaction")));
    app.seed_history_entry(HistoryEntry::CompactionDivider);
    app.seed_history_entry(history_message(Message::assistant("before handoff")));
    app.seed_history_entry(HistoryEntry::PlanHandoff(handoff, None));
    app.seed_history_entry(history_message(Message::System {
        content: "trailing system".to_string(),
    }));

    let buffer = rendered_buffer(&mut app, 100, 80);
    assert!(
        full_width_message_separator_rows(&buffer, true).is_empty(),
        "plans, errors, and compaction markers interrupt chat adjacency"
    );
}

#[test]
fn timer_only_tails_reuse_one_blank_history_gap_without_a_leading_gap() {
    for event in [
        None,
        Some(SessionEvent::TurnRetrying {
            call: 1,
            turn_id: TEST_TURN_ID,
            attempt: 2,
            max_attempts: 4,
            retry_after: std::time::Duration::from_secs(4),
            error: "offline".to_string(),
        }),
        Some(SessionEvent::CompactionStarted {
            turn_id: TEST_TURN_ID,
            trigger: CompactionTrigger::Manual,
        }),
        Some(SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (Message::User {
                content: Vec::new(),
            })
            .into(),
        }),
    ] {
        for width in [24, 80] {
            for hidden_history in [false, true] {
                for entry in [
                    None,
                    Some(history_message(Message::user(
                        "committed content that wraps narrowly",
                    ))),
                    Some(HistoryEntry::Error(
                        "committed error that wraps narrowly".into(),
                    )),
                    Some(HistoryEntry::CompactionDivider),
                ] {
                    let mut app = App::new();
                    let selectable = entry
                        .as_ref()
                        .is_some_and(|entry| !matches!(entry, HistoryEntry::CompactionDivider));
                    if let Some(entry) = entry {
                        app.seed_history_entry(entry);
                        if selectable {
                            app.select_for_test(cursor(0, 0));
                        }
                    }
                    if hidden_history {
                        let mut hidden = history_message(Message::assistant("hidden diagnostic"));
                        let HistoryEntry::Conversation(entry) = &mut hidden else {
                            unreachable!()
                        };
                        entry.blocks[0].visibility =
                            crate::presentation::BlockVisibility::Diagnostics;
                        app.seed_history_entry(hidden);
                    }
                    start_timed_tail(&mut app, event.clone());
                    let buffer = rendered_buffer(&mut app, width, 60);
                    let content = conversation_content_area(&buffer, false);
                    let entries_total: usize = app
                        .view_cache()
                        .entries()
                        .iter()
                        .map(|entry| entry.height + usize::from(entry.height > 0))
                        .sum();
                    let status_y = content.y + entries_total as u16;
                    assert_eq!(app.view_scroll(), 0);
                    assert_eq!(buffer[(content.x, status_y)].symbol(), "◐", "{event:?}");
                    assert!(full_width_message_separator_rows(&buffer, false).is_empty());
                    assert!(
                        (content.y..content.bottom())
                            .all(|y| !buffer_row_text(&buffer, y).contains("Assistant"))
                    );
                    if entries_total > 0 {
                        assert_blank_conversation_row(&buffer, status_y - 1);
                        assert_eq!(buffer[(2, status_y - 2)].symbol(), "┃", "exactly one gap");
                    } else {
                        assert_eq!(
                            status_y, content.y,
                            "no visible history means no leading gap"
                        );
                    }
                    let end = app
                        .render_parts()
                        .view
                        .conversation_viewport()
                        .visible_range()
                        .end();
                    for y in status_y..content.y + end as u16 {
                        assert_eq!(buffer[(2, y)].symbol(), " ", "{event:?}, status row {y}");
                        assert!(
                            (0..width - 1)
                                .all(|x| buffer[(x, y)].bg == ZEVRIA_DARK.surfaces.canvas)
                        );
                    }
                    if selectable {
                        let entry = &app.view_cache().entries()[0];
                        let selection = entry.selection.expect("selected committed content");
                        assert!(
                            selection.end() <= entry.height,
                            "selection excludes layout gaps and timing"
                        );
                        assert_eq!(
                            buffer[(content.x, content.y + selection.start() as u16)].bg,
                            ZEVRIA_DARK.surfaces.selection_background
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn real_streamed_messages_keep_chat_separators_and_own_a_blank_status_boundary() {
    for width in [24, 80] {
        for message in [
            Message::assistant("live answer that wraps across several rows"),
            mixed_reasoning_message(),
            Message::assistant(""),
        ] {
            for (entry, is_chat) in [
                (None, false),
                (
                    Some(history_message(Message::user("committed prompt"))),
                    true,
                ),
                (Some(HistoryEntry::Error("committed error".into())), false),
            ] {
                let mut app = App::new();
                if let Some(entry) = entry {
                    app.seed_history_entry(entry);
                    app.select_for_test(cursor(0, 0));
                }
                start_timed_tail(
                    &mut app,
                    Some(SessionEvent::AssistantStreamUpdated {
                        turn_id: TEST_TURN_ID,
                        snapshot: (message.clone()).into(),
                    }),
                );
                let buffer = rendered_buffer(&mut app, width, 60);
                let content = conversation_content_area(&buffer, false);
                let entries_total: usize = app
                    .view_cache()
                    .entries()
                    .iter()
                    .map(|entry| entry.height + usize::from(entry.height > 0))
                    .sum();
                let (stream_lines, stream_height) =
                    app.view_cache().streaming().expect("real streamed message");
                let stream_y = content.y + entries_total as u16;
                let gap_y = stream_y + stream_height as u16;
                assert_eq!(app.view_scroll(), 0);
                assert!(buffer_row_text(&buffer, stream_y).contains("● Assistant"));
                assert_eq!(
                    full_width_message_separator_rows(&buffer, false),
                    if is_chat {
                        vec![stream_y - 1]
                    } else {
                        Vec::new()
                    }
                );
                if entries_total > 0 && !is_chat {
                    assert_blank_conversation_row(&buffer, stream_y - 1);
                }
                for y in stream_y..gap_y {
                    assert_eq!(buffer[(2, y)].symbol(), "┃");
                    assert_eq!(buffer[(2, y)].fg, ZEVRIA_DARK.roles.assistant);
                    assert!(
                        (0..width - 1).all(|x| buffer[(x, y)].bg == ZEVRIA_DARK.surfaces.canvas)
                    );
                }
                assert_blank_conversation_row(&buffer, gap_y);
                assert!(buffer_row_text(&buffer, gap_y + 1).contains("◐ streaming"));
                assert!(
                    !stream_lines
                        .iter()
                        .any(|line| line_text(line).contains("streaming"))
                );
                assert_eq!(
                    stream_height,
                    crate::layout::prepare::wrapped_height(stream_lines, content.width)
                );
                let status_height = crate::layout::prepare::wrapped_height(
                    &[ratatui::text::Line::raw("◐ streaming · 0s")],
                    content.width,
                );
                for y in gap_y + 1..gap_y + 1 + status_height as u16 {
                    assert_eq!(buffer[(2, y)].symbol(), " ", "status row {y}");
                    assert!(
                        (0..width - 1).all(|x| buffer[(x, y)].bg == ZEVRIA_DARK.surfaces.canvas)
                    );
                }
                let expected_total = entries_total + stream_height + 1 + status_height;
                assert_eq!(
                    app.render_parts()
                        .view
                        .conversation_viewport()
                        .visible_range()
                        .end(),
                    expected_total
                );
            }
        }
    }
}

#[test]
fn wrapped_tail_boundaries_project_correctly_when_scrolled_or_bottom_followed() {
    for event in [
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 1,
        },
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (Message::assistant(
                "a streamed message with enough words to wrap over several rows\nlast streamed row",
            ))
            .into(),
        },
        SessionEvent::TurnRetrying {
            call: 1,
            turn_id: TEST_TURN_ID,
            attempt: 2,
            max_attempts: 5,
            retry_after: std::time::Duration::from_secs(4),
            error: "**literal error** with enough words to wrap\nlast error row".into(),
        },
    ] {
        for width in [20, 30, 48] {
            let mut app = App::new();
            app.seed_history_entry(history_message(Message::user(
                "history content that wraps across rows",
            )));
            start_timed_tail(&mut app, Some(event.clone()));
            // Spacious frames have one less content column at the same width.
            let reference = rendered_buffer(&mut app, width + 1, 80);
            let reference_content = conversation_content_area(&reference, false);
            assert_eq!(app.view_scroll(), 0);
            let total = app
                .render_parts()
                .view
                .conversation_viewport()
                .visible_range()
                .end();
            let history_end = app.view_cache().entries()[0].height;
            let stream_height = app.view_cache().streaming().map_or(0, |(_, height)| height);
            let gap = if stream_height > 0 {
                history_end + 1 + stream_height
            } else {
                history_end
            };
            assert_blank_conversation_row(&reference, reference_content.y + gap as u16);
            let status_start = gap + 1;
            for row in status_start..total {
                assert_eq!(
                    reference[(2, reference_content.y + row as u16)].symbol(),
                    " ",
                    "{event:?}, width {width}, status row {row}"
                );
            }
            if matches!(event, SessionEvent::TurnRetrying { .. }) {
                let status_text = (reference_content.y + status_start as u16
                    ..reference_content.y + total as u16)
                    .map(|y| {
                        (reference_content.x..reference_content.right())
                            .map(|x| reference[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let status_text = status_text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert_eq!(
                    status_text,
                    "◐ ⚠ reconnecting (attempt 2/5) · next attempt in 4s · 0s request interrupted: **literal error** with enough words to wrap last error row"
                );
            }
            for height in [1, 4, 8, 12, 20] {
                for top in [0, gap - 1, gap, gap + 1, gap + 2, usize::MAX] {
                    app.set_view_for_test(top, top == usize::MAX);
                    let buffer = rendered_buffer(&mut app, width, height);
                    let content = conversation_content_area(&buffer, false);
                    assert_eq!(content.width, reference_content.width);
                    let expected_top = top.min(total.saturating_sub(usize::from(content.height)));
                    assert_eq!(app.view_scroll(), expected_top);
                    assert_eq!(app.view_follow(), top == usize::MAX);
                    for row in 0..content.height {
                        let logical_row = expected_top + usize::from(row);
                        if logical_row >= total {
                            break;
                        }
                        let y = content.y + row;
                        let reference_y = reference_content.y + logical_row as u16;
                        for col in 0..content.width {
                            assert_eq!(
                                buffer[(content.x + col, y)],
                                reference[(reference_content.x + col, reference_y)],
                                "{event:?}, {width}x{height}, top {top}, row {logical_row}, col {col}"
                            );
                        }
                        assert_eq!(
                            buffer[(1, y)],
                            reference[(2, reference_y)],
                            "projected gutter at row {logical_row}"
                        );
                        if logical_row == gap {
                            assert_blank_conversation_row(&buffer, y);
                        } else if logical_row >= status_start {
                            assert_eq!(
                                buffer[(1, y)].symbol(),
                                " ",
                                "{event:?}, {width}x{height}, top {top}, status row {logical_row}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn acp_role_transitions_separate_groups_without_splitting_attached_blocks() {
    let (mut app, mut transcript) = acp_transcript_app();
    for event in [
        AgentRunEvent::Prompt {
            text: "review".to_string(),
            continuation: false,
            repair: None,
        },
        AgentRunEvent::Thought {
            text: "thinking".to_string(),
            message_id: Some("thought".to_string()),
        },
        AgentRunEvent::ToolCall {
            id: "separator-tool".to_string(),
            title: "Inspect source".to_string(),
            kind: "read".to_string(),
            status: "completed".to_string(),
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
        },
        AgentRunEvent::Plan {
            plan: AgentStructuredPlan {
                plan_id: Some("separator-plan".to_string()),
                markdown: None,
                entries: vec![AgentPlanEntry {
                    content: "Keep attached".to_string(),
                    priority: "high".to_string(),
                    status: "in_progress".to_string(),
                }],
            },
        },
        AgentRunEvent::Stderr {
            text: "diagnostic stays attached".to_string(),
        },
        AgentRunEvent::AgentMessage {
            text: "assistant continuation".to_string(),
            message_id: Some("assistant-one".to_string()),
        },
        AgentRunEvent::UserMessage {
            text: "user follow-up".to_string(),
            message_id: Some("user-follow-up".to_string()),
        },
        AgentRunEvent::Protocol {
            direction: zevria_workflow::AgentProtocolDirection::AgentToClient,
            json: "diagnostic packet".to_string(),
        },
        AgentRunEvent::AgentMessage {
            text: "assistant response".to_string(),
            message_id: Some("assistant-two".to_string()),
        },
    ] {
        apply_agent_event(&mut app, &mut transcript, event);
    }
    app.handle_event(key(KeyCode::Char('d')));

    let buffer = rendered_buffer(&mut app, 120, 60);
    let separators = full_width_message_separator_rows(&buffer, true);
    assert_eq!(
        separators.len(),
        0,
        "ACP role transitions use quiet spacing, not horizontal rules"
    );
    let rendered = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<String>();
    for attached in [
        "thinking",
        "Inspect source",
        "Keep attached",
        "diagnostic stays attached",
        "assistant continuation",
    ] {
        assert!(rendered.contains(attached), "missing {attached:?}");
    }
}

#[test]
fn internal_role_separator_stays_outside_selection_and_cached_row_ranges() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "prompt".to_string(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::AgentMessage {
            text: "selected answer".to_string(),
            message_id: Some("answer".to_string()),
        },
    );
    app.select_for_test(cursor(0, 1));

    let buffer = rendered_buffer(&mut app, 60, 14);
    let content = conversation_content_area(&buffer, true);
    let separators = full_width_message_separator_rows(&buffer, true);
    assert!(separators.is_empty());
    for x in content.x..content.right() {
        let cell = &buffer[(x, content.y + 2)];
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.bg, ZEVRIA_DARK.surfaces.canvas);
    }

    let entry = &app.view_cache().entries()[0];
    assert_eq!(entry.lines.len(), 5);
    assert_eq!(entry.height, 5);
    assert_eq!(entry.selection, Some(RowRange::new(4, 5)));
    let selected_y = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .contains("selected answer")
        })
        .expect("selected answer row");
    assert_eq!(
        buffer[(content.x, selected_y)].bg,
        ZEVRIA_DARK.surfaces.selection_background
    );

    let rebuilds = app.view_cache().rebuilds;
    let block_rebuilds = app.view_cache().block_rebuilds;
    let _ = rendered_buffer(&mut app, 60, 14);
    assert_eq!(app.view_cache().rebuilds, rebuilds);
    assert_eq!(app.view_cache().block_rebuilds, block_rebuilds);
}

#[test]
fn renders_assistant_table_as_grid() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(Text::new(
            "| Name | Score |\n|------|-------|\n| alpha | 1 |",
        ))],
    }));

    let text = rendered_text(&mut app, 70, 30);
    // Header labels, cell values, and the column separator all reach the
    // rendered buffer through the full markdown → Paragraph path.
    for expected in ["Name", "Score", "alpha", "│", "┼"] {
        assert!(
            text.contains(expected),
            "buffer missing {expected:?} in {text:?}"
        );
    }
}

#[test]
fn wrapped_list_items_hang_in_the_rendered_buffer() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(Text::new(
            "- alpha beta gamma delta epsilon zeta eta theta",
        ))],
    }));

    // 20 columns wide → 18 inside the border, so the item must wrap.
    let text = rendered_text(&mut app, 20, 20);
    // The bullet stays on the first row; continuations start at the
    // content column (4 spaces after the left border), not at column 0.
    for expected in [
        "┃    • alpha",
        "┃      beta",
        "┃      gamma",
        "┃      delta",
        "┃      epsilon",
        "┃      zeta eta",
        "┃      theta",
    ] {
        assert!(
            text.contains(expected),
            "buffer missing {expected:?} in {text:?}"
        );
    }
}

#[test]
fn multiline_list_items_hang_in_the_rendered_buffer() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(Text::new(
            "- first line\n  second line",
        ))],
    }));
    app.select_for_test(cursor(0, 0));

    let text = rendered_text(&mut app, 20, 20);
    assert!(
        text.contains("┃    • first"),
        "rendered transcript: {text:?}"
    );
    assert!(
        text.contains("┃      second"),
        "rendered transcript: {text:?}"
    );
    assert!(
        !text.contains("┃second line"),
        "continuation reached the accent rail"
    );

    // The narrower borderless content width wraps both Markdown source rows.
    // Selection geometry uses the same cached wrapped-height calculation.
    let entry = &app.view_cache().entries()[0];
    assert_eq!(entry.lines.len(), 5);
    assert_eq!(entry.height, 5);
    assert_eq!(entry.selection, Some(RowRange::new(1, 5)));
}

#[test]
fn wide_tables_stay_aligned_in_the_rendered_buffer() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(Text::new(
            "| Name | Description |\n|------|-------------|\n| a | one two three four |",
        ))],
    }));

    // 20 columns wide → 18 inside the border: the Description column is
    // narrowed and its cell wraps within the column, so every table line
    // fits the pane and the `│` separators stay vertically aligned.
    let text = rendered_text(&mut app, 20, 20);
    for expected in [
        "┃  Name │ Descr",
        "┃       │ iptio",
        "┃       │ n",
        "┃  ─────┼──────",
        "┃  a    │ one",
        "┃       │ two",
        "┃       │ three",
        "┃       │ four",
    ] {
        assert!(
            text.contains(expected),
            "buffer missing {expected:?} in {text:?}"
        );
    }
}

#[test]
fn layout_cache_reuses_unchanged_entries() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "fc_1",
                Some("call_1"),
                "command",
                json!({}),
            )]),
        },
    );
    app.seed_history_entry(history_message(Message::user("hello")));

    rendered_text(&mut app, 80, 12);
    let after_first = app.view_cache().rebuilds;
    assert_eq!(after_first, 2, "the first frame renders both entries");

    rendered_text(&mut app, 80, 12);
    assert_eq!(
        app.view_cache().rebuilds,
        after_first,
        "an unchanged frame rebuilds nothing"
    );

    // Finishing the tool call re-renders only the entry holding it.
    apply_turn_event(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: tool_result_message("fc_1", Some("call_1"), "command", "done"),
        },
    );
    rendered_text(&mut app, 80, 12);
    assert_eq!(app.view_cache().rebuilds, after_first + 1);

    // A width change re-renders everything.
    rendered_text(&mut app, 60, 12);
    assert_eq!(app.view_cache().rebuilds, after_first + 3);
}

#[test]
fn acp_layout_cache_rebuilds_only_the_changed_semantic_block() {
    let (mut app, mut transcript) = acp_transcript_app();
    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Prompt {
            text: "review".to_string(),
            continuation: false,
            repair: None,
        },
    );
    apply_agent_preview(
        &mut app,
        &mut transcript,
        AgentRunEvent::AgentMessage {
            text: "first snapshot".to_string(),
            message_id: Some("answer".to_string()),
        },
    );

    rendered_text(&mut app, 100, 20);
    let initial_blocks = app.view_cache().block_rebuilds;
    assert_eq!(initial_blocks, 2);

    apply_agent_preview(
        &mut app,
        &mut transcript,
        AgentRunEvent::AgentMessage {
            text: "second snapshot".to_string(),
            message_id: Some("answer".to_string()),
        },
    );
    rendered_text(&mut app, 100, 20);
    assert_eq!(
        app.view_cache().block_rebuilds,
        initial_blocks + 1,
        "a stream revision reparses only its active Markdown block"
    );

    apply_agent_event(
        &mut app,
        &mut transcript,
        AgentRunEvent::Status {
            status: AgentRunStatus::Running,
            detail: None,
        },
    );
    rendered_text(&mut app, 100, 20);
    assert_eq!(
        app.view_cache().block_rebuilds,
        initial_blocks + 1,
        "a hidden diagnostic does not invalidate polished content"
    );

    app.handle_event(key(KeyCode::Char('d')));
    rendered_text(&mut app, 100, 20);
    assert_eq!(
        app.view_cache().block_rebuilds,
        initial_blocks + 2,
        "showing diagnostics renders only the newly visible block"
    );
}

#[test]
fn moving_the_selection_repositions_and_clears_the_background_band() {
    let mut app = App::new();
    app.seed_history_entry(history_message(Message::Assistant {
        id: None,
        content: vec![
            AssistantContent::text("first item"),
            AssistantContent::text("second item"),
        ],
    }));

    fn row_has_selection_background(app: &mut App, needle: &str) -> bool {
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height).any(|y| {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            row.contains(needle)
                && (0..39).all(|x| buffer[(x, y)].bg == SELECTION_BG)
                && buffer[(39, y)].bg != SELECTION_BG
        })
    }

    app.select_for_test(cursor(0, 0));
    assert!(row_has_selection_background(&mut app, "first item"));
    assert!(!row_has_selection_background(&mut app, "second item"));

    app.handle_event(key(KeyCode::Char('j')));
    assert_eq!(app.selection(), cursor(0, 1));
    assert!(row_has_selection_background(&mut app, "second item"));
    assert!(!row_has_selection_background(&mut app, "first item"));

    app.handle_event(key(KeyCode::Esc));
    assert_eq!(app.selection_scope(), Some(SelectionScope::Message));
    assert!(row_has_selection_background(&mut app, "first item"));
    assert!(row_has_selection_background(&mut app, "second item"));
    app.handle_event(key(KeyCode::Esc));
    assert!(!row_has_selection_background(&mut app, "first item"));
    assert!(!row_has_selection_background(&mut app, "second item"));
}

#[test]
fn streamed_deltas_extend_the_rendered_tail() {
    let mut app = App::new();
    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (assistant_message(vec![
                AssistantContent::Reasoning(Reasoning::summaries(vec!["pondering".to_string()])),
                AssistantContent::text("part one"),
            ]))
            .into(),
        },
    );
    let first = rendered_text(&mut app, 40, 12);
    assert!(first.contains("pondering"));
    assert!(first.contains("part one"));

    // The next snapshot extends the trailing text block, as streaming does.
    apply_turn_event(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (assistant_message(vec![
                AssistantContent::Reasoning(Reasoning::summaries(vec!["pondering".to_string()])),
                AssistantContent::text("part one and part two"),
            ]))
            .into(),
        },
    );
    let second = rendered_text(&mut app, 40, 12);
    assert!(second.contains("part two"));

    apply_turn_event(
        &mut app,
        SessionEvent::StreamCleared {
            turn_id: TEST_TURN_ID,
        },
    );
    let cleared = rendered_text(&mut app, 40, 12);
    assert!(!cleared.contains("part one"));
}

#[test]
fn busy_without_streaming_shows_roleless_running_status() {
    let mut app = App::new();
    assert!(app.begin_operation_for_test(OperationKind::Submit, SessionMode::Build));
    let buffer = rendered_buffer(&mut app, 40, 8);
    let rendered = (0..buffer.area.height)
        .map(|y| buffer_row_text(&buffer, y))
        .collect::<String>();
    assert!(!rendered.contains("Assistant"));
    assert!(rendered.contains("◐ running… · 0s"));
    let content = conversation_content_area(&buffer, false);
    assert_eq!(buffer[(1, content.y)].symbol(), " ");
}

#[test]
fn scrolled_viewport_shows_the_expected_window() {
    let mut app = App::new();
    for index in 0..12 {
        app.seed_history_entry(history_message(Message::user(format!("msg {index:02}"))));
    }
    // Each entry is three rows — header, text, separator — so the transcript
    // is 36 rows against a viewport of 8 (height 10 minus the border).
    app.set_view_for_test(0, false);

    let top = rendered_text(&mut app, 30, 10);
    assert!(top.contains("msg 00"));
    assert!(!top.contains("msg 11"));

    app.set_view_for_test(15, false);
    let middle = rendered_text(&mut app, 30, 10);
    assert!(middle.contains("msg 05"));
    assert!(!middle.contains("msg 00"));
    assert!(!middle.contains("msg 11"));

    app.set_view_for_test(300, false);
    let bottom = rendered_text(&mut app, 30, 10);
    assert!(bottom.contains("msg 11"), "scroll clamps to the bottom");
    assert!(!bottom.contains("msg 00"));
}

#[test]
fn workspace_header_preserves_identity_across_panes_and_global_modals() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join(".git")).expect("git directory");
    std::fs::write(workspace.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let mut views = test_session_views_in(App::new(), workspace.path().to_path_buf());
    let workspace_display = expected_workspace_display(workspace.path());
    let width = workspace_header_test_width(&workspace_display, " main", 2).max(160);

    let root_header = rendered_views_row_cells(&mut views, width, 24, 0);
    assert_workspace_header_cells(
        &root_header,
        &workspace_display,
        " main",
        2,
        ZEVRIA_DARK.workflow.build,
    );
    let root_rule = rendered_views_row_cells(&mut views, width, 24, 1);
    assert_workspace_header_rule(&root_rule);

    views.restore_child(
        SubtaskId::new("header-child"),
        Some(child_launch("header-child", "header inspection")),
        vec![TranscriptItem::Message(Message::user("inspect"))],
    );
    assert_eq!(views.handle_event(ctrl('i')), None);
    let inspect_header = rendered_views_row_cells(&mut views, width, 24, 0);
    assert_eq!(
        cell_row_text(&inspect_header),
        cell_row_text(&root_header),
        "subtask inspection must retain the same workspace identity"
    );
    assert_workspace_header_cells(
        &inspect_header,
        &workspace_display,
        " main",
        2,
        ZEVRIA_DARK.roles.tools,
    );
    assert_eq!(
        inspect_header[2].fg,
        rendered_views_status_cells(&mut views, width, 24)[2].fg
    );
    assert_eq!(
        rendered_views_row_cells(&mut views, width, 24, 1),
        root_rule,
        "subtask inspection must retain the workspace header rule"
    );

    assert_eq!(views.handle_event(ctrl('o')), None);
    views.open_session_picker(vec![session_summary("header-picker", None)]);
    assert_eq!(
        rendered_views_row_cells(&mut views, width, 24, 0),
        root_header,
        "the picker must remain below the protected header"
    );
    assert_eq!(
        rendered_views_row_cells(&mut views, width, 24, 1),
        root_rule,
        "the picker must remain below the protected header rule"
    );
    assert_eq!(views.handle_event(key(KeyCode::Esc)), None);

    views.apply(SessionEvent::QuestionAsked {
        turn_id: TEST_TURN_ID,
        request: question_request("header-question"),
    });
    assert_eq!(
        rendered_views_row_cells(&mut views, width, 24, 0),
        root_header,
        "the question modal must remain below the protected header"
    );
    assert_eq!(
        rendered_views_row_cells(&mut views, width, 24, 1),
        root_rule,
        "the question modal must remain below the protected header rule"
    );
}

#[test]
fn workspace_header_follows_idle_mode_with_composer_and_status() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join(".git")).expect("git directory");
    std::fs::write(workspace.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let mut views = test_session_views_in(App::new(), workspace.path().to_path_buf());
    let workspace_display = expected_workspace_display(workspace.path());
    let width = workspace_header_test_width(&workspace_display, " main", 2).max(160);
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).expect("terminal");

    for (index, (mode, accent)) in [
        ("Build", ZEVRIA_DARK.workflow.build),
        ("Plan", ZEVRIA_DARK.workflow.plan),
        ("Build", ZEVRIA_DARK.workflow.build),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            let previous = views.root().next_mode();
            let Some(UiAction::SetMode { request_id, mode }) =
                views.handle_event(key(KeyCode::BackTab))
            else {
                panic!("idle mode shortcut must request management selection");
            };
            assert_eq!(views.root().next_mode(), previous);
            assert!(views.root().mode_selection_pending());
            views.apply(SessionEvent::ModeResult {
                request_id,
                result: zevria_session_api::ModeSelectionResult::Accepted {
                    mode,
                    changed: true,
                },
            });
            assert!(!views.root().is_busy());
        }
        terminal
            .draw(|frame| views.render(frame))
            .expect("render mode");
        let buffer = terminal.backend().buffer();
        let header = (0..width)
            .map(|x| buffer[(x, 0)].clone())
            .collect::<Vec<_>>();
        assert_workspace_header_cells(&header, &workspace_display, " main", 2, accent);
        let rule = (0..width)
            .map(|x| buffer[(x, 1)].clone())
            .collect::<Vec<_>>();
        assert_workspace_header_rule(&rule);
        let caption_y = (2..23)
            .find(|&y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains(&format!(" {mode} ╮"))
            })
            .expect("composer mode caption");
        let caption_x = (2..width - 2)
            .find(|&x| buffer[(x, caption_y)].symbol() == &mode[..1])
            .expect("composer mode cell");
        assert_eq!(buffer[(caption_x, caption_y)].fg, accent);
        assert_eq!(buffer[(2, 23)].fg, accent);
        assert_eq!(buffer[(2, 23)].modifier, Modifier::BOLD);
    }
}

#[test]
fn workspace_header_and_rule_follow_resize_thresholds_and_narrow_widths() {
    let workspace = deterministic_test_workspace().join("zevria");
    assert!(workspace.is_absolute());
    let workspace_display = expected_workspace_display(&workspace);
    let wide_width = workspace_header_test_width(&workspace_display, "", 2).max(120);
    let mut views = test_session_views_in(App::new(), workspace);
    for (width, height) in [
        (wide_width, 21),
        (wide_width, 20),
        (wide_width, 6),
        (wide_width, 5),
        (10, 21),
        (10, 20),
        (10, 6),
        (wide_width, 21),
    ] {
        assert_eq!(views.handle_event(Event::Resize(width, height)), None);
        let rows = rendered_views_rows(&mut views, width, height);
        if height < 6 {
            assert!(rows.iter().all(|row| !row.contains('')));
            continue;
        }
        let gutter = if height >= 21 { 2 } else { 1 };
        assert!(rows[0].starts_with(&format!("{} ", " ".repeat(gutter))));
        if width == 10 {
            assert!(rows[0].contains('…'));
        } else {
            assert!(rows[0].contains(&workspace_display));
        }
        if height >= 21 {
            assert_workspace_header_rule(&rendered_views_row_cells(&mut views, width, height, 1));
        } else {
            assert!(!rows[1].contains("────"));
        }
    }
}

#[test]
fn workspace_header_icon_visibility_respects_inset_width() {
    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git directory");
    std::fs::write(
        workspace.join(".git/HEAD"),
        "ref: refs/heads/very-long-branch\n",
    )
    .expect("HEAD");
    let mut views = test_session_views_in(App::new(), workspace);

    for height in [6, 20, 21] {
        let gutter = if height >= 21 { 2 } else { 1 };
        let padding = " ".repeat(usize::from(gutter));
        for (content_width, expected) in [
            (0, ""),
            (1, "…"),
            (2, "…e"),
            (3, " …"),
            (4, " …e"),
            (5, " …ce"),
            (6, " …ace"),
            (7, " …pace"),
            (8, " …  v…"),
            (9, " …e  v…"),
            (10, " …ce  v…"),
        ] {
            let width = content_width + 2 * gutter;
            assert_eq!(views.handle_event(Event::Resize(width, height)), None);
            let rows = rendered_views_rows(&mut views, width, height);
            assert_eq!(
                rows[0],
                format!("{padding}{expected}{padding}"),
                "frame {width}x{height}"
            );
            assert!(!rows[1].contains(['', '']));
        }
    }
}

#[test]
fn workspace_header_renders_detached_and_non_git_workspaces() {
    for (head, git_label) in [
        (None, ""),
        (
            Some("0123456789abcdef0123456789abcdef01234567\n"),
            " detached@01234567",
        ),
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        if let Some(head) = head {
            std::fs::create_dir_all(workspace.path().join(".git")).expect("git directory");
            std::fs::write(workspace.path().join(".git/HEAD"), head).expect("HEAD");
        }
        let mut views = test_session_views_in(App::new(), workspace.path().to_path_buf());
        let workspace_display = expected_workspace_display(workspace.path());
        let width = workspace_header_test_width(&workspace_display, git_label, 2).max(160);
        assert_workspace_header_cells(
            &rendered_views_row_cells(&mut views, width, 24, 0),
            &workspace_display,
            git_label,
            2,
            ZEVRIA_DARK.workflow.build,
        );
    }
}

#[test]
fn workspace_header_refreshes_after_root_tool_results_and_focus_regain() {
    let workspace = tempfile::tempdir().expect("workspace");
    let head = workspace.path().join(".git/HEAD");
    std::fs::create_dir_all(head.parent().expect("git directory")).expect("git directory");
    std::fs::write(&head, "ref: refs/heads/main\n").expect("initial HEAD");

    let mut root = App::new();
    start_empty_turn(&mut root, TEST_TURN_ID, SessionMode::Build);
    let mut views = test_session_views_in(root, workspace.path().to_path_buf());
    let workspace_display = expected_workspace_display(workspace.path());
    let width = workspace_header_test_width(&workspace_display, " focus-regained", 1).max(120);
    assert_workspace_header_cells(
        &rendered_views_row_cells(&mut views, width, 20, 0),
        &workspace_display,
        " main",
        1,
        ZEVRIA_DARK.workflow.build,
    );

    std::fs::write(&head, "ref: refs/heads/tool-result\n").expect("updated HEAD");
    let unrefreshed = rendered_views_row_cells(&mut views, width, 20, 0);
    assert_workspace_header_cells(
        &unrefreshed,
        &workspace_display,
        " main",
        1,
        ZEVRIA_DARK.workflow.build,
    );
    views.apply(SessionEvent::ToolResults {
        turn_id: TEST_TURN_ID,
        message: tool_result_message("branch-refresh", None, "command", "done"),
        metadata: Vec::new(),
    });
    assert_workspace_header_cells(
        &rendered_views_row_cells(&mut views, width, 20, 0),
        &workspace_display,
        " tool-result",
        1,
        ZEVRIA_DARK.workflow.build,
    );

    std::fs::write(&head, "ref: refs/heads/focus-regained\n").expect("focus HEAD");
    assert_eq!(views.handle_event(Event::FocusGained), None);
    assert_workspace_header_cells(
        &rendered_views_row_cells(&mut views, width, 20, 0),
        &workspace_display,
        " focus-regained",
        1,
        ZEVRIA_DARK.workflow.build,
    );
}
