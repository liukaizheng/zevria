use super::*;
use ratatui::{Terminal, backend::TestBackend};
use zevria_foundation::QuestionOption;

const CORE_DETAIL: &str =
    "Build the core crate first.\n\n```rust\nfn build_core() -> bool {\n    true\n}\n```";

fn request() -> QuestionRequest {
    let cases = (1..=30)
        .map(|i| format!("    run_case({i});\n"))
        .collect::<String>();
    QuestionRequest {
        id: QuestionRequestId::new("preview"),
        questions: vec![QuestionPrompt {
            id: "targets".into(),
            header: "Targets".into(),
            question: "Which crates should the release cover?".into(),
            options: vec![
                QuestionOption {
                    label: "Core".into(),
                    description: "Build the shared runtime first.".into(),
                    detail: Some(CORE_DETAIL.into()),
                },
                QuestionOption {
                    label: "TUI".into(),
                    description: "Exercise the terminal test suite.".into(),
                    detail: Some(format!(
                        "Run the terminal cases in order.\n\n```rust\nfn build_tui() {{\n{cases}}}\n```\n\nThe terminal tests are ready."
                    )),
                },
                QuestionOption {
                    label: "Desktop".into(),
                    description: "Include the desktop crate.".into(),
                    detail: None,
                },
            ],
            kind: QuestionPromptKind::MultiSelect {
                min_selections: Some(1),
                max_selections: None,
                allow_other: true,
            },
            required: true,
            default: None,
        }],
        source_label: None,
        dismissible: true,
        allow_choice_notes: true,
    }
}

