use crate::*;
#[tokio::test]
async fn ensemble_router_is_bounded_and_never_retargets_delayed_controls() {
    let router = WorkerControlRouter::default();
    let run_id = EnsembleRunId::new();
    let (registration, mut rx) = router.register(run_id.clone(), TurnId::new(2));
    let control = WorkerControl {
        request_id: WorkerControlId::new(),
        target: WorkerControlTarget {
            turn_id: TurnId::new(2),
            run_id,
            worker_id: AgentRunId::new(),
        },
        action: WorkerControlAction::Retry,
    };
    router.route(control.clone()).unwrap();
    assert_eq!(rx.recv().await, Some(control.clone()));
    drop(registration);
    let (_next, _receiver) = router.register(EnsembleRunId::new(), TurnId::new(3));
    assert!(router.route(control).is_err());
}
