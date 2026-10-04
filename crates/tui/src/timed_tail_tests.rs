//! Deterministic render-only clock and visible-pane regressions.

use super::*;
use crate::app::{ConversationTail, RetryCountdown};
use std::time::{Duration, Instant};

fn start_at(app: &mut App, now: Instant) {
    reduce_at(
        app,
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::User {
                content: Vec::new(),
            },
            mode: SessionMode::Build,
        },
        now,
    );
}

fn reduce_at(app: &mut App, event: SessionEvent, now: Instant) {
    assert!(app.reduce_at(event, now).is_empty());
}

fn elapsed(app: &mut App) -> Option<Duration> {
    match app.render_parts().tail {
        ConversationTail::None => None,
        ConversationTail::Waiting { elapsed, .. }
        | ConversationTail::Compacting { elapsed, .. }
        | ConversationTail::Streaming { elapsed, .. }
        | ConversationTail::Retrying { elapsed, .. } => Some(elapsed),
    }
}

fn retry_event(attempt: usize, delay: Duration, error: &str) -> SessionEvent {
    SessionEvent::TurnRetrying {
        call: 1,
        turn_id: TEST_TURN_ID,
        attempt,
        max_attempts: 5,
        retry_after: delay,
        error: error.into(),
    }
}

fn network_event(
    call: usize,
    attempt: usize,
    status: zevria_session_api::event::NetworkStatus,
) -> SessionEvent {
    SessionEvent::NetworkStatus {
        turn_id: TEST_TURN_ID,
        call,
        attempt,
        max_attempts: 4,
        transport: zevria_session_api::event::NetworkTransport::Http,
        status,
    }
}

#[test]
fn routine_first_attempts_keep_the_ordinary_display_for_each_model_call() {
    use zevria_session_api::event::{NetworkStatus, NetworkTransport};
    for transport in [NetworkTransport::WebSocket, NetworkTransport::Http] {
        let mut app = App::new();
        let now = Instant::now();
        start_at(&mut app, now);
        let status = |call, attempt, status| SessionEvent::NetworkStatus {
            turn_id: TEST_TURN_ID,
            call,
            attempt,
            max_attempts: 4,
            transport,
            status,
        };
        for call in [1, 2] {
            reduce_at(
                &mut app,
                SessionEvent::ModelCallStarted {
                    turn_id: TEST_TURN_ID,
                    call,
                },
                now,
            );
            let waiting = rendered_text(&mut app, 160, 20);
            assert!(waiting.contains("running…"));
            for phase in [
                NetworkStatus::AttemptStarted,
                NetworkStatus::Connecting,
                NetworkStatus::AwaitingResponse,
            ] {
                reduce_at(&mut app, status(call, 1, phase), now);
                assert!(app.render_parts().network_status.is_none());
                assert_eq!(rendered_text(&mut app, 160, 20), waiting);
            }
            reduce_at(
                &mut app,
                SessionEvent::AssistantStreamUpdated {
                    turn_id: TEST_TURN_ID,
                    snapshot: Message::assistant("normal preview").into(),
                },
                now,
            );
            let streaming = rendered_text(&mut app, 160, 20);
            reduce_at(
                &mut app,
                status(call, 1, NetworkStatus::ProgressResumed),
                now,
            );
            assert_eq!(rendered_text(&mut app, 160, 20), streaming);
            if call == 1 {
                // A prior call's recovery must not make the next call noisy.
                reduce_at(&mut app, retry_event(2, Duration::ZERO, "offline"), now);
                reduce_at(
                    &mut app,
                    status(call, 2, NetworkStatus::AttemptStarted),
                    now,
                );
                assert!(app.render_parts().network_status.is_some());
            }
        }
    }
}

#[test]
fn retry_attempts_still_show_transport_phases_and_clear_the_countdown() {
    use zevria_session_api::event::{NetworkStatus, NetworkTransport};
    for (transport, name) in [
        (NetworkTransport::WebSocket, "WebSocket"),
        (NetworkTransport::Http, "HTTP"),
    ] {
        let mut app = App::new();
        let now = Instant::now();
        start_at(&mut app, now);
        reduce_at(
            &mut app,
            retry_event(2, Duration::from_secs(1), "offline"),
            now,
        );
        let status = |status| SessionEvent::NetworkStatus {
            turn_id: TEST_TURN_ID,
            call: 1,
            attempt: 2,
            max_attempts: 4,
            transport,
            status,
        };
        for (phase, text) in [
            (
                NetworkStatus::AttemptStarted,
                format!("Starting {name} attempt"),
            ),
            (NetworkStatus::Connecting, format!("Connecting via {name}")),
            (
                NetworkStatus::AwaitingResponse,
                format!("Awaiting {name} response"),
            ),
        ] {
            reduce_at(&mut app, status(phase), now + Duration::from_secs(1));
            assert!(app.retry_notice().is_none());
            assert_eq!(
                app.render_parts().network_status,
                Some((format!("{text} · attempt 2/4"), false))
            );
            let rendered = rendered_text(&mut app, 160, 20);
            assert!(rendered.contains(&text));
            assert!(!rendered.contains("reconnecting"));
        }
        reduce_at(
            &mut app,
            status(NetworkStatus::ProgressResumed),
            now + Duration::from_secs(2),
        );
        assert!(app.render_parts().network_status.is_none());
    }
}

