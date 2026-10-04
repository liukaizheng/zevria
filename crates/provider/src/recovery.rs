//! Completion-scoped recovery. Bounds silence, never total useful work.

use std::{future::Future, time::Duration};
use tokio::time::Instant;
use zevria_session_api::{
    ProgressReporter,
    event::{NetworkStatus, NetworkTransport},
};

use crate::config::NetworkConfig;

#[derive(Debug, Clone)]
pub(crate) struct RecoveryPolicy {
    pub(crate) max_attempts: usize,
    pub(crate) connect_timeout: Duration,
    pub(crate) send_timeout: Duration,
    pub(crate) stall_warning: Duration,
    pub(crate) response_idle_timeout: Duration,
    pub(crate) backoff_base: Duration,
    pub(crate) backoff_cap: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self::from(&NetworkConfig::default())
    }
}

impl From<&NetworkConfig> for RecoveryPolicy {
    fn from(config: &NetworkConfig) -> Self {
        Self {
            max_attempts: config.max_attempts,
            connect_timeout: Duration::from_secs(config.connect_timeout_seconds),
            send_timeout: Duration::from_secs(config.request_start_timeout_seconds),
            stall_warning: Duration::from_secs(config.stall_warning_seconds),
            response_idle_timeout: Duration::from_secs(config.response_idle_timeout_seconds),
            backoff_base: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(8),
        }
    }
}

impl RecoveryPolicy {
    /// Capped exponential backoff with 75–100% jitter. Calculate once per retry:
    /// the event and sleep must use exactly the same duration.
    pub(crate) fn backoff(&self, retry: usize) -> Duration {
        let entropy = uuid::Uuid::new_v4().as_u128() as u32;
        self.backoff_with_sample(retry, entropy)
    }

    fn backoff_with_sample(&self, retry: usize, sample: u32) -> Duration {
        let shift = u32::try_from(retry.saturating_sub(1)).unwrap_or(u32::MAX);
        let capped = self
            .backoff_base
            .saturating_mul(2u32.saturating_pow(shift))
            .min(self.backoff_cap);
        capped.mul_f64(0.75 + 0.25 * (f64::from(sample) / f64::from(u32::MAX)))
    }
}

pub(crate) struct AttemptBudget {
    used: usize,
    limit: usize,
}

impl AttemptBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }

    pub(crate) fn start(&mut self) -> Option<usize> {
        if self.used >= self.limit {
            return None;
        }
        self.used += 1;
        Some(self.used)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.limit - self.used
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AttemptProgress<'a> {
    pub(crate) reporter: &'a ProgressReporter,
    pub(crate) attempt: usize,
    pub(crate) max_attempts: usize,
    pub(crate) transport: NetworkTransport,
}

impl AttemptProgress<'_> {
    pub(crate) async fn status(&self, status: NetworkStatus) {
        self.reporter
            .network_status(self.attempt, self.max_attempts, self.transport, status)
            .await;
    }
}

#[derive(Debug)]
pub(crate) struct ResponseIdleTimeout;

impl std::fmt::Display for ResponseIdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "response_idle_timeout: no meaningful response progress before the inactivity deadline",
        )
    }
}
impl std::error::Error for ResponseIdleTimeout {}

pub(crate) struct ProgressClock {
    last_progress: Instant,
    warning: Duration,
    idle: Duration,
    warned: bool,
    observed: bool,
}

impl ProgressClock {
    pub(crate) fn new(policy: &RecoveryPolicy) -> Self {
        Self {
            last_progress: Instant::now(),
            warning: policy.stall_warning,
            idle: policy.response_idle_timeout,
            warned: false,
            observed: false,
        }
    }

    pub(crate) async fn advanced(&mut self, progress: AttemptProgress<'_>) {
        self.last_progress = Instant::now();
        if self.warned || !self.observed {
            progress.status(NetworkStatus::ProgressResumed).await;
        }
        self.warned = false;
        self.observed = true;
    }

