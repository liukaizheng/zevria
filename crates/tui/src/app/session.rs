//! Session lifecycle state and transition guards.

use std::time::{Duration, Instant};

use rig_core::message::Message;
use zevria_foundation::ModelProfileRef;
use zevria_foundation::ModelRole;
use zevria_foundation::SessionMode;
use zevria_foundation::TurnId;
use zevria_model::CompactionTrigger;
use zevria_model::ContextTokenSnapshot;
use zevria_model::TokenUsage;
use zevria_workflow::PlanDecision;
use zevria_workflow::PlanResolution;
use zevria_workflow::PlanVersion;
use zevria_workflow::PlanWorkflowState;

use crate::presentation::{DisplayTurn, NativeHeader};

pub(crate) const fn role_for_mode(mode: SessionMode) -> ModelRole {
    match mode {
        SessionMode::Build => ModelRole::Build,
        SessionMode::Plan => ModelRole::Plan,
    }
}

/// The frontend operation that owns a pending or active engine turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationKind {
    ModelManagement,
    ModeManagement,
    Submit,
    TranscriptEdit,
    Skill,
    Ensemble,
    ManualCompaction,
    PlanRevise {
        expected: PlanVersion,
    },
    PlanImplementCurrent {
        expected: PlanVersion,
    },
    PlanImplementFresh {
        expected: PlanVersion,
    },
    /// Work announced authoritatively by the engine without a local action.
    AuthoritativeTurn,
    AuthoritativeEnsemble,
    AuthoritativePlanHandoff,
}

impl OperationKind {
    pub(crate) const fn plan_decision(expected: PlanVersion, decision: PlanDecision) -> Self {
        match decision {
            PlanDecision::Revise => Self::PlanRevise { expected },
            PlanDecision::ImplementCurrent => Self::PlanImplementCurrent { expected },
            PlanDecision::ImplementFresh => Self::PlanImplementFresh { expected },
        }
    }

    const fn accepts(self, start: StartKind) -> bool {
        matches!(
            (self, start),
            (
                Self::Submit | Self::TranscriptEdit | Self::Skill | Self::AuthoritativeTurn,
                StartKind::Turn,
            ) | (
                Self::TranscriptEdit | Self::Ensemble | Self::AuthoritativeEnsemble,
                StartKind::Ensemble,
            ) | (
                Self::PlanImplementCurrent { .. } | Self::AuthoritativePlanHandoff,
                StartKind::PlanHandoff,
            )
        )
    }

    pub(crate) const fn is_management(self) -> bool {
        matches!(self, Self::ModelManagement | Self::ModeManagement)
    }

    pub(crate) const fn is_transcript_edit(self) -> bool {
        matches!(self, Self::TranscriptEdit)
    }