#[test]
fn network_warning_preserves_preview_and_rejects_stale_call_attempt_and_turn_statuses() {
    use zevria_session_api::event::NetworkStatus;
    let mut app = App::new();
    let now = Instant::now();
    start_at(&mut app, now);
    reduce_at(
        &mut app,
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 1,
        },
        now,
    );
    reduce_at(
        &mut app,
        network_event(1, 1, NetworkStatus::AttemptStarted),
        now,
    );
    reduce_at(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: Message::assistant("preview stays visible").into(),
        },
        now,
    );
    let quiet = NetworkStatus::Quiet {
        idle_for: Duration::from_secs(30),
        retry_in: Duration::from_secs(150),
    };
    reduce_at(
        &mut app,
        network_event(1, 1, quiet.clone()),
        now + Duration::from_secs(30),
    );
    let text = rendered_text(&mut app, 160, 20);
    assert!(text.contains("preview stays visible"));
    assert!(text.contains("No response progress for 30s · automatic retry in 2m 30s"));
    // A coalesced/repeated snapshot is not evidence of classified progress.
    reduce_at(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: Message::assistant("preview stays visible").into(),
        },
        now + Duration::from_secs(31),
    );
    assert!(rendered_text(&mut app, 160, 20).contains("No response progress"));
    reduce_at(
        &mut app,
        network_event(1, 1, NetworkStatus::ProgressResumed),
        now + Duration::from_secs(35),
    );
    assert!(!rendered_text(&mut app, 160, 20).contains("No response progress"));
    assert!(rendered_text(&mut app, 160, 20).contains("preview stays visible"));
    reduce_at(
        &mut app,
        retry_event(2, Duration::from_secs(1), "idle timeout"),
        now + Duration::from_secs(180),
    );
    reduce_at(
        &mut app,
        network_event(1, 2, NetworkStatus::AttemptStarted),
        now + Duration::from_secs(181),
    );
    assert!(app.retry_notice().is_none());
    assert!(!rendered_text(&mut app, 160, 20).contains("reconnecting"));
    reduce_at(
        &mut app,
        network_event(1, 1, quiet.clone()),
        now + Duration::from_secs(182),
    );
    assert!(!rendered_text(&mut app, 160, 20).contains("No response progress"));
    reduce_at(
        &mut app,
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 2,
        },
        now + Duration::from_secs(183),
    );
    reduce_at(
        &mut app,
        network_event(1, 4, quiet.clone()),
        now + Duration::from_secs(184),
    );
    assert!(app.render_parts().network_status.is_none());
    reduce_at(
        &mut app,
        network_event(2, 1, quiet.clone()),
        now + Duration::from_secs(185),
    );
    assert!(app.render_parts().network_status.is_some());
    reduce_at(
        &mut app,
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
        now + Duration::from_secs(186),
    );
    reduce_at(
        &mut app,
        network_event(2, 1, quiet),
        now + Duration::from_secs(187),
    );
    assert!(app.render_parts().network_status.is_none());
}

#[test]
fn child_network_warning_is_isolated_from_root_preview_and_cleared_on_completion() {
    use zevria_session_api::event::NetworkStatus;
    let now = Instant::now();
    let mut root = App::new();
    start_at(&mut root, now);
    reduce_at(
        &mut root,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: Message::assistant("root preview").into(),
        },
        now,
    );
    let mut views = test_session_views(root);
    let id = SubtaskId::new("network-child");
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: child_descriptor("network-child", "network child"),
    });
    for event in [
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("child"),
            mode: SessionMode::Build,
        },
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 1,
        },
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: Message::assistant("child preview").into(),
        },
        network_event(
            1,
            1,
            NetworkStatus::Quiet {
                idle_for: Duration::from_secs(30),
                retry_in: Duration::from_secs(150),
            },
        ),
    ] {
        views.apply(SessionEvent::SubtaskSession {
            id: id.clone(),
            event: Box::new(event),
        });
    }
    let text = rendered_views_text(&mut views, 160, 25);
    assert!(text.contains("root preview") && !text.contains("No response progress"));
    views.handle_event(ctrl('i'));
    let text = rendered_views_text(&mut views, 160, 25);
    assert!(text.contains("child preview") && text.contains("No response progress"));
    views.apply(SessionEvent::SubtaskSession {
        id,
        event: Box::new(SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: Message::assistant("child complete"),
            display_attempt_id: None,
        }),
    });
    assert!(!rendered_views_text(&mut views, 160, 25).contains("No response progress"));
}

const TIMED_FRAMES: [(u64, &str); 6] = [
    (0, "◐"),
    (249, "◐"),
    (250, "◓"),
    (500, "◑"),
    (750, "◒"),
    (1000, "◐"),
];

#[test]
fn actual_tail_spinner_is_info_colored_independently_of_headline() {
    for phase in ["waiting", "streaming", "compacting", "retrying"] {
        let mut app = App::new();
        let now = Instant::now();
        start_at(&mut app, now);
        match phase {
            "streaming" => reduce_at(
                &mut app,
                SessionEvent::AssistantStreamUpdated {
                    turn_id: TEST_TURN_ID,
                    snapshot: (Message::assistant("cached body")).into(),
                },
                now,
            ),
            "compacting" => reduce_at(
                &mut app,
                SessionEvent::CompactionStarted {
                    turn_id: TEST_TURN_ID,
                    trigger: CompactionTrigger::AutomaticMidTurn,
                },
                now,
            ),
            "retrying" => reduce_at(
                &mut app,
                retry_event(1, Duration::from_secs(4), "offline"),
                now,
            ),
            _ => {}
        }
        for (millis, glyph) in TIMED_FRAMES {
            app.observe_clock(now + Duration::from_millis(millis));
            let buffer = rendered_buffer(&mut app, 100, 20);
            let (x, y) = (0..buffer.area.height)
                .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
                .find(|&position| buffer[position].symbol() == glyph)
                .unwrap();
            assert_eq!(buffer[(x, y)].fg, crate::theme::theme().feedback.info);
            assert_eq!(
                buffer[(x + 2, y)].fg,
                if phase == "retrying" {
                    crate::theme::theme().feedback.warning
                } else {
                    crate::theme::theme().text.muted
                }
            );
        }
    }
}

