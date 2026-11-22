use super::*;
use crate::{
    timeline::{BlockKind, Step, StepPosition, TimelineRow, web::WebRow},
    ui::status::StatusLine,
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

fn rows(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<TimelineRow> {
    view.read_with(cx, |view, cx| view.transcript.read(cx).test_data(cx))
}

fn web_rows(view: &Entity<DesktopWindow>, cx: &TestAppContext) -> Vec<TimelineRow> {
    rows(view, cx)
        .into_iter()
        .filter(|row| matches!(row.data.kind, BlockKind::Step(Step::Web(_))))
        .collect()
}

fn web_step(row: &TimelineRow) -> &WebRow {
    let BlockKind::Step(Step::Web(web)) = &row.data.kind else {
        panic!("expected web step")
    };
    web
}

#[gpui_kit::test]
fn web_actions_grow_in_place_then_auto_collapse_and_copy_exactly(cx: &mut TestAppContext) {
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
    assert_eq!(web_step(&first).status(), StatusIcon::Running);
    assert!(first.expanded);
    controller.read_with(cx, |controller, _| {
        let status = StatusLine::new(controller.model.visible(), controller.cancelling);
        assert_eq!(status.activity, "Searching the web");
    });
    let entity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&first.data.key, cx)
            .unwrap()
    });
    attempt.hosted_activity[0].action =
        Some(json!({"type":"search","queries":["gpui text view links","gpui link safety"]}));
    attempt.hosted_activity[1].action = Some(json!({"type":"search","query":"gpui components"}));
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    assert_eq!(web_rows(&view, cx)[0].data.key, first.data.key);
    assert_eq!(
        entity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&first.data.key, cx)
            .unwrap())
    );
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("web-action-1").bounds().origin.y < window.find("web-action-2").bounds().origin.y);
        let mut row = window.within(first.data.key.clone());
        assert_eq!(row.find("block-disclosure").expanded(), Some(true));
        assert!(row.try_find_by_label("Copy web activity").is_some());
        row.hover("block-disclosure", cx);
        row.click("copy", cx);
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(),
            Some("Web search: gpui text view links · gpui link safety ✓\nWeb search: gpui components ◐"));
    }).unwrap();

    attempt.hosted_activity[1].status = HostedActivityStatus::Completed;
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let completed = web_rows(&view, cx).remove(0);
    assert_eq!(completed.data.key, first.data.key);
    assert_eq!(web_step(&completed).status(), StatusIcon::Done);
    assert_eq!(web_step(&completed).counts(), "2 searches");
    assert!(!completed.expanded);
    assert!(completed.data.primary_copy().ends_with("gpui components ✓"));
}

#[gpui_kit::test]
fn ordered_reasoning_splits_web_steps_and_preserves_first_override(cx: &mut TestAppContext) {
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
    let initial = web_rows(&view, cx);
    let [first] = initial.as_slice() else {
        panic!("expected one coalesced web step before reasoning arrives")
    };
    assert_eq!(web_step(first).actions.len(), 2);
    assert!(!first.expanded);
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(first.data.key.clone())
            .click("block-disclosure", cx)
    })
    .unwrap();
    frame(window, cx);
    assert!(web_rows(&view, cx)[0].expanded);
    attempt.presentation.push(reasoning(1));
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let ordered = rows(&view, cx)
        .into_iter()
        .filter(|r| !matches!(r.data.kind, BlockKind::Live))
        .collect::<Vec<_>>();
    let [prompt, before, thought, after] = ordered.as_slice() else {
        panic!("expected the web steps on either side of the thought")
    };
    assert!(matches!(prompt.data.kind, BlockKind::Prompt { .. }));
    assert_eq!(web_step(before).single().unwrap().output_index, 0);
    assert_eq!(web_step(after).single().unwrap().output_index, 2);
    assert_eq!(before.position, Some(StepPosition::Only));
    assert_eq!(after.position, Some(StepPosition::Only));
    assert_eq!(thought.data.thought().unwrap().segments.len(), 1);
    assert_eq!(
        thought.data.primary_copy(),
        "Check the documentation before answering."
    );
    assert_eq!(before.data.key, first.data.key);
    assert!(before.expanded);
    assert!(!after.expanded);
}

