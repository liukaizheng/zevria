use super::*;
use std::{future::Future, task::Poll, time::Duration};
use tokio_util::sync::CancellationToken;
use zevria_session_api::{SubtaskChannels, session_event_channel, subtask_channels_with_capacity};

fn fixture(capacity: usize) -> (tempfile::TempDir, LaunchSubtasksTool, SubtaskChannels) {
    let root = tempfile::tempdir().unwrap();
    let (events, _) = session_event_channel(64);
    let channels = subtask_channels_with_capacity("root", events, capacity);
    let tool = LaunchSubtasksTool::new(channels.launcher.clone(), root.path().to_path_buf());
    (root, tool, channels)
}

fn task(workspace: Option<&str>) -> LaunchSubtaskSpec {
    LaunchSubtaskSpec {
        title: "Same title".into(),
        prompt: "Independent self-contained task".into(),
        r#type: if workspace.is_some() {
            LaunchSubtaskType::Build
        } else {
            LaunchSubtaskType::Explore
        },
        workspace: workspace.map(str::to_owned),
    }
}
fn batch(tasks: Vec<LaunchSubtaskSpec>) -> LaunchSubtasksArgs {
    LaunchSubtasksArgs { tasks }
}
fn context(token: CancellationToken, build: bool) -> ToolContext {
    let mut context = ToolContext::new();
    context.insert(ToolCallId("outer-call".into()));
    context.insert(
        TurnContext::new(TurnId::new(1), SessionMode::Build, token).with_build_subtasks(build),
    );
    context
}

#[tokio::test]
async fn whole_batch_validation_never_launches_or_creates_the_valid_first_workspace() {
    let (root, tool, mut channels) = fixture(1);
    let mut blank = task(None);
    blank.title = "  ".into();
    let mut prompt = task(None);
    prompt.prompt = "\n".into();
    let mut invalid_explore = task(Some("bad"));
    invalid_explore.r#type = LaunchSubtaskType::Explore;
    for tasks in [
        vec![],
        vec![task(Some("fresh")), blank],
        vec![task(Some("fresh")), prompt],
        vec![task(Some("fresh")), task(Some(".zevria/store"))],
        vec![task(Some("fresh")), task(Some("../outside"))],
        vec![task(Some("fresh")), invalid_explore],
        vec![task(Some("fresh")), task(Some("fresh/nested"))],
        vec![task(Some("fresh/nested")), task(Some("fresh"))],
        vec![task(Some("fresh")), task(Some("fresh"))],
    ] {
        let result = tool
            .call(&mut context(CancellationToken::new(), true), batch(tasks))
            .await;
        assert!(matches!(
            result,
            Err(LaunchSubtasksError::InvalidArguments(_))
        ));
        assert!(!root.path().join("fresh").exists());
        assert!(channels.requests.try_recv().is_err());
        assert!(
            channels
                .launcher
                .reserve_workspace(root.path().join("fresh"))
                .is_ok()
        );
    }
    // Authorization of the second entry happens before even resolving the first.
    let result = tool
        .call(
            &mut context(CancellationToken::new(), false),
            batch(vec![task(None), task(Some("fresh"))]),
        )
        .await;
    assert!(
        result.unwrap_err().to_string().contains(
            "tasks[1]: Builder subtasks require an explicit /orchestrate <prompt> request"
        )
    );
    assert!(!root.path().join("fresh").exists());
    assert!(channels.requests.try_recv().is_err());
    for wire in [
        serde_json::json!({"tasks":[{"title":"ok","prompt":"ok","type":"explore"},{"title":"bad","prompt":"ok","type":"explore","extra":true}]}),
        serde_json::json!({"tasks":[],"extra":true}),
        serde_json::json!({"tasks":[{"title":"ok","prompt":"ok","type":"review"}]}),
    ] {
        assert!(serde_json::from_value::<LaunchSubtasksArgs>(wire).is_err());
    }
}

