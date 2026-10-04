//! Event orchestration.

use super::*;
use std::path::PathBuf;

/// Correlated outcome of an idle-only durable root mode selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeSelectionResult {
    Accepted { mode: SessionMode, changed: bool },
    Rejected { code: String, message: String },
}

impl ModeSelectionResult {
    pub fn rejected(code: &str, message: impl Into<String>) -> Self {
        Self::Rejected {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Provider-neutral events consumed by frontends.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// Independent hosted activity; never a local function call or text fence.
    WebSearchUpdated {
        turn_id: TurnId,
        attempt: crate::WebSearchAttemptRecord,
    },
    ModeResult {
        request_id: String,
        result: ModeSelectionResult,
    },
    /// An accepted workflow action explicitly selected a mode. Management uses
    /// only ModeResult so stale/cancelled replies cannot act as workflow traffic.
    ModeChanged {
        mode: SessionMode,
    },
    ModelsResult {
        request_id: String,
        result: crate::models::ModelManagementResult,
    },
    SkillsResult {
        request_id: String,
        result: crate::skill::SkillManagementResult,
    },
    /// Bounded invalidation only. Clients query metadata pages separately.
    SkillsChanged {
        revision: String,
        counts: crate::skill::SkillCatalogCounts,
    },
    /// A manual or automatic checkpoint request is in flight.
    CompactionStarted {
        turn_id: TurnId,
        trigger: CompactionTrigger,
    },
    /// A checkpoint was durably installed and now controls model history.
    CompactionCompleted {
        turn_id: TurnId,
        trigger: CompactionTrigger,
        backend: CompactionBackend,
    },
    /// The user message has been accepted into local history and transcript
    /// persistence has been attempted.
    TurnStarted {
        turn_id: TurnId,
        message: Message,
        mode: SessionMode,
    },
    /// One model/tool-loop dispatch is beginning. `call` is 1-based and
    /// restarts for each turn's loop, counting invocations of the provider's
    /// `complete` method, not individual tools or network requests. Provider
    /// retries/continuations and compaction requests do not advance it.
    ModelCallStarted {
        turn_id: TurnId,
        call: usize,
    },
    /// The compact ensemble command and one descriptor per configured worker
    /// have been durably accepted.
    EnsembleStarted {
        turn_id: TurnId,
        start: EnsembleStart,
        resumed: bool,
    },
    /// One normalized, inspectable ACP update. Worker transcript persistence
    /// happens before this publication.
    AgentRunUpdated {
        turn_id: TurnId,
        ensemble_run_id: EnsembleRunId,
        agent_run_id: AgentRunId,
        event: AgentRunEvent,
    },
    /// Reliable host review projection; never a lossy model preview.
    WorkerReviewUpdated {
        target: WorkerControlTarget,
        state: Box<WorkerReviewState>,
    },
    WorkerControlResult {
        result: WorkerControlResult,
    },
    /// Terminal provider-neutral worker outcome.
    AgentRunFinished {
        turn_id: TurnId,
        ensemble_run_id: EnsembleRunId,
        outcome: AgentRunOutcome,
    },
    /// Every worker is terminal and the bounded synthesis input is durable.
    EnsembleReportsReady {
        turn_id: TurnId,
        run_id: EnsembleRunId,
        agents: Vec<AgentRunSummary>,
    },
    /// Unified provider preview, including activity-only attempts.
    AssistantStreamUpdated {
        turn_id: TurnId,
        snapshot: crate::AssistantStreamSnapshot,
    },
    /// Any previously displayed streaming snapshot must be removed.
    StreamCleared {
        turn_id: TurnId,
    },
    /// A completed assistant tool-call message.
    Intermediate {
        turn_id: TurnId,
        message: Message,
        display_attempt_id: Option<String>,
    },
    /// A correlated tool-result batch plus display-only metadata.
    ToolResults {
        turn_id: TurnId,
        message: Message,
        metadata: Vec<ToolResultMetadata>,
    },
    /// The final assistant response for the user turn.
    TurnCompleted {
        turn_id: TurnId,
        message: Message,
        display_attempt_id: Option<String>,
    },
    /// Recovery repaired terminal workflow records for an assistant response
    /// that was already present in the restored transcript. Frontends should
    /// unlock the turn without appending another message.
    TurnRecovered {
        turn_id: TurnId,
        display_attempt_id: Option<String>,
    },
    /// A semantic Build prompt backed by a typed handoff record rather than a
    /// user-authored message.
    PlanHandoffStarted {
        turn_id: TurnId,
        handoff: PlanHandoff,
    },
    /// Authoritative Plan workflow snapshot after a live transition, not startup
    /// readiness. Hosts seed restored state from `SessionEngine::plan_state`;
    /// frontends never infer it from assistant messages or turn completion.
    PlanStateChanged {
        state: PlanWorkflowState,
    },
    /// The transcript remains canonical, but its convenience Markdown
    /// projection could not be written at a Ready transition.
    PlanProjectionWarning {
        version: PlanVersion,
        path: PathBuf,
        error: String,
    },
    /// The session manager must create a new session and deliver this through
    /// `TurnCommand::StartFromPlan` without converting it to text.
    FreshPlanHandoffRequested {
        handoff: PlanHandoff,
    },
    /// The user turn failed after retaining all completed local work.
    TurnFailed {
        turn_id: TurnId,
        error: String,
    },
    /// Refused before durable acceptance. No transcript error row exists.
    TurnRejected {
        turn_id: TurnId,
        error: String,
    },
    /// The user cancelled the turn after retaining all completed work.
    TurnCancelled {
        turn_id: TurnId,
    },
    /// Token accounting for the most recently completed model response —
    /// including intra-turn tool-loop responses, so a frontend can show live
    /// prompt-cache effectiveness.
    UsageUpdated {
        turn_id: TurnId,
        usage: TokenUsage,
        profile: ModelProfileRef,
        model_role: ModelRole,
        input_token_limit: u64,
        context_window_tokens: u64,
    },
    /// Projected input accounting for the next complete provider request.
    ContextUsageUpdated {
        turn_id: TurnId,
        snapshot: ContextTokenSnapshot,
    },
    /// Transient completion networking metadata, never assistant/model input.
    NetworkStatus {
        turn_id: TurnId,
        call: usize,
        attempt: usize,
        max_attempts: usize,
        transport: NetworkTransport,
        status: NetworkStatus,
    },
    /// The provider lost its connection mid-turn and is reconnecting; the
    /// turn is still in flight. Frontends should show a transient status,
    /// not a failure.
    TurnRetrying {
        turn_id: TurnId,
        call: usize,
        attempt: usize,
        max_attempts: usize,
        /// Delay before the next recovery attempt; zero means immediate reconnect.
        retry_after: std::time::Duration,
        error: String,
    },
    /// A subtask launch was queued; emitted by the launcher before the
    /// blocking tool call starts waiting, so the frontend row can open the
    /// live child immediately.
    SubtaskLaunched {
        turn_id: TurnId,
        /// The real outer tool call correlation, shared by all entries.
        call_id: String,
        /// Zero-based position in the launch_subtasks input array.
        entry_index: usize,
        descriptor: SubtaskDescriptor,
    },
    /// A forwarded event from one child subsession. Never touches the root
    /// pane's streaming or busy state.
    SubtaskSession {
        id: SubtaskId,
        event: Box<SessionEvent>,
    },
    /// A subtask lifecycle transition (Starting → Running → terminal).
    SubtaskStatus {
        turn_id: TurnId,
        id: SubtaskId,
        status: SubtaskStatus,
    },
    /// A blocking model tool is waiting for structured user input.
    QuestionAsked {
        turn_id: TurnId,
        request: QuestionRequest,
    },
    /// A previously announced question stopped waiting. Frontends must match
    /// both identities so a delayed close cannot remove a newer modal.
    QuestionClosed {
        turn_id: TurnId,
        request_id: QuestionRequestId,
    },
    /// Transcript persistence health changed. A degraded session remains
    /// inspectable but rejects new work until its complete in-memory history
    /// can be atomically repaired.
    PersistenceChanged {
        path: PathBuf,
        error: Option<String>,
    },
}

/// Latest coalesced streaming value for one root or child turn.
///
/// `revision` is transport-local ordering metadata assigned by the channel
/// that publishes this state. Relaying a child state through its parent
/// assigns a fresh parent-channel revision.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStreamState {
    pub revision: u64,
    pub turn_id: TurnId,
    pub message: Option<Message>,
    pub attempt: Option<crate::WebSearchAttemptRecord>,
}

/// One lossy delivery of changed root and child streaming states.
///
/// A batch contains only targets whose latest state is newer than both their
/// most recently delivered state and their latest lifecycle fence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStreamBatch {
    pub root: Option<SessionStreamState>,
    pub subtasks: HashMap<SubtaskId, SessionStreamState>,
    /// Changed accumulated ACP segments in transport sequence order. More
    /// than one segment from the same worker can be present when the consumer
    /// falls behind a thought-to-message transition.
    pub agent_runs: Vec<AgentRunStreamState>,
}

