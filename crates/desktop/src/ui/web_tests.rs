use super::*;
use crate::ui::{
    blocks::{BlockData, BlockKind, Gutter, LiveTail},
    status::StatusLine,
};
use gpui_kit::test::TestQueryExt as _;
use serde_json::{Value, json};
use zevria_content::{
    AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
    AssistantSourceAddress, AssistantStreamSnapshot, CitedSource, HostedActivity,
    HostedActivityKind, HostedActivityStatus, ResponseAttempt, ResponseAttemptOutcome,
};
use zevria_foundation::ModelProfileRef;
use zevria_presentation::status_icon::StatusIcon;
use zevria_transcript::{IncompleteResponse, transcript::TranscriptItem};

const TURN: TurnId = TurnId::new(90);

fn start(controller: &Entity<SessionController>, cx: &mut TestAppContext) -> ResponseAttempt {
    event(
        controller,
        E::TurnStarted {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Trace the source."),
        },
        cx,
    );
    event(
        controller,
        E::ModelCallStarted {
            turn_id: TURN,
            call: 1,
        },
        cx,
    );
    ResponseAttempt::new(ModelProfileRef::new("fixture", "fixture"))
}

fn web(output_index: u64, status: HostedActivityStatus, action: Option<Value>) -> HostedActivity {
    HostedActivity {
        kind: HostedActivityKind::WebSearch,
        item_id: Some(format!("web_{output_index}")),
        output_index,
        status,
        action,
    }
}

fn part(
    output_index: u64,
    part: AssistantPartIdentity,
    content: AssistantPresentationContent,
) -> AssistantPresentationPart {
    AssistantPresentationPart {
        source: AssistantSourceAddress {
            output_index,
            part,
            item_id: None,
        },
        content,
    }
}

fn reasoning(index: u64) -> AssistantPresentationPart {
    part(
        index,
        AssistantPartIdentity::Summary(0),
        AssistantPresentationContent::Reasoning {
            text: "Check the documentation before answering.".into(),
        },
    )
}

fn stream(
    controller: &Entity<SessionController>,
    attempt: &mut ResponseAttempt,
    cx: &mut TestAppContext,
) {
    attempt.touch();
    let message =
        rig_core::message::Message::from(vec![rig_core::message::AssistantContent::Reasoning(
            rig_core::message::Reasoning::new("Check the documentation before answering."),
        )]);
    event(
        controller,
        E::AssistantStreamUpdated {
            turn_id: TURN,
            snapshot: AssistantStreamSnapshot {
                message: Some(message),
                attempt: Some(attempt.clone()),
            },
        },
        cx,
    );
}

fn frame(window: Window, cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn rows(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<BlockData> {
    view.read_with(cx, |view, cx| view.transcript.read(cx).test_data(cx))
}

fn web_rows(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<BlockData> {
    rows(view, cx)
        .into_iter()
        .filter(|row| matches!(row.kind, BlockKind::Web(_)))
        .collect()
}

#[gpui_kit::test]
fn web_actions_grow_in_place_with_a_live_tip_node_and_exact_copy(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup_at_size(cx, 720., 520.);
    let mut attempt = start(&controller, cx);
    attempt.presentation.push(reasoning(0));
    attempt.hosted_activity = vec![
        web(1, HostedActivityStatus::Completed, None),
        web(2, HostedActivityStatus::Searching, None),
    ];
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let mut initial = web_rows(&view, cx);
    assert_eq!(initial.len(), 1);
    let first = initial.remove(0);
    assert_eq!(first.gutter, Gutter::Glyph(StatusIcon::Running));
    assert!(first.expanded);
    assert!(rows(&view, cx).iter().any(|b| matches!(&b.kind, BlockKind::Live(LiveTail::Activity(phrase)) if phrase == "Searching the web")));
    controller.read_with(cx, |controller, _| {
        let status = StatusLine::new(controller.model.visible(), controller.cancelling);
        assert_eq!(status.activity, "Searching the web");
    });
    let entity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&first.key, cx)
            .unwrap()
    });
    cx.update_window(window.into(), |_, window, _| {
        let node = window.find("web-thread-node").bounds();
        let header = window.find("web-header").bounds();
        assert!(node.size.width > px(0.) && node.origin.x < header.origin.x);
    })
    .unwrap();

    attempt.hosted_activity[0].action =
        Some(json!({"type":"search","queries":["gpui text view links","gpui link safety"]}));
    attempt.hosted_activity[1].action = Some(json!({"type":"search","query":"gpui components"}));
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    assert_eq!(web_rows(&view, cx)[0].key, first.key);
    assert_eq!(
        entity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&first.key, cx)
            .unwrap())
    );
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("web-action-1").bounds().origin.y < window.find("web-action-2").bounds().origin.y);
        let mut row = window.within(first.key.clone());
        assert!(row.try_find_by_label("Web activity details").is_some());
        assert!(row.try_find_by_label("Copy web activity").is_some());
        row.hover("web-header", cx);
        row.click("copy", cx);
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(),
            Some("Web search: gpui text view links · gpui link safety ✓\nWeb search: gpui components ◐"));
    }).unwrap();

    attempt.hosted_activity[1].status = HostedActivityStatus::Completed;
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let completed = web_rows(&view, cx).remove(0);
    assert_eq!(completed.key, first.key);
    assert_eq!(completed.gutter, Gutter::Glyph(StatusIcon::Done));
    let BlockKind::Web(row) = completed.kind else {
        unreachable!()
    };
    assert_eq!(row.counts(), "2 searches");
    assert!(completed.primary.ends_with("gpui components ✓"));
}

