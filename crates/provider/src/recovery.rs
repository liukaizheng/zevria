//! Completion-scoped recovery. Bounds silence, never total useful work.

use std::{future::Future, sync::Mutex, time::Duration};
use tokio::time::Instant;
use zevria_session_api::{
    ProgressReporter,
    event::{NetworkStatus, NetworkTransport},
};

use crate::{config::NetworkConfig, http::diagnostic_detail};

pub(crate) const MAX_INTERRUPTED_RESPONSES: usize = 4;

#[derive(Debug, Clone)]
pub(crate) struct RecoveryPolicy {
    pub(crate) max_attempts: usize,
    pub(crate) recovery_window: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) send_timeout: Duration,
    pub(crate) stall_warning: Duration,
    pub(crate) response_idle_timeout: Duration,
    pub(crate) websocket_keepalive: Duration,
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
            recovery_window: Duration::from_secs(config.recovery_window_seconds),
            connect_timeout: Duration::from_secs(config.connect_timeout_seconds),
            send_timeout: Duration::from_secs(config.request_start_timeout_seconds),
            stall_warning: Duration::from_secs(config.stall_warning_seconds),
            response_idle_timeout: Duration::from_secs(config.response_idle_timeout_seconds),
            websocket_keepalive: Duration::from_secs(config.websocket_keepalive_seconds),
            backoff_base: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(15),
        }
    }
}