fn assert_spinner(text: &str, expected: &str) {
    for glyph in ["◐", "◓", "◑", "◒"] {
        assert_eq!(
            text.matches(glyph).count(),
            usize::from(glyph == expected),
            "expected only spinner {expected}: {text}"
        );
    }
}

#[test]
fn numbered_header_only_tail_refreshes_decoration_outside_selection_and_gutter() {
    let mut app = App::new();
    let now = Instant::now();
    start_at(&mut app, now);
    reduce_at(
        &mut app,
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 1,
        },
        now,
    );
    let reference = rendered_buffer(&mut app, 100, 20);
    let content = conversation_content_area(&reference, false);
    let (lines, header_height) = app.view_cache().streaming().unwrap();
    let header_spans = lines[0].spans[..2].to_vec();
    assert_eq!(
        lines[0].spans[2].style.fg,
        Some(crate::theme::theme().text.muted)
    );
    let rebuilds = (app.view_cache().rebuilds, app.view_cache().block_rebuilds);
    assert_eq!(header_height, 1);
    assert_blank_conversation_row(&reference, content.y + 1);
    let gutter = crate::frame_layout::Metrics::for_height(reference.area.height).hpad_left;
    assert_eq!(
        reference[(gutter, content.y)].fg,
        crate::theme::ZEVRIA_DARK.roles.assistant
    );
    assert_eq!(
        reference[(gutter, content.y + 2)].symbol(),
        " ",
        "spinner has no assistant gutter"
    );
    double_escape(&mut app);
    assert_eq!(
        app.selection(),
        None,
        "header-only tails have no selectable block"
    );
    assert!(app.history().is_empty());
    for (millis, glyph) in TIMED_FRAMES {
        app.observe_clock(now + Duration::from_millis(millis));
        let text = rendered_text(&mut app, 100, 20);
        assert_eq!(
            text.matches(&format!("● Assistant · #(1 - 1) · {}s", millis / 1000))
                .count(),
            1
        );
        assert_spinner(&text, glyph);
        assert_eq!(
            app.view_cache().streaming().unwrap().0[0].spans[..2],
            header_spans
        );
        assert_eq!(
            (app.view_cache().rebuilds, app.view_cache().block_rebuilds),
            rebuilds
        );
    }
    reduce_at(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (opaque_reasoning_message()).into(),
        },
        now + Duration::from_secs(2),
    );
    assert_eq!(
        rendered_text(&mut app, 100, 20)
            .matches("● Assistant · #(1 - 1)")
            .count(),
        1
    );
    reduce_at(
        &mut app,
        retry_event(1, Duration::from_secs(4), "offline"),
        now + Duration::from_secs(3),
    );
    let text = rendered_text(&mut app, 100, 20);
    assert!(text.contains("4s · 3s"));
    assert!(text.contains("● Assistant · #(1 - 1) · 3s"));
    reduce_at(
        &mut app,
        SessionEvent::CompactionStarted {
            turn_id: TEST_TURN_ID,
            trigger: CompactionTrigger::AutomaticMidTurn,
        },
        now + Duration::from_secs(4),
    );
    let text = rendered_text(&mut app, 100, 20);
    assert!(text.contains("Compacting context… · 4s"));
    assert!(text.contains("● Assistant · #(1 - 1) · 4s"));
    reduce_at(
        &mut app,
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
        now + Duration::from_secs(5),
    );
    assert!(!rendered_text(&mut app, 100, 20).contains("● Assistant"));
    assert!(app.view_cache().streaming().is_none());
}

#[test]
fn timed_stream_headers_advance_without_tokens_or_semantic_body_rebuilds() {
    for message in [
        assistant_message(vec![
            AssistantContent::text("**cached Markdown**\n\n```rust\nfn main() {}\n```"),
            AssistantContent::text("another cached block"),
        ]),
        opaque_reasoning_message(),
        Message::assistant(""),
        Message::User {
            content: Vec::new(),
        },
    ] {
        for width in [8, 26, 30, 100] {
            let mut app = App::new();
            let now = Instant::now();
            start_at(&mut app, now);
            reduce_at(
                &mut app,
                SessionEvent::ModelCallStarted {
                    turn_id: TEST_TURN_ID,
                    call: 1,
                },
                now,
            );
            reduce_at(
                &mut app,
                SessionEvent::AssistantStreamUpdated {
                    turn_id: TEST_TURN_ID,
                    snapshot: message.clone().into(),
                },
                now,
            );
            let buffer = rendered_buffer(&mut app, width, 30);
            let wrap_width = conversation_content_area(&buffer, false).width.max(1);
            let body = app.view_cache().streaming().unwrap().0[1..].to_vec();
            let rebuilt = app.view_cache().block_rebuilds;
            for seconds in [0, 9, 10, 59, 60, 65, 3599, 3600, 3723] {
                app.observe_clock(now + Duration::from_secs(seconds));
                rendered_buffer(&mut app, width, 30);
                let (lines, height) = app.view_cache().streaming().unwrap();
                assert_eq!(
                    line_text(&lines[0]),
                    format!(
                        "● Assistant · #(1 - 1) · {}",
                        crate::text::format_elapsed(Duration::from_secs(seconds))
                    )
                );
                assert_eq!(lines[1..], body);
                assert_eq!(
                    height,
                    crate::layout::prepare::wrapped_height(lines, wrap_width)
                );
                assert_eq!(app.view_cache().block_rebuilds, rebuilt);
                assert!(
                    app.history().is_empty(),
                    "streaming decoration is not history"
                );
            }
        }
    }
}

