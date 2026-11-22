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
        ToolResultContent, UserContent,
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, SystemTime},
    };
    use zevria_content::{ImageReadSummary, PromptImage};
    use zevria_desktop::{
        actions,
        controller::{Phase, SessionController},
        ui::{assets::AppAssets, theme, window::DesktopWindow},
    };
    use zevria_foundation::{
        CommandJobId, CommandJobMode, CommandJobNotice, CommandJobNoticeKind, CommandJobSnapshot,
        CommandJobStatus, CommandJobView, FileChange, FileChangeOutput, ModelProfileRef, ModelRole,
        QuestionOption, QuestionPrompt, QuestionPromptKind, QuestionRequest, QuestionRequestId,
        READ_IMAGE_TOOL_NAME, SessionMode, SubtaskDescriptor, SubtaskId, SubtaskKind,
        SubtaskStatus, ToolCallOutcome, ToolResultDetail, ToolResultMetadata, TurnId,
    };
    use zevria_model::{ContextTokenSnapshot, ContextTokenSource, TokenUsage};
    use zevria_session_api::{
        CommandControlAction, CommandControlResult, ControlCommand, SessionCommand,
        SessionEvent as E,
    };
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
        Jobs,
        Subtasks,
        ThinkingLive,
        ThinkingRestored,
        ThinkingInterrupted,
        ThinkingUnsaved,
        WebSearch,
        WebSearchLive,
        ImageRead,
        CodeHeavy,
        TranscriptFolds,
        Plan,
        Question,
        QuestionDetail,
        QuestionNote,
        Confirm,
        Completion,
        Models,
        ModelConversion,
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
                Self::Jobs => "jobs",
                Self::Subtasks => "subtasks",
                Self::ThinkingLive => "thinking-live",
                Self::ThinkingRestored => "thinking-restored",
                Self::ThinkingInterrupted => "thinking-interrupted",
                Self::ThinkingUnsaved => "thinking-unsaved",
                Self::WebSearch => "web-search",
                Self::WebSearchLive => "web-search-live",
                Self::ImageRead => "image-read",
                Self::CodeHeavy => "code-heavy-answer",
                Self::TranscriptFolds => "transcript-folds",
                Self::Plan => "plan-review",
                Self::Question => "question",
                Self::QuestionDetail => "question-detail",
                Self::QuestionNote => "question-note",
                Self::Confirm => "replace-confirmation",
                Self::Completion => "completion",
                Self::Models => "model-picker",
                Self::ModelConversion => "model-conversion",
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
        if std::env::var_os("ZEVRIA_DESKTOP_GALLERY_MODELS_ONLY").is_some() {
            let configured = zevria_theme::generate_theme("#1E1E2E".parse()?)?
                .palette
                .tokens();
            for scene in [Scene::Models, Scene::ModelConversion] {
                for (width, height) in [(1240., 840.), (720., 520.), (1920., 1080.)] {
                    for (name, tokens) in [
                        ("dark", zevria_theme::ZEVRIA_GRAPHITE),
                        ("light", light),
                        ("configured", configured),
                    ] {
                        capture(
                            scene,
                            width,
                            height,
                            tokens,
                            &directory.join(format!(
                                "{}-{name}-{}x{}.png",
                                scene.name(),
                                width as u32,
                                height as u32
                            )),
                        )?;
                    }
                }
            }
            return Ok(());
        }
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
                Scene::Jobs,
                Scene::Subtasks,
                Scene::WebSearch,
                Scene::WebSearchLive,
                Scene::ImageRead,
                Scene::ThinkingLive,
                Scene::ThinkingRestored,
                Scene::ThinkingInterrupted,
                Scene::ThinkingUnsaved,
                Scene::CodeHeavy,
                Scene::TranscriptFolds,
                Scene::Plan,
                Scene::Question,
                Scene::QuestionDetail,
                Scene::QuestionNote,
                Scene::Confirm,
                Scene::Completion,
                Scene::Models,
                Scene::ModelConversion,
            ] {
                capture(
                    scene,
                    width,
                    match scene {
                        Scene::TranscriptFolds => 1600.,
                        Scene::ImageRead => 1080.,
                        _ => 840.,
                    },
                    zevria_theme::ZEVRIA_GRAPHITE,
                    &directory.join(format!("{}-{}.png", scene.name(), width as u32)),
                )?;
            }
            for scene in [
                Scene::Working,
                Scene::Jobs,
                Scene::Subtasks,
                Scene::ImageRead,
                Scene::ThinkingLive,
                Scene::ThinkingRestored,
                Scene::ThinkingInterrupted,
                Scene::ThinkingUnsaved,
                Scene::CodeHeavy,
                Scene::Question,
                Scene::QuestionDetail,
                Scene::Plan,
                Scene::Models,
                Scene::ModelConversion,
            ] {
                capture(
                    scene,
                    width,
                    if scene == Scene::ImageRead {
                        1080.
                    } else {
                        840.
                    },
                    light,
                    &directory.join(format!("{}-light-{}.png", scene.name(), width as u32)),
                )?;
            }
        }
        for scene in [Scene::WebSearch, Scene::WebSearchLive] {
            capture(
                scene,
                1240.,
                840.,
                light,
                &directory.join(format!("{}-light-1240.png", scene.name())),
            )?;
        }
        capture(
            Scene::TranscriptFolds,
            1240.,
            1600.,
            light,
            &directory.join("transcript-folds-light-1240.png"),
        )?;
        for scene in [
            Scene::Empty,
            Scene::Working,
            Scene::Subtasks,
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
        for scene in [
            Scene::ThinkingLive,
            Scene::WebSearchLive,
            Scene::Question,
            Scene::QuestionNote,
            Scene::Plan,
            Scene::Models,
            Scene::ModelConversion,
        ] {
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
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
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
            if matches!(scene, Scene::Models | Scene::ModelConversion) {
                model_scene(&controller, scene == Scene::ModelConversion, cx);
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
            if scene == Scene::ImageRead {
                controller.update(cx, |c, cx| {
                    c.model.root.conversation.restore(image_read_items());
                    cx.notify();
                });
            }
            if scene == Scene::TranscriptFolds {
                controller.update(cx, |c, cx| {
                    c.model.root.conversation.restore(fold_items());
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
            if scene == Scene::Jobs { jobs_scene(&controller, &mut receiver, cx)?; }
            if scene == Scene::Subtasks { subtasks_scene(&controller, cx); }
            if matches!(scene, Scene::WebSearch | Scene::WebSearchLive) {
                web_scene(&controller, scene == Scene::WebSearchLive, cx)?;
            }
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
                    allow_choice_notes: false,
                    id: QuestionRequestId::new("gallery-question"), source_label: Some("Explore: desktop layout".into()), dismissible: true,
                    questions: vec![QuestionPrompt { id: "approach".into(), header: "Review approach".into(),
                        question: "How should the transcript present completed tool calls?".into(), kind: QuestionPromptKind::SingleSelect { allow_other: true },
                        options: vec![QuestionOption { label: "Compact gutter rows".into(),description: "Keep the log readable during long tool loops.".into(), detail: None },
                            QuestionOption { label: "Show all details".into(),description: "Expand every call's input and output.".into(), detail: None }], required: true, default: None },
                        QuestionPrompt { id: "notes".into(),header: "Additional notes".into(),question: "Anything else to keep in mind?".into(),
                            kind: QuestionPromptKind::Text { min_length: None, max_length: None }, options: vec![],required: false,default: None }],
                }},cx));
            }
            if scene == Scene::QuestionDetail {
                controller.update(cx, |c, cx| {
                    let turn_id = TurnId::new(8);
                    c.fixture_event(E::TurnStarted { turn_id, mode: SessionMode::Plan,
                        message: Message::user("Plan validation for the new question preview.") }, cx);
                    c.fixture_event(E::QuestionAsked { turn_id, request: QuestionRequest {
                        id: QuestionRequestId::new("gallery-question-detail"), source_label: None,
                        dismissible: true, allow_choice_notes: true,
                        questions: vec![QuestionPrompt {
                            id: "targets".into(), header: "Validation targets".into(),
                            question: "Which crates should get dedicated regression tests?".into(),
                            kind: QuestionPromptKind::MultiSelect { min_selections: Some(1), max_selections: None, allow_other: true },
                            required: true, default: Some(vec!["Core".to_string()].into()),
                            options: vec![
                                QuestionOption { label: "Core".into(), description: "Check the answer wire format.".into(),
                                    detail: Some("Pin the response shape before changing either frontend. A multi-select answer should serialize as an array, even when the user chooses just one label.\n\n```rust\nlet answer = QuestionAnswerValue::Strings(\n    vec![\"Core\".into(), \"TUI\".into()],\n);\nassert_eq!(\n    serde_json::to_value(answer)?,\n    json!([\"Core\", \"TUI\"]),\n);\n```\n\nAdd a second case for mixed batches: text still returns a string.".into()) },
                                QuestionOption { label: "TUI".into(), description: "Exercise paging and keyboard focus.".into(),
                                    detail: Some("Render the wizard at wide and compact widths. Paging should leave the current selection unchanged.\n\n```rust\nlet backend = TestBackend::new(80, 24);\nlet mut terminal = Terminal::new(backend)?;\n```".into()) },
                                QuestionOption { label: "Desktop".into(), description: "Verify the dock in both layouts.".into(), detail: None },
                            ],
                        }],
                    } }, cx);
                });
            }
            if scene == Scene::QuestionNote {
                controller.update(cx, |c, cx| {
                    let turn_id = TurnId::new(8);
                    c.fixture_event(E::TurnStarted { turn_id, mode: SessionMode::Plan, message: Message::user("Plan the transcript redesign.") }, cx);
                    c.fixture_event(E::QuestionAsked { turn_id, request: QuestionRequest {
                        id: QuestionRequestId::new("gallery-question-note"), source_label: None,
                        dismissible: true, allow_choice_notes: true,
                        questions: vec![QuestionPrompt {
                            id: "scope".into(), header: "Scope".into(), question: "How broad should this change be?".into(),
                            kind: QuestionPromptKind::SingleSelect { allow_other: true }, required: true,
                            default: Some("Focused (Recommended)".to_string().into()),
                            options: vec![QuestionOption { label: "Focused (Recommended)".into(), description: "Change only the requested workflow.".into(), detail: None },
                                QuestionOption { label: "Broad".into(), description: "Include adjacent cleanup.".into(), detail: None }],
                        }],
                    } }, cx);
                });
            }
            if scene == Scene::Completion { controller.update(cx, |c,cx| c.edit_draft("/".into(),cx)); }
            let (window,_view) = gpui_kit::open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds { origin: Default::default(),size: size(px(width),px(height)) })),
                focus: false,show: false,..gpui_kit::component::TitleBar::window_options()
            },cx,|window,cx| cx.new(|cx| DesktopWindow::new(controller.clone(),window,cx)))?;
            Ok((window,controller))
        })?;
        settle(&mut cx, window)?;
        if scene == Scene::ImageRead {
            cx.update_window(window, |_, window, cx| {
                window.dispatch_action(Box::new(actions::JumpToBeginning), cx);
            })?;
            settle(&mut cx, window)?;
            // Close the first read through its normal disclosure; leave the other cases open.
            let block = cx.update_window(window, |_, window, cx| {
                // The prompt also has a disclosure; scope the click to the first tool row.
                let output = window
                    .find_all("copy-output")
                    .into_iter()
                    .next()
                    .expect("the first image read has an output action");
                let block = output
                    .path()
                    .iter()
                    .rev()
                    .find(|id| matches!(id, gpui_kit::ElementId::View(_)))
                    .expect("copy output belongs to a block view")
                    .clone();
                window.within(block.clone()).click("block-disclosure", cx);
                block
            })?;
            settle(&mut cx, window)?;
            cx.update_window(window, |_, window, _| {
                assert_eq!(
                    window.within(block).find("block-disclosure").expanded(),
                    Some(false)
                );
            })?;
        }
        if scene == Scene::Jobs && width == 1240. {
            // Show a docked inspector at 1240 px and an overlay at 720 px.
            cx.update_window(window, |_, window, cx| {
                window.dispatch_action(Box::new(actions::ToggleSidebar), cx);
            })?;
            settle(&mut cx, window)?;
        }
        if matches!(
            scene,
            Scene::ThinkingLive | Scene::ThinkingInterrupted | Scene::ThinkingUnsaved
        ) {
            cx.update(|cx| thinking_scene(scene, &controller, cx));
            settle(&mut cx, window)?;
        }
        if scene == Scene::QuestionNote {
            cx.update_window(window, |_, window, cx| {
                window.click(("question-note", 0usize), cx);
                window.render_frame(cx);
                window.input("Only for the first release", cx);
            })?;
            settle(&mut cx, window)?;
        }
        if scene == Scene::TranscriptFolds {
            cx.update_window(window, |_, window, cx| {
                window.dispatch_action(Box::new(actions::FoldEarlierContent), cx);
                window.dispatch_action(Box::new(actions::JumpToBeginning), cx);
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
    fn image_read_items() -> Vec<zevria_transcript::transcript::TranscriptItem> {
        use zevria_transcript::transcript::TranscriptItem as T;

        let capture = image_read_raster(1440, 900, zevria_theme::ZEVRIA_GRAPHITE);
        let resized = image_read_raster(2048, 1536, zevria_theme::ZEVRIA_GRAPHITE_LIGHT);
        let mut calls = Vec::new();
        let mut results = Vec::new();
        let mut metadata = Vec::new();
        for (file, image, original) in [
            ("overview.png", Some(&capture), None),
            ("capture.png", Some(&capture), None),
            (
                "big.png",
                Some(&resized),
                Some(((4000, 3000), 6 * 1024 * 1024)),
            ),
            ("missing.png", None, None),
        ] {
            let file_path = format!("docs/shots/{file}");
            let call = ToolCall::new(
                CallId::from_wire(file),
                ToolFunction::new(
                    ToolName::new(READ_IMAGE_TOOL_NAME).unwrap(),
                    serde_json::json!({"file_path": file_path}),
                ),
            );
            let path = format!("/Users/engineer/codes/rust/zevria/{file_path}");
            let (content, outcome, diagnostic) = if let Some(image) = image {
                let summary = ImageReadSummary {
                    path,
                    mime: image.mime_type().into(),
                    dimensions: image.dimensions(),
                    bytes: image.encoded_len(),
                    original,
                };
                (
                    vec![
                        ToolResultContent::text(summary.to_string()),
                        image.to_tool_result_content(),
                    ],
                    ToolCallOutcome::Success,
                    None,
                )
            } else {
                let diagnostic =
                    format!("failed to resolve {path}: No such file or directory (os error 2)");
                (
                    vec![ToolResultContent::text(format!(
                        "status: error\nerror: {diagnostic}"
                    ))],
                    ToolCallOutcome::Error,
                    Some(diagnostic),
                )
            };
            let mut result = call.result(content);
            result.is_error = !outcome.is_success();
            results.push(UserContent::ToolResult(result));
            calls.push(AssistantContent::ToolCall(call));
            metadata.push(ToolResultMetadata {
                id: file.into(),
                call_id: None,
                tool_name: READ_IMAGE_TOOL_NAME.into(),
                outcome,
                diagnostic,
                detail: None,
            });
        }
        vec![
            T::Message(Message::user("Inspect the screenshots in docs/shots.")),
            T::Message(Message::Assistant(AssistantMessage::new(calls))),
            T::ToolResults {
                message: Message::User { content: results },
                metadata,
                skill_applications: vec![],
            },
        ]
    }

    // A deterministic miniature editor screenshot with a gutter, a header,
    // and syntax bands. Use different themes to make the two rasters distinct.
    fn image_read_raster(width: u32, height: u32, t: zevria_theme::ZevriaTheme) -> PromptImage {
        let pixels = image::RgbaImage::from_fn(width, height, |x, y| {
            let color = match (x * 100 / width, y * 100 / height) {
                (_, 0..=10) | (0..=19, _) => t.surfaces.panel,
                (20, _) | (_, 11) => t.surfaces.border,
                (28..=65, 24..=27) => t.syntax.keyword,
                (34..=91, 34..=37) => t.syntax.function,
                (34..=81, 44..=47) => t.syntax.string,
                (34..=57, 54..=57) => t.syntax.constant,
                (28..=73, 64..=67) => t.syntax.keyword,
                (_, 91..=99) => t.workflow.build,
                _ => t.surfaces.canvas,
            };
            let (r, g, b) = zevria_theme::rgb_channels(color);
            image::Rgba([r, g, b, 255])
        });
        PromptImage::from_rgba(width as usize, height as usize, pixels.as_raw()).unwrap()
    }

    fn fold_items() -> Vec<zevria_transcript::transcript::TranscriptItem> {
        use zevria_transcript::transcript::TranscriptItem as T;
        let thought = |text: &str| {
            T::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(rig_core::message::Reasoning::new(text)),
            ])))
        };
        let tool = |id: &str, name: &str, args| {
            T::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::ToolCall(ToolCall::new(
                    CallId::from_wire(id),
                    ToolFunction::new(ToolName::new(name).unwrap(), args),
                )),
            ])))
        };
        vec![
            T::Message(Message::user("Fix the restored transcript preview.\n\nThe session picker should show the first real prompt, even when metadata comes first.")),
            T::Message(Message::assistant("I'll reproduce the failure and trace the reader.")),
            thought("Read past the metadata to find the first prompt."),
            tool("preview-test", "command", serde_json::json!({"command":"rtk cargo test --release -p zevria-transcript"})),
            tool("preview-edit", "edit", serde_json::json!({"file_path":"crates/transcript/src/transcript.rs"})),
            T::ToolResults { message: Message::tool_result(CallId::from_wire("preview-edit"), ToolName::new("edit").unwrap(), "Updated preview scanning."), metadata: vec![ToolResultMetadata {
                id: "preview-edit".into(), call_id: None, tool_name: "edit".into(), outcome: ToolCallOutcome::Success, diagnostic: None,
                detail: Some(ToolResultDetail::FileChanges(vec![FileChangeOutput { path: "crates/transcript/src/transcript.rs".into(), change: FileChange::Update {
                    unified_diff: "--- a/transcript.rs\n+++ b/transcript.rs\n@@ -1 +1,2 @@\n-lines.take(16)\n+lines\n+    .find_map(prompt_preview)\n".into(), move_path: None,
                }}])),
            }], skill_applications: vec![] },
            T::Message(Message::assistant("The preview now finds the first readable prompt, including sessions with a long metadata prefix. The restore test passes.")),
            T::Message(Message::user("Plan the regression coverage before adding it.")),
            T::Message(Message::assistant("I'll cover the restore boundary without changing the transcript format.")),
            thought("Include metadata, Unicode and an empty conversation."),
            T::Plan(zevria_workflow::PlanRecord::Ready { artifact: PlanArtifact {
                version: PlanVersion { id: PlanId::new(), revision: 1 }, title: "Cover restored prompt previews".into(), markdown: "## Regression coverage\n\nTest a long metadata prefix, a Unicode prompt and an empty conversation. Keep the persisted format unchanged.".into(), source_turn_id: TurnId::new(2),
            }}),
            tool("coverage-read", "command", serde_json::json!({"command":"rtk rg -n restore_roundtrip crates/transcript"})),
            T::Message(Message::assistant("The plan keeps the tests at the restore boundary. No persisted format change is needed.")),
            T::Message(Message::user("Run the workspace checks.")),
            T::Message(Message::assistant("I'll check the workspace and the prompt-prefix regression tests.")),
            thought("Check the Desktop leaf boundary too."),
            tool("workspace-check", "command", serde_json::json!({"command":"rtk cargo check --release --locked --workspace --all-targets"})),
            T::Message(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(rig_core::message::Reasoning::new("Checked the log and cache boundaries.\n\nThe restored cases preserve the full prompt.")),
                AssistantContent::Reasoning(rig_core::message::Reasoning::new("Confirmed the targeted tests pass.\n\nThe workspace check ran separately.")),
                AssistantContent::text("The targeted restore checks passed. The workspace check was stopped before it finished.\n\nThe prompt prefix is unchanged."),
            ]))),
        ]
    }
    fn model_scene(
        controller: &Entity<SessionController>,
        conversion: bool,
        cx: &mut gpui_kit::App,
    ) {
        use zevria_foundation::{ModelContextPolicy, ReasoningLevel as Level};
        use zevria_model::models::{
            ModelCandidate, ModelManagementResult, ModelSelection, ModelSelectionPreview,
            ModelSelectionScope,
        };
        controller.update(cx, |c, cx| {
            let context = ModelContextPolicy { profile: ModelProfileRef::new("configured-provider", "model-with-a-long-configured-profile-name"), context_window_tokens: 128000, input_token_limit: 100000, retained_user_tokens: 10000, image_accounting: Default::default() };
            let mut other = context.clone();
            other.profile = ModelProfileRef::new("other-provider", "different-model");
            for role in ModelRole::ALL { c.model.root.install_model(role, &context, None); c.model.root.install_reasoning(role, Level::Medium); }
            c.model.root.conversation.restore(vec![zevria_transcript::transcript::TranscriptItem::Message(Message::user("Keep the saved thinking and adjust effort for this session.")), zevria_transcript::transcript::TranscriptItem::Message(thinking_message())]);
            c.draft = "Keep this draft while choosing a model.".into();
            c.open_model_picker(ModelSelectionScope::SessionOnly, cx);
            let request_id = c.model.models.request_id.clone();
            c.fixture_event(E::ModelsResult { request_id: request_id.clone(), result: ModelManagementResult::Catalog { mode: SessionMode::Build, scope: ModelSelectionScope::SessionOnly, current: ModelSelection::new(context.profile.clone(), Level::Medium), profiles: vec![ModelCandidate { context: context.clone(), reasoning_levels: vec![Level::Low, Level::Medium, Level::High] }, ModelCandidate { context: other.clone(), reasoning_levels: vec![Level::Medium, Level::High] }], revision: "gallery-revision".into() } }, cx);
            c.select_model(usize::from(conversion), cx);
            if conversion {
                c.apply_model_selection(cx);
                c.fixture_event(E::ModelsResult { request_id: request_id.clone(), result: ModelManagementResult::ConfirmationRequired(ModelSelectionPreview { request_id, session_generation: "gallery-runtime".into(), generation: 1, mode: SessionMode::Build, scope: ModelSelectionScope::SessionOnly, source: ModelSelection::new(context.profile, Level::Medium), target: ModelSelection::new(other.profile, Level::Medium), revision: "gallery-revision".into(), reason: "The effective checkpoint contains source-model-only opaque context. The destination cannot replay it without a portable source-model summary.".into() }) }, cx);
            }
            cx.notify();
        });
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
    fn web_scene(
        controller: &Entity<SessionController>,
        live: bool,
        cx: &mut gpui_kit::App,
    ) -> anyhow::Result<()> {
        use serde_json::json;
        use zevria_content::{
            AssistantStreamSnapshot, ResponseAttempt, response::responses_native_display,
        };
        use zevria_transcript::transcript::TranscriptItem;
        let prompt =
            Message::user("How should I make a page address clickable in a GPUI Kit transcript?");
        let profile = ModelProfileRef::new("fixture", "fixture");
        let mut items = vec![
            json!({"type":"reasoning","id":"web_reasoning","summary":[{"type":"summary_text","text":"Check the link component and the keyboard alternative before recommending a pattern."}]}),
            json!({"type":"web_search_call","id":"web_search_a","status":"completed","action":{"type":"search","queries":["gpui kit text view links","gpui kit open_url button"]}}),
        ];
        if live {
            items.push(json!({"type":"web_search_call","id":"web_search_b","status":"searching","action":{"type":"search"}}));
            let (presentation, hosted_activity) = responses_native_display(&items);
            let mut attempt = ResponseAttempt::new(profile);
            attempt.presentation = presentation;
            attempt.hosted_activity = hosted_activity;
            attempt.touch();
            controller.update(cx, |c, cx| {
                let turn_id = TurnId::new(32);
                c.fixture_event(
                    E::TurnStarted {
                        turn_id,
                        mode: SessionMode::Build,
                        message: prompt,
                    },
                    cx,
                );
                c.fixture_event(E::ModelCallStarted { turn_id, call: 1 }, cx);
                c.fixture_event(
                    E::AssistantStreamUpdated {
                        turn_id,
                        snapshot: AssistantStreamSnapshot {
                            message: None,
                            attempt: Some(attempt),
                        },
                    },
                    cx,
                );
            });
        } else {
            let link = "https://docs.rs/gpui-kit/latest/gpui_kit/component/link/struct.Link.html";
            let button =
                "https://docs.rs/gpui-kit/latest/gpui_kit/component/button/struct.Button.html";
            let project = "https://crates.io/crates/gpui-kit";
            let first = "Use a `Link` for the readable address and a labeled `Button` for the keyboard action.";
            let text = format!(
                "{first}\n\nGive both the same checked destination. Keep the host visible, show the full address on hover, and leave unsupported schemes as text."
            );
            let citation = |title: &str, url: &str, end: usize| json!({"type":"url_citation","title":title,"url":url,"start_index":0,"end_index":end});
            items.extend([
                json!({"type":"web_search_call","id":"web_search_b","status":"completed","action":{"type":"search","query":"gpui kit keyboard accessible link button"}}),
                json!({"type":"web_search_call","id":"web_open","status":"completed","action":{"type":"open_page","url":link}}),
                json!({"type":"web_search_call","id":"web_find","status":"completed","action":{"type":"find_in_page","url":button,"pattern":"accessibility_label"}}),
                json!({"type":"message","id":"web_answer","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[citation("Link API",link,first.len()),citation("Button API",button,first.len()),citation("GPUI Kit",project,text.len())]}]}),
            ]);
            let replay = zevria_model::ProviderReplay::openai_responses(profile, items);
            controller.update(cx, |c, cx| {
                c.model.root.restore_transcript(vec![
                    TranscriptItem::Message(prompt),
                    TranscriptItem::provider_message(replay)?,
                ]);
                cx.notify();
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Ok(())
    }

    fn jobs_scene(
        controller: &Entity<SessionController>,
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
        cx: &mut gpui_kit::App,
    ) -> anyhow::Result<()> {
        controller.update(cx, |c, cx| {
            let turn_id = TurnId::new(8);
            c.fixture_event(E::TurnStarted { turn_id, mode: SessionMode::Build,
                message: Message::user("Run the release checks and watch the trace while the tests finish.") }, cx);
            c.fixture_event(E::ModelCallStarted { turn_id, call: 1 }, cx);
            c.fixture_event(E::Intermediate { turn_id,
                message: Message::assistant("The UI bundle built successfully. The release tests and trace are still running."), attempt_id: None }, cx);
            let now = SystemTime::now();
            let mut snapshots = Vec::new();
            for (key, command, age, mode, finished) in [
                ("517cd1", "npm run build", 57, CommandJobMode::Background, true),
                ("5b9ef6", "tail -f /tmp/release-trace.log", 44, CommandJobMode::Background, false),
                ("db03c8", "rtk cargo test --release -p zevria-presentation", 42, CommandJobMode::Foreground, false),
            ] {
                let started_at = now - Duration::from_secs(age);
                let status = if finished { CommandJobStatus::Exited { code: Some(0), success: true, signal: None } } else { CommandJobStatus::Running };
                let call_id = format!("call-{key}");
                let snapshot = CommandJobSnapshot { id: CommandJobId::new(format!("job-{key}")),
                    call_id: Some(call_id.clone()), command: command.into(), mode, status, started_at,
                    finished_at: finished.then(|| started_at + Duration::from_millis(8300)),
                    stdout_bytes: if key == "5b9ef6" { 12_850 } else { 567 }, stderr_bytes: 82,
                };
                let call = ToolCall::new(CallId::from_wire(&call_id), ToolFunction::new(ToolName::new("command").unwrap(),
                    serde_json::json!({"command": command, "background": mode == CommandJobMode::Background})));
                c.fixture_event(E::Intermediate { turn_id, message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(call)])), attempt_id: None }, cx);
                c.fixture_event(E::CommandJobUpdated { job: snapshot.clone() }, cx);
                snapshots.push(snapshot);
            }
            c.fixture_event(E::CommandJobsReported { turn_id, notice: CommandJobNotice::new(CommandJobNoticeKind::Update,
                vec![snapshots[0].clone()], snapshots[1..].to_vec(), "The UI build finished successfully.".into()) }, cx);
            c.toggle_jobs(cx);
            for _ in 0..2 {
                let SessionCommand::Control(ControlCommand::Command(request)) = receiver.try_recv()? else {
                    anyhow::bail!("expected a job inspector request");
                };
                let CommandControlAction::Inspect { id } = request.action else {
                    anyhow::bail!("expected to inspect jobs");
                };
                let views = snapshots.iter().filter(|job| id.as_ref().is_none_or(|id| id == &job.id)).map(|snapshot| CommandJobView {
                    snapshot: snapshot.clone(),
                    stdout_tail: id.as_ref().map_or_else(String::new, |_| include_str!("fixtures/jobs-stdout.txt").into()),
                    stderr_tail: id.as_ref().map_or_else(String::new, |_| include_str!("fixtures/jobs-stderr.txt").into()),
                }).collect();
                c.fixture_event(E::CommandControlResult { request_id: request.request_id, result: CommandControlResult::Inspected(views) }, cx);
            }
            Ok(())
        })
    }

    fn subtasks_scene(controller: &Entity<SessionController>, cx: &mut gpui_kit::App) {
        controller.update(cx, |c, cx| {
            let turn_id = TurnId::new(9);
            c.fixture_event(
                E::TurnStarted {
                    turn_id,
                    mode: SessionMode::Build,
                    message: Message::user("Fix the failing transcript preview test."),
                },
                cx,
            );
            for (id, title, kind, status, workspace) in [
                (
                    "preview",
                    "Trace the session preview",
                    SubtaskKind::Explore,
                    SubtaskStatus::Completed,
                    "crates/transcript",
                ),
                (
                    "restore",
                    "Check the restore round trip",
                    SubtaskKind::Explore,
                    SubtaskStatus::Running,
                    "crates/host",
                ),
                (
                    "fixture",
                    "Update the transcript fixtures",
                    SubtaskKind::Build,
                    SubtaskStatus::Running,
                    "crates/desktop/tests",
                ),
            ] {
                c.fixture_event(
                    E::SubtaskLaunched {
                        turn_id,
                        call_id: format!("call-{id}"),
                        entry_index: 0,
                        descriptor: SubtaskDescriptor {
                            id: SubtaskId::new(id),
                            parent_session_id: "gallery".into(),
                            title: title.into(),
                            kind,
                            workspace: Some(workspace.into()),
                            status: SubtaskStatus::Starting,
                        },
                    },
                    cx,
                );
                c.fixture_event(
                    E::SubtaskStatus {
                        turn_id,
                        id: SubtaskId::new(id),
                        status,
                    },
                    cx,
                );
            }
            c.toggle_subtasks(cx);
        });
    }

    fn working(controller: &Entity<SessionController>, cx: &mut gpui_kit::App) {
        controller.update(cx, |c,cx| {
            let turn_id = TurnId::new(8);
            c.fixture_event(E::TurnStarted { turn_id,mode: SessionMode::Build,message: Message::user("Fix the failing transcript preview test.") },cx);
            c.fixture_event(E::ModelCallStarted { turn_id, call: 1 }, cx);
            c.fixture_event(E::Intermediate { turn_id,message: Message::assistant("I'll reproduce the failure, then trace the restored prompt."),attempt_id: None },cx);
            let profile = ModelProfileRef::new("fixture", "fixture");
            c.fixture_event(E::UsageUpdated {
                turn_id,
                usage: TokenUsage { input_tokens: 12_300, cached_tokens: 10_200, output_tokens: 2_100, total_tokens: 14_400 },
                profile: profile.clone(), model_role: ModelRole::Build,
                input_token_limit: 100_000, context_window_tokens: 128_000,
            }, cx);
            c.fixture_event(E::ContextUsageUpdated { turn_id, snapshot: ContextTokenSnapshot {
                profile, model_role: ModelRole::Build,
                projected_input_tokens: Some(14_800), known_input_tokens: 14_800,
                unresolved_media: Default::default(), source: ContextTokenSource::UsagePlusDelta,
                automatic_trigger: 80_000, input_token_limit: 100_000, context_window_tokens: 128_000,
            }}, cx);
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