impl RecoveryPolicy {
    /// Capped exponential backoff with 75–100% jitter. Calculate once per retry:
    /// the event and sleep must use exactly the same duration.
    fn backoff_with_sample(&self, retry: usize, sample: u32) -> Duration {
        let shift = u32::try_from(retry.saturating_sub(1)).unwrap_or(u32::MAX);
        let capped = self
            .backoff_base
            .saturating_mul(2u32.saturating_pow(shift))
            .min(self.backoff_cap);
        capped.mul_f64(0.75 + 0.25 * (f64::from(sample) / f64::from(u32::MAX)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    RequestStart,
    Idle,
    Connectivity,
    Server,
    RateLimited,
    Protocol,
}

/// All retry categories emitted by either adapter. HTTP 408 is a server
/// response, not a local header timeout, so it must not escalate that timer.
pub(crate) fn failure_class(category: &str) -> FailureClass {
    match category {
        "request_timeout" | "request_start_timeout" => FailureClass::RequestStart,
        "response_idle_timeout" => FailureClass::Idle,
        "request_error"
        | "transport"
        | "stream"
        | "sse_transport_error"
        | "premature_eof"
        | "connect_error"
        | "disconnect"
        | "close_frame"
        | "end_of_stream"
        | "read_error"
        | "send_error"
        | "send_timeout"
        | "pong_error"
        | "upstream_disconnect"
        | "pump_stopped"
        | "startup_connect_failed"
        | "not_connected"
        | "http_fallback"
        | "local_cancellation" => FailureClass::Connectivity,
        "server_error"
        | "overloaded"
        | "response_failed_transient"
        | "request_timeout_status"
        | "connection_limit" => FailureClass::Server,
        "rate_limited" | "rate_limit" | "rate_limit_exceeded" => FailureClass::RateLimited,
        "protocol"
        | "invalid_tool_json"
        | "stale_response_id"
        | "websocket_upgrade_required"
        | "thinking_binding_mismatch"
        | "inbound_overflow" => FailureClass::Protocol,
        _ => {
            debug_assert!(false, "unclassified retry category: {category}");
            FailureClass::Protocol
        }
    }
}

pub(crate) struct AttemptFailure {
    pub(crate) category: &'static str,
    pub(crate) class: FailureClass,
    pub(crate) error: anyhow::Error,
    pub(crate) server_delay: Option<Duration>,
    pub(crate) upstream_request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttemptPlan {
    pub(crate) attempt: usize,
    pub(crate) request_start_deadline: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExhaustedReason {
    Window,
    Interrupted,
    AttemptCap,
    ServerDelayBeyondWindow(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Retry {
        next_attempt: usize,
        delay: Duration,
        retrying_for: Duration,
    },
    Exhausted(ExhaustedReason),
}

struct RecoveryState {
    attempts: usize,
    started: Instant,
    attempt_started: Instant,
    last_progress: Option<Instant>,
    made_progress: bool,
    consecutive_failures: usize,
    interrupted: usize,
    request_start_deadline: Duration,
    next_request_start_deadline: Duration,
    jitter_sample: u32,
}

/// One controller per immutable prepared request. Interior state is synchronized
/// because provider futures are Send and attempts borrow &Recovery across awaits.
/// No lock is held across an await or while polling network IO.
pub(crate) struct Recovery<'a> {
    policy: &'a RecoveryPolicy,
    state: Mutex<RecoveryState>,
}

impl<'a> Recovery<'a> {
    pub(crate) fn new(policy: &'a RecoveryPolicy) -> Self {
        let now = Instant::now();
        Self {
            policy,
            state: Mutex::new(RecoveryState {
                attempts: 0,
                started: now,
                attempt_started: now,
                last_progress: None,
                made_progress: false,
                consecutive_failures: 0,
                interrupted: 0,
                request_start_deadline: policy.send_timeout,
                next_request_start_deadline: policy.send_timeout,
                jitter_sample: 0,
            }),
        }
    }

    pub(crate) fn start_attempt(&self) -> Option<AttemptPlan> {
        let mut state = self.state.lock().expect("recovery state");
        if state.attempts >= self.policy.max_attempts {
            return None;
        }
        let now = Instant::now();
        if state.attempts == 0 {
            state.started = now;
        }
        state.attempts += 1;
        state.attempt_started = now;
        state.made_progress = false;
        state.request_start_deadline = state.next_request_start_deadline;
        state.jitter_sample = uuid::Uuid::new_v4().as_u128() as u32;
        Some(AttemptPlan {
            attempt: state.attempts,
            request_start_deadline: state.request_start_deadline,
        })
    }

    pub(crate) fn mark_progress(&self) {
        let mut state = self.state.lock().expect("recovery state");
        state.last_progress = Some(Instant::now());
        state.made_progress = true;
        state.consecutive_failures = 0;
    }

    fn outage_since(state: &RecoveryState) -> Instant {
        state
            .last_progress
            .unwrap_or(state.started)
            .max(state.started)
    }

    fn computed_delay(&self, state: &RecoveryState, progressed: bool) -> Duration {
        let retry = if progressed {
            1
        } else {
            state.consecutive_failures.saturating_add(1)
        };
        self.policy.backoff_with_sample(retry, state.jitter_sample)
    }

    pub(crate) fn decide(&self, failure: &AttemptFailure) -> Decision {
        let mut state = self.state.lock().expect("recovery state");
        let delay = failure
            .server_delay
            .unwrap_or_else(|| self.computed_delay(&state, state.made_progress));
        if failure.class == FailureClass::RequestStart {
            state.next_request_start_deadline = state
                .request_start_deadline
                .saturating_mul(2)
                .min(self.policy.response_idle_timeout);
        }
        if state.made_progress {
            state.interrupted += 1;
            state.consecutive_failures = 0;
        } else {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        }
        if state.interrupted >= MAX_INTERRUPTED_RESPONSES {
            return Decision::Exhausted(ExhaustedReason::Interrupted);
        }
        if state.attempts >= self.policy.max_attempts {
            return Decision::Exhausted(ExhaustedReason::AttemptCap);
        }
        let remaining = self
            .policy
            .recovery_window
            .saturating_sub(Self::outage_since(&state).elapsed());
        if delay > remaining {
            return Decision::Exhausted(if failure.server_delay.is_some() {
                ExhaustedReason::ServerDelayBeyondWindow(delay)
            } else {
                ExhaustedReason::Window
            });
        }
        // Even a zero-delay retry cannot start after the outage window.
        if Self::outage_since(&state).elapsed() > self.policy.recovery_window {
            return Decision::Exhausted(ExhaustedReason::Window);
        }
        Decision::Retry {
            next_attempt: state.attempts + 1,
            delay,
            retrying_for: remaining.saturating_sub(delay),
        }
    }

    /// Predict using this attempt's sampled delay, so quiet warnings never
    /// promise a retry that its cap or known deadline would prohibit.
    pub(crate) fn will_retry_at(&self, deadline: Instant, progressed: bool) -> bool {
        let state = self.state.lock().expect("recovery state");
        let progressed = progressed || state.made_progress;
        state.attempts < self.policy.max_attempts
            && state.interrupted + usize::from(progressed) < MAX_INTERRUPTED_RESPONSES
            && deadline
                .saturating_duration_since(Self::outage_since(&state))
                .saturating_add(self.computed_delay(&state, progressed))
                <= self.policy.recovery_window
    }

    pub(crate) fn log_failure(&self, failure: &AttemptFailure, decision: Decision) {
        let state = self.state.lock().expect("recovery state");
        let next_delay = match decision {
            Decision::Retry { delay, .. } => Some(ms(delay)),
            _ => None,
        };
        tracing::warn!(
            timeout_category = failure.category, attempt = state.attempts,
            elapsed_ms = ms(state.attempt_started.elapsed()),
            upstream_request_id = ?failure.upstream_request_id.as_deref().map(crate::connection::log_identifier),
            made_progress = state.made_progress, next_delay_ms = ?next_delay,
            request_start_deadline_ms = ms(state.request_start_deadline),
            window_remaining_ms = ms(self.policy.recovery_window.saturating_sub(Self::outage_since(&state).elapsed())),
            error = %diagnostic_detail(&format!("{:#}", failure.error)),
            "completion attempt failed"
        );
    }

    pub(crate) fn log_exhausted(&self, reason: ExhaustedReason, last: &AttemptFailure) {
        let state = self.state.lock().expect("recovery state");
        tracing::warn!(reason = ?reason, timeout_category = last.category, attempt = state.attempts,
            elapsed_ms = ms(state.started.elapsed()),
            upstream_request_id = ?last.upstream_request_id.as_deref().map(crate::connection::log_identifier),
            made_progress = state.made_progress, next_delay_ms = ?Option::<u64>::None,
            request_start_deadline_ms = ms(state.request_start_deadline),
            window_remaining_ms = ms(self.policy.recovery_window.saturating_sub(Self::outage_since(&state).elapsed())),
            error = %diagnostic_detail(&format!("{:#}", last.error)), "completion recovery exhausted");
    }

    /// Core publishes Display only. Duplicate the complete cause in the outer
    /// context while retaining the original error for typed/source inspection.
    pub(crate) fn exhausted_error(
        &self,
        reason: ExhaustedReason,
        last: AttemptFailure,
        provider_key: &str,
    ) -> anyhow::Error {
        self.log_exhausted(reason, &last);
        let state = self.state.lock().expect("recovery state");
        let stop = match reason {
            ExhaustedReason::Window => "recovery window expired".to_owned(),
            ExhaustedReason::Interrupted => format!(
                "{MAX_INTERRUPTED_RESPONSES} interrupted responses (partial-generation cost cap)"
            ),
            ExhaustedReason::AttemptCap => format!(
                "absolute attempt cap ({}) reached",
                self.policy.max_attempts
            ),
            ExhaustedReason::ServerDelayBeyondWindow(delay) => format!(
                "server requested a retry delay of {}s, longer than the remaining recovery window",
                delay.as_secs_f64()
            ),
        };
        let hint = match last.class {
            FailureClass::RequestStart => format!(
                "The provider did not send response headers; check the relay or increase providers.{provider_key}.network.request_start_timeout_seconds"
            ),
            FailureClass::Idle => format!(
                "The response stopped making progress; for quiet models increase providers.{provider_key}.network.response_idle_timeout_seconds"
            ),
            FailureClass::Connectivity => "Check the network or proxy connection".to_owned(),
            FailureClass::Server | FailureClass::RateLimited => {
                "The provider is unavailable or rate limited; check provider status and quota"
                    .to_owned()
            }
            FailureClass::Protocol => {
                "Check provider compatibility and the reported protocol failure".to_owned()
            }
        };
        let detail = format!(
            "Model request failed after {} attempts in {:.1}s; recovery stopped: {stop}. Last failure [{}]: {}: {:#}. {hint}. Recovery window: providers.{provider_key}.network.recovery_window_seconds = {}. Completed local tool work has not been rerun.",
            state.attempts,
            state.started.elapsed().as_secs_f64(),
            last.category,
            last.category,
            last.error,
            self.policy.recovery_window.as_secs_f64()
        );
        last.error.context(detail)
    }
}

fn ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[derive(Clone, Copy)]
pub(crate) struct AttemptProgress<'a> {
    pub(crate) reporter: &'a ProgressReporter,
    pub(crate) recovery: &'a Recovery<'a>,
    pub(crate) attempt: usize,
    pub(crate) transport: NetworkTransport,
}

impl AttemptProgress<'_> {
    pub(crate) async fn status(&self, status: NetworkStatus) {
        self.reporter
            .network_status(self.attempt, self.transport, status)
            .await;
    }

    /// Pin the send future across the warning just like the streaming read.
    pub(crate) async fn request_start<F: Future>(
        &self,
        policy: &RecoveryPolicy,
        deadline: Duration,
        request: F,
    ) -> Result<F::Output, RequestStartTimeout> {
        let start = Instant::now();
        tokio::pin!(request);
        let mut warned = false;
        loop {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(start + deadline) => {
                    tracing::warn!(attempt = self.attempt, transport = ?self.transport,
                        phase = "request_start", timeout_category = "request_start_timeout",
                        idle_ms = ms(start.elapsed()), "response header deadline expired");
                    return Err(RequestStartTimeout);
                },
                () = tokio::time::sleep_until(start + policy.stall_warning), if !warned && policy.stall_warning < deadline => {
                    warned = true;
                    tracing::info!(attempt = self.attempt, transport = ?self.transport, phase = "request_start", idle_ms = ms(start.elapsed()), "response headers are quiet");
                    self.status(NetworkStatus::Quiet { idle_for: start.elapsed(), retry_in: deadline.saturating_sub(start.elapsed()), will_retry: self.recovery.will_retry_at(start + deadline, false) }).await;
                }
                value = &mut request => return Ok(value),
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct RequestStartTimeout;
impl std::fmt::Display for RequestStartTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request_start_timeout: no response headers before the request-start deadline")
    }
}
impl std::error::Error for RequestStartTimeout {}

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
        progress.recovery.mark_progress();
        if self.warned || !self.observed {
            progress.status(NetworkStatus::ProgressResumed).await;
        }
        self.warned = false;
        self.observed = true;
    }

    /// Pin parser/read futures across warnings: recreating them can discard
    /// partially read SSE frames. Timer-first selection bounds ignored events.
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
                    tracing::warn!(attempt = progress.attempt, transport = ?progress.transport, phase = "response", timeout_category = "response_idle_timeout", idle_ms = ms(self.last_progress.elapsed()), "response progress deadline expired");
                    return Err(ResponseIdleTimeout);
                },
                () = tokio::time::sleep_until(self.last_progress + self.warning), if !self.warned => {
                    self.warned = true;
                    let idle_for = self.last_progress.elapsed();
                    tracing::info!(attempt = progress.attempt, transport = ?progress.transport, phase = "response", idle_ms = ms(idle_for), "response progress is quiet");
                    progress.status(NetworkStatus::Quiet { idle_for, retry_in: self.idle.saturating_sub(idle_for), will_retry: progress.recovery.will_retry_at(self.last_progress + self.idle, self.observed) }).await;
                }
                value = &mut next => return Ok(value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(category: &'static str) -> AttemptFailure {
        AttemptFailure {
            category,
            class: failure_class(category),
            error: anyhow::anyhow!("test cause"),
            server_delay: None,
            upstream_request_id: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn window_limits_no_progress_and_progress_restarts_it() {
        let policy = RecoveryPolicy {
            max_attempts: 1000,
            ..RecoveryPolicy::default()
        };
        let recovery = Recovery::new(&policy);
        recovery.start_attempt().unwrap();
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(
            recovery.decide(&failure("request_error")),
            Decision::Exhausted(ExhaustedReason::Window)
        );
        recovery.mark_progress();
        let Decision::Retry {
            delay,
            retrying_for,
            ..
        } = recovery.decide(&failure("request_error"))
        else {
            panic!("progress must reset window");
        };
        assert!((Duration::from_millis(375)..=Duration::from_millis(500)).contains(&delay));
        assert_eq!(retrying_for + delay, Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn window_stops_before_scheduling_a_start_beyond_300_seconds() {
        let policy = RecoveryPolicy {
            max_attempts: 1000,
            ..RecoveryPolicy::default()
        };
        let recovery = Recovery::new(&policy);
        let started = Instant::now();
        loop {
            recovery.start_attempt().unwrap();
            recovery.state.lock().unwrap().jitter_sample = u32::MAX;
            match recovery.decide(&failure("request_error")) {
                Decision::Retry { delay, .. } => tokio::time::advance(delay).await,
                Decision::Exhausted(reason) => {
                    assert_eq!(reason, ExhaustedReason::Window);
                    assert!(started.elapsed() <= Duration::from_secs(300));
                    assert!(started.elapsed() + policy.backoff_cap > Duration::from_secs(300));
                    let error = recovery.exhausted_error(reason, failure("request_error"), "test");
                    assert!(error.to_string().contains("recovery window expired"));
                    assert!(error.to_string().contains("recovery_window_seconds = 300"));
                    break;
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_resets_exponent_and_preserves_work_beyond_the_original_window() {
        let policy = RecoveryPolicy::default();
        let recovery = Recovery::new(&policy);
        for expected in [0.5, 1.0, 2.0, 4.0, 8.0] {
            recovery.start_attempt().unwrap();
            recovery.state.lock().unwrap().jitter_sample = u32::MAX;
            assert!(
                matches!(recovery.decide(&failure("request_error")), Decision::Retry { delay, .. } if delay == Duration::from_secs_f64(expected))
            );
        }
        recovery.start_attempt().unwrap();
        recovery.state.lock().unwrap().jitter_sample = u32::MAX;
        tokio::time::advance(Duration::from_secs(600)).await;
        // The window never interrupts a running, useful attempt.
        recovery.mark_progress();
        assert!(
            matches!(recovery.decide(&failure("premature_eof")), Decision::Retry { delay, retrying_for, .. } if delay == Duration::from_millis(500) && retrying_for == Duration::from_millis(299_500))
        );
        recovery.start_attempt().unwrap();
        recovery.state.lock().unwrap().jitter_sample = u32::MAX;
        assert!(
            matches!(recovery.decide(&failure("request_error")), Decision::Retry { delay, .. } if delay == Duration::from_millis(500))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interruptions_and_attempts_are_independent_backstops() {
        let policy = RecoveryPolicy::default();
        let recovery = Recovery::new(&policy);
        for attempt in 1..=4 {
            assert_eq!(recovery.start_attempt().unwrap().attempt, attempt);
            assert_eq!(recovery.will_retry_at(Instant::now(), true), attempt < 4);
            assert!(recovery.will_retry_at(Instant::now(), false));
            recovery.mark_progress();
            let decision = recovery.decide(&failure("premature_eof"));
            if attempt == 4 {
                assert_eq!(decision, Decision::Exhausted(ExhaustedReason::Interrupted));
            } else {
                assert!(matches!(decision, Decision::Retry { .. }));
            }
        }
        let policy = RecoveryPolicy {
            max_attempts: 1,
            ..policy
        };
        let recovery = Recovery::new(&policy);
        recovery.start_attempt().unwrap();
        assert_eq!(
            recovery.decide(&failure("request_error")),
            Decision::Exhausted(ExhaustedReason::AttemptCap)
        );
        assert_eq!(recovery.start_attempt(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn header_deadlines_escalate_only_on_local_header_timeouts() {
        let policy = RecoveryPolicy::default();
        let recovery = Recovery::new(&policy);
        for seconds in [30, 60, 120, 180, 180] {
            assert_eq!(
                recovery.start_attempt().unwrap().request_start_deadline,
                Duration::from_secs(seconds)
            );
            recovery.decide(&failure("request_timeout"));
        }
        let recovery = Recovery::new(&policy);
        for category in ["request_timeout_status", "server_error", "rate_limit"] {
            assert_eq!(
                recovery.start_attempt().unwrap().request_start_deadline,
                Duration::from_secs(30)
            );
            recovery.decide(&failure(category));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hints_and_will_retry_respect_window_and_caps() {
        let policy = RecoveryPolicy::default();
        let recovery = Recovery::new(&policy);
        recovery.start_attempt().unwrap();
        assert!(recovery.will_retry_at(Instant::now() + Duration::from_secs(299), false));
        assert!(!recovery.will_retry_at(Instant::now() + Duration::from_secs(300), false));
        let mut last = failure("rate_limited");
        last.server_delay = Some(Duration::from_secs(45));
        assert!(
            matches!(recovery.decide(&last), Decision::Retry { delay, retrying_for, .. } if delay == Duration::from_secs(45) && retrying_for == Duration::from_secs(255))
        );
        last.server_delay = Some(Duration::from_secs(301));
        let decision = recovery.decide(&last);
        assert_eq!(
            decision,
            Decision::Exhausted(ExhaustedReason::ServerDelayBeyondWindow(
                Duration::from_secs(301)
            ))
        );
        let error = recovery.exhausted_error(
            ExhaustedReason::ServerDelayBeyondWindow(Duration::from_secs(301)),
            last,
            "test",
        );
        assert!(error.to_string().contains("301s"));
        let policy = RecoveryPolicy {
            max_attempts: 1,
            ..policy
        };
        let recovery = Recovery::new(&policy);
        recovery.start_attempt().unwrap();
        assert!(!recovery.will_retry_at(Instant::now(), false));
    }

    #[test]
    fn shared_budget_and_jitter_are_bounded() {
        let policy = RecoveryPolicy::default();
        assert_eq!(policy.backoff_cap, Duration::from_secs(15));
        for retry in [1, 2, 4, 6, usize::MAX] {
            let low = policy.backoff_with_sample(retry, 0);
            let high = policy.backoff_with_sample(retry, u32::MAX);
            assert!(low <= high && high <= policy.backoff_cap);
            assert_eq!(low, high.mul_f64(0.75));
        }
    }

    fn reporter() -> (
        ProgressReporter,
        zevria_session_api::event::SessionEventReceiver,
    ) {
        let (sender, receiver) = zevria_session_api::session_event_channel(32);
        (ProgressReporter::new(sender), receiver)
    }

    #[tokio::test(start_paused = true)]
    async fn warning_preserves_pending_read_and_progress_rearms_watchdog() {
        use zevria_session_api::{SessionEvent, SessionUpdate};
        let (reporter, mut events) = reporter();
        let (send, recv) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            let policy = RecoveryPolicy::default();
            let recovery = Recovery::new(&policy);
            recovery.start_attempt().unwrap();
            let mut clock = ProgressClock::new(&policy);
            let progress = AttemptProgress {
                reporter: &reporter,
                recovery: &recovery,
                attempt: 1,
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
                status: NetworkStatus::Quiet {
                    will_retry: true,
                    ..
                },
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

    #[tokio::test(start_paused = true)]
    async fn warns_once_then_expires_at_absolute_idle_deadline() {
        use zevria_session_api::{SessionEvent, SessionUpdate};
        let (reporter, mut events) = reporter();
        let waiting = tokio::spawn(async move {
            let policy = RecoveryPolicy::default();
            let recovery = Recovery::new(&policy);
            recovery.start_attempt().unwrap();
            let progress = AttemptProgress {
                reporter: &reporter,
                recovery: &recovery,
                attempt: 1,
                transport: NetworkTransport::Http,
            };
            ProgressClock::new(&policy)
                .next(std::future::pending::<()>(), progress)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            matches!(events.recv().await, Some(SessionUpdate::Lifecycle(SessionEvent::NetworkStatus { status: NetworkStatus::Quiet { idle_for, retry_in, will_retry: true }, .. })) if idle_for == Duration::from_secs(30) && retry_in == Duration::from_secs(150))
        );
        tokio::time::advance(Duration::from_secs(149)).await;
        assert!(!waiting.is_finished());
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(waiting.await.unwrap().is_err());
    }
}