#[test]
fn terminals_freeze_uncommitted_clocks_without_retaining_transient_headers_or_bodies() {
    for terminal in [
        SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: opaque_reasoning_message(),
            display_attempt_id: None,
        },
        SessionEvent::TurnRecovered {
            turn_id: TEST_TURN_ID,
            display_attempt_id: None,
        },
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "failed".into(),
        },
        SessionEvent::TurnRejected {
            turn_id: TEST_TURN_ID,
            error: "rejected".into(),
        },
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    ] {
        for streaming in [
            None,
            Some(Message::assistant("transient content")),
            Some(opaque_reasoning_message()),
        ] {
            let mut app = App::new();
            let now = Instant::now();
            start_at(&mut app, now);
            reduce_at(
                &mut app,
                SessionEvent::ModelCallStarted {
                    turn_id: TEST_TURN_ID,
                    call: 1,
                },
                now + Duration::from_secs(3),
            );
            let header = app.render_parts().assistant_header.unwrap();
            if let Some(message) = streaming {
                reduce_at(
                    &mut app,
                    SessionEvent::AssistantStreamUpdated {
                        turn_id: TEST_TURN_ID,
                        snapshot: message.into(),
                    },
                    now + Duration::from_secs(4),
                );
            }
            assert!(rendered_text(&mut app, 100, 20).contains("● Assistant"));
            reduce_at(&mut app, terminal.clone(), now + Duration::from_secs(10));
            app.observe_clock(now + Duration::from_secs(100));
            let text = rendered_text(&mut app, 100, 20);
            assert!(!text.contains("● Assistant"));
            assert!(!text.contains("transient content"));
            assert!(app.view_cache().streaming().is_none());
            assert_eq!(
                app.render_parts().header_timings.elapsed(header),
                Some(Duration::from_secs(7))
            );
        }
    }
}

#[test]
fn pending_clock_starts_at_input_lock_and_waiting_frames_derive_from_elapsed() {
    let mut app = App::new();
    enter_insert(&mut app);
    app.set_input_for_test("prompt", 6);
    let now = Instant::now();
    assert!(matches!(
        app.handle_event_at(ctrl_enter(), now),
        Some(UiAction::Submit { .. })
    ));
    for (millis, glyph) in TIMED_FRAMES {
        app.observe_clock(now + Duration::from_millis(millis));
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains(&format!("{glyph} running… · {}s", millis / 1000)));
        assert_spinner(&text, glyph);
        assert!(!text.contains("Assistant"));
        assert_eq!(crate::text::display_width(glyph), 1);
        assert!(
            app.history().is_empty(),
            "pending timing never creates history"
        );
    }
    start_at(&mut app, now + Duration::from_secs(12));
    assert_eq!(elapsed(&mut app), Some(Duration::from_secs(12)));
    assert!(rendered_text(&mut app, 80, 12).contains("◐ running… · 12s"));
    // Rendering and backward observations cannot advance or rewind the clock.
    app.observe_clock(now);
    assert_eq!(elapsed(&mut app), Some(Duration::from_secs(12)));
    assert!(rendered_text(&mut app, 80, 12).contains("◐ running… · 12s"));
}

#[test]
fn tools_keep_waiting_and_retry_tails_and_progress_replaces_the_notice() {
    let mut app = App::new();
    let now = Instant::now();
    start_at(&mut app, now);
    reduce_at(
        &mut app,
        SessionEvent::Intermediate {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: assistant_message(vec![tool_call(
                "call-slow",
                None,
                "command",
                json!({"command": "slow tool"}),
            )]),
        },
        now + Duration::from_secs(12),
    );
    assert_eq!(tool_status(&app, 0, 0), ToolCallStatus::Executing);
    let text = rendered_rows(&mut app, 120, 16).join("\n");
    assert!(text.contains("slow tool ◐"));
    assert!(text.contains("◐ running… · 12s"));
    assert_eq!(
        text.matches("Assistant").count(),
        1,
        "only the tool message"
    );
    let cached = app.view_cache().entries()[0].lines.clone();
    let rebuilds = (app.view_cache().rebuilds, app.view_cache().block_rebuilds);
    for (millis, glyph) in TIMED_FRAMES {
        app.observe_clock(now + Duration::from_secs(12) + Duration::from_millis(millis));
        let text = rendered_rows(&mut app, 120, 16).join("\n");
        assert!(
            text.contains("slow tool ◐"),
            "cached tool icon stays static"
        );
        assert_spinner(
            text.lines().find(|line| line.contains("running…")).unwrap(),
            glyph,
        );
        assert_eq!(app.view_cache().entries()[0].lines, cached);
        assert_eq!(
            (app.view_cache().rebuilds, app.view_cache().block_rebuilds),
            rebuilds
        );
    }
    reduce_at(
        &mut app,
        retry_event(2, Duration::from_secs(4), "offline"),
        now + Duration::from_secs(13),
    );
    let text = rendered_rows(&mut app, 120, 16).join("\n");
    assert!(text.contains("slow tool ◐"));
    assert!(text.contains("◐ ⚠ reconnecting (attempt 2/5) · next attempt in 4s · 13s"));
    assert_eq!(
        text.matches("Assistant").count(),
        1,
        "only the tool message"
    );
    assert_spinner(
        text.lines()
            .find(|line| line.contains("reconnecting"))
            .unwrap(),
        "◐",
    );
    reduce_at(
        &mut app,
        SessionEvent::ToolResults {
            turn_id: TEST_TURN_ID,
            message: Message::tool_result("call-slow", "command", "done"),
            metadata: Vec::new(),
        },
        now + Duration::from_secs(15),
    );
    assert!(app.retry_notice().is_none());
    assert_eq!(elapsed(&mut app), Some(Duration::from_secs(15)));
    assert!(rendered_text(&mut app, 120, 16).contains("running… · 15s"));
}

