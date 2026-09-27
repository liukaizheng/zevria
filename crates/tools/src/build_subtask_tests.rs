use super::*;
use rig_core::tool::ToolErrorKind;
use tokio_util::sync::CancellationToken;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionUpdate;
use zevria_session_api::SubtaskChannels;
use zevria_session_api::session_event_channel;
use zevria_session_api::subtask_channels;

const ORCHESTRATE_REQUIRED: &str = "tasks[0]: Builder subtasks require an explicit /orchestrate <prompt> request; use `explore` where available or ask the user to submit `/orchestrate <prompt>` in Build";

fn fixture() -> (tempfile::TempDir, LaunchSubtasksTool, SubtaskChannels) {
    let root = tempfile::tempdir().unwrap();
    let (events, _) = session_event_channel(32);
    let channels = subtask_channels("root", events);
    let tool = LaunchSubtasksTool::new(channels.launcher.clone(), root.path().to_path_buf());
    (root, tool, channels)
}

fn args(workspace: Option<&str>) -> LaunchSubtaskSpec {
    LaunchSubtaskSpec {
        title: "Create a page".into(),
        prompt: "Make the page from input.txt".into(),
        r#type: LaunchSubtaskType::Build,
        workspace: workspace.map(str::to_string),
    }
}

fn context_for_mode(mode: Option<SessionMode>, authorized: bool) -> ToolContext {
    let mut context = ToolContext::new();
    if let Some(mode) = mode {
        context.insert(
            TurnContext::new(TurnId::new(1), mode, CancellationToken::new())
                .with_build_subtasks(authorized),
        );
    }
    context
}

#[tokio::test]
async fn invalid_kind_workspace_arguments_reject_before_preparation() {
    let (root, tool, mut channels) = fixture();
    for (mode, args, expected) in [
        (
            SessionMode::Build,
            args(None),
            "build subtasks require a non-null workspace subdirectory",
        ),
        (
            SessionMode::Build,
            LaunchSubtaskSpec {
                r#type: LaunchSubtaskType::Explore,
                ..args(Some("fresh"))
            },
            "explore subtasks must omit workspace or set it to null",
        ),
    ] {
        let mut context = context_for_mode(Some(mode), true);
        let error = tool.call(&mut context, batch(args)).await.unwrap_err();
        assert_eq!(error.to_string(), format!("tasks[0]: {expected}"));
        assert_eq!(tool.map_error(error).kind(), ToolErrorKind::InvalidArgs);
        assert!(context.result::<ToolResultDetail>().is_none());
        assert!(channels.requests.try_recv().is_err());
        assert!(!root.path().join("fresh").exists());
    }
    for field in ["", ",\"workspace\":null"] {
        let value: LaunchSubtaskSpec = serde_json::from_str(&format!(
            "{{\"title\":\"title\",\"prompt\":\"prompt\",\"type\":\"explore\"{field}}}"
        ))
        .unwrap();
        assert!(value.workspace.is_none());
    }
}

