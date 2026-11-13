//! Idle preparation shares the engine task and loses to accepted commands.
use super::*;

struct PrewarmGuard {
    active: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}
impl Drop for PrewarmGuard {
    fn drop(&mut self) {
        assert_eq!(self.active.fetch_sub(1, Ordering::SeqCst), 1);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct PrewarmingProvider {
    inner: GatedProvider,
    started: mpsc::UnboundedSender<ModelRole>,
    pending: bool,
    active: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}
impl ModelProvider for PrewarmingProvider {
    fn prewarm(&mut self, role: ModelRole) -> zevria_session_api::PrewarmFuture<'_> {
        Box::pin(async move {
            assert_eq!(self.inner.in_flight.load(Ordering::SeqCst), 0);
            assert_eq!(self.active.fetch_add(1, Ordering::SeqCst), 0);
            let _guard = PrewarmGuard {
                active: self.active.clone(),
                dropped: self.dropped.clone(),
            };
            self.started.send(role).unwrap();
            if self.pending {
                std::future::pending::<()>().await;
            }
        })
    }
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        assert_eq!(
            self.active.load(Ordering::SeqCst),
            0,
            "idle work must be dropped before dispatch"
        );
        self.inner.complete(request, progress)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn cancel(&mut self) {
        self.inner.cancel();
    }
}

#[tokio::test]
async fn pending_prewarm_yields_to_mode_turn_and_shutdown_without_detached_work() {
    let directory = tempfile::tempdir().unwrap();
    let (inner, mut calls) = GatedProvider::new();
    let (started, mut idle) = mpsc::unbounded_channel();
    let active = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let provider = PrewarmingProvider {
        inner,
        started,
        pending: true,
        active: active.clone(),
        dropped: dropped.clone(),
    };
    let engine = engine(
        provider,
        TranscriptWriter::create(directory.path()).unwrap(),
    )
    .with_mode_management();
    let (events, receiver) = session_event_channel(32);
    let mut session = RunningSession::start(engine, [], events, receiver);
    assert_eq!(
        tokio::time::timeout(TIMEOUT, idle.recv()).await.unwrap(),
        Some(ModelRole::Build)
    );
    session.send(SessionCommand::Manage(
        crate::session::ManagementCommand::SetMode {
            request_id: "plan".into(),
            mode: SessionMode::Plan,
        },
    ));
    session.until(|event| matches!(event, SessionEvent::ModeResult { request_id, .. } if request_id == "plan")).await;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, idle.recv()).await.unwrap(),
        Some(ModelRole::Plan)
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    session.send(submit("ordinary work wins"));
    let call = session.call(&mut calls).await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    answer(call, "done");
    session.completed(TurnId::new(1)).await;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, idle.recv()).await.unwrap(),
        Some(ModelRole::Build)
    );
    session.shutdown().await;
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn queued_work_wins_and_completed_prewarm_does_not_spin_in_an_idle_period() {
    let directory = tempfile::tempdir().unwrap();
    let (inner, mut calls) = GatedProvider::new();
    let (started, mut idle) = mpsc::unbounded_channel();
    let provider = PrewarmingProvider {
        inner,
        started,
        pending: false,
        active: Arc::new(AtomicUsize::new(0)),
        dropped: Arc::new(AtomicUsize::new(0)),
    };
    let engine = engine(
        provider,
        TranscriptWriter::create(directory.path()).unwrap(),
    );
    let (events, receiver) = session_event_channel(32);
    let mut session = RunningSession::start(engine, [submit("already queued")], events, receiver);
    let call = session.call(&mut calls).await;
    assert!(
        idle.try_recv().is_err(),
        "queued commands must be polled before prewarm"
    );
    answer(call, "done");
    session.completed(TurnId::new(1)).await;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, idle.recv()).await.unwrap(),
        Some(ModelRole::Build)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), idle.recv())
            .await
            .is_err()
    );
    session.shutdown().await;
}