#[gpui_kit::test]
fn visible_reasoning_splits_web_groups_and_keeps_the_first_fold(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    let mut attempt = start(&controller, cx);
    attempt.hosted_activity = vec![
        web(
            0,
            HostedActivityStatus::Completed,
            Some(json!({"type":"search","query":"one"})),
        ),
        web(
            2,
            HostedActivityStatus::Failed,
            Some(json!({"type":"search","query":"two"})),
        ),
    ];
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let first = web_rows(&view, cx).remove(0);
    assert!(first.expanded);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("web-count-meta").is_some());
        window
            .within(first.key.clone())
            .click("block-disclosure", cx);
    })
    .unwrap();
    frame(window, cx);
    assert!(!web_rows(&view, cx)[0].expanded);
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.try_find("web-action-list").is_none())
    })
    .unwrap();
    attempt.presentation.push(reasoning(1));
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let split = web_rows(&view, cx);
    assert_eq!(split.len(), 2);
    assert_eq!(split[0].key, first.key);
    assert!(!split[0].expanded);
    assert!(split[1].expanded);
}

#[gpui_kit::test]
fn closed_web_attempt_keeps_explicit_interrupted_and_unconfirmed_states(cx: &mut TestAppContext) {
    let (window, view, controller, _) = setup(cx);
    let mut attempt = start(&controller, cx);
    attempt.hosted_activity = vec![
        web(
            0,
            HostedActivityStatus::Searching,
            Some(json!({"type":"search","query":"one"})),
        ),
        web(
            1,
            HostedActivityStatus::Interrupted,
            Some(json!({"type":"open_page"})),
        ),
    ];
    stream(&controller, &mut attempt, cx);
    attempt.close(ResponseAttemptOutcome::Interrupted);
    event(
        &controller,
        E::ResponseAttemptUpdated {
            turn_id: TURN,
            attempt,
        },
        cx,
    );
    event(&controller, E::TurnCancelled { turn_id: TURN }, cx);
    frame(window, cx);
    assert_eq!(
        web_rows(&view, cx)[0].gutter,
        Gutter::Glyph(StatusIcon::Interrupted)
    );
    cx.update_window(window.into(), |_, window, _| {
        assert!(
            window
                .within("web-action-0")
                .try_find("web-unconfirmed")
                .is_some()
        );
        assert!(
            window
                .within("web-action-1")
                .try_find("web-interrupted")
                .is_some()
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn web_page_text_and_keyboard_control_only_open_safe_urls(cx: &mut TestAppContext) {
    let (window, _, controller, _) = setup(cx);
    let mut attempt = start(&controller, cx);
    let safe = "https://www.example.org/docs?p=1&q=2#topic";
    attempt.hosted_activity = vec![
        web(
            0,
            HostedActivityStatus::Completed,
            Some(json!({"type":"open_page","url":safe})),
        ),
        web(
            1,
            HostedActivityStatus::Completed,
            Some(json!({"type":"open_page","url":"https://www.example.org/\u{7}docs"})),
        ),
    ];
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("web-open-1").is_none());
        assert!(window.try_find("web-url-1").is_none());
        let button = window.find_by_label("Open page in browser");
        assert_eq!(button.role(), Some(gpui_kit::Role::Button));
        window.click("web-open-0", cx);
    })
    .unwrap();
    assert_eq!(cx.opened_url().as_deref(), Some(safe));
    cx.update(|cx| cx.open_url("https://example.test/marker"));
    cx.update_window(window.into(), |_, window, cx| window.click("web-url-0", cx))
        .unwrap();
    assert_eq!(cx.opened_url().as_deref(), Some(safe));
}

#[gpui_kit::test]
fn answer_sources_update_independently_and_restore_with_working_chips(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    let mut attempt = start(&controller, cx);
    let text = "A focused answer [Source](https://example.org/).";
    attempt.presentation.push(part(
        0,
        AssistantPartIdentity::Content(0),
        AssistantPresentationContent::answer(text),
    ));
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.try_find("answer-sources").is_none())
    })
    .unwrap();
    let page = |url: &str, title: &str| CitedSource {
        url: url.into(),
        title: Some(title.into()),
    };
    let mut sources = vec![
        page("https://docs.rs/gpui", "GPUI"),
        page("https://example.org/", "The original source"),
        page("https://docs.rs/gpui/struct.Div.html", "Div"),
    ];
    sources.extend((2..9).map(|i| page(&format!("https://example{i}.org/"), "Extra reference")));
    attempt.presentation[0].content = AssistantPresentationContent::Answer {
        text: text.into(),
        sources,
    };
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let answer = rows(&view, cx)
        .into_iter()
        .find(|row| row.primary == text)
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("answer-sources").is_some());
        assert!(
            window
                .try_find_by_label("Show 2 sources from docs.rs")
                .is_some()
        );
        assert!(
            window
                .try_find_by_label("Open source from example.org")
                .is_some()
        );
        assert!(window.try_find("source-domain-example8.org").is_none());
        window.click("show-all-sources", cx);
        assert!(window.try_find("source-domain-example8.org").is_some());
        window.click("source-domain-example.org", cx);
    })
    .unwrap();
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.org/"));
    cx.update_window(window.into(), |_, window, cx| {
        window.click("source-domain-docs.rs", cx);
        assert!(window.try_find("popup-menu").is_some());
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(cx.opened_url().as_deref(), Some("https://docs.rs/gpui"));
    cx.update_window(window.into(), |_, window, cx| {
        window.click("source-domain-docs.rs", cx);
        window.press("down", cx);
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://docs.rs/gpui/struct.Div.html")
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.click("source-domain-docs.rs", cx);
        assert!(window.try_find("popup-menu").is_some());
        window.press("escape", cx);
    })
    .unwrap();
    frame(window, cx);
    assert!(
        receiver.try_recv().is_err(),
        "Escape must not stop the turn behind the menu"
    );
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("popup-menu").is_none());
        window
            .within(answer.key.clone())
            .click("block-disclosure", cx);
    })
    .unwrap();
    frame(window, cx);
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.try_find("answer-sources").is_none())
    })
    .unwrap();

    attempt.close(ResponseAttemptOutcome::Interrupted);
    let record = IncompleteResponse::from_attempt(&attempt).unwrap().unwrap();
    let saved = serde_json::to_vec(&TranscriptItem::IncompleteResponse(record)).unwrap();
    let restored: TranscriptItem = serde_json::from_slice(&saved).unwrap();
    controller.update(cx, |c, cx| {
        c.model.root.restore_transcript(vec![
            TranscriptItem::Message(Message::user("Trace the source.")),
            restored,
        ]);
        cx.notify();
    });
    frame(window, cx);
    let restored = rows(&view, cx)
        .into_iter()
        .find(|row| row.primary == text)
        .unwrap();
    assert_eq!(restored.sources, answer.sources);
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.try_find("answer-sources").is_some());
        window.within(restored.key).click("message-disclosure", cx);
    })
    .unwrap();
    frame(window, cx);
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.try_find("answer-sources").is_none())
    })
    .unwrap();
}