#[tokio::test]
async fn mixed_outcomes_keep_all_reports_and_input_order_even_on_supervisor_shutdown() {
    for shutdown in [false, true] {
        let (root, tool, mut channels) = fixture(1);
        let call = tokio::spawn(async move {
            let mut context = context(CancellationToken::new(), true);
            let result = tool
                .call(&mut context, batch(vec![task(Some("a")), task(Some("b"))]))
                .await;
            (result, context)
        });
        let first = channels.requests.recv().await.unwrap();
        let second = channels.requests.recv().await.unwrap();
        let ids = [first.descriptor.id.clone(), second.descriptor.id.clone()];
        assert_ne!(ids[0], ids[1]);
        second
            .outcome
            .send(SubtaskOutcome::Completed {
                report: "SUCCESS_FROM_SECOND".into(),
            })
            .unwrap();
        drop(second.workspace);
        if shutdown {
            drop(first);
        } else {
            first
                .outcome
                .send(SubtaskOutcome::Failed {
                    error: "FIRST_FAILED".into(),
                })
                .unwrap();
            drop(first.workspace);
        }
        let (result, context) = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap();
        let output = result.unwrap_err().to_string();
        assert!(output.contains("SUCCESS_FROM_SECOND"));
        assert!(output.contains(if shutdown {
            "supervisor shut down"
        } else {
            "FIRST_FAILED"
        }));
        assert!(output.find(ids[0].as_str()) < output.find(ids[1].as_str()));
        let entries = context.result::<ToolResultDetail>().unwrap().subtasks();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, SubtaskStatus::Failed);
        assert_eq!(entries[1].status, SubtaskStatus::Completed);
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.index, index);
            assert_eq!(entry.launch.as_ref().unwrap().id, ids[index]);
        }
        assert!(
            channels
                .launcher
                .reserve_workspaces(&[root.path().join("a"), root.path().join("b")])
                .is_ok()
        );
    }
}

#[tokio::test]
async fn queue_cancellation_retains_accepted_identity_and_completed_report_without_dropping_slots()
{
    let (root, tool, mut channels) = fixture(1);
    let token = CancellationToken::new();
    let mut context = context(token.clone(), true);
    let mut call = Box::pin(tool.call(
        &mut context,
        batch(vec![task(Some("a")), task(Some("b")), task(Some("c"))]),
    ));
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    for path in ["a", "b", "c"] {
        assert!(
            channels
                .launcher
                .reserve_workspace(root.path().join(path))
                .is_err()
        );
    }
    token.cancel();
    let first = channels.requests.recv().await.unwrap();
    let id = first.descriptor.id.clone();
    first
        .outcome
        .send(SubtaskOutcome::Completed {
            report: "ALREADY_COMPLETED".into(),
        })
        .unwrap();
    drop(first.workspace);
    let output = call.await.unwrap_err().to_string();
    assert!(output.contains("ALREADY_COMPLETED"));
    assert!(output.contains("requested: 3\nlaunched: 1"));
    assert!(context.result::<ToolCancelled>().is_some());
    let entries = context.result::<ToolResultDetail>().unwrap().subtasks();
    assert_eq!(entries[0].launch.as_ref().unwrap().id, id);
    assert_eq!(entries[0].status, SubtaskStatus::Completed);
    assert_eq!(entries.len(), 3);
    for entry in &entries[1..] {
        assert!(entry.launch.is_none());
        assert_eq!(entry.status, SubtaskStatus::Cancelled);
    }
    assert!(channels.requests.try_recv().is_err());
    for path in ["a", "b", "c"] {
        assert!(
            channels
                .launcher
                .reserve_workspace(root.path().join(path))
                .is_ok()
        );
    }
}

#[tokio::test]
async fn execution_cancellation_still_waits_for_all_accepted_children() {
    let (_, tool, mut channels) = fixture(2);
    let token = CancellationToken::new();
    let mut context = context(token.clone(), false);
    let mut call = Box::pin(tool.call(&mut context, batch(vec![task(None), task(None)])));
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let first = channels.requests.recv().await.unwrap();
    let second = channels.requests.recv().await.unwrap();
    first
        .outcome
        .send(SubtaskOutcome::Completed {
            report: "KEPT".into(),
        })
        .unwrap();
    token.cancel();
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    second.outcome.send(SubtaskOutcome::Cancelled).unwrap();
    assert!(call.await.unwrap_err().to_string().contains("KEPT"));
    assert!(context.result::<ToolCancelled>().is_some());
    assert_eq!(
        context.result::<ToolResultDetail>().unwrap().subtasks()[1].status,
        SubtaskStatus::Cancelled
    );
}

