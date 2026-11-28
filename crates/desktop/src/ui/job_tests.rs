use super::*;
use crate::{
    disclosure::Phase,
    timeline::{BlockKind, TimelineRow},
};
use gpui_kit::base::test_support;
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
use zevria_session_api::{JobEvent, PlanEvent, TurnEvent};
use zevria_transcript::transcript::TranscriptItem;

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

fn exited(mut job: CommandJobSnapshot, code: i32) -> CommandJobSnapshot {
    job.status = CommandJobStatus::Exited {
        code: Some(code),
        success: code == 0,
        signal: None,
    };
    job.finished_at = Some(job.started_at + Duration::from_millis(32_200));
    job
}

fn notice_rows(view: &Entity<DesktopWindow>, cx: &gpui_kit::App) -> Vec<TimelineRow> {
    view.read(cx)
        .transcript
        .read(cx)
        .test_data(cx)
        .into_iter()
        .filter(|row| matches!(&row.data.kind, BlockKind::Jobs { .. }))
        .collect()
}

fn painted(window: &gpui_kit::Window, needle: &str) -> bool {
    test_support::texts(window)
        .iter()
        .any(|text| text.visible() && text.text().contains(needle))
}

fn notice_fixture(done: CommandJobSnapshot, running: CommandJobSnapshot) -> CommandJobNotice {
    let text = format!(
        "Command job update: these background jobs finished.\n\n{}\n<stdout>\nBuild succeeded (0 warnings)\n</stdout>\n<stderr>\nBuild trace\n</stderr>\n\nStill running:\n{} · running · 14.2s · {}",
        zevria_foundation::command_job_line(&done),
        running.id,
        running.command,
    );
    CommandJobNotice::new(
        CommandJobNoticeKind::Update,
        vec![done],
        vec![running],
        text,
    )
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
    assert!(request.request_id.starts_with("jobs-"));
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
        E::Job(JobEvent::ControlResult {
            request_id: request.request_id,
            result: Box::new(result),
        }),
        cx,
    );
}

#[gpui_kit::test]
fn job_copy_feedback_times_out_and_clears_on_selection_change(cx: &mut TestAppContext) {
    let (window, _, controller, mut receiver) = setup(cx);
    let first = job("copy-first", CommandJobMode::Background);
    let second = job("copy-second", CommandJobMode::Background);
    controller.update(cx, |c, cx| c.inspect_job(first.id.clone(), cx));
    cx.run_until_parked();
    let list = request(&mut receiver);
    assert_eq!(list.action, Request::Inspect { id: None });
    reply(
        &controller,
        list,
        Reply::Inspected(vec![view(first.clone()), view(second.clone())]),
        cx,
    );
    let mut load_output = |snapshot: CommandJobSnapshot, stdout: &str, cx: &mut TestAppContext| {
        let inspect = request(&mut receiver);
        assert_eq!(
            inspect.action,
            Request::Inspect {
                id: Some(snapshot.id.clone())
            }
        );
        let mut output = view(snapshot);
        output.stdout_tail = stdout.into();
        output.stderr_tail = "stderr".into();
        reply(&controller, inspect, Reply::Inspected(vec![output]), cx);
    };
    let copy_output = |expected: &str, cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_copy_label(&window.find("job-copy-output"), "Copy output");
            window.click("job-copy-output", cx);
            assert_copy_label(&window.find("job-copy-output"), "Copied output");
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(expected)
            );
        })
        .unwrap();
        cx.run_until_parked();
    };
    load_output(first, "first output", cx);
    copy_output("first output\nstderr", cx);
    cx.executor().advance_clock(COPY_FEEDBACK_DURATION);
    cx.run_until_parked();
    copy_output("first output\nstderr", cx);

    cx.update_window(window.into(), |_, window, cx| {
        window
            .within(format!("job-row-{}", second.id))
            .click("select-job", cx);
    })
    .unwrap();
    cx.run_until_parked();
    load_output(second, "second output", cx);
    copy_output("second output\nstderr", cx);
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
        event(
            &controller,
            E::Job(JobEvent::Updated {
                job: Box::new(job.clone()),
            }),
            cx,
        );
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
    assert!(!controller.read_with(cx, |c, _| c.model.root.jobs_inspector().is_open()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("jobs-panel").is_none());
        window.click("status-jobs", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(controller.read_with(cx, |c, _| c.model.root.jobs_inspector().is_open()));
    event(
        &controller,
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(PlanWorkflowState::Ready {
                artifact: PlanArtifact {
                    version: PlanVersion {
                        id: PlanId::new(),
                        revision: 1,
                    },
                    title: "Verify the working tree".into(),
                    markdown: "# Review the checks".into(),
                    source_turn_id: TURN,
                },
            }),
        }),
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
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Run checks"),
            references: Vec::new(),
        }),
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
        assert!(c.model.root.jobs_inspector().is_open());
        assert_eq!(c.draft, "a draft to keep");
    });
    reply(&controller, list, Reply::Inspected(Vec::new()), cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!controller.read_with(cx, |c, _| c.model.root.jobs_inspector().is_open()));
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
        assert!(c.model.root.jobs_inspector().is_open());
        assert!(c.model.root.is_busy());
        assert!(c.draft.is_empty());
    });
}

