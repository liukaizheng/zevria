//! Low-cost lifecycle timing, including early returns and failed operations.

use std::time::{Duration, Instant};
use zevria_foundation::TurnId;

#[derive(Clone, Copy)]
pub(in crate::session) enum RoundPhase {
    Prepare,
    Model,
    CommitResponse,
    Tools,
    CommitResults,
}

/// One summary per dispatched model call. Dropping a cancelled/failed round
/// accounts for its current phase too, without changing any publication path.
pub(in crate::session) struct RoundTimings {
    turn: TurnId,
    call: usize,
    items: usize,
    tool_calls: usize,
    phase: RoundPhase,
    phase_started: Instant,
    durations: [Duration; 5],
    dispatched: bool,
}

impl RoundTimings {
    pub(in crate::session) fn new(turn: TurnId, call: usize, items: usize) -> Self {
        Self {
            turn,
            call,
            items,
            tool_calls: 0,
            phase: RoundPhase::Prepare,
            phase_started: Instant::now(),
            durations: [Duration::ZERO; 5],
            dispatched: false,
        }
    }

    pub(in crate::session) fn advance(&mut self, phase: RoundPhase) {
        self.advance_at(phase, Instant::now());
    }

    fn advance_at(&mut self, phase: RoundPhase, now: Instant) {
        self.durations[self.phase as usize] += now.duration_since(self.phase_started);
        self.phase_started = now;
        self.phase = phase;
        self.dispatched |= matches!(phase, RoundPhase::Model);
    }

    pub(in crate::session) fn tool_calls(&mut self, calls: usize) {
        self.tool_calls = calls;
    }
}

impl Drop for RoundTimings {
    fn drop(&mut self) {
        if !self.dispatched {
            return;
        }
        self.advance(self.phase);
        let [prepare, model, commit_response, tools, commit_results] = self.durations;
        tracing::info!(
            target: "zevria_core::round",
            turn = self.turn.get(),
            call = self.call,
            items = self.items,
            prepare_us = prepare.as_micros() as u64,
            model_ms = model.as_millis() as u64,
            commit_response_us = commit_response.as_micros() as u64,
            tool_calls = self.tool_calls,
            tools_ms = tools.as_millis() as u64,
            commit_results_us = commit_results.as_micros() as u64,
            "model round summary"
        );
    }
}

pub(in crate::session) struct LifecycleTiming {
    operation: &'static str,
    records: usize,
    started: std::time::Instant,
}

impl LifecycleTiming {
    pub(in crate::session) fn new(operation: &'static str, records: usize) -> Self {
        Self {
            operation,
            records,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for LifecycleTiming {
    fn drop(&mut self) {
        tracing::debug!(
            target: "zevria_core::lifecycle",
            operation = self.operation,
            records = self.records,
            elapsed_us = self.started.elapsed().as_micros() as u64,
            "session lifecycle timing"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_accounts_each_phase_once_with_no_tool_time_for_a_final() {
        let mut round = RoundTimings::new(TurnId::new(1), 2, 100);
        let start = round.phase_started;
        round.advance_at(RoundPhase::Model, start + Duration::from_micros(25));
        round.advance_at(
            RoundPhase::CommitResponse,
            start + Duration::from_micros(2025),
        );
        round.advance_at(
            RoundPhase::CommitResponse,
            start + Duration::from_micros(2075),
        );
        assert!(round.dispatched);
        assert_eq!(
            round.durations,
            [
                Duration::from_micros(25),
                Duration::from_millis(2),
                Duration::from_micros(50),
                Duration::ZERO,
                Duration::ZERO
            ]
        );
        // The synthetic clock is ahead of the real clock; suppress drop-time accounting.
        round.dispatched = false;
    }

    #[test]
    fn tool_and_result_commit_have_distinct_buckets() {
        let mut round = RoundTimings::new(TurnId::new(1), 1, 10);
        let start = round.phase_started;
        round.advance_at(RoundPhase::Model, start);
        round.advance_at(RoundPhase::CommitResponse, start);
        round.advance_at(RoundPhase::Tools, start + Duration::from_micros(20));
        round.tool_calls(3);
        round.advance_at(
            RoundPhase::CommitResults,
            start + Duration::from_micros(3020),
        );
        round.advance_at(
            RoundPhase::CommitResults,
            start + Duration::from_micros(3060),
        );
        assert_eq!(round.tool_calls, 3);
        assert_eq!(
            round.durations[RoundPhase::CommitResponse as usize],
            Duration::from_micros(20)
        );
        assert_eq!(
            round.durations[RoundPhase::Tools as usize],
            Duration::from_millis(3)
        );
        assert_eq!(
            round.durations[RoundPhase::CommitResults as usize],
            Duration::from_micros(40)
        );
        round.dispatched = false;
    }
}
