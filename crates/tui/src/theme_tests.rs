//! Each installed-theme scenario runs alone in a subprocess: no OnceLock test
//! ordering, runtime reset hooks, or globally mutable test palette.
use super::*;
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyEvent, KeyModifiers},
    style::{Color, Modifier, Style},
};
use rig_core::message::Message;
use zevria_theme::{HexRgb, generate_theme, install_theme};
use zevria_tui_widgets::render_startup_frame;

#[test]
fn isolated_theme_lifetimes() {
    for mode in ["#1E1E2E", "#FFFFFF", "fallback"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "theme_tests::installed_palette_child",
                "--nocapture",
            ])
            .env("ZEVRIA_TEST_THEME", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn assert_status_tokens_and_selection() {
    use crate::presentation::{
        BlockVisibility, PresentationBlock, PresentationBlockId, PresentationBlockKind,
        TranscriptAppearance, WebActivityPresentation,
    };
    use crate::status_icon::StatusIcon;
    let t = *theme::theme();
    for (icon, expected) in [
        (StatusIcon::Pending, t.text.muted),
        (StatusIcon::Running, t.feedback.info),
        (StatusIcon::Done, t.feedback.success),
        (StatusIcon::Failed, t.feedback.error),
        (StatusIcon::Denied, t.feedback.error),
        (StatusIcon::Interrupted, t.feedback.warning),
        (StatusIcon::Dismissed, t.text.muted),
        (StatusIcon::Updated, t.text.muted),
    ] {
        assert_eq!(icon.color(), expected);
        let block = PresentationBlock {
            id: PresentationBlockId(0),
            revision: 0,
            role: None,
            prompt_group: None,
            prompt: None,
            visibility: BlockVisibility::Always,
            kind: PresentationBlockKind::WebActivity(WebActivityPresentation {
                detail: Some(
                    "A deliberately long themed hosted activity label that must fold".into(),
                ),
                members: vec![0],
                outcomes: vec![(icon, 1)],
            }),
        };
        for width in [1, 40, 100] {
            for folded in [false, true] {
                for selected in [false, true] {
                    let mut lines = Vec::new();
                    layout::prepare::render_conversation_block(
                        &block,
                        &mut lines,
                        layout::prepare::ConversationBlockContext {
                            width,
                            header_role: None,
                            header: None,
                            separator_before: false,
                            selected,
                            folded,
                            reasoning_heading: false,
                            appearance: TranscriptAppearance::Native,
                        },
                    );
                    let glyph = lines
                        .iter()
                        .flat_map(|line| &line.spans)
                        .find(|span| span.content == icon.glyph(0))
                        .unwrap();
                    assert_eq!(
                        glyph.style.fg,
                        Some(if selected {
                            t.surfaces.canvas
                        } else {
                            expected
                        })
                    );
                    if selected {
                        assert!(
                            lines
                                .iter()
                                .flat_map(|line| &line.spans)
                                .all(|span| span.style.fg == Some(t.surfaces.canvas)
                                    && span.style.bg == Some(t.surfaces.selection_background))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn installed_palette_child() {
    let Ok(mode) = std::env::var("ZEVRIA_TEST_THEME") else {
        return;
    };
    let background = if mode == "fallback" { "#FFFFFF" } else { &mode };
    let definition = generate_theme(background.parse().unwrap()).unwrap();
    if mode == "fallback" {
        assert_eq!(*theme::theme(), theme::ZEVRIA_DARK);
        assert!(install_theme(&definition).is_err());
        assert_status_tokens_and_selection();
        return;
    }
    install_theme(&definition).unwrap();
    let t = *theme::theme();
    assert_status_tokens_and_selection();
    assert_ne!(t, theme::ZEVRIA_DARK);
    let HexRgb(r, g, b) = definition.palette.canvas;
    assert_eq!(t.surfaces.canvas, Color::Rgb(r, g, b));
    let HexRgb(r, g, b) = definition.palette.panel;
    assert_eq!(t.surfaces.panel, Color::Rgb(r, g, b));
    assert_ne!(t.surfaces.panel, t.surfaces.canvas);
    assert!(install_theme(&definition).is_err());
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    for status in ["Connecting…", "Resuming…"] {
        terminal.draw(|f| render_startup_frame(f, status)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.bg == t.surfaces.canvas)
        );
        assert_eq!(buffer[(0, 0)].fg, t.surfaces.border_strong);
    }

    // Syntax, Markdown aliases, and final nested-style selection override.
    let mut lines = markdown::markdown_lines(
        "# Heading\n\n**strong** `inline` [link](url)\n\n```rust\nfn demo() { let number = 42; } // comment\n```",
        Style::default().fg(t.text.primary),
        100,
    );
    for (needle, expected) in [
        ("Heading", t.content.heading),
        ("strong", t.workflow.plan),
        ("inline", t.syntax.string),
        ("link", t.feedback.info),
        ("fn", t.syntax.keyword),
        ("42", t.syntax.constant),
        ("comment", t.text.muted),
    ] {
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains(needle) && s.style.fg == Some(expected)),
            "{needle}"
        );
    }
    for line in &mut lines {
        chrome::style_selected_line(line);
        for span in &line.spans {
            assert_eq!(span.style.fg, Some(t.surfaces.canvas));
            assert_eq!(span.style.bg, Some(t.surfaces.selection_background));
        }
    }
    let change = zevria_foundation::FileChangeOutput {
        path: "demo.rs".into(),
        change: zevria_foundation::FileChange::Update {
            unified_diff: diffy::create_patch("let old = 1;\n", "let new = 2;\n").to_string(),
            move_path: None,
        },
    };
    let lines = diff_render::render_file_change(
        &change,
        80,
        diff_render::DiffRenderPolicy::Complete,
        diff_render::DiffRowBackgrounds::Enabled,
    );
    for (sign, fg, bg) in [
        (
            " + ",
            t.feedback.success,
            t.content.diff_addition_background,
        ),
        (" - ", t.feedback.error, t.content.diff_deletion_background),
    ] {
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains(sign)
                    && s.style.fg == Some(fg)
                    && s.style.bg == Some(bg))
        );
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains("let")
                    && s.style.fg == Some(t.syntax.keyword)
                    && s.style.bg == Some(bg))
        );
    }

    // Full frames, role/status accents, fresh panes and reconstructed content.
    for (message, color, background) in [
        (Message::user("user body"), t.roles.you, t.surfaces.panel),
        (
            Message::assistant("assistant body"),
            t.roles.assistant,
            t.surfaces.canvas,
        ),
        (
            Message::System {
                content: "system body".into(),
            },
            t.roles.system,
            t.surfaces.canvas,
        ),
    ] {
        for inspect in [false, true] {
            let mut app = if inspect {
                App::subtask_inspect("Explore child")
            } else {
                App::new()
            };
            app.seed_history_entry(
                HistoryEntry::from_message(message.clone(), ToolCallStatus::Finished).unwrap(),
            );
            app.seed_history_entry(
                HistoryEntry::from_message(
                    Message::assistant("neighboring answer"),
                    ToolCallStatus::Finished,
                )
                .unwrap(),
            );
            let mut content = ratatui::layout::Rect::default();
            terminal
                .draw(|f| content = app.render_surface(f, false, true).conversation_content)
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!(
                buffer
                    .content()
                    .iter()
                    .any(|c| c.symbol() == "┃" && c.fg == color)
            );
            assert_eq!(buffer[(content.x, content.y)].fg, color);
            assert_eq!(buffer[(content.x, content.y + 1)].fg, t.text.primary);
            let first_end = content.y + app.view_cache().entries()[0].height as u16;
            for y in content.y..content.bottom() {
                for x in 0..buffer.area.width {
                    let expected = if y < first_end && (content.x..content.right()).contains(&x) {
                        background
                    } else {
                        // The next assistant, separator, gutters and scrollbar
                        // stay on the installed canvas in either pane type.
                        t.surfaces.canvas
                    };
                    assert_eq!(
                        buffer[(x, y)].bg,
                        expected,
                        "{mode}, inspect={inspect}, ({x}, {y})"
                    );
                }
            }
            for cell in buffer.content() {
                assert!(matches!(cell.fg, Color::Rgb(..)) && matches!(cell.bg, Color::Rgb(..)));
            }
        }
    }
    let workspace = tempfile::tempdir().unwrap();
    let mut views = SessionViews::new(App::new(), workspace.path().to_path_buf());
    terminal.draw(|f| views.render(f)).unwrap();
    let original = terminal.backend().buffer().clone();
    assert!((0..100).all(|x| original[(x, 0)].bg == t.surfaces.canvas));
    assert_eq!(original[(2, 0)].symbol(), "");
    assert_eq!(original[(2, 0)].fg, t.workflow.build);
    assert_eq!(original[(2, 0)].modifier, Modifier::BOLD);
    assert_eq!(original[(5, 0)].fg, t.text.muted);
    for x in 2..98 {
        assert_eq!(original[(x, 1)].symbol(), "─");
        assert_eq!(original[(x, 1)].fg, t.surfaces.border);
        assert_eq!(original[(x, 1)].bg, t.surfaces.canvas);
    }
    assert!(
        original
            .content()
            .iter()
            .any(|c| c.symbol() == "B" && c.fg == t.workflow.build)
    );
    views.open_session_picker(Vec::new());
    terminal.draw(|f| views.render(f)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.bg == t.surfaces.overlay)
    );
    views.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    terminal.draw(|f| views.render(f)).unwrap();
    assert_eq!(
        terminal.backend().buffer(),
        &original,
        "modal teardown restores the same palette"
    );
    assert_eq!(
        *theme::theme(),
        t,
        "new panes/highlights cannot replace an installed theme"
    );
}