impl SessionStreamBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.root.is_none() && self.subtasks.is_empty() && self.agent_runs.is_empty()
    }
}

/// One lossy, accumulated ACP message/thought preview. `event` is always an
/// `AgentMessage` or `Thought`; the complete normalized deltas remain durable
/// in the worker JSONL transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRunStreamState {
    pub revision: u64,
    pub turn_id: TurnId,
    pub ensemble_run_id: EnsembleRunId,
    pub agent_run_id: AgentRunId,
    pub event: AgentRunEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct AgentRunPreviewKey {
    pub(super) agent_run_id: AgentRunId,
    pub(super) segment_revision: u64,
}

/// The single ordered update contract exposed to session consumers.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // Keep lifecycle matching ergonomic on the TUI hot path.
pub enum SessionUpdate {
    /// Lossless, bounded, backpressured session lifecycle traffic.
    Lifecycle(SessionEvent),
    /// Lossy, coalesced streaming previews for only the changed panes.
    Streams(SessionStreamBatch),
}

#[derive(Debug)]
pub(super) struct SequencedSessionEvent {
    pub(super) sequence: u64,
    pub(super) event: SessionEvent,
}

/// Full latest-value storage behind the watch channel. This stays private so
/// consumers cannot bypass lifecycle fencing by borrowing the watch value.
#[derive(Debug, Clone, Default)]
pub(super) struct SessionStreamSnapshot {
    pub(super) root: Option<SessionStreamState>,
    pub(super) subtasks: HashMap<SubtaskId, SessionStreamState>,
    pub(super) agent_runs: HashMap<AgentRunPreviewKey, AgentRunStreamState>,
    pub(super) active_agent_runs: HashMap<AgentRunId, AgentRunPreviewKey>,
}