#[test]
fn streaming_status_animates_outside_caches_even_for_opaque_or_empty_bodies() {
    for (message, has_message_rows) in [
        (Message::assistant("cached streamed body"), true),
        (opaque_reasoning_message(), false),
        (Message::assistant(""), true),
        // A zero-block snapshot still has a timed streaming phase.
        (
            Message::User {
                content: Vec::new(),
            },
            false,
        ),
    ] {
        let mut app = App::new();
        app.seed_history_entry(history_message(Message::user("committed prompt")));
        let now = Instant::now();
        start_at(&mut app, now);
        reduce_at(
            &mut app,
            SessionEvent::AssistantStreamUpdated {
                turn_id: TEST_TURN_ID,
                snapshot: message.into(),
            },
            now,
        );
        let first = rendered_text(&mut app, 100, 16);
        assert!(first.contains("◐ streaming · 0s"));
        assert_eq!(
            first.matches("Assistant").count(),
            usize::from(has_message_rows)
        );
        let cached = app
            .view_cache()
            .streaming()
            .map(|(lines, _)| lines.to_vec());
        let rebuilds = (app.view_cache().rebuilds, app.view_cache().block_rebuilds);
        for (millis, glyph) in TIMED_FRAMES {
            app.observe_clock(now + Duration::from_millis(millis));
            let text = rendered_text(&mut app, 100, 16);
            assert!(text.contains(&format!("{glyph} streaming · {}s", millis / 1000)));
            assert_spinner(&text, glyph);
            assert_eq!(
                text.matches("Assistant").count(),
                usize::from(has_message_rows)
            );
            assert_eq!(
                app.view_cache()
                    .streaming()
                    .map(|(lines, _)| lines.to_vec()),
                cached
            );
        }
        app.observe_clock(now + Duration::from_secs(65));
        let text = rendered_text(&mut app, 100, 16);
        assert_eq!(text.matches("◐ streaming · 1m 05s").count(), 1);
        assert_spinner(&text, "◐");
        assert_eq!(
            text.matches("cached streamed body").count(),
            first.matches("cached streamed body").count()
        );
        assert_opaque_reasoning_absent(
            &text,
            &[
                OPAQUE_ENCRYPTED_REASONING_PAYLOAD,
                OPAQUE_REDACTED_REASONING_PAYLOAD,
            ],
        );
        assert_eq!(
            app.view_cache()
                .streaming()
                .map(|(lines, _)| lines.to_vec()),
            cached
        );
        assert_eq!(
            (app.view_cache().rebuilds, app.view_cache().block_rebuilds),
            rebuilds
        );
        assert_eq!(app.history().len(), 1);
        assert!(!laid_out_transcript_text(&app).contains("streaming"));
    }
}