#[tokio::test]
async fn closed_supervisor_keeps_all_unlaunched_slots_and_releases_all_workspaces() {
    let (root, tool, mut channels) = fixture(1);
    channels.requests.close();
    let mut context = context(CancellationToken::new(), true);
    assert!(
        tool.call(&mut context, batch(vec![task(Some("a")), task(Some("b"))]))
            .await
            .unwrap_err()
            .to_string()
            .contains("launched: 0")
    );
    let entries = context.result::<ToolResultDetail>().unwrap().subtasks();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.launch.is_none() && entry.status == SubtaskStatus::Failed)
    );
    assert!(
        channels
            .launcher
            .reserve_workspaces(&[root.path().join("a"), root.path().join("b")])
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn batch_alias_conflicts_do_not_create_directories_or_leak_reservations() {
    use std::os::unix::fs::symlink;
    let (root, tool, channels) = fixture(1);
    fs::create_dir(root.path().join("safe")).unwrap();
    symlink(root.path().join("safe"), root.path().join("alias")).unwrap();
    for tasks in [
        vec![task(Some("safe/new")), task(Some("alias/new"))],
        vec![task(Some("safe/new")), task(Some("alias/new/nested"))],
    ] {
        assert!(tool.prepare_workspaces(&tasks).is_err());
        assert!(!root.path().join("safe/new").exists());
        assert!(
            channels
                .launcher
                .reserve_workspace(root.path().join("safe"))
                .is_ok()
        );
    }
    let active = channels
        .launcher
        .reserve_workspace(root.path().join("safe/existing"))
        .unwrap();
    assert!(
        tool.prepare_workspaces(&[task(Some("fresh")), task(Some("alias/existing/nested"))])
            .is_err()
    );
    assert!(!root.path().join("fresh").exists());
    assert!(
        channels
            .launcher
            .reserve_workspace(root.path().join("safe/existing"))
            .is_err()
    );
    drop(active);
    assert!(
        tool.prepare_workspaces(&[task(Some("fresh")), task(Some("safe/existing"))])
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn filesystem_failure_releases_every_guard_but_does_not_claim_directory_rollback() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (root, tool, channels) = fixture(1);
    fs::create_dir(root.path().join("blocked")).unwrap();
    fs::set_permissions(
        root.path().join("blocked"),
        fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    let result = tool.prepare_workspaces(&[task(Some("created")), task(Some("blocked/new"))]);
    fs::set_permissions(
        root.path().join("blocked"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert!(result.is_err());
    assert!(root.path().join("created").is_dir());
    assert!(
        channels
            .launcher
            .reserve_workspaces(&[root.path().join("created"), root.path().join("blocked/new")])
            .is_ok()
    );
}

#[tokio::test]
async fn enqueue_failure_waits_for_accepted_children_and_preserves_completed_report() {
    let (root, tool, mut channels) = fixture(1);
    let mut context = context(CancellationToken::new(), true);
    let mut call = Box::pin(tool.call(
        &mut context,
        batch(vec![task(Some("a")), task(Some("b")), task(Some("c"))]),
    ));
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let first = channels.requests.recv().await.unwrap();
    channels.requests.close();
    std::future::poll_fn(|cx| {
        assert!(
            call.as_mut().poll(cx).is_pending(),
            "enqueue errors cannot discard accepted workers"
        );
        Poll::Ready(())
    })
    .await;
    assert!(
        channels
            .launcher
            .reserve_workspace(root.path().join("a"))
            .is_err()
    );
    first
        .outcome
        .send(SubtaskOutcome::Completed {
            report: "COMPLETED_AFTER_ENQUEUE_FAILURE".into(),
        })
        .unwrap();
    drop(first.workspace);
    assert!(
        call.await
            .unwrap_err()
            .to_string()
            .contains("COMPLETED_AFTER_ENQUEUE_FAILURE")
    );
    let entries = context.result::<ToolResultDetail>().unwrap().subtasks();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].status, SubtaskStatus::Completed);
    assert!(entries[0].launch.is_some());
    assert!(
        entries[1..]
            .iter()
            .all(|entry| entry.launch.is_none() && entry.status == SubtaskStatus::Failed)
    );
    assert!(context.result::<ToolCancelled>().is_none());
    assert!(
        channels
            .launcher
            .reserve_workspaces(&[
                root.path().join("a"),
                root.path().join("b"),
                root.path().join("c")
            ])
            .is_ok()
    );
}

#[tokio::test]
async fn failed_first_child_does_not_end_batch_until_second_reports() {
    let (_, tool, mut channels) = fixture(2);
    let mut context = context(CancellationToken::new(), false);
    let mut call = Box::pin(tool.call(&mut context, batch(vec![task(None), task(None)])));
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let first = channels.requests.recv().await.unwrap();
    let second = channels.requests.recv().await.unwrap();
    first
        .outcome
        .send(SubtaskOutcome::Failed {
            error: "FIRST_ERROR".into(),
        })
        .unwrap();
    std::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    second
        .outcome
        .send(SubtaskOutcome::Completed {
            report: "LATE_SUCCESS".into(),
        })
        .unwrap();
    let output = call.await.unwrap_err().to_string();
    assert!(output.contains("FIRST_ERROR"));
    assert!(output.contains("LATE_SUCCESS"));
}