#[derive(Clone)]
pub struct SessionEventSender {
    pub(super) lifecycle: mpsc::Sender<SequencedSessionEvent>,
    pub(super) streams: watch::Sender<SessionStreamSnapshot>,
    pub(super) sequence: Arc<Mutex<u64>>,
    hosted_actions: Arc<Mutex<std::collections::HashSet<(AgentRunId, String)>>>,
}

pub struct SessionEventReceiver {
    pub(super) lifecycle: mpsc::Receiver<SequencedSessionEvent>,
    pub(super) streams: watch::Receiver<SessionStreamSnapshot>,
    pub(super) stream_cleanup: watch::Sender<SessionStreamSnapshot>,
    pub(super) lifecycle_closed: bool,
    pub(super) streams_closed: bool,
    pub(super) streams_drained: bool,
    pub(super) stream_wake_pending: bool,
    pub(super) pending_lifecycle: Option<SequencedSessionEvent>,
    pub(super) root_fence: u64,
    pub(super) subtask_fences: HashMap<SubtaskId, u64>,
    pub(super) agent_run_fences: HashMap<AgentRunId, u64>,
    pub(super) root_emitted: u64,
    pub(super) subtask_emitted: HashMap<SubtaskId, u64>,
    pub(super) agent_run_emitted: HashMap<AgentRunPreviewKey, u64>,
    hosted_actions: std::collections::HashSet<(AgentRunId, String)>,
}

pub fn session_event_channel(capacity: usize) -> (SessionEventSender, SessionEventReceiver) {
    let (lifecycle_tx, lifecycle_rx) = mpsc::channel(capacity.max(1));
    let (stream_tx, stream_rx) = watch::channel(SessionStreamSnapshot::default());
    let sequence = Arc::new(Mutex::new(0));
    (
        SessionEventSender {
            lifecycle: lifecycle_tx,
            streams: stream_tx.clone(),
            sequence,
            hosted_actions: Default::default(),
        },
        SessionEventReceiver {
            lifecycle: lifecycle_rx,
            streams: stream_rx,
            stream_cleanup: stream_tx,
            lifecycle_closed: false,
            streams_closed: false,
            streams_drained: false,
            stream_wake_pending: false,
            pending_lifecycle: None,
            root_fence: 0,
            subtask_fences: HashMap::new(),
            agent_run_fences: HashMap::new(),
            root_emitted: 0,
            subtask_emitted: HashMap::new(),
            agent_run_emitted: HashMap::new(),
            hosted_actions: Default::default(),
        },
    )
}

