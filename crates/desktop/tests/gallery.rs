//! Opt-in real Metal renders. This executable must run on the macOS main thread.
#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("The desktop screenshot gallery requires macOS and Metal.");
    std::process::ExitCode::FAILURE
}
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    native::run()
}

#[cfg(target_os = "macos")]
mod native {
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, HeadlessAppContext, WindowBounds,
        WindowOptions, px, size, test::TestWindowExt as _,
    };
    use rig_core::message::{
        AssistantContent, AssistantMessage, CallId, Message, ToolCall, ToolFunction, ToolName,
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, SystemTime},
    };
    use zevria_desktop::{
        actions,
        controller::{Phase, SessionController},
        ui::{assets::AppAssets, theme, window::DesktopWindow},
    };
    use zevria_foundation::{
        FileChange, FileChangeOutput, QuestionOption, QuestionPrompt, QuestionPromptKind,
        QuestionRequest, QuestionRequestId, SessionMode, ToolCallOutcome, ToolResultDetail,
        ToolResultMetadata, TurnId,
    };
    use zevria_session_api::SessionEvent as E;
    use zevria_workflow::{PlanArtifact, PlanId, PlanVersion, PlanWorkflowState};

    const PLAN: &str = "# Gutter-led native redesign\n\n## Goal\n\nMake Zevria's long tool loops easy to scan without changing session behavior. The editor gutter carries speakers and status; everything around it stays quiet.\n\n## Design language\n\n- Warm graphite planes for reading, recessed wells for literal content.\n- Build mint and Plan blue appear only where the mode acts.\n- SF Pro for conversation, **Charter for documents**, Menlo for code.\n\n## Implementation\n\n1. Validate the palette and map every visible Kit slot.\n2. Align the transcript and composer on one content axis.\n3. Preserve element IDs and exercise all existing desktop flows.\n\n```rust\nlet palette = zevria_theme::ZEVRIA_GRAPHITE;\n```\n\n## Validation\n\nRun the native UI tests and render this gallery at both widths. Prompt-prefix regression tests must remain green.\n";
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Scene {
        Config,
        NoWorkspace,
        Starting,
        Stopping,
        Failed,
        Idle,
        Empty,
        Working,
        ThinkingLive,
        ThinkingRestored,
        ThinkingInterrupted,
        ThinkingUnsaved,
        CodeHeavy,
        Plan,
        Question,
        Confirm,
        Completion,
        Math,
        Session,
    }
    impl Scene {
        fn name(self) -> &'static str {
            match self {
                Self::Config => "config-error",
                Self::NoWorkspace => "no-workspace",
                Self::Starting => "starting",
                Self::Stopping => "stopping",
                Self::Failed => "failed",
                Self::Idle => "idle",
                Self::Empty => "empty-session",
                Self::Working => "working",
                Self::ThinkingLive => "thinking-live",
                Self::ThinkingRestored => "thinking-restored",
                Self::ThinkingInterrupted => "thinking-interrupted",
                Self::ThinkingUnsaved => "thinking-unsaved",
                Self::CodeHeavy => "code-heavy-answer",
                Self::Plan => "plan-review",
                Self::Question => "question",
                Self::Confirm => "replace-confirmation",
                Self::Completion => "completion",
                Self::Math => "math",
                Self::Session => "session",
            }
        }
    }
    pub fn run() -> anyhow::Result<()> {
        let directory = std::env::var_os("ZEVRIA_DESKTOP_GALLERY_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/desktop-gallery")
            });
        std::fs::create_dir_all(&directory)?;
        let light = zevria_theme::ZEVRIA_GRAPHITE_LIGHT;
        for width in [1240., 720.] {
            for scene in [
                Scene::Config,
                Scene::NoWorkspace,
                Scene::Starting,
                Scene::Stopping,
                Scene::Failed,
                Scene::Idle,
                Scene::Empty,
                Scene::Working,
                Scene::ThinkingLive,
                Scene::ThinkingRestored,
                Scene::ThinkingInterrupted,
                Scene::ThinkingUnsaved,
                Scene::CodeHeavy,
                Scene::Plan,
                Scene::Question,
                Scene::Confirm,
                Scene::Completion,
            ] {
                capture(
                    scene,
                    width,
                    840.,
                    zevria_theme::ZEVRIA_GRAPHITE,
                    &directory.join(format!("{}-{}.png", scene.name(), width as u32)),
                )?;
            }
            for scene in [
                Scene::Working,
                Scene::ThinkingLive,
                Scene::ThinkingRestored,
                Scene::ThinkingInterrupted,
                Scene::ThinkingUnsaved,
                Scene::CodeHeavy,
                Scene::Question,
                Scene::Plan,
            ] {
                capture(
                    scene,
                    width,
                    840.,
                    light,
                    &directory.join(format!("{}-light-{}.png", scene.name(), width as u32)),
                )?;
            }
        }
        for scene in [
            Scene::Empty,
            Scene::Working,
            Scene::Question,
            Scene::Plan,
            Scene::Completion,
        ] {
            capture(
                scene,
                1920.,
                1080.,
                zevria_theme::ZEVRIA_GRAPHITE,
                &directory.join(format!("{}-1920x1080.png", scene.name())),
            )?;
        }
        for scene in [Scene::ThinkingLive, Scene::Question, Scene::Plan] {
            for (name, tokens) in [("dark", zevria_theme::ZEVRIA_GRAPHITE), ("light", light)] {
                capture(
                    scene,
                    720.,
                    520.,
                    tokens,
                    &directory.join(format!("{}-{name}-720x520.png", scene.name())),
                )?;
            }
        }
        for width in [1240., 720.] {
            capture(
                Scene::Math,
                width,
                3200.,
                zevria_theme::ZEVRIA_GRAPHITE,
                &directory.join(format!("math-{}.png", width as u32)),
            )?;
            if std::env::var_os("ZEVRIA_DESKTOP_GALLERY_SESSION").is_some() {
                capture(
                    Scene::Session,
                    width,
                    3200.,
                    zevria_theme::ZEVRIA_GRAPHITE,
                    &directory.join(format!("session-{}.png", width as u32)),
                )?;
            }
        }
        capture(
            Scene::Math,
            1240.,
            3200.,
            light,
            &directory.join("math-light-1240.png"),
        )?;
        println!(
            "Desktop gallery: {} (logical viewports at native 2× scale)",
            directory.display()
        );
        Ok(())
    }
    fn settle(cx: &mut HeadlessAppContext, window: AnyWindowHandle) -> anyhow::Result<()> {
        for _ in 0..6 {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))?;
            cx.run_until_parked();
            cx.advance_clock(Duration::from_millis(50));
        }
        cx.update_window(window, |_, window, cx| window.render_frame(cx))?;
        Ok(())
    }
    fn capture(
        scene: Scene,
        width: f32,
        height: f32,
        tokens: zevria_theme::ZevriaTheme,
        output: &Path,
    ) -> anyhow::Result<()> {
        // File::open through the transcript projection, never a host/resume,
        // writer, provider, lease, or repair path. Load before entering GPUI.
        let restored = if scene == Scene::Session {
            let path = std::env::var_os("ZEVRIA_DESKTOP_GALLERY_SESSION").ok_or_else(|| {
                anyhow::anyhow!("session scene requires ZEVRIA_DESKTOP_GALLERY_SESSION")
            })?;
            let items = zevria_transcript::transcript::load(Path::new(&path))?;
            let replay = zevria_transcript::validate_session_replay(&items)?;
            Some((items, replay))
        } else {
            None
        };
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(AppAssets),
            gpui_kit::platform::current_headless_renderer,
        );
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let (window,controller) = cx.update(|cx| -> anyhow::Result<_> {
            gpui_kit::init(cx); theme::apply(tokens,cx); actions::install(cx); cx.set_reduce_motion(true);
            let controller = cx.new(|cx| {
                let mut c = SessionController::fixture(sender,cx);
                c.workspace = Some("/Users/engineer/codes/rust/zevria".into());
                c.sessions = [("Refactor provider caching",7200),("Add plan drafts",86400),("Restore transcript previews",345600)].into_iter().enumerate()
                    .map(|(i,(preview,seconds))| zevria_host::sessions::SessionSummary {
                        id: format!("gallery-session-{i}"), path: PathBuf::from(format!("/tmp/gallery-{i}.jsonl")),
                        preview: Some(preview.into()), modified: SystemTime::now() - Duration::from_secs(seconds),
                    }).collect();
                match scene {
                    Scene::Config => c.config_error = Some("No model provider is configured. Add a provider to config.toml and choose its model in models.jsonc.".into()),
                    Scene::NoWorkspace => c.workspace = None,
                    Scene::Starting => c.phase = Phase::Starting,
                    Scene::Stopping => c.phase = Phase::Stopping,
                    Scene::Failed => c.phase = Phase::Failed("The model connection closed before the turn could finish.".into()),
                    Scene::Idle => c.phase = Phase::Idle,
                    _ => {}
                }
                c
            });
            if let Some((items, replay)) = restored {
                controller.update(cx, |c, cx| {
                    c.model.root.restore_transcript(items);
                    c.model.root.apply_plan_snapshot(replay.plan);
                    c.model.root.apply_selected_mode(replay.instructions.persisted_mode().unwrap_or_default());
                    cx.notify();
                });
            }
            if scene == Scene::ThinkingRestored {
                controller.update(cx, |c, cx| {
                    c.model.root.conversation.restore(vec![
                        zevria_transcript::transcript::TranscriptItem::Message(Message::user("Fix the failing transcript preview test.")),
                        zevria_transcript::transcript::TranscriptItem::Message(thinking_message()),
                        zevria_transcript::transcript::TranscriptItem::Message(Message::assistant("The preview now scans until it finds the prompt. The restore round-trip test passes.")),
                    ]);
                    cx.notify();
                });
            }
            if scene == Scene::CodeHeavy {
                controller.update(cx, |c, cx| {
                    c.model.root.conversation.restore(vec![
                        zevria_transcript::transcript::TranscriptItem::Message(Message::user("Show the preview fix and its regression test.")),
                        zevria_transcript::transcript::TranscriptItem::Message(Message::assistant("## Restore the first readable prompt\n\nScan until the first user message instead of stopping at a fixed line count.\n\n```rust\nfn prompt_preview(items: &[TranscriptItem]) -> Option<&str> {\n    items.iter().find_map(|item| match item {\n        TranscriptItem::Message(Message::User { content }) => {\n            content.iter().find_map(readable_text)\n        }\n        _ => None,\n    })\n}\n```\n\n### Verify the boundary\n\n```sh\ncargo test -p zevria-transcript restore_roundtrip\ncargo test -p zevria-desktop\n```\n\n| Case | Expected |\n| --- | --- |\n| Metadata before the prompt | First readable prompt |\n| No user message | No preview |\n| Unicode prompt | Original text |")),
                    ]);
                    cx.notify();
                });
            }
            if scene == Scene::Math {
                controller.update(cx, |c, cx| {
                    c.model.root.conversation.restore(vec![
                        zevria_transcript::transcript::TranscriptItem::Message(Message::user("Prove Hölder’s inequality using Young’s inequality and normalization.")),
                        zevria_transcript::transcript::TranscriptItem::Message(Message::assistant(include_str!("fixtures/holder-proof.md"))),
                    ]);
                    cx.notify();
                });
            }
            if matches!(scene,Scene::Working | Scene::Question | Scene::Confirm) { working(&controller,cx); }
            if scene == Scene::Plan {
                controller.update(cx, |c,cx| {
                    c.model.root.conversation.restore(vec![
                        zevria_transcript::transcript::TranscriptItem::Message(Message::user("Plan the desktop redesign before changing files.")),
                        zevria_transcript::transcript::TranscriptItem::Message(Message::assistant("The plan is ready. Review the document before implementation.")),
                    ]);
                    c.fixture_event(E::PlanStateChanged { state: PlanWorkflowState::Ready { artifact: PlanArtifact {
                        version: PlanVersion { id: PlanId::new(), revision: 4 }, title: "Gutter-led native redesign of Zevria Desktop".into(),
                        markdown: PLAN.into(),source_turn_id: TurnId::new(1),
                    }}},cx);
                });
            }
            if scene == Scene::Question {
                controller.update(cx, |c,cx| c.fixture_event(E::QuestionAsked { turn_id: TurnId::new(8), request: QuestionRequest {
                    id: QuestionRequestId::new("gallery-question"), source_label: Some("Explore: desktop layout".into()), dismissible: true,
                    questions: vec![QuestionPrompt { id: "approach".into(), header: "Review approach".into(),
                        question: "How should the transcript present completed tool calls?".into(), kind: QuestionPromptKind::SingleSelect { allow_other: true },
                        options: vec![QuestionOption { label: "Compact gutter rows".into(),description: "Keep the log readable during long tool loops.".into() },
                            QuestionOption { label: "Show all details".into(),description: "Expand every call's input and output.".into() }], required: true, default: None },
                        QuestionPrompt { id: "notes".into(),header: "Additional notes".into(),question: "Anything else to keep in mind?".into(),
                            kind: QuestionPromptKind::Text { min_length: None, max_length: None }, options: vec![],required: false,default: None }],
                }},cx));
            }
            if scene == Scene::Completion { controller.update(cx, |c,cx| c.edit_draft("/".into(),cx)); }
            let (window,_view) = gpui_kit::open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds { origin: Default::default(),size: size(px(width),px(height)) })),
                focus: false,show: false,..gpui_kit::component::TitleBar::window_options()
            },cx,|window,cx| cx.new(|cx| DesktopWindow::new(controller.clone(),window,cx)))?;
            Ok((window,controller))
        })?;
        settle(&mut cx, window)?;
        if matches!(
            scene,
            Scene::ThinkingLive | Scene::ThinkingInterrupted | Scene::ThinkingUnsaved
        ) {
            cx.update(|cx| thinking_scene(scene, &controller, cx));
            settle(&mut cx, window)?;
        }
        if scene == Scene::Working {
            let key = controller.read_with(&cx, |c, _| {
                let conversation = &c.model.root.conversation;
                conversation
                    .history()
                    .iter()
                    .find_map(|entry| match entry {
                        zevria_presentation::conversation::HistoryEntry::Conversation(e) => e
                            .blocks
                            .iter()
                            .find(|b| {
                                b.native_tool()
                                    .is_some_and(|(call, _)| call.function.name.as_str() == "edit")
                            })
                            .map(|b| format!("{:?}-{}", conversation.epoch(), b.id.0)),
                        _ => None,
                    })
                    .expect("fixture contains edit")
            });
            cx.update_window(window, |_, window, cx| {
                window.within(key).click("expand", cx)
            })?;
            settle(&mut cx, window)?;
        }
        if scene == Scene::Confirm {
            // A native action opens the authoritative inline confirmation layer.
            cx.update_window(window, |_, window, cx| {
                window.dispatch_action(Box::new(actions::NewSession), cx)
            })?;
            settle(&mut cx, window)?;
        }
        cx.update_window(window, |_, window, cx| {
            window.blur(cx);
            window.render_frame(cx);
        })?;
        settle(&mut cx, window)?;
        let screenshot: image::RgbaImage = cx.capture_screenshot(window)?;
        screenshot.save(output)?;
        println!("{}", output.display());
        Ok(())
    }
    fn thinking_message() -> Message {
        Message::Assistant(AssistantMessage::new(vec![AssistantContent::Reasoning(
            rig_core::message::Reasoning::new(
                "**Preparing notes**\n\nThe restored preview only scans sixteen lines, so metadata can hide the first prompt.\n\n### Trace the restore boundary\n\nI'll trace the transcript reader and use the first readable user message as the boundary. The new test should include a long metadata prefix and a Unicode prompt, without changing the persisted transcript.\n\n```rust\nlet preview = items.iter().find_map(readable_prompt);\n```",
            ),
        )]))
    }
    fn thinking_scene(
        scene: Scene,
        controller: &Entity<SessionController>,
        cx: &mut gpui_kit::App,
    ) {
        controller.update(cx, |c, cx| {
            let turn_id = TurnId::new(31);
            c.fixture_event(E::TurnStarted { turn_id, mode: SessionMode::Build, message: Message::user("Fix the failing transcript preview test.") }, cx);
            c.fixture_event(E::ModelCallStarted { turn_id, call: 1 }, cx);
            if scene == Scene::ThinkingInterrupted {
                let mut attempt = zevria_content::ResponseAttempt::new(zevria_foundation::ModelProfileRef::new("fixture", "fixture"));
                attempt.presentation.push(zevria_content::AssistantPresentationPart {
                    source: zevria_content::AssistantSourceAddress { output_index: 0, part: zevria_content::AssistantPartIdentity::Summary(0), item_id: None },
                    content: zevria_content::AssistantPresentationContent::Reasoning { text: "The restored preview only scans sixteen lines. I was tracing the reader when the response stopped.".into() },
                });
                attempt.close(zevria_content::ResponseAttemptOutcome::Interrupted);
                c.fixture_event(E::ResponseAttemptUpdated { turn_id, attempt }, cx);
                c.fixture_event(E::TurnCancelled { turn_id }, cx);
            } else {
                c.fixture_event(E::AssistantStreamUpdated { turn_id, snapshot: zevria_content::AssistantStreamSnapshot { message: Some(thinking_message()), attempt: None } }, cx);
                if scene == Scene::ThinkingUnsaved { c.fixture_event(E::TurnCancelled { turn_id }, cx); }
            }
        });
    }
    fn working(controller: &Entity<SessionController>, cx: &mut gpui_kit::App) {
        controller.update(cx, |c,cx| {
            let turn_id = TurnId::new(8);
            c.fixture_event(E::TurnStarted { turn_id,mode: SessionMode::Build,message: Message::user("Fix the failing transcript preview test.") },cx);
            c.fixture_event(E::ModelCallStarted { turn_id, call: 1 }, cx);
            c.fixture_event(E::Intermediate { turn_id,message: Message::assistant("I'll reproduce the failure, then trace the restored prompt."),attempt_id: None },cx);
            for (id,name,args) in [
                ("command-ok","command",serde_json::json!({"command":"rtk cargo test -p zevria-transcript"})),
                ("command-fail","command",serde_json::json!({"command":"rtk proxy cargo test restore_roundtrip -- --nocapture 2>&1 | tail -20"})),
                ("edit","edit",serde_json::json!({"file_path":"crates/transcript/src/transcript.rs"})),
                ("command-live","command",serde_json::json!({"command":"rtk cargo test -p zevria-transcript restore_roundtrip"})),
            ] {
                let call = ToolCall::new(CallId::from_wire(id),ToolFunction::new(ToolName::new(name).unwrap(),args));
                c.fixture_event(E::Intermediate { turn_id,message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(call)])),attempt_id: None },cx);
                if id != "command-live" {
                    c.fixture_event(E::ToolResults { turn_id,message: Message::tool_result(CallId::from_wire(id),ToolName::new(name).unwrap(),
                        if id == "command-fail" { "assertion failed: restored preview is empty" } else if id == "edit" { "Updated transcript preview scanning." } else { "All 18 tests passed." }),
                        metadata: vec![ToolResultMetadata { id: id.into(),call_id: None,tool_name: name.into(),
                            outcome: if id == "command-fail" { ToolCallOutcome::Error } else { ToolCallOutcome::Success },diagnostic: None,
                            detail: if id == "edit" { Some(ToolResultDetail::FileChanges(vec![FileChangeOutput { path: "crates/transcript/src/transcript.rs".into(),
                                change: FileChange::Update { unified_diff: "--- a/crates/transcript/src/transcript.rs\n+++ b/crates/transcript/src/transcript.rs\n@@ -1 +1,3 @@\n-let preview = lines.take(16);\n+let preview = lines\n+    .take(PREVIEW_SCAN_LINES)\n+    .find_map(prompt_preview);\n".into(),move_path: None } }])) } else { None },
                        }] },cx);
                }
            }
            let observed = c.model.root.session.observed_at();
            c.model.observe_clock(observed + Duration::from_secs(4));
            cx.notify();
        });
    }
}