#[tokio::test]
async fn launch_capability_matrix_is_independent_of_nominal_mode() {
    for (mode, build_subtasks) in [
        (Some(SessionMode::Build), false),
        (Some(SessionMode::Build), true),
        (Some(SessionMode::Plan), false),
        (Some(SessionMode::Plan), true),
        (Some(SessionMode::Build), false),
        (Some(SessionMode::Build), true),
        (None, false),
    ] {
        for kind in [LaunchSubtaskType::Explore, LaunchSubtaskType::Build] {
            let root = tempfile::tempdir().unwrap();
            let (events, mut events_rx) = session_event_channel(32);
            let mut channels = subtask_channels("root", events);
            let tool =
                LaunchSubtasksTool::new(channels.launcher.clone(), root.path().to_path_buf());
            let mut context = ToolContext::new();
            if let Some(mode) = mode {
                context.insert(
                    TurnContext::new(TurnId::new(1), mode, CancellationToken::new())
                        .with_build_subtasks(build_subtasks),
                );
            }
            context.insert(ToolCallId("call-mode-matrix".into()));
            let arguments = LaunchSubtaskSpec {
                r#type: kind,
                workspace: (kind == LaunchSubtaskType::Build).then(|| "fresh/nested".into()),
                ..args(None)
            };
            let allowed = kind == LaunchSubtaskType::Explore || build_subtasks;
            // A failure guard only: unauthorized calls must reject, and accepted
            // calls must enqueue without needing their report first.
            let (result, descriptor) =
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    tokio::join!(tool.call(&mut context, batch(arguments)), async {
                        if !allowed {
                            return None;
                        }
                        let request = channels.requests.recv().await.expect("accepted launch");
                        assert_eq!(request.turn.mode, mode.unwrap_or(SessionMode::Build));
                        assert_eq!(request.descriptor.kind, SubtaskKind::from(kind));
                        assert_eq!(
                            request.workspace.is_some(),
                            kind == LaunchSubtaskType::Build
                        );
                        if let Some(workspace) = &request.workspace {
                            assert!(workspace.path.is_dir());
                            assert!(
                                channels
                                    .launcher
                                    .reserve_workspace(&workspace.path)
                                    .is_err()
                            );
                        }
                        request
                            .outcome
                            .send(SubtaskOutcome::Completed {
                                report: "mode matrix report".into(),
                            })
                            .unwrap();
                        Some(request.descriptor)
                    })
                })
                .await
                .expect("mode validation or launch must finish");

            if let Some(descriptor) = descriptor {
                assert!(result.unwrap().contains("mode matrix report"));
                let metadata = context
                    .result::<ToolResultDetail>()
                    .and_then(|detail| detail.subtasks().first())
                    .and_then(|entry| entry.launch.as_ref())
                    .expect("launch metadata");
                assert_eq!(metadata.id, descriptor.id);
                assert_eq!(metadata.kind, descriptor.kind);
                assert_eq!(metadata.workspace, descriptor.workspace);
                assert!(matches!(
                    events_rx.try_recv().expect("launch event"),
                    SessionUpdate::Lifecycle(SessionEvent::SubtaskLaunched {
                        call_id, descriptor: launched, ..
                    }) if call_id == "call-mode-matrix" && launched == descriptor
                ));
            } else {
                let error =
                    result.expect_err("missing capability must reject builders regardless of mode");
                assert_eq!(error.to_string(), ORCHESTRATE_REQUIRED);
                assert_eq!(tool.map_error(error).kind(), ToolErrorKind::InvalidArgs);
                assert!(context.result::<ToolResultDetail>().is_none());
            }
            assert!(context.result::<ToolCancelled>().is_none());
            assert!(
                channels.requests.try_recv().is_err(),
                "no unexpected launch queued"
            );
            assert!(events_rx.try_recv().is_err(), "no unexpected launch event");
            assert_eq!(
                root.path().join("fresh").exists(),
                allowed && kind == LaunchSubtaskType::Build
            );
            assert!(
                channels
                    .launcher
                    .reserve_workspace(root.path().join("fresh/nested"))
                    .is_ok(),
                "no reservation may remain after rejection or completion"
            );
        }
    }
}

#[tokio::test]
async fn forbidden_builder_modes_reject_before_workspace_resolution_or_reservation() {
    let (root, tool, mut channels) = fixture();
    let reserved = root.path().join("reserved");
    let _reservation = channels.launcher.reserve_workspace(&reserved).unwrap();
    fs::write(root.path().join("file"), "preserve me").unwrap();
    let missing_startup = LaunchSubtasksTool::new(
        channels.launcher.clone(),
        root.path().join("missing-startup"),
    );
    for mode in [Some(SessionMode::Build), Some(SessionMode::Plan), None] {
        for workspace in [
            None,
            Some(""),
            Some(".zevria/child"),
            Some("reserved/nested"),
            Some("file/nested"),
        ] {
            let mut context = context_for_mode(mode, false);
            let error = tool
                .call(&mut context, batch(args(workspace)))
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                ORCHESTRATE_REQUIRED,
                "mode {mode:?} must be checked before workspace {workspace:?}"
            );
            assert_eq!(tool.map_error(error).kind(), ToolErrorKind::InvalidArgs);
            assert!(context.result::<ToolResultDetail>().is_none());
        }
        // Even an unresolvable startup root must not be inspected first.
        let mut context = context_for_mode(mode, false);
        let error = missing_startup
            .call(&mut context, batch(args(Some("fresh"))))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), ORCHESTRATE_REQUIRED);
        assert_eq!(
            missing_startup.map_error(error).kind(),
            ToolErrorKind::InvalidArgs
        );
        assert!(context.result::<ToolResultDetail>().is_none());
        assert!(channels.requests.try_recv().is_err());
        assert!(
            channels.launcher.reserve_workspace(&reserved).is_err(),
            "existing ownership must remain held"
        );
        assert!(!reserved.exists());
        assert!(!root.path().join(".zevria").exists());
        assert!(!root.path().join("missing-startup").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "preserve me"
        );
    }
}