#[gpui_kit::test]
fn attempt_thought_rekeys_by_anchor_without_overwriting_an_existing_group(cx: &mut TestAppContext) {
    use AssistantPartIdentity::{Content, Summary};
    use AssistantPresentationContent as Part;
    let (window, view, controller, _) = setup(cx);
    let mut attempt = start(&controller, cx);
    attempt.presentation = vec![
        reasoning(3),
        part(
            4,
            Content(0),
            Part::answer("The documentation confirms it."),
        ),
        part(
            5,
            Summary(0),
            Part::Reasoning {
                text: "Check the result".into(),
            },
        ),
    ];
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let initial = rows(&view, cx);
    let [_, first, _, later] = initial.as_slice() else {
        panic!("expected two thought groups separated by prose")
    };
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(first.data.key.clone())
            .click("block-disclosure", cx)
    })
    .unwrap();
    frame(window, cx);
    assert!(rows(&view, cx)[1].expanded);
    let identity = view.read_with(cx, |view, cx| {
        view.transcript
            .read(cx)
            .test_identity(&first.data.key, cx)
            .unwrap()
    });

    // The first insertion has the same text as the old group, so matching
    // the live ordinal alone would send its override to the wrong row.
    attempt.presentation.extend([
        reasoning(0),
        part(1, Content(0), Part::answer("I found the documentation.")),
        part(
            2,
            Summary(0),
            Part::Reasoning {
                text: "Review the context first".into(),
            },
        ),
    ]);
    stream(&controller, &mut attempt, cx);
    frame(window, cx);
    let updated = rows(&view, cx);
    let [_, earlier, _, prepended, _, retained] = updated.as_slice() else {
        panic!("expected the earlier thought and prepended segment")
    };
    assert_eq!(earlier.data.primary_copy(), first.data.primary_copy());
    assert!(!earlier.expanded);
    assert_ne!(prepended.data.key, first.data.key);
    assert_eq!(
        prepended.data.primary_copy(),
        "Review the context first\n\nCheck the documentation before answering."
    );
    assert!(prepended.expanded);
    assert_eq!(retained.data.key, later.data.key);
    assert_eq!(
        identity,
        view.read_with(cx, |view, cx| view
            .transcript
            .read(cx)
            .test_identity(&prepended.data.key, cx)
            .unwrap())
    );

    // Removing the separator joins the groups, but the expansion of the
    // disappearing group should not override the first group's state.
    attempt
        .presentation
        .retain(|part| part.source.output_index != 1);
    stream(&controller, &mut attempt, cx);
    let joined = rows(&view, cx);
    let [_, joined_group, _, retained] = joined.as_slice() else {
        panic!("expected the two earlier runs to join")
    };
    assert_eq!(joined_group.data.key, earlier.data.key);
    assert_eq!(joined_group.data.thought().unwrap().segments.len(), 3);
    assert!(!joined_group.expanded);
    assert_eq!(retained.data.key, later.data.key);
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
    let closed = web_rows(&view, cx).remove(0);
    assert_eq!(web_step(&closed).status(), StatusIcon::Interrupted);
    assert!(!closed.expanded);
    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(closed.data.key.clone())
            .click("block-disclosure", cx)
    })
    .unwrap();
    frame(window, cx);
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
        window.click("block-disclosure", cx)
    })
    .unwrap();
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
        .find(|row| row.data.primary_copy() == text)
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
        window.press(Shortcut::COLLAPSE_ALL.0, cx);
    })
    .unwrap();
    frame(window, cx);
    cx.update_window(window.into(), |_, window, _| {
        let sources = window.find("answer-sources").bounds();
        let prose = window.find(answer.data.key.clone()).bounds();
        assert_eq!(sources.origin.x, prose.origin.x);
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
        .find(|row| row.data.primary_copy() == text)
        .unwrap();
    assert_eq!(restored.data.sources, answer.data.sources);
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.try_find("answer-sources").is_some());
    })
    .unwrap();
}
