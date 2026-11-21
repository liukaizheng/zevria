use super::*;
use crate::engine::EngineRuntime;
use std::time::{Duration, Instant};
use zevria_host::test_support::BlockedPromptCount;

const PROMPT: &str = "Show the draft before durable admission";

fn until(cx: &mut TestAppContext, ready: impl Fn(&TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.executor().advance_clock(Duration::from_millis(2));
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "session transition timed out");
        std::thread::yield_now();
    }
}

#[gpui_kit::test]
fn first_frame_echoes_the_prompt_while_provider_count_is_blocked(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    for replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let runtime = EngineRuntime::new().unwrap();
        let mut provider = runtime.0.block_on(BlockedPromptCount::start());
        let config = provider.config(&workspace);
        let (window, view, controller) = cx.update(|cx| {
            gpui_kit::init(cx);
            theme::apply(zevria_theme::ZEVRIA_GRAPHITE, cx);
            actions::install(cx);
            cx.set_global(runtime.clone());
            let controller = cx.new(|cx| SessionController::new(Ok(config), Some(workspace), cx));
            let (window, view) =
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| DesktopWindow::new(controller.clone(), window, cx))
                })
                .unwrap();
            (
                window.downcast::<gpui_kit::base::Root>().unwrap(),
                view,
                controller,
            )
        });
        controller.update(cx, |this, cx| this.start_initial(false, cx));
        until(cx, |cx| {
            controller.read_with(cx, |this, _| this.phase == crate::controller::Phase::Live)
        });
        let path = controller.read_with(cx, |this, _| this.model.transcript_path.clone());
        // Measure the idle tip before sending, just as in a live window.
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        controller.update(cx, |this, cx| {
            this.draft = PROMPT.into();
            this.submit(cx);
        });
        runtime.0.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), &mut provider.count_started)
                .await
                .unwrap()
                .unwrap();
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("preparing-prompt").bounds().size.height > px(0.));
            assert!(window.find("pending-prompt-text").bounds().size.height > px(0.));
        })
        .unwrap();
        let data = view.read_with(cx, |view, cx| view.transcript.read(cx).test_data(cx));
        assert_eq!(data.iter().filter(|b| b.body == PROMPT).count(), 1);
        assert!(
            data.iter()
                .filter(|b| b.body == PROMPT)
                .all(|b| b.source.is_none() && b.header.is_none() && b.target.is_none())
        );
        assert!(controller.read_with(cx, |this, _| {
            this.model.root.conversation.history().is_empty()
        }));
        assert!(!path.exists());
        assert!(matches!(
            provider.completion_started.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        if !replace {
            provider.release_count.take().unwrap().send(()).unwrap();
            runtime.0.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), &mut provider.completion_started)
                    .await
                    .unwrap()
                    .unwrap();
            });
            until(cx, |cx| {
                controller.read_with(cx, |this, _| this.model.root.active_turn_id().is_some())
            });
            let data = view.read_with(cx, |view, cx| view.transcript.read(cx).test_data(cx));
            let mut prompts = data.iter().filter(|b| b.body == PROMPT);
            let canonical = prompts.next().expect("canonical prompt in projection");
            assert!(canonical.source.is_some() && canonical.header.is_some());
            assert!(prompts.next().is_none());
            cx.update_window(window.into(), |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("preparing-prompt").is_none());
                assert!(window.find(canonical.key.clone()).bounds().size.height > px(0.));
            })
            .unwrap();
            assert_eq!(
                zevria_transcript::transcript::load(&path)
                    .unwrap()
                    .iter()
                    .filter(|item| item.display_message() == Some(Message::user(PROMPT)))
                    .count(),
                1
            );
        }

        controller.update(cx, |this, cx| this.new_session(cx));
        assert!(controller.read_with(cx, |this, _| this.model.root.pending_prompt().is_none()));
        until(cx, |cx| {
            controller.read_with(cx, |this, _| {
                this.phase == crate::controller::Phase::Live && this.model.transcript_path != path
            })
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("preparing-prompt").is_none());
        })
        .unwrap();
        assert!(controller.read_with(cx, |this, _| {
            this.model.root.conversation.history().is_empty()
        }));
        assert_eq!(path.exists(), !replace);
        controller.update(cx, |this, cx| this.quit(cx));
        until(cx, |_| !path.with_extension("jsonl.lock").exists());
    }
}

#[gpui_kit::test]
fn failed_submit_send_clears_preview_and_retains_the_draft(cx: &mut TestAppContext) {
    let (window, _, controller, receiver) = setup(cx);
    drop(receiver);
    controller.update(cx, |this, cx| {
        this.draft = PROMPT.into();
        this.submit(cx);
    });
    assert!(controller.read_with(cx, |this, _| {
        this.draft == PROMPT
            && this.model.root.pending_prompt().is_none()
            && matches!(this.model.root.conversation.history(), [zevria_presentation::conversation::HistoryEntry::Error(error)] if error.contains("engine stopped"))
    }));
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("preparing-prompt").is_none());
    })
    .unwrap();
}