impl SessionEventSender {
    // Tokio's send error deliberately returns the complete event to its
    // caller. Plan snapshots make that value substantial, but preserving it
    // is part of this channel API rather than accidental error payload bloat.
    #[allow(clippy::result_large_err)]
    pub async fn send(
        &self,
        event: SessionEvent,
    ) -> Result<(), mpsc::error::SendError<SessionEvent>> {
        match event {
            SessionEvent::AssistantStreamUpdated { turn_id, snapshot } => {
                self.stream_snapshot(turn_id, snapshot);
                Ok(())
            }
            SessionEvent::StreamCleared { turn_id } => {
                self.stream_cleared(turn_id);
                Ok(())
            }
            SessionEvent::AgentRunUpdated {
                turn_id,
                ensemble_run_id,
                agent_run_id,
                event: event @ (AgentRunEvent::AgentMessage { .. } | AgentRunEvent::Thought { .. }),
            } => {
                self.agent_run_preview(turn_id, ensemble_run_id, agent_run_id, event);
                Ok(())
            }
            event => {
                let agent_run_boundary = agent_run_preview_boundary(&event).cloned();
                // Capacity is reserved before sequencing, so waiting on
                // lifecycle backpressure cannot reserve an ordering slot.
                let permit = match self.lifecycle.reserve().await {
                    Ok(permit) => permit,
                    Err(_) => return Err(mpsc::error::SendError(event)),
                };
                self.publish_next(|sequence| {
                    let existing = hosted_activity_update(
                        &event,
                        &mut self
                            .hosted_actions
                            .lock()
                            .expect("hosted activity index poisoned"),
                    );
                    if let Some(agent_run_id) = &agent_run_boundary
                        && !existing
                    {
                        // A lossless semantic update ends only the active
                        // coalescing segment. Retain its latest value until
                        // the receiver has delivered it; otherwise a burst
                        // can overwrite an unread thought before lifecycle
                        // traffic is reduced.
                        self.streams.send_modify(|snapshot| {
                            snapshot.active_agent_runs.remove(agent_run_id);
                        });
                    }
                    permit.send(SequencedSessionEvent { sequence, event });
                });
                Ok(())
            }
        }
    }

    #[allow(clippy::result_large_err)]
    pub fn try_send(
        &self,
        event: SessionEvent,
    ) -> Result<(), mpsc::error::TrySendError<SessionEvent>> {
        match event {
            SessionEvent::AssistantStreamUpdated { turn_id, snapshot } => {
                self.stream_snapshot(turn_id, snapshot);
                Ok(())
            }
            SessionEvent::StreamCleared { turn_id } => {
                self.stream_cleared(turn_id);
                Ok(())
            }
            SessionEvent::AgentRunUpdated {
                turn_id,
                ensemble_run_id,
                agent_run_id,
                event: event @ (AgentRunEvent::AgentMessage { .. } | AgentRunEvent::Thought { .. }),
            } => {
                self.agent_run_preview(turn_id, ensemble_run_id, agent_run_id, event);
                Ok(())
            }
            event => {
                let agent_run_boundary = agent_run_preview_boundary(&event).cloned();
                // Like `send`, a failed reservation consumes no sequence.
                let permit = match self.lifecycle.try_reserve() {
                    Ok(permit) => permit,
                    Err(mpsc::error::TrySendError::Full(())) => {
                        return Err(mpsc::error::TrySendError::Full(event));
                    }
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        return Err(mpsc::error::TrySendError::Closed(event));
                    }
                };
                self.publish_next(|sequence| {
                    let existing = hosted_activity_update(
                        &event,
                        &mut self
                            .hosted_actions
                            .lock()
                            .expect("hosted activity index poisoned"),
                    );
                    if let Some(agent_run_id) = &agent_run_boundary
                        && !existing
                    {
                        self.streams.send_modify(|snapshot| {
                            snapshot.active_agent_runs.remove(agent_run_id);
                        });
                    }
                    permit.send(SequencedSessionEvent { sequence, event });
                });
                Ok(())
            }
        }
    }

    pub fn stream_updated(&self, turn_id: TurnId, message: Message) {
        self.stream_snapshot(turn_id, message.into());
    }

    pub fn stream_snapshot(&self, turn_id: TurnId, value: crate::AssistantStreamSnapshot) {
        self.publish_next(|sequence| {
            self.streams.send_modify(|snapshot| {
                snapshot.root = Some(SessionStreamState {
                    revision: sequence,
                    turn_id,
                    message: value.message,
                    attempt: value.attempt,
                });
            });
        });
    }

    pub fn stream_cleared(&self, turn_id: TurnId) {
        self.publish_next(|sequence| {
            self.streams.send_modify(|snapshot| {
                snapshot.root = Some(SessionStreamState {
                    revision: sequence,
                    turn_id,
                    message: None,
                    attempt: None,
                });
            });
        });
    }

    pub fn set_subtask_stream(&self, id: SubtaskId, mut state: SessionStreamState) {
        // Child revisions are meaningful only inside the child channel. The
        // relay is a new parent publication and therefore gets a new parent
        // sequence before it enters the parent's coalesced snapshot.
        self.publish_next(|sequence| {
            state.revision = sequence;
            self.streams.send_modify(|snapshot| {
                snapshot.subtasks.insert(id, state);
            });
        });
    }

    pub fn agent_run_preview(
        &self,
        turn_id: TurnId,
        ensemble_run_id: EnsembleRunId,
        agent_run_id: AgentRunId,
        event: AgentRunEvent,
    ) {
        self.publish_next(|sequence| {
            self.streams.send_modify(|snapshot| {
                let active = snapshot.active_agent_runs.get(&agent_run_id).cloned();
                let previous = active
                    .as_ref()
                    .and_then(|key| snapshot.agent_runs.get(key))
                    .map(|state| state.event.clone());
                let continues_segment = previous
                    .as_ref()
                    .is_some_and(|previous| agent_preview_segments_match(previous, &event));
                let key = if continues_segment {
                    active.expect("a continuing segment has an active key")
                } else {
                    let key = AgentRunPreviewKey {
                        agent_run_id: agent_run_id.clone(),
                        segment_revision: sequence,
                    };
                    snapshot
                        .active_agent_runs
                        .insert(agent_run_id.clone(), key.clone());
                    key
                };
                let event = accumulate_agent_preview(previous.as_ref(), event);
                snapshot.agent_runs.insert(
                    key,
                    AgentRunStreamState {
                        revision: sequence,
                        turn_id,
                        ensemble_run_id,
                        agent_run_id,
                        event,
                    },
                );
            });
        });
    }

    /// Assign the next channel-local sequence and synchronously publish while
    /// still holding the sequencing lock. Callers reserve any asynchronous
    /// capacity first, so this lock is never held across an await.
    pub(super) fn publish_next(&self, publish: impl FnOnce(u64)) {
        let mut sequence = self
            .sequence
            .lock()
            .expect("session sequence lock poisoned");
        *sequence = sequence
            .checked_add(1)
            .expect("session update sequence overflow");
        publish(*sequence);
    }
}