fn render(dialog: &mut QuestionDialog, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| dialog.render(frame, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn rows(buffer: &Buffer) -> impl Iterator<Item = String> + '_ {
    buffer
        .content()
        .chunks(usize::from(buffer.area.width).max(1))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
}

fn rendered_text(buffer: &Buffer) -> String {
    rows(buffer).collect::<Vec<_>>().join("\n")
}

fn press(dialog: &mut QuestionDialog, key: KeyCode) -> QuestionDialogAction {
    dialog.handle_key(key, KeyModifiers::NONE)
}

#[test]
fn preview_layouts_keep_syntax_foreground_outside_selection_band() {
    for (width, height, mode) in [
        (120, 30, QuestionLayoutMode::SideBySide),
        (80, 24, QuestionLayoutMode::Stacked),
    ] {
        let mut dialog = QuestionDialog::new(TurnId::new(1), request());
        let buffer = render(&mut dialog, width, height);
        let text = rendered_text(&buffer);
        println!("{width}×{height}\n{text}");
        let layout = dialog.state().last_layout.unwrap();
        assert_eq!(layout.mode, mode);
        assert_eq!(layout.inner_width, width * 92 / 100 - 4);
        let (detail_width, _) = layout.detail_size.unwrap();
        assert!(text.contains("Build the core crate first."), "{text}");
        assert!(text.contains("Space toggle"), "{text}");
        let keyword = buffer
            .content()
            .windows(2)
            .position(|pair| {
                pair[0].symbol() == "f"
                    && pair[1].symbol() == "n"
                    && pair[0].fg == theme().syntax.keyword
                    && pair[1].fg == theme().syntax.keyword
            })
            .expect("the preview preserves the Rust keyword style");
        let bg = theme().surfaces.selection_background;
        assert!(
            buffer.content().iter().any(|cell| cell.bg == bg),
            "the list keeps its focus band"
        );
        let display_rows = buffer.content().chunks(usize::from(width));
        if mode == QuestionLayoutMode::SideBySide {
            let popup_x = (width - question_width(buffer.area, true)) / 2;
            let detail_x = usize::from(popup_x + 2 + layout.inner_width - detail_width);
            assert!(keyword % usize::from(width) >= detail_x);
            assert!(
                display_rows
                    .flat_map(|row| &row[detail_x..])
                    .all(|cell| cell.bg != bg)
            );
            assert!(text.contains('│'));
        } else {
            let divider_y = text
                .lines()
                .position(|row| row.contains("── Core ─"))
                .unwrap();
            assert!(keyword / usize::from(width) > divider_y);
            assert!(
                display_rows
                    .skip(divider_y)
                    .flatten()
                    .all(|cell| cell.bg != bg)
            );
        }
        assert_ne!(buffer.content()[keyword].bg, bg);
        assert!(!text.contains("PgDn scroll"));
        press(&mut dialog, KeyCode::Down);
        let long_preview = render(&mut dialog, width, height);
        let long_text = rendered_text(&long_preview);
        assert!(
            long_text.contains("PgDn scroll") && long_text.contains("Space toggle"),
            "{long_text}"
        );
        assert_eq!(
            dialog.state().last_layout.unwrap().popup_height,
            layout.popup_height
        );
    }
}

#[test]
fn paging_prefers_the_overflowing_preview_and_focus_resets_its_scroll() {
    let mut request = request();
    request.questions[0]
        .options
        .extend((0..6).map(|index| QuestionOption {
            label: format!("Choice {index}"),
            description: "An extra choice.".into(),
            detail: None,
        }));
    let mut dialog = QuestionDialog::new(TurnId::new(1), request);
    render(&mut dialog, 120, 12);
    assert!(dialog.state().viewport.overflows());
    press(&mut dialog, KeyCode::Down);
    let start = rendered_text(&render(&mut dialog, 120, 12));
    assert!(start.contains("fn build_tui()"));
    assert!(dialog.state().detail.scrollable);
    let list_top = dialog.state().viewport.top();
    press(&mut dialog, KeyCode::PageDown);
    let next = rendered_text(&render(&mut dialog, 120, 12));
    assert!(
        !next.contains("fn build_tui()") && next.contains("run_case("),
        "{next}"
    );
    assert_eq!(dialog.state().viewport.top(), list_top);
    assert!(dialog.state().detail.viewport.top() > 0);
    dialog.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(dialog.state().detail.viewport.top(), 4);
    press(&mut dialog, KeyCode::Up);
    let short_preview = rendered_text(&render(&mut dialog, 120, 12));
    assert!(short_preview.contains("fn build_core()"));
    assert!(!short_preview.contains("PgDn scroll"));
    press(&mut dialog, KeyCode::PageDown);
    assert!(dialog.state().viewport.top() > list_top);
    press(&mut dialog, KeyCode::Down);
    assert!(rendered_text(&render(&mut dialog, 120, 12)).contains("fn build_tui()"));
    assert_eq!(dialog.state().detail.viewport.top(), 0);
}

#[test]
fn plain_other_and_skip_rows_have_muted_placeholders() {
    let mut request = request();
    request.questions[0].required = false;
    let mut dialog = QuestionDialog::new(TurnId::new(1), request);
    for (cursor, label) in [(2, "Desktop"), (3, "Other"), (4, "Skip")] {
        dialog.set_cursor(cursor);
        let buffer = render(&mut dialog, 80, 24);
        let text = rendered_text(&buffer);
        assert!(text.contains(&format!("── {label} ─")));
        let row = rows(&buffer)
            .position(|row| row.contains("No additional detail."))
            .expect("empty preview");
        assert!(
            buffer.content()[row * 80..(row + 1) * 80]
                .iter()
                .any(|cell| cell.symbol() == "N" && cell.fg == theme().text.muted)
        );
        assert!(!text.contains("PgDn scroll"));
    }
}

#[test]
fn tiny_previews_join_the_single_scrollable_body() {
    let mut dialog = QuestionDialog::new(TurnId::new(1), request());
    for (width, height) in [(0, 0), (0, 20), (20, 0), (1, 1), (4, 4), (42, 5), (120, 3)] {
        let _ = render(&mut dialog, width, height);
        assert_eq!(
            dialog.state().last_layout.unwrap().mode,
            QuestionLayoutMode::WholeBody
        );
        assert!(!dialog.state().detail.scrollable);
    }
    let text = (0..8)
        .map(|_| {
            let text = rendered_text(&render(&mut dialog, 42, 5));
            press(&mut dialog, KeyCode::PageDown);
            text
        })
        .collect::<String>();
    assert!(text.contains("── Core ─"), "{text}");
    assert!(text.contains("fn build_core()"), "{text}");
}

#[test]
fn native_multi_select_toggles_and_confirms_the_batch_without_duplicate_other() {
    let mut request = request();
    request.questions.push(QuestionPrompt {
        id: "release".into(),
        header: "Release".into(),
        question: "What version?".into(),
        options: vec![],
        kind: QuestionPromptKind::Text {
            min_length: Some(1),
            max_length: None,
        },
        required: true,
        default: None,
    });
    let mut dialog = QuestionDialog::new(TurnId::new(1), request);
    press(&mut dialog, KeyCode::Enter);
    assert!(rendered_text(&render(&mut dialog, 80, 24)).contains("Select at least one choice."));
    press(&mut dialog, KeyCode::Char(' '));
    press(&mut dialog, KeyCode::Down);
    press(&mut dialog, KeyCode::Char(' '));
    press(&mut dialog, KeyCode::End);
    press(&mut dialog, KeyCode::Char(' '));
    dialog.paste(" Core ");
    press(&mut dialog, KeyCode::Enter); // accept Other, return to choices
    assert_eq!(dialog.question_index, 0);
    press(&mut dialog, KeyCode::Enter); // confirm the selected choices
    assert_eq!(dialog.question_index, 1);
    render(&mut dialog, 80, 24);
    let layout = dialog.state().last_layout.unwrap();
    assert_eq!(layout.mode, QuestionLayoutMode::Pinned);
    assert_eq!(layout.inner_width, 80 * 92 / 100 - 4);
    dialog.paste("0.9");
    let QuestionDialogAction::Respond {
        response: QuestionResponse::Answered { answers },
        ..
    } = press(&mut dialog, KeyCode::Enter)
    else {
        panic!("expected the completed batch");
    };
    assert_eq!(
        answers[0].answer,
        Some(vec!["Core".to_string(), "TUI".to_string()].into())
    );
    assert!(answers[0].note.is_none());
    assert_eq!(answers[1].answer, Some("0.9".to_string().into()));
}
