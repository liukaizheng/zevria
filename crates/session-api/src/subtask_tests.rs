use super::*;
use crate::{ModelRole, SessionMode, TurnId, session_event_channel};
use tokio_util::sync::CancellationToken;

fn turn(token: CancellationToken) -> TurnContext {
    TurnContext::new(TurnId::new(1), SessionMode::Build, token)
}

fn orchestrating_turn(token: CancellationToken) -> TurnContext {
    TurnContext::new(TurnId::new(1), SessionMode::Build, token).with_build_subtasks(true)
}

fn child(launcher: &SubtaskLauncher, path: PathBuf) -> ChildWorkspace {
    let guard = launcher.reserve_workspace(&path).unwrap();
    fs::create_dir_all(&path).unwrap();
    ChildWorkspace::new(fs::canonicalize(path).unwrap(), "child".into(), guard).unwrap()
}

#[tokio::test]
async fn native_builder_permission_matrix_precedes_queue_and_launch_events() {
    for (mode, build_subtasks) in SessionMode::ALL
        .into_iter()
        .flat_map(|mode| [false, true].map(|enabled| (mode, enabled)))
    {
        for kind in [SubtaskKind::Explore, SubtaskKind::Build] {
            let root = tempfile::tempdir().unwrap();
            let (events, mut receiver) = session_event_channel(32);
            let mut channels = subtask_channels("root", events);
            let path = root.path().join("child");
            let workspace =
                (kind == SubtaskKind::Build).then(|| child(&channels.launcher, path.clone()));
            let result = channels
                .launcher
                .launch(
                    "direct",
                    0,
                    "child",
                    kind,
                    "prompt",
                    TurnContext::new(TurnId::new(1), mode, CancellationToken::new())
                        .with_build_subtasks(build_subtasks),
                    workspace,
                )
                .await;
            if kind == SubtaskKind::Build && !build_subtasks {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("require an explicit /orchestrate <prompt> request")
                );
                assert!(channels.requests.try_recv().is_err());
                assert!(receiver.try_recv().is_err());
                assert!(channels.launcher.reserve_workspace(&path).is_ok());
                // Native callers prepared this directory themselves: rejection
                // releases ownership, never removes existing caller artifacts.
                assert!(path.is_dir());
            } else {
                assert!(result.is_ok());
                assert!(channels.requests.try_recv().is_ok());
                assert!(receiver.try_recv().is_ok());
            }
        }
    }
}