impl SessionEventReceiver {
    /// Receive the next lifecycle event or eligible coalesced stream batch.
    ///
    /// One lifecycle event is buffered so accumulated stream segments with a
    /// lower transport sequence can be delivered first. This preserves
    /// thought/message transitions across a burst without making individual
    /// ACP token deltas lossless.
    pub async fn recv(&mut self) -> Option<SessionUpdate> {
        loop {
            match self.try_recv() {
                Ok(update) => return Some(update),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }

            tokio::select! {
                biased;
                event = self.lifecycle.recv(), if !self.lifecycle_closed && self.pending_lifecycle.is_none() => {
                    match event {
                        Some(event) => self.pending_lifecycle = Some(event),
                        None => {
                            self.lifecycle_closed = true;
                            self.streams_closed = true;
                            self.stream_wake_pending = true;
                        }
                    }
                }
                changed = self.streams.changed(), if !self.streams_closed => {
                    if changed.is_err() {
                        self.streams_closed = true;
                    }
                    // `changed` marks the watch value seen. Preserve the wake
                    // until `try_recv` has rechecked lifecycle and folded the
                    // latest snapshot.
                    self.stream_wake_pending = true;
                }
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<SessionUpdate, mpsc::error::TryRecvError> {
        loop {
            self.buffer_lifecycle();

            if !self.stream_wake_pending && !self.streams_drained {
                match self.streams.has_changed() {
                    Ok(true) => self.stream_wake_pending = true,
                    Ok(false) => {}
                    Err(_) => {
                        self.streams_closed = true;
                        // Fold the final value once even if closure and the
                        // last publication became observable together.
                        self.stream_wake_pending = true;
                    }
                }
            }
            // Close the race where lifecycle was published between the first
            // queue check and observing the watch revision.
            self.buffer_lifecycle();

            if let Some(sequence) = self.pending_lifecycle.as_ref().map(|event| event.sequence) {
                if self.stream_wake_pending
                    && let Some(batch) = self.take_stream_batch(Some(sequence))
                {
                    return Ok(SessionUpdate::Streams(batch));
                }
                let event = self
                    .pending_lifecycle
                    .take()
                    .expect("the pending lifecycle sequence came from this event");
                return Ok(self.lifecycle_update(event));
            }

            if self.stream_wake_pending {
                if let Some(batch) = self.take_stream_batch(None) {
                    return Ok(SessionUpdate::Streams(batch));
                }
                // Fenced or unchanged states make an empty batch. Keep
                // looking so closure is reported only after both sources are
                // drained.
                continue;
            }

            return if self.lifecycle_closed
                && self.pending_lifecycle.is_none()
                && self.streams_drained
            {
                Err(mpsc::error::TryRecvError::Disconnected)
            } else {
                Err(mpsc::error::TryRecvError::Empty)
            };
        }
    }

    pub(super) fn buffer_lifecycle(&mut self) {
        if self.lifecycle_closed || self.pending_lifecycle.is_some() {
            return;
        }
        match self.lifecycle.try_recv() {
            Ok(event) => self.pending_lifecycle = Some(event),
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.lifecycle_closed = true;
                self.streams_closed = true;
                self.stream_wake_pending = true;
            }
        }
    }

    pub(super) fn lifecycle_update(&mut self, event: SequencedSessionEvent) -> SessionUpdate {
        let existing = hosted_activity_update(&event.event, &mut self.hosted_actions);
        match &event.event {
            SessionEvent::WebSearchUpdated { .. } | SessionEvent::NetworkStatus { .. } => {}
            SessionEvent::AgentRunUpdated {
                event: AgentRunEvent::ResponseDisplay { .. },
                ..
            } => {}
            SessionEvent::AgentRunUpdated { .. } if existing => {}
            SessionEvent::SubtaskSession { event, .. }
                if matches!(
                    event.as_ref(),
                    SessionEvent::WebSearchUpdated { .. } | SessionEvent::NetworkStatus { .. }
                ) => {}
            SessionEvent::SubtaskSession { id, .. } | SessionEvent::SubtaskStatus { id, .. } => {
                let fence = self.subtask_fences.entry(id.clone()).or_default();
                *fence = (*fence).max(event.sequence);
            }
            SessionEvent::AgentRunUpdated { agent_run_id, .. } => {
                let fence = self
                    .agent_run_fences
                    .entry(agent_run_id.clone())
                    .or_default();
                *fence = (*fence).max(event.sequence);
                self.prune_agent_previews(agent_run_id, event.sequence);
            }
            SessionEvent::AgentRunFinished { outcome, .. } => {
                let fence = self
                    .agent_run_fences
                    .entry(outcome.descriptor.id.clone())
                    .or_default();
                *fence = (*fence).max(event.sequence);
                self.prune_agent_previews(&outcome.descriptor.id, event.sequence);
            }
            _ => self.root_fence = self.root_fence.max(event.sequence),
        }
        SessionUpdate::Lifecycle(event.event)
    }

    pub(super) fn prune_agent_previews(&self, agent_run_id: &AgentRunId, through_sequence: u64) {
        self.stream_cleanup.send_modify(|snapshot| {
            let active = snapshot.active_agent_runs.get(agent_run_id).cloned();
            snapshot.agent_runs.retain(|key, state| {
                &key.agent_run_id != agent_run_id
                    || state.revision > through_sequence
                    || active.as_ref() == Some(key)
            });
        });
    }

    pub(super) fn take_stream_batch(
        &mut self,
        before_sequence: Option<u64>,
    ) -> Option<SessionStreamBatch> {
        let snapshot = self.streams.borrow_and_update();
        let mut batch = SessionStreamBatch::default();
        let mut deferred = false;
        let before_boundary =
            |revision: u64| before_sequence.is_none_or(|sequence| revision < sequence);

        if let Some(state) = &snapshot.root
            && state.revision > self.root_fence
            && state.revision > self.root_emitted
        {
            if before_sequence.is_none() {
                self.root_emitted = state.revision;
                batch.root = Some(state.clone());
            } else {
                deferred = true;
            }
        }

        for (id, state) in &snapshot.subtasks {
            let fence = self.subtask_fences.get(id).copied().unwrap_or_default();
            let emitted = self.subtask_emitted.get(id).copied().unwrap_or_default();
            if state.revision > fence && state.revision > emitted {
                if before_sequence.is_none() {
                    self.subtask_emitted.insert(id.clone(), state.revision);
                    batch.subtasks.insert(id.clone(), state.clone());
                } else {
                    deferred = true;
                }
            }
        }

        let mut agent_runs = Vec::new();
        for (key, state) in &snapshot.agent_runs {
            let fence = self
                .agent_run_fences
                .get(&key.agent_run_id)
                .copied()
                .unwrap_or_default();
            let emitted = self.agent_run_emitted.get(key).copied().unwrap_or_default();
            if state.revision > fence && state.revision > emitted {
                if before_boundary(state.revision) {
                    self.agent_run_emitted.insert(key.clone(), state.revision);
                    agent_runs.push(state.clone());
                } else {
                    deferred = true;
                }
            }
        }
        agent_runs.sort_by_key(|state| state.revision);
        batch.agent_runs = agent_runs;

        self.stream_wake_pending = deferred;
        if self.streams_closed && !deferred {
            self.streams_drained = true;
        }

        (!batch.is_empty()).then_some(batch)
    }
}

pub(super) fn agent_run_boundary(event: &SessionEvent) -> Option<&AgentRunId> {
    match event {
        SessionEvent::AgentRunUpdated { agent_run_id, .. } => Some(agent_run_id),
        SessionEvent::AgentRunFinished { outcome, .. } => Some(&outcome.descriptor.id),
        _ => None,
    }
}

/// Lossless semantic/lifecycle updates fence the coalesced message preview.
/// Raw protocol diagnostics accompany every ACP token and must not clear it,
/// or a token stream degenerates into one independent presentation block per
/// word. The receiver still sequences protocol events through
/// [`agent_run_boundary`]; this helper controls only preview accumulation.
pub(super) fn agent_run_preview_boundary(event: &SessionEvent) -> Option<&AgentRunId> {
    match event {
        SessionEvent::AgentRunUpdated {
            event: AgentRunEvent::ResponseDisplay { .. },
            ..
        } => None,
        SessionEvent::AgentRunUpdated {
            event: AgentRunEvent::Protocol { .. },
            ..
        } => None,
        event => agent_run_boundary(event),
    }
}

fn hosted_activity_update(
    event: &SessionEvent,
    known: &mut std::collections::HashSet<(AgentRunId, String)>,
) -> bool {
    let SessionEvent::AgentRunUpdated {
        agent_run_id,
        event,
        ..
    } = event
    else {
        return false;
    };
    if matches!(
        event,
        AgentRunEvent::ReplayBoundary
            | AgentRunEvent::Prompt {
                continuation: false,
                ..
            }
    ) {
        known.retain(|(agent, _)| agent != agent_run_id);
        return false;
    }
    let id = match event {
        AgentRunEvent::ToolCall { id, .. } | AgentRunEvent::ToolCallUpdate { id, .. } => id,
        _ => return false,
    };
    let key = (agent_run_id.clone(), id.clone());
    if known.contains(&key) {
        return true;
    }
    if event.is_hosted_search_activity() {
        known.insert(key);
    }
    false
}

pub(super) fn accumulate_agent_preview(
    previous: Option<&AgentRunEvent>,
    next: AgentRunEvent,
) -> AgentRunEvent {
    match (previous, next) {
        (
            Some(AgentRunEvent::AgentMessage {
                text: previous,
                message_id: previous_id,
            }),
            AgentRunEvent::AgentMessage { text, message_id },
        ) if previous_id == &message_id => AgentRunEvent::AgentMessage {
            text: format!("{previous}{text}"),
            message_id,
        },
        (
            Some(AgentRunEvent::Thought {
                text: previous,
                message_id: previous_id,
            }),
            AgentRunEvent::Thought { text, message_id },
        ) if previous_id == &message_id => AgentRunEvent::Thought {
            text: format!("{previous}{text}"),
            message_id,
        },
        (_, next) => next,
    }
}

pub(super) fn agent_preview_segments_match(left: &AgentRunEvent, right: &AgentRunEvent) -> bool {
    match (left, right) {
        (
            AgentRunEvent::AgentMessage {
                message_id: left, ..
            },
            AgentRunEvent::AgentMessage {
                message_id: right, ..
            },
        )
        | (
            AgentRunEvent::Thought {
                message_id: left, ..
            },
            AgentRunEvent::Thought {
                message_id: right, ..
            },
        ) => left == right,
        _ => false,
    }
}

/// Completion transport, shared by frontend diagnostics and provider recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkTransport {
    WebSocket,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkStatus {
    AttemptStarted,
    Connecting,
    AwaitingResponse,
    Quiet {
        idle_for: std::time::Duration,
        retry_in: std::time::Duration,
    },
    ProgressResumed,
}

/// Core-owned reliable checkpoint handoff, not a frontend event. The provider
/// cannot clear a finalized attempt or retry until persistence acknowledges it.
pub struct AttemptCheckpoint {
    pub attempt: crate::WebSearchAttemptRecord,
    pub ack: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// A provider's narrow channel for streaming progress.
#[derive(Clone)]
pub struct ProgressReporter {
    pub(super) events: SessionEventSender,
    pub(super) turn: TurnContext,
    pub(super) profile: ModelProfileRef,
    pub(super) model_role: ModelRole,
    pub(super) input_token_limit: u64,
    pub(super) context_window_tokens: u64,
    pub(super) suppressed: bool,
    call: usize,
    web_search: Arc<Mutex<Vec<crate::WebSearchAttemptRecord>>>,
    checkpoints: Option<mpsc::UnboundedSender<AttemptCheckpoint>>,
}

impl ProgressReporter {
    /// Construct a reporter for adapter-level tests and standalone provider
    /// calls that do not have an engine-assigned turn identity.
    pub fn new(events: SessionEventSender) -> Self {
        let policy = CompactionPolicy::default();
        Self::for_turn(
            events,
            TurnContext::new(TurnId::new(0), SessionMode::Build, CancellationToken::new()),
            ModelRole::Build,
            policy.for_role(ModelRole::Build),
        )
    }

    pub fn for_turn(
        events: SessionEventSender,
        turn: TurnContext,
        model_role: ModelRole,
        context: &ModelContextPolicy,
    ) -> Self {
        Self {
            events,
            turn,
            profile: context.profile.clone(),
            model_role,
            input_token_limit: context.input_token_limit,
            context_window_tokens: context.context_window_tokens,
            suppressed: false,
            web_search: Arc::new(Mutex::new(Vec::new())),
            checkpoints: None,
            call: 1,
        }
    }

    /// Reporter for synthetic compaction inference. Cancellation remains
    /// active, while snapshots, retries, and usage stay out of normal UI.
    pub fn silent_for_turn(
        events: SessionEventSender,
        turn: TurnContext,
        model_role: ModelRole,
        context: &ModelContextPolicy,
    ) -> Self {
        Self {
            events,
            turn,
            profile: context.profile.clone(),
            model_role,
            input_token_limit: context.input_token_limit,
            context_window_tokens: context.context_window_tokens,
            suppressed: true,
            web_search: Arc::new(Mutex::new(Vec::new())),
            checkpoints: None,
            call: 1,
        }
    }

    pub fn with_model_call(mut self, call: usize) -> Self {
        self.call = call;
        self
    }

    pub async fn network_status(
        &self,
        attempt: usize,
        max_attempts: usize,
        transport: NetworkTransport,
        status: NetworkStatus,
    ) {
        if !self.suppressed {
            let _ = self
                .events
                .send(SessionEvent::NetworkStatus {
                    turn_id: self.turn.id,
                    call: self.call,
                    attempt,
                    max_attempts,
                    transport,
                    status,
                })
                .await;
        }
    }

    /// Collect before delivery, so cancellation during channel backpressure
    /// cannot erase an already observed update. No lock crosses an await.
    pub fn collect_web_search(&self, attempt: crate::WebSearchAttemptRecord) {
        let mut collected = self
            .web_search
            .lock()
            .expect("web search collector poisoned");
        if let Some(existing) = collected
            .iter_mut()
            .find(|existing| existing.id == attempt.id)
        {
            if attempt.revision >= existing.revision {
                *existing = attempt;
            }
        } else {
            collected.push(attempt);
        }
    }

    pub fn with_checkpoints(mut self, sender: mpsc::UnboundedSender<AttemptCheckpoint>) -> Self {
        self.checkpoints = Some(sender);
        self
    }

    pub async fn checkpoint_web_search(
        &self,
        attempt: crate::WebSearchAttemptRecord,
    ) -> anyhow::Result<()> {
        self.collect_web_search(attempt.clone());
        if self.suppressed || !attempt.has_display() {
            return Ok(());
        }
        if let Some(sender) = &self.checkpoints {
            let (ack, received) = tokio::sync::oneshot::channel();
            sender
                .send(AttemptCheckpoint { attempt, ack })
                .map_err(|_| anyhow::anyhow!("attempt checkpoint receiver closed"))?;
            received
                .await
                .map_err(|_| anyhow::anyhow!("attempt checkpoint acknowledgement lost"))?
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    pub fn stream_snapshot(&self, snapshot: crate::AssistantStreamSnapshot) {
        if let Some(attempt) = &snapshot.attempt {
            self.collect_web_search(attempt.clone());
        }
        if !self.suppressed {
            self.events.stream_snapshot(self.turn.id, snapshot);
        }
    }

    pub async fn web_search_updated(&self, attempt: crate::WebSearchAttemptRecord) {
        self.collect_web_search(attempt.clone());
        if !self.suppressed {
            let _ = self
                .events
                .send(SessionEvent::WebSearchUpdated {
                    turn_id: self.turn.id,
                    attempt,
                })
                .await;
        }
    }

    /// Called by core only after releasing the provider future and its request.
    pub fn drain_web_search(
        &self,
        outcome: crate::WebSearchAttemptOutcome,
    ) -> Vec<crate::WebSearchAttemptRecord> {
        let mut attempts = std::mem::take(
            &mut *self
                .web_search
                .lock()
                .expect("web search collector poisoned"),
        );
        for attempt in &mut attempts {
            attempt.finish(outcome);
        }
        attempts
    }

    pub fn events(&self) -> &SessionEventSender {
        &self.events
    }

    pub fn turn(&self) -> &TurnContext {
        &self.turn
    }

    /// Publish a complete, displayable streaming snapshot.
    pub fn stream_updated(&self, message: Message) {
        if self.suppressed {
            return;
        }
        self.events.stream_updated(self.turn.id, message);
    }

    /// Explicitly discard the frontend's current streaming snapshot.
    pub fn stream_cleared(&self) {
        if self.suppressed {
            return;
        }
        self.events.stream_cleared(self.turn.id);
    }

    /// Publish the completed response's token accounting.
    pub async fn usage_updated(&self, usage: TokenUsage) {
        if self.suppressed {
            return;
        }
        let _ = self
            .events
            .send(SessionEvent::UsageUpdated {
                turn_id: self.turn.id,
                usage,
                profile: self.profile.clone(),
                model_role: self.model_role,
                input_token_limit: self.input_token_limit,
                context_window_tokens: self.context_window_tokens,
            })
            .await;
    }

    /// Announce that the provider is recovering from a lost connection and
    /// the turn is being retried, not failed.
    pub async fn retrying(
        &self,
        attempt: usize,
        max_attempts: usize,
        retry_after: std::time::Duration,
        error: String,
    ) {
        if self.suppressed {
            return;
        }
        let _ = self
            .events
            .send(SessionEvent::TurnRetrying {
                turn_id: self.turn.id,
                call: self.call,
                attempt,
                max_attempts,
                retry_after,
                error,
            })
            .await;
    }
}