#[test]
fn raw_workspace_syntax_and_non_directories_are_actionable() {
    let (root, tool, _) = fixture();
    for path in [
        "",
        " ",
        "/",
        ".",
        "./x",
        "x/./y",
        "..",
        "x/../y",
        ".zevria",
        ".zevria/artifacts",
    ] {
        assert!(tool.prepare_workspace(path).is_err(), "accepted {path:?}");
    }
    assert!(
        tool.prepare_workspace(root.path().to_str().unwrap())
            .is_err()
    );
    fs::write(root.path().join("file"), "input").unwrap();
    for path in ["file", "file/nested"] {
        assert!(
            tool.prepare_workspace(path)
                .unwrap_err()
                .to_string()
                .contains("not a directory")
        );
    }
    #[cfg(windows)]
    for path in ["C:\\tmp", "C:relative", "\\\\server\\share", "x\\.\\y"] {
        assert!(tool.prepare_workspace(path).is_err());
    }
}

#[test]
fn creation_preserves_content_and_overlap_rejects_before_creation() {
    let (root, tool, channels) = fixture();
    fs::create_dir(root.path().join("books")).unwrap();
    fs::write(root.path().join("books/input.txt"), "keep me").unwrap();
    let workspace = tool.prepare_workspace("books//").unwrap();
    assert_eq!(
        workspace.path,
        fs::canonicalize(root.path().join("books")).unwrap()
    );
    assert_eq!(workspace.display, "books");
    assert_eq!(
        fs::read_to_string(workspace.path.join("input.txt")).unwrap(),
        "keep me"
    );
    let error = tool.prepare_workspace("books/new/nested").unwrap_err();
    assert!(error.to_string().contains("overlaps reserved directory"));
    assert!(!root.path().join("books/new").exists());
    assert!(tool.prepare_workspace("books-extra").is_ok());
    drop(workspace);
    let nested = tool.prepare_workspace("books/new/nested").unwrap();
    assert!(nested.path.is_dir());
    assert!(channels.launcher.reserve_workspace(&nested.path).is_err());
    drop(nested);
    assert!(tool.prepare_workspace("books").is_ok());
    assert_eq!(
        fs::read_to_string(root.path().join("books/input.txt")).unwrap(),
        "keep me"
    );
}

#[cfg(unix)]
#[test]
fn symlinks_resolve_before_reservation_and_cannot_alias_protected_or_external_roots() {
    use std::os::unix::fs::symlink;
    let (root, tool, _) = fixture();
    let external = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("safe")).unwrap();
    fs::create_dir(root.path().join(".zevria")).unwrap();
    for (alias, target) in [
        ("outside", external.path().to_path_buf()),
        ("root-alias", root.path().to_path_buf()),
        ("storage", root.path().join(".zevria")),
        ("dangling", root.path().join("missing")),
        ("one", root.path().join("safe")),
        ("two", root.path().join("safe")),
    ] {
        symlink(target, root.path().join(alias)).unwrap();
    }
    for alias in ["outside", "root-alias", "storage", "dangling"] {
        assert!(tool.prepare_workspace(alias).is_err());
        assert!(tool.prepare_workspace(&format!("{alias}/new")).is_err());
    }
    assert!(!external.path().join("new").exists());
    let owned = tool.prepare_workspace("one/new").unwrap();
    assert_eq!(
        owned.path,
        fs::canonicalize(root.path().join("safe/new")).unwrap()
    );
    assert_eq!(owned.display, "one/new");
    assert!(tool.prepare_workspace("two/new/missing").is_err());
    assert!(!owned.path.join("missing").exists());
    drop(owned);
    assert!(tool.prepare_workspace("two/new").is_ok());
}

#[test]
fn existing_case_aliases_contend_when_the_host_filesystem_supports_them() {
    let (root, tool, _) = fixture();
    fs::create_dir(root.path().join("case-directory")).unwrap();
    if fs::symlink_metadata(root.path().join("CASE-DIRECTORY")).is_err() {
        return; // A case-sensitive host has no such alias to exercise.
    }
    let owned = tool.prepare_workspace("case-directory").unwrap();
    assert!(tool.prepare_workspace("CASE-DIRECTORY/new").is_err());
    assert!(!owned.path.join("new").exists());
    fs::create_dir(root.path().join(".zevria")).unwrap();
    assert!(tool.prepare_workspace(".ZEVRIA/new").is_err());
    assert!(!root.path().join(".zevria/new").exists());
}