#[gpui_kit::test]
fn command_row_controls_background_then_stop_without_cancelling_the_turn(cx: &mut TestAppContext) {
    let (window, view, controller, mut receiver) = setup(cx);
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Start a command"),
            references: Vec::new(),
        }),
        cx,
    );
    event(
        &controller,
        E::Turn(TurnEvent::ModelCallStarted {
            turn_id: TURN,
            call: 1,
        }),
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
        E::Turn(TurnEvent::Intermediate {
            turn_id: TURN,
            message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            attempt_id: None,
        }),
        cx,
    );
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(job.clone()),
        }),
        cx,
    );
    let collapsed_height = |window: &mut gpui_kit::Window| {
        assert_eq!(window.find("block-disclosure").expanded(), Some(false));
        window.find("timeline-step").bounds().size.height
    };
    let (height, stop_right) = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("jobs-panel").is_none());
            let height = collapsed_height(window);
            let stop_right = window.find("job-stop").bounds().right();
            let transcript = view.read(cx).transcript.read(cx);
            let row = transcript
                .test_data(cx)
                .into_iter()
                .find(|r| r.data.is_disclosure())
                .unwrap();
            let focus = transcript.test_focus(&row.data.key, cx);
            window.focus(&focus, cx);
            for action in ["copy", "show-in-jobs", "job-background"] {
                window.press("tab", cx);
                assert_eq!(
                    window.within("timeline-step").find(action).focused(),
                    Some(true)
                );
            }
            window.click("job-background", cx);
            window.render_frame(cx);
            assert_eq!(collapsed_height(window), height);
            (height, stop_right)
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
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(job.clone()),
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("job-background").is_none());
        assert_eq!(collapsed_height(window), height);
        assert_eq!(window.find("job-stop").bounds().right(), stop_right);
        assert_eq!(window.find("block-disclosure").focused(), Some(true));
        for action in ["copy", "show-in-jobs", "job-stop"] {
            window.press("tab", cx);
            assert_eq!(
                window.within("timeline-step").find(action).focused(),
                Some(true)
            );
        }
        window.click("job-stop", cx);
        window.render_frame(cx);
        assert_eq!(collapsed_height(window), height);
        assert_eq!(window.find("job-stop").focused(), Some(true));
    })
    .unwrap();
    cx.run_until_parked();
    let stop = request(&mut receiver);
    assert_eq!(stop.action, Request::Terminate { id: job.id.clone() });

    job.finished_at = Some(SystemTime::now());
    job.status = CommandJobStatus::Exited {
        code: Some(0),
        success: true,
        signal: None,
    };
    event(
        &controller,
        E::Job(JobEvent::Updated { job: Box::new(job) }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("job-stop").is_none());
        assert_eq!(collapsed_height(window), height);
        assert_eq!(window.find("block-disclosure").focused(), Some(true));
    })
    .unwrap();
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
        assert!(!c.model.root.jobs_inspector().is_open());
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
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Read jobs notice"),
            references: Vec::new(),
        }),
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
        E::Job(JobEvent::Reported {
            turn_id: TURN,
            notice: Box::new(CommandJobNotice::new(
                CommandJobNoticeKind::Update,
                vec![snapshot],
                Vec::new(),
                text.into(),
            )),
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_copy_label(
            &window.within("timeline-jobs-notice").find("copy"),
            "Copy notice",
        );
        window.within("timeline-jobs-notice").click("copy", cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(text)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn restored_legacy_notice_has_frozen_output_and_no_live_controls(cx: &mut TestAppContext) {
    let (window, desktop, controller, _) = setup_at_size(cx, 720., 840.);
    cx.update(|cx| cx.set_reduce_motion(true));
    let mut running = job("historic", CommandJobMode::Background);
    running.started_at = SystemTime::now() - Duration::from_secs(21 * 3600);
    let done = exited(job("historic-done", CommandJobMode::Background), 0);
    let notice = notice_fixture(done, running);
    controller.update(cx, |c, cx| {
        theme::apply(zevria_theme::ZEVRIA_GRAPHITE_LIGHT, cx);
        c.model
            .root
            .restore_transcript(vec![TranscriptItem::CommandJobNotice(Box::new(
                notice.clone(),
            ))]);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(!controller.read_with(cx, |c, _| c.clock_required()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        let [row]: [TimelineRow; 1] = notice_rows(&desktop, cx).try_into().unwrap();
        assert_eq!(row.phase, Phase::Restored);
        assert!(!row.expanded && !row.needs_clock());
        assert!(
            !desktop
                .read(cx)
                .transcript
                .read(cx)
                .test_row_ticks(&row.data.key)
        );
        assert_eq!(window.find("block-disclosure").expanded(), Some(false));
        assert!(window.try_find("notice-stdout").is_none());
        window.click("block-disclosure", cx);
    })
    .unwrap();
    for expanded in [true, false] {
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("block-disclosure").expanded(), Some(expanded));
            window.press("enter", cx);
        })
        .unwrap();
    }
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("notice-also-running").visible());
        assert!(window.find("notice-snapshot-clock").visible());
        for id in [
            "notice-running-icon",
            "notice-still-running",
            "notice-live-badge",
            "job-stop",
            "notice-show-in-jobs",
            "show-in-jobs",
        ] {
            assert!(window.try_find(id).is_none(), "unexpected {id}");
        }
        assert!(painted(window, "14.2s at notice"));
        assert!(painted(window, "was running"));
        assert!(!painted(window, "21h"));
        assert!(painted(window, "Build succeeded (0 warnings)"));
        assert!(painted(window, "Build trace"));
        window.click("copy", cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(notice.text())
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn live_notice_settles_without_history_changes_or_collapsing(cx: &mut TestAppContext) {
    let (window, desktop, controller, mut receiver) = setup(cx);
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Run both checks"),
            references: Vec::new(),
        }),
        cx,
    );
    let running = job("notice-live", CommandJobMode::Background);
    let done = exited(job("notice-done", CommandJobMode::Background), 0);
    for job in [&running, &done] {
        event(
            &controller,
            E::Job(JobEvent::Updated {
                job: Box::new(job.clone()),
            }),
            cx,
        );
    }
    event(
        &controller,
        E::Job(JobEvent::Reported {
            turn_id: TURN,
            notice: Box::new(
                notice_fixture(done.clone(), running.clone())
                    .with_reported_at(running.started_at + Duration::from_millis(14_200)),
            ),
        }),
        cx,
    );
    let row_id = format!("notice-row-running-{}", running.id);
    let key = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            let [row]: [TimelineRow; 1] = notice_rows(&desktop, cx).try_into().unwrap();
            assert!(row.expanded && row.phase != Phase::Restored);
            assert!(
                desktop
                    .read(cx)
                    .transcript
                    .read(cx)
                    .test_row_ticks(&row.data.key)
            );
            for id in [
                "notice-live-badge",
                "notice-still-running",
                "notice-running-icon",
                "job-stop",
            ] {
                assert!(window.try_find(id).is_some(), "missing {id}");
            }
            assert_eq!(
                window
                    .within(row_id.clone())
                    .find("notice-show-in-jobs")
                    .label(),
                Some("Show in Jobs")
            );
            window
                .within(row_id.clone())
                .click("notice-show-in-jobs", cx);
            row.data.key.clone()
        })
        .unwrap();
    cx.run_until_parked();
    let list = request(&mut receiver);
    assert_eq!(list.action, Request::Inspect { id: None });
    reply(
        &controller,
        list,
        Reply::Inspected(vec![view(done), view(running.clone())]),
        cx,
    );
    let inspect = request(&mut receiver);
    assert_eq!(
        inspect.action,
        Request::Inspect {
            id: Some(running.id.clone())
        }
    );
    reply(
        &controller,
        inspect,
        Reply::Inspected(vec![view(running.clone())]),
        cx,
    );
    controller.read_with(cx, |c, _| {
        assert_eq!(c.model.root.jobs_inspector().inspected(), Some(&running.id))
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("jobs-panel").visible());
        window.click("status-jobs", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("jobs-panel").is_none());
        window.within(row_id.clone()).click("job-stop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        let focus = desktop.read(cx).transcript.read(cx).test_focus(&key, cx);
        window.focus(&focus, cx);
        for _ in 0..5 {
            window.press("tab", cx);
        }
        assert_eq!(
            window.within(row_id.clone()).find("job-stop").focused(),
            Some(true)
        );
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        request(&mut receiver).action,
        Request::Terminate {
            id: running.id.clone()
        }
    );
    let revision = controller.read_with(cx, |c, _| c.model.root.conversation.revision());
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(exited(running, 0)),
        }),
        cx,
    );
    controller.read_with(cx, |c, _| {
        assert!(c.model.root.is_busy());
        assert_eq!(c.model.root.conversation.revision(), revision);
    });
    assert!(receiver.try_recv().is_err());
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(notice_rows(&desktop, cx)[0].expanded);
        assert!(!desktop.read(cx).transcript.read(cx).test_row_ticks(&key));
        assert!(window.find("notice-also-running").visible());
        assert!(painted(window, "14.2s at notice"));
        assert!(painted(window, "now exited 0"));
        assert!(window.try_find("notice-live-badge").is_none());
        assert!(window.try_find("notice-running-icon").is_none());
        assert!(window.within(row_id.clone()).try_find("job-stop").is_none());
        assert!(
            window
                .within(row_id)
                .try_find("notice-show-in-jobs")
                .is_some()
        );
        assert_eq!(window.find("block-disclosure").focused(), Some(true));
        window.press(Shortcut::COLLAPSE_ALL.0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    desktop.read_with(cx, |_, cx| assert!(!notice_rows(&desktop, cx)[0].expanded));
}