#[test]
fn retry_countdown_rounds_up_expires_without_recovery_and_uses_warning_style() {
    let mut app = App::new();
    let now = Instant::now();
    start_at(&mut app, now);
    let received = now + Duration::from_secs(12);
    reduce_at(
        &mut app,
        retry_event(
            2,
            Duration::from_millis(1500),
            "**literal error**\nsecond error line",
        ),
        received,
    );
    for (millis, glyph, wording) in [
        (0, "◐", "next attempt in 2s"),
        (249, "◐", "next attempt in 2s"),
        (250, "◓", "next attempt in 2s"),
        (500, "◑", "next attempt in 1s"),
        (750, "◒", "next attempt in 1s"),
        (1000, "◐", "next attempt in 1s"),
        (1500, "◑", "waiting for next attempt to start"),
        (1750, "◒", "waiting for next attempt to start"),
        (2000, "◐", "waiting for next attempt to start"),
        (2249, "◐", "waiting for next attempt to start"),
        (2250, "◓", "waiting for next attempt to start"),
        (2500, "◑", "waiting for next attempt to start"),
        (2750, "◒", "waiting for next attempt to start"),
        (3000, "◐", "waiting for next attempt to start"),
    ] {
        app.observe_clock(received + Duration::from_millis(millis));
        let text = rendered_text(&mut app, 120, 14);
        assert!(text.contains(wording));
        assert!(text.contains("request interrupted: **literal error**"));
        assert!(text.contains("second error line"));
        assert_spinner(&text, glyph);
        assert!(!text.contains("Assistant"));
        assert!(text.contains(&format!("{wording} · {}s", 12 + millis / 1000)));
        assert!(app.retry_notice().is_some());
    }
    assert!(matches!(
        app.render_parts().tail,
        ConversationTail::Retrying {
            countdown: RetryCountdown::Elapsed,
            ..
        }
    ));
    let buffer = rendered_buffer(&mut app, 120, 14);
    let warning = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "⚠")
        .unwrap();
    assert_eq!(warning.fg, crate::theme::theme().feedback.warning);
    assert_eq!(crate::text::display_width("⚠"), 1);
    let rows = rendered_rows(&mut app, 120, 14);
    let error_y = rows
        .iter()
        .position(|row| row.contains("request interrupted:"))
        .unwrap();
    let error_offset = rows[error_y].find("request interrupted:").unwrap();
    let error_x = crate::text::display_width(&rows[error_y][..error_offset]);
    assert_eq!(
        buffer[(error_x as u16, error_y as u16)].fg,
        crate::theme::theme().text.muted
    );

    reduce_at(
        &mut app,
        retry_event(3, Duration::ZERO, "new error"),
        received + Duration::from_secs(3),
    );
    let text = rendered_text(&mut app, 120, 14);
    assert!(text.contains("attempt 3/5) · reconnecting now · 15s"));
    assert!(!text.contains("literal error"));
    for (millis, glyph) in TIMED_FRAMES {
        app.observe_clock(received + Duration::from_secs(3) + Duration::from_millis(millis));
        let text = rendered_text(&mut app, 120, 14);
        assert_spinner(&text, glyph);
        assert!(!text.contains("Assistant"));
        assert!(text.contains(&format!(
            "attempt 3/5) · reconnecting now · {}s",
            15 + millis / 1000
        )));
    }
    reduce_at(
        &mut app,
        SessionEvent::AssistantStreamUpdated {
            turn_id: TEST_TURN_ID,
            snapshot: (Message::assistant("resumed")).into(),
        },
        received + Duration::from_secs(4),
    );
    assert!(app.retry_notice().is_none());
    assert!(rendered_text(&mut app, 120, 14).contains("streaming · 16s"));
}

#[test]
fn compaction_animates_without_a_header_and_keeps_whole_operation_time() {
    for trigger in [
        CompactionTrigger::Manual,
        CompactionTrigger::AutomaticPreTurn,
        CompactionTrigger::AutomaticMidTurn,
    ] {
        let mut app = App::new();
        if trigger != CompactionTrigger::AutomaticMidTurn {
            enter_insert(&mut app);
            let prompt = if trigger == CompactionTrigger::Manual {
                "/compact"
            } else {
                "prompt"
            };
            app.set_input_for_test(prompt, prompt.len());
        }
        let now = Instant::now();
        if trigger == CompactionTrigger::AutomaticMidTurn {
            start_at(&mut app, now);
        } else {
            assert!(app.handle_event_at(ctrl_enter(), now).is_some());
        }
        reduce_at(
            &mut app,
            SessionEvent::CompactionStarted {
                turn_id: TEST_TURN_ID,
                trigger,
            },
            now + Duration::from_secs(12),
        );
        for (millis, glyph) in TIMED_FRAMES {
            app.observe_clock(now + Duration::from_secs(12) + Duration::from_millis(millis));
            let text = rendered_text(&mut app, 100, 12);
            assert!(
                text.contains(&format!(
                    "{glyph} Compacting context… · {}s",
                    12 + millis / 1000
                )),
                "{trigger:?} at {millis}ms: {text}"
            );
            assert!(!text.contains("Assistant"));
            assert_spinner(&text, glyph);
            assert_eq!(
                elapsed(&mut app),
                Some(Duration::from_secs(12) + Duration::from_millis(millis))
            );
        }
        reduce_at(
            &mut app,
            SessionEvent::CompactionCompleted {
                turn_id: TEST_TURN_ID,
                trigger,
                backend: CompactionBackend::LocalSummary,
            },
            now + Duration::from_secs(13),
        );
        if trigger == CompactionTrigger::Manual {
            assert_eq!(elapsed(&mut app), None);
        } else {
            if trigger == CompactionTrigger::AutomaticPreTurn {
                start_at(&mut app, now + Duration::from_secs(14));
            }
            let expected = if trigger == CompactionTrigger::AutomaticPreTurn {
                14
            } else {
                13
            };
            assert_eq!(elapsed(&mut app), Some(Duration::from_secs(expected)));
            assert!(
                rendered_text(&mut app, 100, 12).contains(&format!("◐ running… · {expected}s"))
            );
        }
    }
    let mut manual = App::new();
    enter_insert(&mut manual);
    manual.set_input_for_test("/compact", 8);
    let now = Instant::now();
    assert!(matches!(
        manual.handle_event_at(ctrl_enter(), now),
        Some(UiAction::Compact { .. })
    ));
    for (millis, glyph) in TIMED_FRAMES {
        manual.observe_clock(now + Duration::from_millis(millis));
        let text = rendered_text(&mut manual, 100, 12);
        assert!(text.contains(&format!("{glyph} Compacting context… · {}s", millis / 1000)));
        assert!(!text.contains("Assistant"));
        assert_spinner(&text, glyph);
    }
}

