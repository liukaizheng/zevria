//! Snapshot construction is rate-limited, not just channel delivery.
use std::{future::Future, time::Duration};
use tokio::time::Instant;

const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(30);

#[derive(Default)]
pub(crate) struct StreamCadence {
    last: Option<Instant>,
    dirty: bool,
}

impl StreamCadence {
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn take_due(&mut self, terminal: bool) -> bool {
        if !self.dirty
            || (!terminal
                && self
                    .last
                    .is_some_and(|last| last.elapsed() < SNAPSHOT_INTERVAL))
        {
            return false;
        }
        self.dirty = false;
        self.last = Some(Instant::now());
        true
    }

    /// Pin the network read across snapshot ticks: cancelling/recreating an
    /// SSE parser future can discard partially read frames.
    pub(crate) async fn next<F: Future>(
        &mut self,
        next: F,
        mut publish: impl FnMut(),
    ) -> F::Output {
        tokio::pin!(next);
        loop {
            if !self.dirty {
                return next.await;
            }
            let deadline = self
                .last
                .map_or_else(Instant::now, |last| last + SNAPSHOT_INTERVAL);
            tokio::select! {
                biased;
                result = &mut next => return result,
                () = tokio::time::sleep_until(deadline) => {
                    if self.take_due(false) { publish(); }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn bursts_are_throttled_but_terminal_and_quiet_tail_are_flushed() {
        let mut cadence = StreamCadence::default();
        cadence.mark_dirty();
        assert!(cadence.take_due(false));
        for _ in 0..1000 {
            cadence.mark_dirty();
            assert!(!cadence.take_due(false));
        }
        let mut snapshots = 0;
        cadence
            .next(tokio::time::sleep(Duration::from_millis(40)), || {
                snapshots += 1
            })
            .await;
        assert_eq!(
            snapshots, 1,
            "dirty content is delivered even without another frame"
        );
        cadence.mark_dirty();
        assert!(
            cadence.take_due(true),
            "terminal events bypass the interval"
        );
        assert!(!cadence.take_due(true));
    }
}