#[test]
fn identity_mismatch_releases_guard_without_enqueuing() {
    let (root, _tool, mut channels) = fixture();
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let guard = channels.launcher.reserve_workspace(&a).unwrap();
    assert!(
        ChildWorkspace::new(fs::canonicalize(b).unwrap(), "a".into(), guard)
            .unwrap_err()
            .to_string()
            .contains("retry")
    );
    assert!(channels.launcher.reserve_workspace(a).is_ok());
    assert!(channels.requests.try_recv().is_err());
}

#[tokio::test]
async fn accepted_success_failure_and_cancel_keep_workspace_metadata_and_artifacts() {
    for outcome in [
        SubtaskOutcome::Completed {
            report: "index.html created".into(),
        },
        SubtaskOutcome::Failed {
            error: "fixture failure".into(),
        },
        SubtaskOutcome::Cancelled,
    ] {
        let (root, tool, mut channels) = fixture();
        let call = tokio::spawn(async move {
            let mut context = context_for_mode(Some(SessionMode::Build), true);
            let result = tool
                .call(&mut context, batch(args(Some("pages/book-1"))))
                .await;
            (result, context)
        });
        let mut request = channels.requests.recv().await.unwrap();
        assert_eq!(request.descriptor.kind, SubtaskKind::Build);
        assert_eq!(
            request.descriptor.workspace.as_deref(),
            Some("pages/book-1")
        );
        let path = &request.workspace.as_ref().unwrap().path;
        fs::write(path.join("index.html"), "partial or complete").unwrap();
        drop(request.workspace.take());
        request.outcome.send(outcome.clone()).unwrap();
        let (result, context) = call.await.unwrap();
        let metadata = context
            .result::<ToolResultDetail>()
            .and_then(|detail| detail.subtasks().first())
            .and_then(|entry| entry.launch.as_ref())
            .unwrap();
        assert_eq!(metadata.id, request.descriptor.id);
        assert_eq!(metadata.workspace.as_deref(), Some("pages/book-1"));
        match outcome {
            SubtaskOutcome::Completed { .. } => assert!(
                result
                    .unwrap()
                    .contains("workspace: pages/book-1\nstatus: completed\nreport:")
            ),
            SubtaskOutcome::Failed { .. } => {
                assert!(matches!(result, Err(LaunchSubtasksError::Failed(_))))
            }
            SubtaskOutcome::Cancelled => assert!(result.is_err()),
        }
        assert!(root.path().join("pages/book-1/index.html").is_file());
        assert!(
            channels
                .launcher
                .reserve_workspace(root.path().join("pages/book-1"))
                .is_ok()
        );
    }
}

#[tokio::test]
async fn already_cancelled_launch_never_creates_a_directory_or_metadata() {
    let (root, tool, mut channels) = fixture();
    let token = CancellationToken::new();
    token.cancel();
    let mut context = ToolContext::new();
    context.insert(
        TurnContext::new(TurnId::new(1), SessionMode::Build, token).with_build_subtasks(true),
    );
    assert!(
        tool.call(&mut context, batch(args(Some("fresh"))))
            .await
            .is_err()
    );
    assert!(context.result::<ToolCancelled>().is_some());
    let detail = context.result::<ToolResultDetail>().unwrap();
    assert_eq!(detail.subtasks().len(), 1);
    assert!(detail.subtasks()[0].launch.is_none());
    assert_eq!(detail.subtasks()[0].status, SubtaskStatus::Cancelled);
    assert!(!root.path().join("fresh").exists());
    assert!(channels.requests.try_recv().is_err());
}

fn batch(task: LaunchSubtaskSpec) -> LaunchSubtasksArgs {
    LaunchSubtasksArgs { tasks: vec![task] }
}

impl LaunchSubtasksTool {
    fn prepare_workspace(&self, raw: &str) -> anyhow::Result<ChildWorkspace> {
        Ok(self
            .prepare_workspaces(&[args(Some(raw))])?
            .remove(0)
            .unwrap())
    }
}