#[test]
fn accepted_terminals_and_restoration_remove_timers_and_retry_notices() {
    for terminal in [
        SessionEvent::TurnCompleted {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
            message: Message::assistant("done"),
        },
        SessionEvent::TurnRecovered {
            display_attempt_id: None,
            turn_id: TEST_TURN_ID,
        },
        SessionEvent::TurnRejected {
            turn_id: TEST_TURN_ID,
            error: "rejected".into(),
        },
        SessionEvent::TurnFailed {
            turn_id: TEST_TURN_ID,
            error: "failed".into(),
        },
        SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        },
    ] {
        for pending in [true, false] {
            let mut app = App::new();
            enter_insert(&mut app);
            app.set_input_for_test("prompt", 6);
            let now = Instant::now();
            assert!(app.handle_event_at(ctrl_enter(), now).is_some());
            if !pending {
                start_at(&mut app, now + Duration::from_secs(1));
                reduce_at(
                    &mut app,
                    retry_event(1, Duration::from_secs(8), "offline"),
                    now + Duration::from_secs(2),
                );
            }
            reduce_at(&mut app, terminal.clone(), now + Duration::from_secs(3));
            assert!(!app.is_busy());
            assert!(app.retry_notice().is_none());
            assert_eq!(elapsed(&mut app), None);
            start_at(&mut app, now + Duration::from_secs(20));
            assert_eq!(elapsed(&mut app), Some(Duration::ZERO));
            app.restore(vec![TranscriptItem::Message(Message::user("restored"))]);
            assert_eq!(elapsed(&mut app), None);
            assert!(app.retry_notice().is_none());
            assert!(!rendered_text(&mut app, 100, 12).contains("running…"));
        }
    }
}

#[test]
fn pending_plan_decisions_hide_the_tail_and_settlement_does_not_leak_time() {
    for choice in ['2', '3'] {
        let mut app = App::new();
        let artifact = test_plan_artifact();
        let now = Instant::now();
        reduce_at(
            &mut app,
            SessionEvent::PlanStateChanged {
                state: PlanWorkflowState::Ready {
                    artifact: artifact.clone(),
                },
            },
            now,
        );
        app.handle_event_at(key(KeyCode::Char(choice)), now);
        assert!(matches!(
            app.handle_event_at(key(KeyCode::Enter), now),
            Some(UiAction::ResolvePlan { .. })
        ));
        assert!(app.is_busy());
        app.observe_clock(now + Duration::from_secs(10));
        assert_eq!(elapsed(&mut app), None);
        assert!(!rendered_text(&mut app, 120, 30).contains("running…"));
        let state = if choice == '2' {
            PlanWorkflowState::Resolved {
                artifact,
                resolution: PlanResolution::ImplementedFresh,
            }
        } else {
            PlanWorkflowState::Planning {
                id: artifact.version.id,
                previous: Some(artifact),
            }
        };
        reduce_at(
            &mut app,
            SessionEvent::PlanStateChanged { state },
            now + Duration::from_secs(12),
        );
        assert!(!app.is_busy());
        start_at(&mut app, now + Duration::from_secs(20));
        assert_eq!(elapsed(&mut app), Some(Duration::ZERO));
    }
}

#[test]
fn clock_only_draws_preserve_scrolled_views_selection_history_and_committed_caches() {
    let mut app = App::new();
    for index in 0..20 {
        app.seed_history_entry(history_message(Message::user(format!("committed {index}"))));
    }
    let now = Instant::now();
    start_at(&mut app, now);
    rendered_text(&mut app, 40, 12);
    app.handle_event_at(key(KeyCode::Up), now);
    rendered_text(&mut app, 40, 12);
    let scroll = app.view_scroll();
    assert!(!app.view_follow());
    let rebuilds = (app.view_cache().rebuilds, app.view_cache().block_rebuilds);
    for seconds in [59, 60, 3599, 3600, 3723] {
        app.observe_clock(now + Duration::from_secs(seconds));
        rendered_text(&mut app, 40, 12);
        assert_eq!(app.view_scroll(), scroll);
        assert!(!app.view_follow());
        assert_eq!(app.history().len(), 20);
        assert_eq!(
            (app.view_cache().rebuilds, app.view_cache().block_rebuilds),
            rebuilds
        );
    }
    app.select_for_test(cursor(19, 0));
    rendered_text(&mut app, 40, 12);
    let selection = app.selection();
    let scroll = app.view_scroll();
    app.observe_clock(now + Duration::from_secs(7200));
    rendered_text(&mut app, 40, 12);
    assert_eq!(app.selection(), selection);
    assert_eq!(app.view_scroll(), scroll);
    assert_eq!(
        app.handle_event_at(key(KeyCode::Char('y')), now + Duration::from_secs(7200)),
        Some(UiAction::Copy {
            text: "committed 19".into()
        })
    );
    assert!(!laid_out_transcript_text(&app).contains("running"));
}

#[test]
fn multiline_retry_errors_wrap_as_plain_text_and_small_viewports_remain_bounded() {
    let mut app = App::new();
    let now = Instant::now();
    start_at(&mut app, now);
    let error = format!(
        "**not markdown** {}\nlast error line",
        "long error ".repeat(8)
    );
    reduce_at(
        &mut app,
        retry_event(1, Duration::from_millis(500), &error),
        now,
    );
    let text = rendered_text(&mut app, 60, 40);
    assert!(text.contains("**not markdown**"));
    assert!(text.contains("last error line"));
    for width in [1, 8, 20, 40] {
        for height in [1, 4, 8, 14] {
            let buffer = rendered_buffer(&mut app, width, height);
            assert_eq!(buffer.area.width, width);
            assert_eq!(buffer.area.height, height);
        }
    }
    app.observe_clock(now + Duration::from_secs(3600));
    assert!(rendered_text(&mut app, 120, 40).contains("1h 00m 00s"));
}

