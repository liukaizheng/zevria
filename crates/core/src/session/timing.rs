//! Low-cost lifecycle timing, including early returns and failed operations.

pub(super) struct LifecycleTiming {
    operation: &'static str,
    records: usize,
    started: std::time::Instant,
}

impl LifecycleTiming {
    pub(super) fn new(operation: &'static str, records: usize) -> Self {
        Self {
            operation,
            records,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for LifecycleTiming {
    fn drop(&mut self) {
        tracing::info!(
            target: "zevria_core::lifecycle",
            operation = self.operation,
            records = self.records,
            elapsed_us = self.started.elapsed().as_micros() as u64,
            "session lifecycle timing"
        );
    }
}
