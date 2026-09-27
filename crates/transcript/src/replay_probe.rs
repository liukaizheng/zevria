//! Feature-gated replay probes. Async scopes install their counters only while
//! polling, so task migration and interleaved admissions cannot mix counts.
//! No counters or probe branches are compiled into production builds.
use std::cell::Cell;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InstructionReplayCounts {
    pub full_replays: usize,
    pub full_records: usize,
    pub pin_replays: usize,
    pub pin_records: usize,
    pub suffix_applications: usize,
    pub suffix_records: usize,
    pub proposals: usize,
    pub validations: usize,
    pub header_scans: usize,
}

thread_local! {
    static COUNTS: Cell<Option<InstructionReplayCounts>> = const { Cell::new(None) };
}

pub(crate) fn record(update: impl FnOnce(&mut InstructionReplayCounts)) {
    COUNTS.with(|counts| {
        if let Some(mut current) = counts.get() {
            update(&mut current);
            counts.set(Some(current));
        }
    });
}

/// Measure one synchronous scope. Nested scopes and unwinding restore the
/// caller's probe; unrelated test threads and fixture setup are never counted.
pub fn measure_instruction_replay<T>(run: impl FnOnce() -> T) -> (T, InstructionReplayCounts) {
    struct Restore(Option<InstructionReplayCounts>);
    impl Drop for Restore {
        fn drop(&mut self) {
            COUNTS.set(self.0);
        }
    }
    let _restore = Restore(COUNTS.replace(Some(InstructionReplayCounts::default())));
    let result = run();
    (result, COUNTS.get().expect("active replay probe"))
}

/// Poll-local scope: no thread-local state or replay borrow survives an await.
pub async fn measure_instruction_replay_async<T>(
    run: impl std::future::Future<Output = T>,
) -> (T, InstructionReplayCounts) {
    let mut run = std::pin::pin!(run);
    let mut total = InstructionReplayCounts::default();
    let result = std::future::poll_fn(|cx| {
        let ((result, accumulated), _) = measure_instruction_replay(|| {
            COUNTS.set(Some(total));
            let result = run.as_mut().poll(cx);
            (result, COUNTS.get().expect("active replay probe"))
        });
        total = accumulated;
        result
    })
    .await;
    (result, total)
}
