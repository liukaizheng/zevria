use super::*;
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, ToolCall, ToolFunction, ToolName,
};
use std::time::{Duration, SystemTime};
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobNotice, CommandJobNoticeKind, CommandJobSnapshot,
    CommandJobStatus, CommandJobView,
};
use zevria_session_api::{
    CommandControl, CommandControlAction as Request, CommandControlResult as Reply,
};

const TURN: TurnId = TurnId::new(8);

fn job(key: &str, mode: CommandJobMode) -> CommandJobSnapshot {
    CommandJobSnapshot {
        id: CommandJobId::new(format!("job-{key}")),
        call_id: Some(format!("call-{key}")),
        command: "sleep 60".into(),
        mode,
        status: CommandJobStatus::Running,
        started_at: SystemTime::now() - Duration::from_secs(2),
        finished_at: None,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

fn view(snapshot: CommandJobSnapshot) -> CommandJobView {
    CommandJobView {
        snapshot,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    }
}

fn request(receiver: &mut Receiver) -> CommandControl {
    let SessionCommand::Control(ControlCommand::Command(request)) = receiver.try_recv().unwrap()
    else {
        panic!("expected a command job control, not a turn command");
    };
    assert!(request.request_id.starts_with("desktop.jobs."));
    request
}

fn reply(
    controller: &Entity<SessionController>,
    request: CommandControl,
    result: Reply,
    cx: &mut TestAppContext,
) {
    event(
        controller,
        E::CommandControlResult {
            request_id: request.request_id,
            result,
        },
        cx,
    );
}

#[gpui_kit::test]
fn compact_jobs_chip_toggles_a_panel_that_starts_closed_and_hides_under_plan(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, _, controller, mut receiver) = setup_at_size(cx, 720., 520.);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("status-jobs").is_none());
        assert!(window.try_find("jobs-panel").is_none());
    })
    .unwrap();
    let mut finished = job("ready", CommandJobMode::Background);
    finished.started_at -= Duration::from_secs(10);
    finished.finished_at = Some(finished.started_at + Duration::from_secs(3));
    finished.status = CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    };
    let running = job("running", CommandJobMode::Background);
    for job in [&finished, &running] {
        event(&controller, E::CommandJobUpdated { job: job.clone() }, cx);
    }
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("jobs-panel").is_none());
        assert!(window.find("status-jobs").visible());
        window.click("status-jobs", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let list = request(&mut receiver);
    assert!(matches!(list.action, Request::Inspect { id: None }));
    reply(
        &controller,
        list,
        Reply::Inspected(vec![view(finished.clone()), view(running.clone())]),
        cx,
    );
    let detail = request(&mut receiver);
    assert_eq!(
        detail.action,
        Request::Inspect {
            id: Some(running.id.clone())
        }
    );
    reply(
        &controller,
        detail,
        Reply::Inspected(vec![view(running)]),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        let panel = window.find("jobs-panel").bounds();
        assert!(panel.bottom() <= window.find("status-bar").bounds().top());
        assert!(window.find("status-jobs").visible());
        window.press("up", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let detail = request(&mut receiver);
    assert_eq!(
        detail.action,
        Request::Inspect {
            id: Some(finished.id.clone())
        }
    );
    reply(
        &controller,
        detail,
        Reply::Inspected(vec![view(finished)]),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("status-jobs", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!controller.read_with(cx, |c, _| c.model.jobs.is_open()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("jobs-panel").is_none());
        window.click("status-jobs", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(controller.read_with(cx, |c, _| c.model.jobs.is_open()));
    event(
        &controller,
        E::PlanStateChanged {
            state: PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version: PlanVersion {
                        id: PlanId::new(),
                        revision: 1,
                    },
                    title: "Verify the working tree".into(),
                    markdown: "# Review the checks".into(),
                    source_turn_id: TURN,
                },
            },
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("plan-review").visible());
        assert!(window.try_find("jobs-panel").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn jobs_shortcut_keeps_draft_and_slash_opens_during_a_turn(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    event(
        &controller,
        E::TurnStarted {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Run checks"),
        },
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("a draft to keep", cx);
        window.press(Shortcut::TOGGLE_JOBS.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let list = request(&mut receiver);
    controller.read_with(cx, |c, _| {
        assert!(c.model.jobs.is_open());
        assert_eq!(c.draft, "a draft to keep");
    });
    reply(&controller, list, Reply::Inspected(Vec::new()), cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!controller.read_with(cx, |c, _| c.model.jobs.is_open()));
    cx.update_window(window.into(), |_, window, cx| {
        view.read(cx).composer.clone().update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        window.render_frame(cx);
        window.input("/jobs", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(matches!(
        request(&mut receiver).action,
        Request::Inspect { id: None }
    ));
    assert!(receiver.try_recv().is_err());
    controller.read_with(cx, |c, _| {
        assert!(c.model.jobs.is_open());
        assert!(c.model.root.is_busy());
        assert!(c.draft.is_empty());
    });
}

#[gpui_kit::test]
fn command_row_controls_background_then_stop_without_cancelling_the_turn(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    event(
        &controller,
        E::TurnStarted {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Start a command"),
        },
        cx,
    );
    event(
        &controller,
        E::ModelCallStarted {
            turn_id: TURN,
            call: 1,
        },
        cx,
    );
    let mut job = job("inline", CommandJobMode::Foreground);
    let call = ToolCall::new(
        CallId::from_wire(job.call_id.as_deref().unwrap()),
        ToolFunction::new(
            ToolName::new("command").unwrap(),
            serde_json::json!({"command": "sleep 60"}),
        ),
    );
    event(
        &controller,
        E::Intermediate {
            turn_id: TURN,
            message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            attempt_id: None,
        },
        cx,
    );
    event(&controller, E::CommandJobUpdated { job: job.clone() }, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("jobs-panel").is_none());
        window.click("job-background", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let background = request(&mut receiver);
    assert_eq!(
        background.action,
        Request::MoveToBackground { id: job.id.clone() }
    );
    job.mode = CommandJobMode::Background;
    reply(&controller, background, Reply::Accepted(job.clone()), cx);
    event(&controller, E::CommandJobUpdated { job: job.clone() }, cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("job-background").is_none());
        window.click("job-stop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let stop = request(&mut receiver);
    assert_eq!(stop.action, Request::Terminate { id: job.id });
    reply(
        &controller,
        stop,
        Reply::Rejected {
            message: "That job just finished.".into(),
        },
        cx,
    );
    assert!(receiver.try_recv().is_err());
    controller.read_with(cx, |c, _| {
        use zevria_presentation::conversation::HistoryEntry;
        assert!(c.model.root.is_busy());
        assert!(!c.model.jobs.is_open());
        assert!(c.model.root.conversation.history().iter().any(
            |entry| matches!(entry, HistoryEntry::Error(text) if text == "That job just finished.")
        ));
    });
}

#[gpui_kit::test]
fn job_notice_copies_original_model_visible_text(cx: &mut TestAppContext) {
    let (window, _, controller, _) = setup(cx);
    event(
        &controller,
        E::TurnStarted {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Read jobs notice"),
        },
        cx,
    );
    let mut snapshot = job("done", CommandJobMode::Background);
    snapshot.finished_at = Some(SystemTime::now());
    snapshot.status = CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    };
    let text = "Command job update: exact output\n\nrtk proxy cargo test\n";
    event(
        &controller,
        E::CommandJobsReported {
            turn_id: TURN,
            notice: CommandJobNotice::new(
                CommandJobNoticeKind::Update,
                vec![snapshot],
                Vec::new(),
                text.into(),
            ),
        },
        cx,
    );
    let key = controller.read_with(cx, |c, _| {
        let conversation = &c.model.root.conversation;
        let index = conversation
            .history()
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    zevria_presentation::conversation::HistoryEntry::JobNotice(..)
                )
            })
            .unwrap();
        format!("{:?}-entry-{index}", conversation.epoch())
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.within(key).click("copy", cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(text)
        );
    })
    .unwrap();
}
