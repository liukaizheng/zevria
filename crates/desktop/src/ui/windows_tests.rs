use super::*;
use crate::ui::window::Replacement;
use gpui_kit::{ClipboardItem, Focusable as _, base::test_support};
use zevria_session_api::PlanEvent;

fn assert_painted(window: &gpui_kit::Window, label: &str) {
    assert!(
        test_support::texts(window)
            .iter()
            .any(|text| text.visible() && text.text() == label),
        "{label:?} was not painted"
    );
}

#[gpui_kit::test]
fn ctrl_enter_confirms_only_a_visible_unblocked_decision(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("keep the draft", cx);
        window.press("ctrl-enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        receiver.try_recv().is_err(),
        "Ctrl+Enter must not submit a draft"
    );
    assert_eq!(
        controller.read_with(cx, |c, _| c.draft.clone()),
        "keep the draft"
    );

    let version = PlanVersion {
        id: PlanId::new(),
        revision: 1,
    };
    event(
        &controller,
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version,
                    title: "Plan".into(),
                    markdown: "Review the change".into(),
                    source_turn_id: TurnId::new(1),
                },
            }),
        }),
        cx,
    );
    view.update(cx, |view, cx| {
        view.pending_replace = Some(Replacement::New);
        cx.notify();
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-enter", cx);
    })
    .unwrap();
    assert!(
        receiver.try_recv().is_err(),
        "a replacement modal blocks the background Plan"
    );

    view.update(cx, |view, cx| {
        view.pending_replace = None;
        cx.notify();
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-enter", cx);
    })
    .unwrap();
    assert_eq!(
        receiver.try_recv().unwrap(),
        SessionCommand::Turn(TurnCommand::ResolvePlan {
            expected: version,
            decision: PlanDecision::ImplementCurrent,
        })
    );
}

#[gpui_kit::test]
fn stock_ctrl_editing_keys_still_reach_the_composer(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    controller.update(cx, |c, cx| c.edit_draft("retain this draft".into(), cx));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(view.read(cx).composer.focus_handle(cx).is_focused(window));
        window.press("ctrl-a", cx);
        window.press("ctrl-c", cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("retain this draft")
        );
        cx.write_to_clipboard(ClipboardItem::new_string("pasted".into()));
        window.press("ctrl-v", cx);
        assert_eq!(view.read(cx).composer.read(cx).value().as_str(), "pasted");
        window.press("ctrl-z", cx);
        assert_eq!(
            view.read(cx).composer.read(cx).value().as_str(),
            "retain this draft"
        );
    })
    .unwrap();
    assert!(receiver.try_recv().is_err());
}

#[gpui_kit::test]
fn windows_view_menu_dispatches_to_the_window_and_restores_focus(cx: &mut TestAppContext) {
    let (window, view, _, _) = setup(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("close").visible());
        assert_painted(window, "Ctrl+N");
        assert!(view.read(cx).sidebar_visible(1240.));
        // The fourth top-level menu is View; its first item toggles the sidebar.
        window.within(3_usize).click("menu", cx);
        assert!(window.find("popup-menu").visible());
        assert_painted(window, "Ctrl+Shift+S");
        window.within("popup-menu").click(0_usize, cx);
    })
    .unwrap();
    // GPUI defers menu action dispatch until the window update has returned.
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        assert!(!view.read(cx).sidebar_visible(1240.));
        assert!(view.read(cx).composer.focus_handle(cx).is_focused(window));
    })
    .unwrap();
}

#[gpui_kit::test]
fn windows_quit_shortcut_and_menu_stop_the_controller(cx: &mut TestAppContext) {
    for menu in [false, true] {
        let (window, _, _, mut receiver) = setup(cx);
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            if menu {
                window.within(0_usize).click("menu", cx);
                assert_painted(window, "Ctrl+Q");
                window.press("down", cx);
                window.press("enter", cx);
            } else {
                window.press("ctrl-q", cx);
            }
        })
        .unwrap();
        cx.run_until_parked();
        // Ordered shutdown drops the command sender, even with no real engine in this fixture.
        assert_eq!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        );
    }
}