    pub(crate) const fn is_plan_decision(self) -> bool {
        matches!(
            self,
            Self::PlanRevise { .. }
                | Self::PlanImplementCurrent { .. }
                | Self::PlanImplementFresh { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingOperation {
    pub(crate) started_at: Instant,
    pub(crate) kind: OperationKind,
    pub(crate) mode: SessionMode,
    pub(crate) role: ModelRole,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveOperation {
    pub(crate) started_at: Instant,
    pub(crate) id: TurnId,
    /// Raw dispatch marker, used only to reject duplicate/stale events.
    pub(crate) model_call: Option<usize>,
    pub(crate) display_turn: Option<DisplayTurn>,
    pub(crate) call_header: Option<NativeHeader>,
    pub(crate) kind: OperationKind,
    pub(crate) mode: SessionMode,
    pub(crate) role: ModelRole,
    pub(crate) phase: ActivePhase,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ActivePhase {
    AwaitingStart,
    Running(TurnTail),
    Compacting { trigger: CompactionTrigger },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnTail {
    Waiting,
    Streaming(Message),
    Retrying(RetryNotice),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetryNotice {
    pub(crate) attempt: usize,
    pub(crate) max_attempts: usize,
    pub(crate) error: String,
    pub(crate) retry_after: Duration,
    pub(crate) received_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetryCountdown {
    Immediate,
    Pending(Duration),
    Elapsed,
}

impl RetryNotice {
    pub(crate) fn countdown(&self, now: Instant) -> RetryCountdown {
        if self.retry_after.is_zero() {
            return RetryCountdown::Immediate;
        }
        let remaining = self
            .retry_after
            .saturating_sub(now.saturating_duration_since(self.received_at));
        if remaining.is_zero() {
            RetryCountdown::Elapsed
        } else {
            RetryCountdown::Pending(remaining)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SessionActivity {
    Idle,
    Pending(PendingOperation),
    Active(ActiveOperation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartKind {
    Turn,
    Ensemble,
    PlanHandoff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartTransition {
    Pending(OperationKind),
    Awaiting(OperationKind),
    Authoritative(OperationKind),
}

impl StartTransition {
    pub(crate) const fn operation(self) -> OperationKind {
        match self {
            Self::Pending(kind) | Self::Awaiting(kind) | Self::Authoritative(kind) => kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompactionTransition {
    pub(crate) kind: OperationKind,
    pub(crate) trigger: CompactionTrigger,
}

/// Immutable configured profile fallbacks shared by every pane in one UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfiguredModelProfiles {
    profiles: [Option<ModelProfileRef>; ModelRole::COUNT],
}

impl Default for ConfiguredModelProfiles {
    fn default() -> Self {
        Self {
            profiles: std::array::from_fn(|_| None),
        }
    }
}

impl ConfiguredModelProfiles {
    pub(crate) fn from_iter(
        profiles: impl IntoIterator<Item = (ModelRole, ModelProfileRef)>,
    ) -> Self {
        let mut configured = Self::default();
        for (role, profile) in profiles {
            configured.profiles[role.index()] = Some(profile);
        }
        configured
    }

    pub(crate) fn set(&mut self, role: ModelRole, profile: ModelProfileRef) {
        self.profiles[role.index()] = Some(profile);
    }

    pub(crate) fn get(&self, role: ModelRole) -> Option<&ModelProfileRef> {
        self.profiles[role.index()].as_ref()
    }
}

/// Latest response accounting and limits supplied by one authoritative event.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResponseUsageSnapshot {
    profile: ModelProfileRef,
    usage: TokenUsage,
    input_token_limit: u64,
    context_window_tokens: u64,
}

impl ResponseUsageSnapshot {
    pub(crate) const fn usage(&self) -> TokenUsage {
        self.usage
    }

    #[cfg(test)]
    pub(crate) fn profile(&self) -> &ModelProfileRef {
        &self.profile
    }

    #[cfg(test)]
    pub(crate) const fn limits(&self) -> (u64, u64) {
        (self.input_token_limit, self.context_window_tokens)
    }
}

/// Event-authoritative model identity and accounting for one model role.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RoleTelemetry {
    profile: Option<ModelProfileRef>,
    response: Option<ResponseUsageSnapshot>,
    context: Option<ContextTokenSnapshot>,
}

impl RoleTelemetry {
    pub(crate) fn authoritative_profile(&self) -> Option<&ModelProfileRef> {
        self.profile.as_ref()
    }

    pub(crate) fn response(&self) -> Option<&ResponseUsageSnapshot> {
        self.response.as_ref()
    }

    pub(crate) fn context(&self) -> Option<&ContextTokenSnapshot> {
        self.context.as_ref()
    }

    fn observe_profile(&mut self, profile: &ModelProfileRef) {
        if self.profile.as_ref() == Some(profile) {
            return;
        }
        self.profile = Some(profile.clone());
        self.response = None;
        self.context = None;
    }
}

/// Session activity plus durable response/accounting health.
#[derive(Debug)]
pub(crate) struct SessionState {
    observed_at: Instant,
    next_mode: SessionMode,
    activity: SessionActivity,
    telemetry: [RoleTelemetry; ModelRole::COUNT],
    reasoning: [Option<zevria_foundation::ReasoningLevel>; ModelRole::COUNT],
    persistence_error: Option<String>,
    pub(super) pending_mode_selection: Option<super::mode::PendingModeSelection>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            observed_at: Instant::now(),
            next_mode: SessionMode::Build,
            activity: SessionActivity::Idle,
            telemetry: std::array::from_fn(|_| RoleTelemetry::default()),
            reasoning: [None; ModelRole::COUNT],
            persistence_error: None,
            pending_mode_selection: None,
        }
    }
}

impl SessionState {
    /// A child can emit activity before its authoritative launch descriptor.
    /// Reconcile cached activity (including compaction), never role-keyed usage.
    pub(crate) fn reconcile_inspect_role(&mut self, role: ModelRole) {
        match &mut self.activity {
            SessionActivity::Pending(pending) => pending.role = role,
            SessionActivity::Active(active) => active.role = role,
            SessionActivity::Idle => {}
        }
    }

    /// Observe time outside rendering; stale observations cannot rewind presentation.
    pub(crate) fn observe_clock(&mut self, now: Instant) {
        self.observed_at = self.observed_at.max(now);
    }

    pub(crate) fn observed_at(&self) -> Instant {
        self.observed_at
    }

    pub(crate) fn elapsed(&self) -> Option<Duration> {
        let started_at = match &self.activity {
            SessionActivity::Idle => return None,
            SessionActivity::Pending(pending) => pending.started_at,
            SessionActivity::Active(active) => active.started_at,
        };
        Some(self.observed_at.saturating_duration_since(started_at))
    }

    pub(crate) fn install_reasoning(
        &mut self,
        role: ModelRole,
        level: zevria_foundation::ReasoningLevel,
    ) {
        self.reasoning[role.index()] = Some(level);
    }

    pub(crate) fn reasoning(&self, role: ModelRole) -> Option<zevria_foundation::ReasoningLevel> {
        self.reasoning[role.index()]
    }

    pub(crate) fn finish_model_management(&mut self) {
        if matches!(&self.activity, SessionActivity::Pending(pending) if pending.kind == OperationKind::ModelManagement)
        {
            self.activity = SessionActivity::Idle;
        }
    }

    pub(crate) fn finish_mode_management(&mut self) {
        if matches!(&self.activity, SessionActivity::Pending(pending) if pending.kind == OperationKind::ModeManagement)
        {
            self.activity = SessionActivity::Idle;
        }
    }

    pub(crate) fn install_model(
        &mut self,
        role: ModelRole,
        profile: ModelProfileRef,
        snapshot: Option<ContextTokenSnapshot>,
    ) {
        self.telemetry[role.index()] = RoleTelemetry {
            profile: Some(profile),
            response: None,
            context: snapshot,
        };
    }

    pub(crate) fn begin_operation(
        &mut self,
        kind: OperationKind,
        mode: SessionMode,
        role: ModelRole,
    ) -> bool {
        if !matches!(self.activity, SessionActivity::Idle) {
            return false;
        }
        self.activity = SessionActivity::Pending(PendingOperation {
            started_at: self.observed_at,
            kind,
            mode,
            role,
        });
        true
    }

    pub(crate) const fn is_busy(&self) -> bool {
        !matches!(self.activity, SessionActivity::Idle)
    }

    pub(crate) const fn is_compacting(&self) -> bool {
        matches!(
            self.activity,
            SessionActivity::Active(ActiveOperation {
                phase: ActivePhase::Compacting { .. },
                ..
            })
        )
    }

    pub(crate) const fn next_mode(&self) -> SessionMode {
        self.next_mode
    }

    pub(crate) fn set_next_mode(&mut self, mode: SessionMode) {
        if self.next_mode != mode && role_for_mode(mode) == ModelRole::Build {
            // A projected next-request count from the other policy is not interchangeable.
            self.telemetry[ModelRole::Build.index()].context = None;
        }
        self.next_mode = mode;
    }

    pub(crate) const fn active_id(&self) -> Option<TurnId> {
        match &self.activity {
            SessionActivity::Active(active) => Some(active.id),
            SessionActivity::Idle | SessionActivity::Pending(_) => None,
        }
    }

    pub(crate) const fn display_turn(&self) -> Option<DisplayTurn> {
        match &self.activity {
            SessionActivity::Active(active) => active.display_turn,
            SessionActivity::Idle | SessionActivity::Pending(_) => None,
        }
    }

    pub(crate) const fn call_header(&self) -> Option<NativeHeader> {
        match &self.activity {
            SessionActivity::Active(active) => active.call_header,
            SessionActivity::Idle | SessionActivity::Pending(_) => None,
        }
    }

    pub(crate) fn bind_display_turn(&mut self, turn: DisplayTurn) {
        if let SessionActivity::Active(active) = &mut self.activity {
            active.display_turn = Some(turn);
        }
    }

    pub(crate) fn bind_call_header(&mut self, header: NativeHeader) {
        if let SessionActivity::Active(active) = &mut self.activity {
            active.call_header = Some(header);
        }
    }

    pub(crate) fn model_call_started(&mut self, id: TurnId, call: usize) -> bool {
        let SessionActivity::Active(active) = &mut self.activity else {
            return false;
        };
        if active.id != id
            || !matches!(active.phase, ActivePhase::Running(_))
            || call == 0
            || active.model_call.is_some_and(|previous| call <= previous)
        {
            return false;
        }
        active.model_call = Some(call);
        active.phase = ActivePhase::Running(TurnTail::Waiting);
        true
    }

    pub(crate) const fn cancel_target(&self) -> Option<Option<TurnId>> {
        match &self.activity {
            SessionActivity::Idle => None,
            SessionActivity::Pending(pending)
                if matches!(
                    pending.kind,
                    OperationKind::ModeManagement | OperationKind::ModelManagement
                ) =>
            {
                None
            }
            SessionActivity::Pending(_) => Some(None),
            SessionActivity::Active(active) => Some(Some(active.id)),
        }
    }

    #[cfg(test)]
    pub(crate) const fn in_flight_mode(&self) -> Option<SessionMode> {
        match &self.activity {
            SessionActivity::Pending(pending) => Some(pending.mode),
            SessionActivity::Active(active) => Some(active.mode),
            SessionActivity::Idle => None,
        }
    }

    pub(crate) const fn in_flight_role(&self) -> Option<ModelRole> {
        match &self.activity {
            SessionActivity::Pending(pending) => Some(pending.role),
            SessionActivity::Active(active) => Some(active.role),
            SessionActivity::Idle => None,
        }
    }

    pub(crate) const fn display_role(&self, pane_override: Option<ModelRole>) -> ModelRole {
        match pane_override {
            Some(role) => role,
            None => match self.in_flight_role() {
                Some(role) => role,
                None => role_for_mode(self.next_mode),
            },
        }
    }

    pub(crate) const fn operation_kind(&self) -> Option<OperationKind> {
        match &self.activity {
            SessionActivity::Pending(pending) => Some(pending.kind),
            SessionActivity::Active(active) => Some(active.kind),
            SessionActivity::Idle => None,
        }
    }

    pub(crate) const fn activity(&self) -> &SessionActivity {
        &self.activity
    }

    pub(crate) fn telemetry(&self, role: ModelRole) -> &RoleTelemetry {
        &self.telemetry[role.index()]
    }

    pub(crate) fn persistence_error(&self) -> Option<&str> {
        self.persistence_error.as_deref()
    }

    pub(crate) fn set_persistence_error(&mut self, error: Option<String>) {
        self.persistence_error = error;
    }

    pub(crate) fn reset_for_restore(&mut self) {
        self.next_mode = SessionMode::Build;
        self.pending_mode_selection = None;
        self.persistence_error = None;
        self.activity = SessionActivity::Idle;
        self.telemetry = std::array::from_fn(|_| RoleTelemetry::default());
    }

    pub(crate) fn start_turn(
        &mut self,
        id: TurnId,
        mode: SessionMode,
        role: ModelRole,
    ) -> Option<StartTransition> {
        self.start(
            id,
            mode,
            role,
            StartKind::Turn,
            OperationKind::AuthoritativeTurn,
        )
    }

    pub(crate) fn start_ensemble(
        &mut self,
        id: TurnId,
        mode: SessionMode,
        role: ModelRole,
    ) -> Option<StartTransition> {
        self.start(
            id,
            mode,
            role,
            StartKind::Ensemble,
            OperationKind::AuthoritativeEnsemble,
        )
    }

    pub(crate) fn start_plan_handoff(&mut self, id: TurnId) -> Option<StartTransition> {
        self.start(
            id,
            SessionMode::Build,
            ModelRole::Build,
            StartKind::PlanHandoff,
            OperationKind::AuthoritativePlanHandoff,
        )
    }

    fn start(
        &mut self,
        id: TurnId,
        announced_mode: SessionMode,
        announced_role: ModelRole,
        start: StartKind,
        authoritative: OperationKind,
    ) -> Option<StartTransition> {
        let (transition, kind, mode, started_at) = match &self.activity {
            SessionActivity::Idle => (
                StartTransition::Authoritative(authoritative),
                authoritative,
                announced_mode,
                self.observed_at,
            ),
            SessionActivity::Pending(pending) if pending.kind.accepts(start) => (
                StartTransition::Pending(pending.kind),
                pending.kind,
                pending.mode,
                pending.started_at,
            ),
            SessionActivity::Active(active)
                if active.id == id
                    && matches!(active.phase, ActivePhase::AwaitingStart)
                    && active.kind.accepts(start) =>
            {
                (
                    StartTransition::Awaiting(active.kind),
                    active.kind,
                    active.mode,
                    active.started_at,
                )
            }
            SessionActivity::Pending(_) | SessionActivity::Active(_) => return None,
        };
        self.activity = SessionActivity::Active(ActiveOperation {
            started_at,
            id,
            model_call: None,
            display_turn: None,
            call_header: None,
            kind,
            mode,
            role: announced_role,
            phase: ActivePhase::Running(TurnTail::Waiting),
        });
        Some(transition)
    }

    pub(crate) fn compaction_started(
        &mut self,
        id: TurnId,
        trigger: CompactionTrigger,
        idle_role: ModelRole,
    ) -> Option<CompactionTransition> {
        let (kind, mode, role, started_at) = match &self.activity {
            SessionActivity::Idle => (
                if trigger == CompactionTrigger::Manual {
                    OperationKind::ManualCompaction
                } else {
                    OperationKind::AuthoritativeTurn
                },
                self.next_mode,
                idle_role,
                self.observed_at,
            ),
            SessionActivity::Pending(pending) if !pending.kind.is_management() => {
                (pending.kind, pending.mode, pending.role, pending.started_at)
            }
            SessionActivity::Pending(_) => return None,
            SessionActivity::Active(active) if active.id == id => {
                (active.kind, active.mode, active.role, active.started_at)
            }
            SessionActivity::Active(_) => return None,
        };
        let (model_call, display_turn, call_header) = match &self.activity {
            SessionActivity::Active(active) => {
                (active.model_call, active.display_turn, active.call_header)
            }
            _ => (None, None, None),
        };
        self.activity = SessionActivity::Active(ActiveOperation {
            started_at,
            id,
            model_call,
            display_turn,
            call_header,
            kind,
            mode,
            role,
            phase: ActivePhase::Compacting { trigger },
        });
        Some(CompactionTransition { kind, trigger })
    }

    pub(crate) fn compaction_completed(
        &mut self,
        id: TurnId,
        trigger: CompactionTrigger,
    ) -> Option<CompactionTransition> {
        let SessionActivity::Active(active) = &self.activity else {
            return None;
        };
        if active.id != id
            || !matches!(
                active.phase,
                ActivePhase::Compacting {
                    trigger: active_trigger
                } if active_trigger == trigger
            )
        {
            return None;
        }
        let transition = CompactionTransition {
            kind: active.kind,
            trigger,
        };
        match trigger {
            CompactionTrigger::Manual => self.activity = SessionActivity::Idle,
            CompactionTrigger::AutomaticPreTurn => {
                let active = match std::mem::replace(&mut self.activity, SessionActivity::Idle) {
                    SessionActivity::Active(active) => active,
                    SessionActivity::Idle | SessionActivity::Pending(_) => unreachable!(),
                };
                self.activity = SessionActivity::Active(ActiveOperation {
                    phase: ActivePhase::AwaitingStart,
                    ..active
                });
            }
            CompactionTrigger::AutomaticMidTurn => {
                let active = match std::mem::replace(&mut self.activity, SessionActivity::Idle) {
                    SessionActivity::Active(active) => active,
                    SessionActivity::Idle | SessionActivity::Pending(_) => unreachable!(),
                };
                self.activity = SessionActivity::Active(ActiveOperation {
                    phase: ActivePhase::Running(TurnTail::Waiting),
                    ..active
                });
            }
        }
        Some(transition)
    }

    pub(crate) fn stream(&mut self, id: TurnId, message: Message) -> bool {
        self.set_tail(id, TurnTail::Streaming(message))
    }

    pub(crate) fn retry(
        &mut self,
        id: TurnId,
        attempt: usize,
        max_attempts: usize,
        retry_after: Duration,
        error: String,
    ) -> bool {
        self.set_tail(
            id,
            TurnTail::Retrying(RetryNotice {
                attempt,
                max_attempts,
                error,
                retry_after,
                received_at: self.observed_at,
            }),
        )
    }

    pub(crate) fn accepts_active(&self, id: TurnId) -> bool {
        self.active_id() == Some(id)
    }

    pub(crate) fn accepts_progress(&self, id: TurnId) -> bool {
        matches!(&self.activity, SessionActivity::Active(active)
            if active.id == id && matches!(active.phase, ActivePhase::Running(_)))
    }

    pub(crate) fn accepts_terminal(&self, id: TurnId) -> bool {
        match &self.activity {
            SessionActivity::Pending(pending) => !pending.kind.is_management(),
            SessionActivity::Active(active) => active.id == id,
            SessionActivity::Idle => false,
        }
    }

    pub(crate) fn progress(&mut self, id: TurnId) -> bool {
        self.set_tail(id, TurnTail::Waiting)
    }

    fn set_tail(&mut self, id: TurnId, tail: TurnTail) -> bool {
        if !self.accepts_progress(id) {
            return false;
        }
        let SessionActivity::Active(active) = &mut self.activity else {
            unreachable!("accepted a running turn");
        };
        active.phase = ActivePhase::Running(tail);
        true
    }

    pub(crate) fn clear_stream(&mut self, id: TurnId) -> bool {
        let SessionActivity::Active(active) = &mut self.activity else {
            return false;
        };
        if active.id != id {
            return false;
        }
        match &active.phase {
            ActivePhase::Running(_) => {
                active.phase = ActivePhase::Running(TurnTail::Waiting);
                true
            }
            ActivePhase::AwaitingStart | ActivePhase::Compacting { .. } => false,
        }
    }

    pub(crate) fn update_usage(
        &mut self,
        id: TurnId,
        usage: TokenUsage,
        profile: ModelProfileRef,
        model_role: ModelRole,
        input_token_limit: u64,
        context_window_tokens: u64,
    ) -> bool {
        if !self.accepts_active(id) {
            return false;
        }
        let slot = &mut self.telemetry[model_role.index()];
        slot.observe_profile(&profile);
        slot.response = Some(ResponseUsageSnapshot {
            profile,
            usage,
            input_token_limit,
            context_window_tokens,
        });
        true
    }

    pub(crate) fn update_context_usage(
        &mut self,
        id: TurnId,
        snapshot: ContextTokenSnapshot,
    ) -> bool {
        if !self.accepts_active(id) {
            return false;
        }
        let slot = &mut self.telemetry[snapshot.model_role.index()];
        slot.observe_profile(&snapshot.profile);
        slot.context = Some(snapshot);
        true
    }

    /// Accept a terminal event for the exact active turn or for the sole
    /// pending operation whose engine ID has not yet been announced.
    pub(crate) fn finish(&mut self, id: TurnId) -> Option<OperationKind> {
        if !self.accepts_terminal(id) {
            return None;
        }
        let kind = match &self.activity {
            SessionActivity::Pending(pending) if !pending.kind.is_management() => pending.kind,
            SessionActivity::Active(active) if active.id == id => active.kind,
            SessionActivity::Idle | SessionActivity::Pending(_) | SessionActivity::Active(_) => {
                return None;
            }
        };
        self.activity = SessionActivity::Idle;
        Some(kind)
    }

    /// Authoritative Plan snapshots settle revision and fresh-session
    /// decisions. Current-session implementation remains in flight until its
    /// handoff start (and eventual terminal event).
    pub(crate) fn settle_plan_state(&mut self, state: &PlanWorkflowState) -> bool {
        let SessionActivity::Pending(pending) = &self.activity else {
            return false;
        };
        let settled = match (pending.kind, state) {
            (
                OperationKind::PlanRevise { expected },
                PlanWorkflowState::Planning {
                    previous: Some(artifact),
                    ..
                },
            ) => artifact.version == expected,
            (
                OperationKind::PlanImplementFresh { expected },
                PlanWorkflowState::Resolved {
                    artifact,
                    resolution: PlanResolution::ImplementedFresh,
                },
            ) => artifact.version == expected,
            _ => false,
        };
        if settled {
            self.activity = SessionActivity::Idle;
        }
        settled
    }

    pub(crate) fn settle_fresh_handoff_request(&mut self, version: PlanVersion) -> bool {
        let SessionActivity::Pending(PendingOperation {
            kind: OperationKind::PlanImplementFresh { expected },
            ..
        }) = self.activity
        else {
            return false;
        };
        if expected != version {
            return false;
        }
        self.activity = SessionActivity::Idle;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: TurnId = TurnId::new(7);

    #[test]
    fn inspect_role_reconciliation_updates_all_activity_without_moving_telemetry() {
        for phase in 0..3 {
            let mut state = SessionState::default();
            state.install_model(
                ModelRole::Explore,
                ModelProfileRef::new("p", "explore"),
                None,
            );
            state.install_model(
                ModelRole::Builder,
                ModelProfileRef::new("p", "builder"),
                None,
            );
            let telemetry = state.telemetry.clone();
            state.begin_operation(
                OperationKind::Submit,
                SessionMode::Build,
                ModelRole::Explore,
            );
            if phase > 0 {
                state.start_turn(TURN, SessionMode::Build, ModelRole::Explore);
            }
            if phase > 1 {
                state.compaction_started(
                    TURN,
                    CompactionTrigger::AutomaticMidTurn,
                    ModelRole::Explore,
                );
            }
            state.reconcile_inspect_role(ModelRole::Builder);
            assert_eq!(state.in_flight_role(), Some(ModelRole::Builder));
            assert_eq!(state.telemetry, telemetry);
            assert_eq!(state.is_compacting(), phase == 2);
        }
    }

    #[test]
    fn pending_operation_locks_and_learns_id() {
        let mut state = SessionState::default();
        assert!(state.begin_operation(OperationKind::Submit, SessionMode::Plan, ModelRole::Plan,));
        assert!(!state.begin_operation(OperationKind::Skill, SessionMode::Plan, ModelRole::Plan,));
        assert_eq!(state.cancel_target(), Some(None));
        assert_eq!(
            state.start_turn(TURN, SessionMode::Plan, ModelRole::Plan),
            Some(StartTransition::Pending(OperationKind::Submit))
        );
        assert_eq!(state.cancel_target(), Some(Some(TURN)));
        assert_eq!(state.in_flight_mode(), Some(SessionMode::Plan));
    }

    #[test]
    fn all_compaction_triggers_have_named_transitions() {
        let mut manual = SessionState::default();
        assert!(manual.begin_operation(
            OperationKind::ManualCompaction,
            SessionMode::Build,
            ModelRole::Build,
        ));
        assert!(
            manual
                .compaction_started(TURN, CompactionTrigger::Manual, ModelRole::Build)
                .is_some()
        );
        assert!(manual.is_compacting());
        assert!(
            manual
                .compaction_completed(TURN, CompactionTrigger::Manual)
                .is_some()
        );
        assert!(!manual.is_busy());

        for trigger in [
            CompactionTrigger::AutomaticPreTurn,
            CompactionTrigger::AutomaticMidTurn,
        ] {
            let mut state = SessionState::default();
            assert!(state.begin_operation(
                OperationKind::Submit,
                SessionMode::Build,
                ModelRole::Build,
            ));
            assert!(
                state
                    .compaction_started(TURN, trigger, ModelRole::Build)
                    .is_some()
            );
            assert!(state.compaction_completed(TURN, trigger).is_some());
            match (trigger, state.activity()) {
                (CompactionTrigger::AutomaticPreTurn, SessionActivity::Active(active)) => {
                    assert!(matches!(active.phase, ActivePhase::AwaitingStart));
                }
                (CompactionTrigger::AutomaticMidTurn, SessionActivity::Active(active)) => {
                    assert!(matches!(
                        active.phase,
                        ActivePhase::Running(TurnTail::Waiting)
                    ));
                }
                _ => panic!("unexpected compaction transition"),
            }
        }
    }

    #[test]
    fn pending_terminal_is_accepted_before_start() {
        let mut state = SessionState::default();
        assert!(state.begin_operation(
            OperationKind::ManualCompaction,
            SessionMode::Build,
            ModelRole::Build,
        ));
        assert_eq!(state.finish(TURN), Some(OperationKind::ManualCompaction));
        assert!(!state.is_busy());
    }

    #[test]
    fn stale_progress_and_terminal_are_inert() {
        let mut state = SessionState::default();
        assert!(
            state.begin_operation(OperationKind::Submit, SessionMode::Build, ModelRole::Build,)
        );
        state
            .start_turn(TURN, SessionMode::Build, ModelRole::Build)
            .unwrap();
        let stale = TurnId::new(8);
        assert!(!state.progress(stale));
        assert_eq!(state.finish(stale), None);
        assert!(state.is_busy());
    }

    #[test]
    fn retry_is_replaced_by_progress() {
        let mut state = SessionState::default();
        assert!(
            state.begin_operation(OperationKind::Submit, SessionMode::Build, ModelRole::Build,)
        );
        state
            .start_turn(TURN, SessionMode::Build, ModelRole::Build)
            .unwrap();
        assert!(state.retry(
            TURN,
            1,
            3,
            Duration::from_millis(500),
            "offline".to_string()
        ));
        assert!(state.progress(TURN));
        assert!(matches!(
            state.activity(),
            SessionActivity::Active(ActiveOperation {
                phase: ActivePhase::Running(TurnTail::Waiting),
                ..
            })
        ));
    }

    #[test]
    fn whole_operation_clock_survives_all_automatic_phases_and_restarts_from_idle() {
        let mut state = SessionState::default();
        let start = state.observed_at();
        assert!(state.begin_operation(OperationKind::Submit, SessionMode::Build, ModelRole::Build));
        assert_eq!(state.elapsed(), Some(Duration::ZERO));
        state.observe_clock(start + Duration::from_secs(2));
        state
            .compaction_started(TURN, CompactionTrigger::AutomaticPreTurn, ModelRole::Build)
            .unwrap();
        state.observe_clock(start + Duration::from_secs(4));
        state
            .compaction_completed(TURN, CompactionTrigger::AutomaticPreTurn)
            .unwrap();
        assert!(matches!(
            state.activity(),
            SessionActivity::Active(ActiveOperation {
                phase: ActivePhase::AwaitingStart,
                ..
            })
        ));
        state.observe_clock(start + Duration::from_secs(5));
        state
            .start_turn(TURN, SessionMode::Build, ModelRole::Build)
            .unwrap();
        assert_eq!(state.elapsed(), Some(Duration::from_secs(5)));
        state.observe_clock(start + Duration::from_secs(6));
        assert!(state.progress(TURN));
        state.observe_clock(start + Duration::from_secs(7));
        assert!(state.retry(TURN, 1, 5, Duration::from_secs(4), "offline".into()));
        state.observe_clock(start + Duration::from_secs(8));
        assert!(state.stream(TURN, Message::assistant("resumed")));
        state.observe_clock(start + Duration::from_secs(9));
        state
            .compaction_started(TURN, CompactionTrigger::AutomaticMidTurn, ModelRole::Build)
            .unwrap();
        state.observe_clock(start + Duration::from_secs(10));
        state
            .compaction_completed(TURN, CompactionTrigger::AutomaticMidTurn)
            .unwrap();
        assert_eq!(state.elapsed(), Some(Duration::from_secs(10)));
        state.observe_clock(start);
        assert_eq!(state.observed_at(), start + Duration::from_secs(10));
        assert_eq!(state.elapsed(), Some(Duration::from_secs(10)));
        assert!(state.finish(TURN).is_some());
        assert_eq!(state.elapsed(), None);

        state.observe_clock(start + Duration::from_secs(20));
        state
            .start_turn(TurnId::new(8), SessionMode::Build, ModelRole::Build)
            .unwrap();
        assert_eq!(state.elapsed(), Some(Duration::ZERO));
        state.observe_clock(start + Duration::from_secs(22));
        assert_eq!(state.elapsed(), Some(Duration::from_secs(2)));
        state.reset_for_restore();
        assert_eq!(state.elapsed(), None);
        state
            .compaction_started(TurnId::new(9), CompactionTrigger::Manual, ModelRole::Build)
            .unwrap();
        assert_eq!(state.elapsed(), Some(Duration::ZERO));
        state
            .compaction_completed(TurnId::new(9), CompactionTrigger::Manual)
            .unwrap();
        assert_eq!(state.elapsed(), None);
    }

    #[test]
    fn countdown_is_typed_saturating_and_only_matching_running_events_replace_it() {
        let mut state = SessionState::default();
        let start = state.observed_at();
        state.begin_operation(OperationKind::Submit, SessionMode::Build, ModelRole::Build);
        assert!(!state.retry(TURN, 1, 5, Duration::ZERO, "not started".into()));
        state
            .start_turn(TURN, SessionMode::Build, ModelRole::Build)
            .unwrap();
        state.observe_clock(start + Duration::from_secs(12));
        state.retry(TURN, 1, 5, Duration::ZERO, "immediate".into());
        let notice = |state: &SessionState| {
            let SessionActivity::Active(ActiveOperation {
                phase: ActivePhase::Running(TurnTail::Retrying(notice)),
                ..
            }) = state.activity()
            else {
                panic!("retry tail")
            };
            notice.clone()
        };
        assert_eq!(
            notice(&state).countdown(state.observed_at()),
            RetryCountdown::Immediate
        );
        state.retry(TURN, 2, 5, Duration::from_millis(1500), "backoff".into());
        let received = state.observed_at();
        assert_eq!(
            notice(&state).countdown(received),
            RetryCountdown::Pending(Duration::from_millis(1500))
        );
        state.observe_clock(received + Duration::from_secs(1));
        assert_eq!(
            notice(&state).countdown(state.observed_at()),
            RetryCountdown::Pending(Duration::from_millis(500))
        );
        state.observe_clock(received + Duration::from_millis(1500));
        assert_eq!(
            notice(&state).countdown(state.observed_at()),
            RetryCountdown::Elapsed
        );
        state.observe_clock(received + Duration::from_secs(2));
        assert_eq!(
            notice(&state).countdown(state.observed_at()),
            RetryCountdown::Elapsed
        );
        let old = notice(&state);
        let stale = TurnId::new(99);
        assert!(!state.retry(stale, 3, 5, Duration::MAX, "stale".into()));
        assert!(!state.stream(stale, Message::assistant("stale")));
        assert!(!state.progress(stale));
        assert!(!state.clear_stream(stale));
        assert_eq!(state.finish(stale), None);
        assert!(
            state
                .start_turn(stale, SessionMode::Build, ModelRole::Build)
                .is_none()
        );
        assert!(
            state
                .compaction_started(stale, CompactionTrigger::AutomaticMidTurn, ModelRole::Build)
                .is_none()
        );
        assert_eq!(notice(&state), old);
        assert_eq!(state.elapsed(), Some(Duration::from_secs(14)));
        state.retry(TURN, 3, 5, Duration::from_secs(4), "new attempt".into());
        assert_eq!(
            notice(&state).countdown(state.observed_at()),
            RetryCountdown::Pending(Duration::from_secs(4))
        );
        assert_eq!(state.elapsed(), Some(Duration::from_secs(14)));
        state.retry(
            TURN,
            4,
            5,
            Duration::MAX,
            "no instant addition overflow".into(),
        );
        assert_eq!(
            notice(&state).countdown(state.observed_at()),
            RetryCountdown::Pending(Duration::MAX)
        );
        assert!(state.progress(TURN));
        assert!(matches!(
            state.activity(),
            SessionActivity::Active(ActiveOperation {
                phase: ActivePhase::Running(TurnTail::Waiting),
                ..
            })
        ));
        state
            .compaction_started(TURN, CompactionTrigger::AutomaticMidTurn, ModelRole::Build)
            .unwrap();
        assert!(!state.retry(TURN, 5, 5, Duration::ZERO, "suppressed compaction".into()));
        assert_eq!(state.elapsed(), Some(Duration::from_secs(14)));
    }

    #[test]
    fn pending_terminal_and_model_management_settlement_drop_the_timer() {
        for kind in [
            OperationKind::Submit,
            OperationKind::ManualCompaction,
            OperationKind::ModelManagement,
        ] {
            let mut state = SessionState::default();
            state.begin_operation(kind, SessionMode::Build, ModelRole::Build);
            state.observe_clock(state.observed_at() + Duration::from_secs(2));
            assert_eq!(state.elapsed(), Some(Duration::from_secs(2)));
            if kind == OperationKind::ModelManagement {
                assert_eq!(state.finish(TURN), None);
                state.finish_model_management();
            } else {
                assert_eq!(state.finish(TURN), Some(kind));
            }
            assert_eq!(state.elapsed(), None);
        }
    }

    #[test]
    fn role_telemetry_is_separate_and_profile_changes_clear_companions() {
        let mut state = SessionState::default();
        assert!(
            state.begin_operation(OperationKind::Submit, SessionMode::Build, ModelRole::Build,)
        );
        state
            .start_turn(TURN, SessionMode::Build, ModelRole::Build)
            .unwrap();
        let old = ModelProfileRef::new("provider-a", "build-a");
        let snapshot = ContextTokenSnapshot {
            profile: old.clone(),
            model_role: ModelRole::Build,
            projected_input_tokens: 12_000,
            source: zevria_model::ContextTokenSource::Exact,
            automatic_trigger: 80_000,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        };
        assert!(state.update_context_usage(TURN, snapshot));
        assert!(state.update_usage(
            TURN,
            TokenUsage {
                input_tokens: 10,
                cached_tokens: 2,
                output_tokens: 3,
                total_tokens: 13,
            },
            old.clone(),
            ModelRole::Build,
            100_000,
            128_000,
        ));
        let build = state.telemetry(ModelRole::Build);
        assert_eq!(build.authoritative_profile(), Some(&old));
        assert!(build.context().is_some());
        assert_eq!(
            build.response().expect("response").limits(),
            (100_000, 128_000)
        );
        assert_eq!(build.response().expect("response").profile(), &old);
        assert!(state.telemetry(ModelRole::Plan).context().is_none());

        let replacement = ModelProfileRef::new("provider-b", "build-b");
        assert!(state.update_usage(
            TURN,
            TokenUsage {
                input_tokens: 20,
                cached_tokens: 4,
                output_tokens: 5,
                total_tokens: 25,
            },
            replacement.clone(),
            ModelRole::Build,
            90_000,
            120_000,
        ));
        let build = state.telemetry(ModelRole::Build);
        assert_eq!(build.authoritative_profile(), Some(&replacement));
        assert!(build.context().is_none());
        assert_eq!(
            build.response().expect("new response").limits(),
            (90_000, 120_000)
        );

        assert!(!state.update_usage(
            TurnId::new(99),
            TokenUsage::default(),
            ModelProfileRef::new("stale", "stale"),
            ModelRole::Plan,
            1,
            1,
        ));
        assert!(
            state
                .telemetry(ModelRole::Plan)
                .authoritative_profile()
                .is_none()
        );
    }
}
