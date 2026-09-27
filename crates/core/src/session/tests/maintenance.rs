//! The same command policy applies during model maintenance and active turns.
use super::*;

struct MaintenanceSettings;
impl zevria_model::models::ModelSettingsService for MaintenanceSettings {
    fn validate(&self, _revision: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn save(
        &self,
        _revision: &str,
        _role: ModelRole,
        _target: &zevria_model::models::ModelSelection,
    ) -> anyhow::Result<String> {
        Ok("saved".into())
    }
}

struct MaintenanceProvider {
    inner: GatedProvider,
    counts: mpsc::UnboundedSender<oneshot::Sender<InputTokenCount>>,
}
impl ModelProvider for MaintenanceProvider {
    fn model_catalog(&self) -> Vec<zevria_model::models::ModelCandidate> {
        vec![zevria_model::models::ModelCandidate {
            context: test_compaction_policy(1_000, 80, 0)
                .for_role(ModelRole::Build)
                .clone(),
            reasoning_levels: zevria_foundation::ReasoningLevel::ALL.to_vec(),
        }]
    }
    fn install_model_update(
        &mut self,
        _role: ModelRole,
        _target: &zevria_model::models::ModelSelection,
    ) {
    }
    fn preflight_input(
        &self,
        target: &ModelProfileRef,
        input: &[ModelRequestItem<'_>],
    ) -> anyhow::Result<zevria_model::models::ReplayPreflight> {
        let estimate = if input
            .iter()
            .all(|item| matches!(item, ModelRequestItem::DeveloperInstruction(_)))
        {
            // Force a cancellable preparation step, without a conversation turn.
            ContextTokenEstimate::new(900, 900)
        } else {
            estimate_model_input_for_profile(input.to_vec(), target)?
        };
        Ok(zevria_model::models::ReplayPreflight::Compatible(estimate))
    }
    fn count_profile<'a>(
        &'a mut self,
        _profile: &'a zevria_model::models::ModelSelection,
        _request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        let (send, receive) = oneshot::channel();
        self.counts.send(send).unwrap();
        Box::pin(async move { Ok(receive.await.expect("test releases count")) })
    }
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        self.inner.complete(request, progress)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn cancel(&mut self) {
        self.inner.cancel();
    }
}

async fn start_maintenance() -> (
    tempfile::TempDir,
    RunningSession,
    oneshot::Sender<InputTokenCount>,
    mpsc::UnboundedReceiver<GatedCall>,
) {
    let (directory, transcript) = test_transcript();
    let (inner, calls) = GatedProvider::new();
    let (counts, mut counted) = mpsc::unbounded_channel();
    let mut engine = engine(MaintenanceProvider { inner, counts }, transcript)
        .with_compaction_policy(test_compaction_policy(1_000, 80, 0))
        .with_model_management(Arc::new(MaintenanceSettings), "revision".into());
    engine.policies = test_policies_for_tools(&[]);
    engine
        .conversation
        .replace_session_models(
            zevria_model::models::SessionModels::new(
                zevria_model::models::ModelSelection::new(
                    test_profile(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                zevria_model::models::ModelSelection::new(
                    test_profile(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
            )
            .unwrap(),
        )
        .unwrap();
    engine.refresh_replay().unwrap();
    let (events, receiver) = session_event_channel(32);
    let run = RunningSession::start(
        engine,
        [SessionCommand::Manage(ManagementCommand::Models {
            request_id: "maintenance".into(),
            request: ModelManagementRequest::Select {
                scope: Scope::SessionOnly,
                mode: SessionMode::Build,
                target: zevria_model::models::ModelSelection::new(
                    test_profile(),
                    zevria_foundation::ReasoningLevel::Medium,
                ),
                revision: "revision".into(),
            },
        })],
        events,
        receiver,
    );
    let count = tokio::time::timeout(TIMEOUT, counted.recv())
        .await
        .unwrap()
        .unwrap();
    (directory, run, count, calls)
}

#[tokio::test]
async fn turn_commands_queue_behind_model_maintenance_and_run_fifo() {
    let (_directory, mut run, count, mut calls) = start_maintenance().await;
    run.send(submit("first queued"));
    run.send(submit("second queued"));
    run.busy_fence("models-remain-idle-only").await;
    run.idle_fence("skills-query-is-live").await;
    assert!(calls.try_recv().is_err());
    assert!(!count.is_closed(), "queries must not cancel maintenance");
    count.send(InputTokenCount::Exact(100)).unwrap();
    run.until(|event| matches!(event, SessionEvent::ModelsResult { request_id, result: ModelManagementResult::Changed { .. } } if request_id == "maintenance")).await;
    for (id, prompt) in [(1, "first queued"), (2, "second queued")] {
        let call = run.call(&mut calls).await;
        assert_call(&call, id, prompt);
        answer(call, "done");
        run.completed(TurnId::new(id)).await;
    }
    let lifecycle = run.shutdown().await;
    assert_turns_do_not_overlap(&lifecycle, &[1, 2]);
    assert!(!lifecycle.iter().any(|event| matches!(
        event,
        SessionEvent::TurnRejected { .. } | SessionEvent::TurnFailed { .. }
    )));
}

#[tokio::test]
async fn queued_cancellation_takes_effect_before_ready_maintenance_is_polled() {
    for cancellation in [
        SessionCommand::Control(ControlCommand::CancelTurn { turn_id: None }),
        SessionCommand::Manage(ManagementCommand::Models {
            request_id: "maintenance".into(),
            request: ModelManagementRequest::Cancel,
        }),
    ] {
        let (_directory, transcript) = test_transcript();
        let engine = engine(ScriptedProvider::new([]), transcript)
            .with_model_management(Arc::new(MaintenanceSettings), "revision".into());
        let (events, receiver) = session_event_channel(32);
        let mut run = RunningSession::start(
            engine,
            [
                SessionCommand::Manage(ManagementCommand::Models {
                    request_id: "maintenance".into(),
                    request: ModelManagementRequest::List {
                        scope: Scope::SessionOnly,
                        mode: SessionMode::Build,
                    },
                }),
                cancellation,
            ],
            events,
            receiver,
        );
        let reply = run
            .until(|event| {
                matches!(event, SessionEvent::ModelsResult { request_id, .. } if request_id == "maintenance")
            })
            .await;
        assert_eq!(
            reply,
            SessionEvent::ModelsResult {
                request_id: "maintenance".into(),
                result: ModelManagementResult::Cancelled,
            }
        );
        assert_eq!(run.shutdown().await, vec![reply]);
    }
}

#[tokio::test]
async fn only_untargeted_turn_cancellation_cancels_maintenance() {
    let (_directory, mut run, count, mut calls) = start_maintenance().await;
    run.send(SessionCommand::Control(ControlCommand::CancelTurn {
        turn_id: Some(TurnId::new(1)),
    }));
    run.idle_fence("targeted-cancel-handled").await;
    assert!(!count.is_closed());
    run.send(submit("after cancelled maintenance"));
    run.send(SessionCommand::Control(ControlCommand::CancelTurn {
        turn_id: None,
    }));
    run.until(|event| matches!(event, SessionEvent::ModelsResult { request_id, result: ModelManagementResult::Cancelled } if request_id == "maintenance")).await;
    assert!(count.is_closed(), "maintenance count future was dropped");
    let call = run.call(&mut calls).await;
    assert_call(&call, 1, "after cancelled maintenance");
    answer(call, "done");
    run.completed(TurnId::new(1)).await;
    assert_turns_do_not_overlap(&run.shutdown().await, &[1]);
}