#[gpui_kit::test]
fn plan_review_hides_notice_inspection_without_hiding_stop(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_reduce_motion(true));
    let (window, desktop, controller, _) = setup(cx);
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Run checks"),
            references: Vec::new(),
        }),
        cx,
    );
    let running = job("reused-id", CommandJobMode::Background);
    let finished_row = format!("notice-row-finished-{}", running.id);
    let running_row = format!("notice-row-running-{}", running.id);
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(running.clone()),
        }),
        cx,
    );
    // Defend against a malformed notice listing the same ID in both sections.
    event(
        &controller,
        E::Job(JobEvent::Reported {
            turn_id: TURN,
            notice: Box::new(notice_fixture(exited(running.clone(), 0), running.clone())),
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("notice-finished-heading").visible());
        assert!(
            window
                .within(finished_row.clone())
                .find("notice-show-in-jobs")
                .visible()
        );
        assert!(
            window
                .within(finished_row.clone())
                .try_find("job-stop")
                .is_none()
        );
        assert!(
            window
                .within(running_row.clone())
                .find("job-stop")
                .visible()
        );
    })
    .unwrap();
    let plan = |state| {
        E::Plan(PlanEvent::StateChanged {
            state: Box::new(state),
        })
    };
    event(
        &controller,
        plan(PlanWorkflowState::Ready {
            artifact: PlanArtifact {
                version: PlanVersion {
                    id: PlanId::new(),
                    revision: 1,
                },
                title: "Review checks".into(),
                markdown: "# Verify results".into(),
                source_turn_id: TURN,
            },
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("plan-review").visible());
        assert!(window.try_find("show-in-jobs").is_none());
        assert!(window.try_find("notice-show-in-jobs").is_none());
        assert!(
            window
                .within(running_row.clone())
                .find("job-stop")
                .visible()
        );
    })
    .unwrap();

    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(exited(running, 0)),
        }),
        cx,
    );
    let generation = controller.read_with(cx, |c, _| c.model.root.jobs_inspector().generation());
    event(&controller, plan(PlanWorkflowState::Idle), cx);
    assert_eq!(
        controller.read_with(cx, |c, _| c.model.root.jobs_inspector().generation()),
        generation
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        let row = &notice_rows(&desktop, cx)[0];
        assert!(
            !desktop
                .read(cx)
                .transcript
                .read(cx)
                .test_row_ticks(&row.data.key)
        );
        assert!(
            window
                .within(finished_row)
                .find("notice-show-in-jobs")
                .visible()
        );
        assert!(
            window
                .within(running_row)
                .find("notice-show-in-jobs")
                .visible()
        );
        assert!(window.try_find("job-stop").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn continuation_notice_summarizes_multiple_jobs_and_expands_captured_output(
    cx: &mut TestAppContext,
) {
    use zevria_foundation::command_job_line;
    let (window, _, controller, _) = setup(cx);
    let done = exited(job("summary-done", CommandJobMode::Background), 0);
    let failed = exited(job("summary-failed", CommandJobMode::Background), 2);
    let stdout = (1..=17)
        .map(|i| format!("progress line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!(
        "Command job update: continued after jobs finished.\n\n{}\n<stdout>\n{stdout}\n</stdout>\n\n{}\n<stderr>\nfailed check\n</stderr>",
        command_job_line(&done),
        command_job_line(&failed),
    );
    let notice = CommandJobNotice::new(
        CommandJobNoticeKind::Continuation,
        vec![done, failed],
        vec![],
        text,
    );
    controller.update(cx, |c, cx| {
        c.model
            .root
            .restore_transcript(vec![TranscriptItem::CommandJobNotice(Box::new(notice))]);
        cx.notify();
    });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("notice-resumed").visible());
        assert!(painted(window, "2 finished · 1 failed"));
        assert_eq!(window.find("block-disclosure").expanded(), Some(false));
        window.click("block-disclosure", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("notice-finished-heading").is_none());
        assert!(painted(window, "progress line 16"));
        assert!(!painted(window, "progress line 17"));
        assert!(!painted(window, "failed check"));
        assert_eq!(
            window.find("show-all-output").label(),
            Some("Show all 18 lines")
        );
        window.click("show-all-output", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(painted(window, "progress line 17"));
        assert!(painted(window, "failed check"));
        assert_eq!(window.find("show-all-output").label(), Some("Show less"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn resumed_session_keeps_old_notice_folded_when_new_notice_arrives(cx: &mut TestAppContext) {
    let (window, desktop, controller, _) = setup(cx);
    let fixture = |id: &str| {
        notice_fixture(
            exited(job(id, CommandJobMode::Background), 0),
            job("other", CommandJobMode::Background),
        )
    };
    controller.update(cx, |c, cx| {
        c.model
            .root
            .restore_transcript(vec![TranscriptItem::CommandJobNotice(Box::new(fixture(
                "old",
            )))]);
        cx.notify();
    });
    cx.run_until_parked();
    event(
        &controller,
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("New work after resuming"),
            references: Vec::new(),
        }),
        cx,
    );
    event(
        &controller,
        E::Job(JobEvent::Reported {
            turn_id: TURN,
            notice: Box::new(fixture("new")),
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        let [old, new]: [TimelineRow; 2] = notice_rows(&desktop, cx).try_into().unwrap();
        assert_eq!(old.phase, Phase::Restored);
        assert!(!old.expanded);
        assert_ne!(new.phase, Phase::Restored);
        assert!(new.expanded);
        assert_eq!(
            window
                .within(old.data.key.clone())
                .find("block-disclosure")
                .expanded(),
            Some(false)
        );
        assert_eq!(
            window
                .within(new.data.key.clone())
                .find("block-disclosure")
                .expanded(),
            Some(true)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn failed_background_request_without_snapshot_keeps_command_badges(cx: &mut TestAppContext) {
    let (window, _, controller, _) = setup(cx);
    let call_id = || CallId::from_wire("failed-background");
    let tool_name = || ToolName::new("command").unwrap();
    let call = ToolCall::new(
        call_id(),
        ToolFunction::new(
            tool_name(),
            serde_json::json!({"command": "run checks", "background": true}),
        ),
    );
    let result = Message::tool_result(
        call_id(),
        tool_name(),
        "exit_code: 19\nduration: 0.2s\n<stderr>\npermission denied\ncheck the workspace\n</stderr>",
    );
    controller.update(cx, |c, cx| {
        c.model.root.restore_transcript(vec![
            TranscriptItem::Message(Message::user("Run checks")),
            TranscriptItem::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::ToolCall(call),
            ]))),
            TranscriptItem::ToolResults {
                message: result,
                metadata: vec![],
                skill_applications: vec![],
            },
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(painted(window, "Background"));
        assert!(painted(window, "exit 19"));
        assert!(painted(window, "2 lines · 0.2s"));
        assert!(window.try_find("job-stop").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn returned_background_command_ticks_settles_and_restores_static(cx: &mut TestAppContext) {
    use zevria_foundation::{
        CommandJobDetail, CommandJobDisposition, ToolCallOutcome, ToolResultDetail,
        ToolResultMetadata,
    };
    let (window, desktop, controller, _) = setup(cx);
    let mut background = job("background-tool", CommandJobMode::Background);
    background.started_at = SystemTime::now() - Duration::from_secs(78);
    let call_id = background.call_id.as_ref().unwrap();
    let tool_name = || ToolName::new("command").unwrap();
    let prompt = Message::user("Start a background command");
    let call = Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
        ToolCall::new(
            CallId::from_wire(call_id),
            ToolFunction::new(
                tool_name(),
                serde_json::json!({"command": background.command, "background": true}),
            ),
        ),
    )]));
    let result = Message::tool_result(
        CallId::from_wire(call_id),
        tool_name(),
        format!(
            "status: running\nid: {}\nduration: 1.2s\n<stdout>\nBuild staged\n</stdout>",
            background.id
        ),
    );
    let metadata = vec![ToolResultMetadata {
        id: call_id.clone(),
        call_id: None,
        tool_name: "command".into(),
        outcome: ToolCallOutcome::Success,
        diagnostic: None,
        detail: Some(ToolResultDetail::CommandJob(CommandJobDetail {
            id: background.id.clone(),
            disposition: CommandJobDisposition::StartedInBackground,
        })),
    }];
    for update in [
        E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: prompt.clone(),
            references: Vec::new(),
        }),
        E::Turn(TurnEvent::ModelCallStarted {
            turn_id: TURN,
            call: 1,
        }),
        E::Turn(TurnEvent::Intermediate {
            turn_id: TURN,
            message: call.clone(),
            attempt_id: None,
        }),
        E::Job(JobEvent::Updated {
            job: Box::new(background.clone()),
        }),
        E::Turn(TurnEvent::ToolResults {
            turn_id: TURN,
            message: result.clone(),
            metadata: metadata.clone(),
        }),
    ] {
        event(&controller, update, cx);
    }
    let key = cx
        .update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            let transcript = desktop.read(cx).transcript.read(cx);
            let row = transcript
                .test_data(cx)
                .into_iter()
                .find(|row| matches!(&row.data.kind, BlockKind::Step(_)))
                .unwrap();
            assert!(!row.data.is_active_work());
            assert!(transcript.test_row_ticks(&row.data.key));
            assert!(painted(window, "Running · 1m"));
            assert!(window.find("job-stop").visible());
            row.data.key.clone()
        })
        .unwrap();
    let id = background.id.clone();
    event(
        &controller,
        E::Job(JobEvent::Updated {
            job: Box::new(exited(background, 1)),
        }),
        cx,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(!desktop.read(cx).transcript.read(cx).test_row_ticks(&key));
        assert!(painted(window, "Background"));
        assert!(painted(window, "exit 1 · 32.2s"));
        assert!(window.try_find("job-stop").is_none());
    })
    .unwrap();
    controller.update(cx, |c, cx| {
        c.model.root = Default::default();
        c.model.root.restore_transcript(vec![
            TranscriptItem::Message(prompt),
            TranscriptItem::Message(call),
            TranscriptItem::ToolResults {
                message: result,
                metadata,
                skill_applications: vec![],
            },
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(!controller.read_with(cx, |c, _| c.clock_required()));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(painted(window, "Background"));
        assert!(!painted(window, "Running ·"));
        assert!(!painted(window, "exit 1"));
        assert!(painted(window, "1 line · 1.2s"));
        assert!(window.try_find("job-stop").is_none());
        assert!(window.try_find("show-in-jobs").is_none());
        window.click("block-disclosure", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(painted(window, id.as_str()));
    })
    .unwrap();
}