#[test]
fn child_call_clocks_are_independent_across_switches_and_hidden_completion() {
    let mut root = App::new();
    // Pin all event receipt to observed future instants so the workspace's
    // real-time forwarding cannot introduce subsecond nondeterminism.
    let now = Instant::now() + Duration::from_secs(60);
    start_at(&mut root, now);
    reduce_at(
        &mut root,
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 1,
        },
        now,
    );
    let mut views = test_session_views(root);
    let id = SubtaskId::new("timed-child");
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "launch".into(),
        entry_index: 0,
        descriptor: child_descriptor("timed-child", "timed child"),
    });
    views.handle_event(ctrl('i'));
    views.observe_clock(now + Duration::from_secs(10));
    for event in [
        SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("child prompt"),
            mode: SessionMode::Build,
        },
        SessionEvent::ModelCallStarted {
            turn_id: TEST_TURN_ID,
            call: 1,
        },
    ] {
        views.apply(SessionEvent::SubtaskSession {
            id: id.clone(),
            event: Box::new(event),
        });
    }
    assert!(rendered_views_text(&mut views, 120, 25).contains("● Assistant · #(1 - 1) · 0s"));
    views.observe_clock(now + Duration::from_secs(15));
    assert!(rendered_views_text(&mut views, 120, 25).contains("● Assistant · #(1 - 1) · 5s"));
    views.handle_event(ctrl('o'));
    views.observe_clock(now + Duration::from_secs(25));
    assert!(rendered_views_text(&mut views, 120, 25).contains("● Assistant · #(1 - 1) · 25s"));
    views.handle_event(ctrl('i'));
    views.observe_clock(now + Duration::from_secs(30));
    assert!(rendered_views_text(&mut views, 120, 25).contains("● Assistant · #(1 - 1) · 20s"));
    views.handle_event(ctrl('o'));
    views.observe_clock(now + Duration::from_secs(50));
    views.apply(SessionEvent::SubtaskSession {
        id: id.clone(),
        event: Box::new(SessionEvent::TurnCompleted {
            turn_id: TEST_TURN_ID,
            message: Message::assistant("child done"),
            display_attempt_id: None,
        }),
    });
    assert_eq!(views.visible_child_id(), None, "completion stayed hidden");
    assert!(rendered_views_text(&mut views, 120, 25).contains("● Assistant · #(1 - 1) · 50s"));
    views.handle_event(ctrl('i'));
    assert!(!views.clock_required());
    views.observe_clock(now + Duration::from_secs(100));
    let text = rendered_views_text(&mut views, 120, 25);
    assert!(text.contains("● Assistant · #(1 - 1) · 20s"));
    assert!(!text.contains("running…"));
    views.handle_event(ctrl('o'));
    assert!(views.clock_required());
    views.observe_clock(now + Duration::from_secs(101));
    assert!(rendered_views_text(&mut views, 120, 25).contains("● Assistant · #(1 - 1) · 1m 41s"));
}

#[test]
fn clock_redraws_follow_visible_native_lifecycle_not_hidden_root_or_acp_work() {
    let mut root = App::new();
    assert!(!test_session_views(App::new()).clock_required());
    enter_insert(&mut root);
    root.set_input_for_test("pending", 7);
    assert!(root.handle_event(ctrl_enter()).is_some());
    let mut views = test_session_views(root);
    assert!(views.clock_required());
    views.apply(SessionEvent::SubtaskLaunched {
        turn_id: TEST_TURN_ID,
        call_id: "child".into(),
        entry_index: 0,
        descriptor: child_descriptor("clock-child", "clock child"),
    });
    let id = SubtaskId::new("clock-child");
    views.handle_event(ctrl('i'));
    assert_eq!(views.visible_child_id(), Some(&id));
    assert!(
        !views.clock_required(),
        "idle child disables a busy hidden root's clock"
    );
    views.handle_event(ctrl('o'));
    views.apply(SessionEvent::SubtaskSession {
        id: id.clone(),
        event: Box::new(SessionEvent::TurnStarted {
            turn_id: TEST_TURN_ID,
            message: Message::user("child prompt"),
            mode: SessionMode::Build,
        }),
    });
    // Forwarded native lifecycle starts timing while the child is hidden.
    let now = Instant::now();
    views.observe_clock(now + Duration::from_secs(12));
    views.handle_event(ctrl('i'));
    assert!(views.clock_required());
    views.observe_clock(now + Duration::from_secs(12));
    assert!(rendered_views_text(&mut views, 100, 14).contains("running… · 12s"));
    views.apply(SessionEvent::SubtaskSession {
        id,
        event: Box::new(SessionEvent::TurnCancelled {
            turn_id: TEST_TURN_ID,
        }),
    });
    assert!(!views.clock_required());
    views.handle_event(ctrl('o'));
    assert!(views.clock_required());
    views.apply(SessionEvent::TurnCancelled {
        turn_id: TEST_TURN_ID,
    });
    assert!(!views.clock_required());

    let run_id = EnsembleRunId::from_string("clock-acp");
    let agent = navigation_agent("clock-agent");
    let mut views = test_session_views(App::new());
    views.apply(SessionEvent::EnsembleStarted {
        turn_id: TEST_TURN_ID,
        start: navigation_ensemble_start(&run_id, &agent),
        resumed: false,
    });
    assert!(views.clock_required());
    views.handle_event(ctrl('i'));
    assert_eq!(views.visible_agent_id(), Some(&agent.id));
    assert!(
        !views.clock_required(),
        "ACP panes do not fabricate native operation timers"
    );
    assert!(!rendered_views_text(&mut views, 100, 14).contains("running…"));
}
