use super::*;
use zevria_foundation::{SubtaskDescriptor, SubtaskId, SubtaskKind, SubtaskStatus};
use zevria_session_api::{SubtaskEvent, TurnEvent};

const TURN: TurnId = TurnId::new(9);

fn launch(
    controller: &Entity<SessionController>,
    name: &str,
    kind: SubtaskKind,
    status: SubtaskStatus,
    cx: &mut TestAppContext,
) -> SubtaskId {
    let id = SubtaskId::new(name);
    event(
        controller,
        E::Subtask(SubtaskEvent::Launched {
            turn_id: TURN,
            call_id: name.into(),
            entry_index: 0,
            descriptor: Box::new(SubtaskDescriptor {
                id: id.clone(),
                parent_session_id: "session".into(),
                title: format!("Inspect {name}"),
                kind,
                workspace: Some(format!("work/{name}")),
                status: SubtaskStatus::Starting,
            }),
        }),
        cx,
    );
    if status != SubtaskStatus::Starting {
        event(
            controller,
            E::Subtask(SubtaskEvent::Status {
                turn_id: TURN,
                id: id.clone(),
                status,
            }),
            cx,
        );
    }
    id
}

#[gpui_kit::test]
fn chip_counts_live_subtasks_and_the_compact_panel_stays_open_when_switching(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, _, controller, _) = setup_at_size(cx, 720., 520.);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("status-subtasks").is_none());
        assert!(window.try_find("subtasks-panel").is_none());
    })
    .unwrap();
    let explore = launch(
        &controller,
        "reader",
        SubtaskKind::Explore,
        SubtaskStatus::Running,
        cx,
    );
    let build = launch(
        &controller,
        "writer",
        SubtaskKind::Build,
        SubtaskStatus::Starting,
        cx,
    );
    launch(
        &controller,
        "docs",
        SubtaskKind::Explore,
        SubtaskStatus::Completed,
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("subtasks-panel").is_none());
        assert_eq!(
            window.find("status-subtasks").label(),
            Some("Show subtasks: 2 subtasks")
        );
        window.click("status-subtasks", cx);
    })
    .unwrap();
    event(
        &controller,
        E::Subtask(SubtaskEvent::Status {
            turn_id: TURN,
            id: explore.clone(),
            status: SubtaskStatus::Completed,
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("subtasks-panel").bounds().bottom()
                <= window.find("status-bar").bounds().top()
        );
        assert_eq!(
            window.find("status-subtasks").label(),
            Some("Hide subtasks: 1 subtask")
        );
        window.click("subtask-row-reader", cx);
    })
    .unwrap();
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.controls.selected_child.clone()),
        Some(explore)
    );
    event(
        &controller,
        E::Subtask(SubtaskEvent::Status {
            turn_id: TURN,
            id: build,
            status: SubtaskStatus::Failed,
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("subtasks-panel").visible());
        assert_eq!(
            window.find("status-subtasks").label(),
            Some("Hide subtasks: Subtasks")
        );
        window.click("subtask-conversation", cx);
    })
    .unwrap();
    assert!(controller.read_with(cx, |c, _| c.model.controls.selected_child.is_none()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("subtasks-panel").visible());
        window.click("status-subtasks", cx);
        window.render_frame(cx);
        assert!(window.try_find("subtasks-panel").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn shortcut_and_slash_open_subtasks_while_busy_and_escape_closes(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Run checks"),
        }),
        cx,
    );
    launch(
        &controller,
        "reader",
        SubtaskKind::Explore,
        SubtaskStatus::Running,
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("keep this draft", cx);
        window.press(Shortcut::TOGGLE_SUBTASKS.0, cx);
        window.render_frame(cx);
        assert!(window.find("subtasks-panel").visible());
    })
    .unwrap();
    assert_eq!(
        controller.read_with(cx, |c, _| c.draft.clone()),
        "keep this draft"
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("subtasks-panel").is_none());
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        view.read(cx).composer.clone().update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        window.render_frame(cx);
        window.input("/subtasks", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(receiver.try_recv().is_err());
    controller.read_with(cx, |c, _| {
        assert!(c.model.subtasks_panel_visible(), "draft: {:?}", c.draft);
        assert!(c.model.root.is_busy());
        assert!(c.draft.is_empty());
    });
}

#[gpui_kit::test]
fn subtasks_and_jobs_replace_each_other(cx: &mut TestAppContext) {
    let (window, _, controller, _receiver) = setup(cx);
    launch(
        &controller,
        "reader",
        SubtaskKind::Explore,
        SubtaskStatus::Running,
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press(Shortcut::TOGGLE_JOBS.0, cx);
        window.render_frame(cx);
        assert!(window.find("jobs-panel").visible());
        window.press(Shortcut::TOGGLE_SUBTASKS.0, cx);
        window.render_frame(cx);
        assert!(window.try_find("jobs-panel").is_none());
        assert!(window.find("subtasks-panel").visible());
        window.press(Shortcut::TOGGLE_JOBS.0, cx);
        window.render_frame(cx);
        assert!(window.find("jobs-panel").visible());
        assert!(window.try_find("subtasks-panel").is_none());
    })
    .unwrap();
    assert!(!controller.read_with(cx, |c, _| c.model.controls.subtasks_open));
}