#[test]
fn five_roles_and_workspace_metadata_contract() {
    for (index, (role, name)) in ModelRole::ALL
        .into_iter()
        .zip(["build", "plan", "review", "explore", "builder"])
        .enumerate()
    {
        assert_eq!(role.index(), index);
        assert_eq!(role.name(), name);
    }
    assert_eq!(SubtaskKind::Build.model_role(), ModelRole::Builder);
    assert_eq!(SubtaskKind::Explore.model_role(), ModelRole::Explore);
    assert_eq!(SubtaskKind::Build.to_string(), "build");
    assert_eq!(
        serde_json::to_string(&SubtaskKind::Build).unwrap(),
        "\"build\""
    );
    let mut metadata: SubtaskLaunchMetadata =
        serde_json::from_str(r#"{"id":"old","title":"old child","kind":"explore"}"#).unwrap();
    assert!(metadata.workspace.is_none());
    assert!(
        serde_json::to_value(&metadata)
            .unwrap()
            .get("workspace")
            .is_none()
    );
    metadata.kind = SubtaskKind::Build;
    metadata.workspace = Some("books/one".into());
    assert_eq!(
        serde_json::from_value::<SubtaskLaunchMetadata>(serde_json::to_value(&metadata).unwrap())
            .unwrap(),
        metadata
    );
}

#[test]
fn reservations_are_component_aware_shared_per_parent_and_raii() {
    let root = tempfile::tempdir().unwrap();
    let (events, _) = session_event_channel(32);
    let channels = subtask_channels("root", events.clone());
    let other = subtask_channels("other", events);
    let path = root.path().join("books/book");
    let guard = channels.launcher.reserve_workspace(&path).unwrap();
    assert!(!path.exists(), "reserving never creates directories");
    for conflict in [&path, &root.path().join("books"), &path.join("nested")] {
        let error = channels
            .launcher
            .clone()
            .reserve_workspace(conflict)
            .unwrap_err();
        let WorkspaceConflict::Overlap { path: occupied } = &error else {
            panic!("expected reservation overlap for {conflict:?}, got {error}");
        };
        assert_eq!(
            occupied,
            guard.path(),
            "must identify the existing reservation"
        );
        // The reserved path has native separators and a canonical ancestor,
        // which need not have the same spelling as the requested path.
        assert!(
            error
                .to_string()
                .contains(&guard.path().display().to_string()),
            "overlap diagnostic must include the reserved path: {error}"
        );
    }
    for sibling in [
        root.path().join("books/book-extra"),
        root.path().join("books/other"),
    ] {
        assert!(channels.launcher.reserve_workspace(sibling).is_ok());
    }
    assert!(other.launcher.reserve_workspace(&path).is_ok());
    drop(guard);
    assert!(channels.launcher.reserve_workspace(&path).is_ok());
}

#[tokio::test]
async fn capacity_one_backpressures_three_concurrent_launches_without_rejection() {
    let (events, _receiver) = session_event_channel(32);
    let mut channels = subtask_channels_with_capacity("root", events, 1);
    let launcher = channels.launcher.clone();
    let launches = futures_util::future::join_all((0..3).map(|n| {
        launcher.launch(
            n.to_string(),
            0,
            format!("task {n}"),
            SubtaskKind::Explore,
            "prompt",
            turn(CancellationToken::new()),
            None,
        )
    }));
    let drain = async {
        for _ in 0..3 {
            let request = channels.requests.recv().await.unwrap();
            request
                .outcome
                .send(SubtaskOutcome::Completed {
                    report: request.descriptor.title,
                })
                .unwrap();
        }
    };
    let (results, ()) = tokio::join!(launches, drain);
    for (n, result) in results.into_iter().enumerate() {
        let (metadata, outcome) = result.unwrap();
        assert_eq!(metadata.title, format!("task {n}"));
        assert_eq!(
            outcome.await.unwrap(),
            SubtaskOutcome::Completed {
                report: format!("task {n}")
            }
        );
    }
}

#[tokio::test]
async fn cancelled_and_closed_sends_do_not_announce_and_release_the_request() {
    use std::{future::Future, task::Poll};
    let root = tempfile::tempdir().unwrap();
    let (events, mut receiver) = session_event_channel(32);
    let mut channels = subtask_channels_with_capacity("root", events, 1);
    let launcher = &channels.launcher;
    let path = root.path().join("child");
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        launcher
            .launch(
                "cancelled",
                0,
                "child",
                SubtaskKind::Build,
                "prompt",
                orchestrating_turn(token),
                Some(child(launcher, path.clone()))
            )
            .await
            .is_err()
    );
    assert!(channels.requests.try_recv().is_err());
    assert!(receiver.try_recv().is_err());
    assert!(launcher.reserve_workspace(&path).is_ok());

    let (_, outcome) = launcher
        .launch(
            "first",
            0,
            "first",
            SubtaskKind::Explore,
            "prompt",
            turn(CancellationToken::new()),
            None,
        )
        .await
        .unwrap();
    receiver.try_recv().unwrap();
    let token = CancellationToken::new();
    let mut blocked = Box::pin(launcher.launch(
        "blocked",
        0,
        "blocked",
        SubtaskKind::Build,
        "prompt",
        orchestrating_turn(token.clone()),
        Some(child(launcher, path.clone())),
    ));
    std::future::poll_fn(|cx| {
        assert!(blocked.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(launcher.reserve_workspace(&path).is_err());
    token.cancel();
    assert!(blocked.await.is_err());
    assert!(launcher.reserve_workspace(&path).is_ok());
    assert!(receiver.try_recv().is_err());
    drop(channels.requests.recv().await.unwrap());
    assert!(outcome.await.is_err());

    channels.requests.close();
    let error = launcher
        .launch(
            "closed",
            0,
            "closed",
            SubtaskKind::Build,
            "prompt",
            orchestrating_turn(CancellationToken::new()),
            Some(child(launcher, path.clone())),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("supervisor"));
    assert!(launcher.reserve_workspace(path).is_ok());
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn dropped_build_request_releases_ownership_and_inconsistent_kinds_fail() {
    let root = tempfile::tempdir().unwrap();
    let (events, _) = session_event_channel(32);
    let mut channels = subtask_channels("root", events);
    let path = root.path().join("child");
    let workspace = child(&channels.launcher, path.clone());
    let (_, outcome) = channels
        .launcher
        .launch(
            "build",
            0,
            "child",
            SubtaskKind::Build,
            "prompt",
            orchestrating_turn(CancellationToken::new()),
            Some(workspace),
        )
        .await
        .unwrap();
    assert!(channels.launcher.reserve_workspace(&path).is_err());
    drop(channels.requests.recv().await.unwrap());
    assert!(outcome.await.is_err());
    assert!(channels.launcher.reserve_workspace(&path).is_ok());
    assert!(
        channels
            .launcher
            .launch(
                "bad",
                0,
                "bad",
                SubtaskKind::Build,
                "prompt",
                orchestrating_turn(CancellationToken::new()),
                None
            )
            .await
            .is_err()
    );
    assert!(
        channels
            .launcher
            .launch(
                "bad",
                0,
                "bad",
                SubtaskKind::Explore,
                "prompt",
                turn(CancellationToken::new()),
                Some(child(&channels.launcher, path))
            )
            .await
            .is_err()
    );
}

#[test]
fn batch_reservation_checks_every_key_before_inserting_and_never_creates_directories() {
    let root = tempfile::tempdir().unwrap();
    let (events, _) = session_event_channel(32);
    let channels = subtask_channels("root", events);
    let launcher = &channels.launcher;
    let path = |name: &str| root.path().join(name);
    let active = launcher
        .reserve_workspaces(&[path("active/a"), path("active/b")])
        .unwrap();
    for paths in [
        vec![path("fresh"), path("active/a/nested")],
        vec![path("fresh"), path("active")],
        vec![path("fresh"), path("fresh")],
        vec![path("fresh"), path("fresh/nested")],
        vec![path("fresh/nested"), path("fresh")],
        vec![path("fresh"), std::path::PathBuf::from("relative")],
    ] {
        assert!(launcher.reserve_workspaces(&paths).is_err());
        assert!(
            launcher.reserve_workspace(path("fresh")).is_ok(),
            "no partial insertion"
        );
        assert!(
            launcher.reserve_workspace(path("active")).is_err(),
            "existing owner survives failed batch"
        );
        assert!(!path("fresh").exists());
    }
    let siblings = launcher
        .reserve_workspaces(&[path("fresh"), path("fresh-extra")])
        .unwrap();
    drop(siblings);
    drop(active);
    assert!(
        launcher
            .reserve_workspaces(&[path("active"), path("fresh")])
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn batch_reservations_reject_pairwise_symlink_aliases_atomically() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("actual")).unwrap();
    symlink(root.path().join("actual"), root.path().join("alias")).unwrap();
    let (events, _) = session_event_channel(32);
    let launcher = subtask_channels("root", events).launcher;
    assert!(
        launcher
            .reserve_workspaces(&[
                root.path().join("actual/missing"),
                root.path().join("alias/missing")
            ])
            .is_err()
    );
    assert!(
        launcher
            .reserve_workspace(root.path().join("actual"))
            .is_ok()
    );
    assert!(!root.path().join("actual/missing").exists());
}