    /// Pin the parser/read future across the warning; dropping and recreating
    /// it there could discard a partially read SSE frame. Absolute deadlines
    /// and timer-first selection also bound streams of ignored ready events.
    pub(crate) async fn next<F: Future>(
        &mut self,
        next: F,
        progress: AttemptProgress<'_>,
    ) -> Result<F::Output, ResponseIdleTimeout> {
        tokio::pin!(next);
        loop {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(self.last_progress + self.idle) => {
                    tracing::warn!(attempt = progress.attempt, transport = ?progress.transport,
                        phase = "response", timeout_category = "response_idle_timeout",
                        idle_ms = self.last_progress.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        "response progress deadline expired");
                    return Err(ResponseIdleTimeout);
                },
                () = tokio::time::sleep_until(self.last_progress + self.warning), if !self.warned => {
                    self.warned = true;
                    let idle_for = self.last_progress.elapsed();
                    tracing::info!(attempt = progress.attempt, transport = ?progress.transport,
                        phase = "response", idle_ms = idle_for.as_millis().min(u64::MAX as u128) as u64,
                        "response progress is quiet; automatic recovery remains armed");
                    progress.status(NetworkStatus::Quiet {
                        idle_for,
                        retry_in: self.idle.saturating_sub(idle_for),
                    }).await;
                }
                value = &mut next => return Ok(value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reporter() -> (
        ProgressReporter,
        zevria_session_api::event::SessionEventReceiver,
    ) {
        let (sender, receiver) = zevria_session_api::session_event_channel(32);
        (ProgressReporter::new(sender), receiver)
    }

    #[tokio::test(start_paused = true)]
    async fn warns_once_then_expires_at_absolute_idle_deadline() {
        use zevria_session_api::{SessionEvent, SessionUpdate};
        let (reporter, mut events) = reporter();
        let waiting = tokio::spawn(async move {
            let mut clock = ProgressClock::new(&RecoveryPolicy::default());
            let progress = AttemptProgress {
                reporter: &reporter,
                attempt: 1,
                max_attempts: 4,
                transport: NetworkTransport::Http,
            };
            clock.next(std::future::pending::<()>(), progress).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            matches!(events.recv().await, Some(SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
            attempt: 1, status: NetworkStatus::Quiet { idle_for, retry_in }, ..
        })) if idle_for == Duration::from_secs(30) && retry_in == Duration::from_secs(150))
        );
        tokio::time::advance(Duration::from_secs(149)).await;
        assert!(!waiting.is_finished());
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(waiting.await.unwrap().is_err());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn warning_preserves_pending_read_and_progress_rearms_watchdog() {
        use zevria_session_api::{SessionEvent, SessionUpdate};
        let (reporter, mut events) = reporter();
        let (send, recv) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            let mut clock = ProgressClock::new(&RecoveryPolicy::default());
            let progress = AttemptProgress {
                reporter: &reporter,
                attempt: 2,
                max_attempts: 4,
                transport: NetworkTransport::WebSocket,
            };
            assert_eq!(
                clock
                    .next(async { recv.await.unwrap() }, progress)
                    .await
                    .unwrap(),
                "partial frame completed"
            );
            clock.advanced(progress).await;
            // More than 180s total is healthy: silence alone owns the deadline.
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_secs(25)).await;
                clock.next(std::future::ready(()), progress).await.unwrap();
                clock.advanced(progress).await;
            }
            clock.next(std::future::pending::<()>(), progress).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(matches!(
            events.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                status: NetworkStatus::Quiet { .. },
                ..
            }))
        ));
        send.send("partial frame completed").unwrap();
        assert!(matches!(
            events.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                status: NetworkStatus::ProgressResumed,
                ..
            }))
        ));
        for _ in 0..8 {
            tokio::time::advance(Duration::from_secs(25)).await;
            tokio::task::yield_now().await;
        }
        assert!(!waiting.is_finished());
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(matches!(
            events.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::NetworkStatus {
                status: NetworkStatus::Quiet { .. },
                ..
            }))
        ));
        tokio::time::advance(Duration::from_secs(150)).await;
        assert!(waiting.await.unwrap().is_err());
    }

    #[test]
    fn shared_budget_and_jitter_are_bounded() {
        let mut budget = AttemptBudget::new(4);
        for attempt in 1..=4 {
            assert_eq!(budget.start(), Some(attempt));
        }
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.start(), None);
        let policy = RecoveryPolicy::default();
        for retry in [1, 2, 4, usize::MAX] {
            let low = policy.backoff_with_sample(retry, 0);
            let high = policy.backoff_with_sample(retry, u32::MAX);
            assert!(low <= high && high <= policy.backoff_cap);
            assert_eq!(low, high.mul_f64(0.75));
        }
    }
}
