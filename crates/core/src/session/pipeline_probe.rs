//! Admission work counters scoped to the polled task, never global timing data.
use std::cell::Cell;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PipelineCounts {
    pub instruction_snapshots: usize,
    pub instruction_updates: usize,
    pub prompt_preparations: usize,
    pub plan_prefix_replays: usize,
    pub orchestration_checks: usize,
    pub projections: usize,
    pub measurements: usize,
    pub replay_installations: usize,
}

tokio::task_local! {
    static COUNTS: Cell<PipelineCounts>;
}

pub(super) fn record(update: impl FnOnce(&mut PipelineCounts)) {
    let _ = COUNTS.try_with(|counts| {
        let mut current = counts.get();
        update(&mut current);
        counts.set(current);
    });
}

#[cfg(test)]
pub(super) async fn measure<T>(run: impl std::future::Future<Output = T>) -> (T, PipelineCounts) {
    COUNTS
        .scope(Cell::new(PipelineCounts::default()), async {
            let result = run.await;
            (result, COUNTS.with(Cell::get))
        })
        .await
}
