//! Ordered worker-log persistence and publication.

use super::interactive::ReviewPublication;
use anyhow::Context as _;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zevria_session_api::{SessionEvent, SessionEventSender, TurnContext};
use zevria_transcript::{
    AGENT_RUN_TRANSCRIPT_VERSION, AgentRunProjection, AgentRunTranscriptHeader,
    AgentRunTranscriptRecord, AgentRunTranscriptWriter,
};
#[cfg(test)]
use zevria_workflow::AgentStructuredPlan;
use zevria_workflow::{
    AgentElicitationOutcome, AgentRunDescriptor, AgentRunEvent, AgentRunId, AgentRunOutcome,
    AgentRunRepair, AgentRunStatus, EnsembleRunId,
};

pub(super) const RUN_LOG_QUEUE_CAPACITY: usize = 1_024;
pub(super) const RUN_LOG_BATCH_BYTES: usize = 64 * 1_024;
pub(super) const RUN_LOG_BATCH_WINDOW: Duration = Duration::from_millis(25);

#[derive(Clone)]
pub(super) struct RunLog {
    #[cfg(test)]
    pub(super) path: PathBuf,
    pub(super) writer: RunLogWriterHandle,
    pub(super) failure: Arc<Mutex<Option<String>>>,
    pub(super) last_permission: Arc<Mutex<Option<PersistedPermissionDecision>>>,
    pub(super) ignore_recovered_repair: Arc<AtomicBool>,
    pub(super) review_publication: Arc<Mutex<Option<ReviewPublication>>>,
    pub(super) event_order: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Clone)]
pub(super) struct PersistedPermissionDecision {
    pub(super) tool_kind: Option<String>,
    pub(super) decision: String,
    pub(super) option_id: Option<String>,
}

impl PersistedPermissionDecision {
    fn repair(&self) -> Option<AgentRunRepair> {
        matches!(self.decision.as_str(), "reject_once" | "reject_always").then(|| {
            AgentRunRepair::HostDeniedPermission {
                tool_kind: self.tool_kind.clone(),
                option_id: self.option_id.clone(),
            }
        })
    }
}

#[derive(Clone)]
pub(super) struct RunLogWriterHandle {
    pub(super) sender: mpsc::Sender<RunLogWriteCommand>,
    pub(super) projection: Arc<Mutex<AgentRunProjection>>,
}

pub(super) enum RunLogWriteCommand {
    Event {
        event: Arc<AgentRunEvent>,
        acknowledgement: Option<oneshot::Sender<Result<(), String>>>,
    },
    Record(Box<RunLogRecordCommand>),
    Barrier {
        acknowledgement: oneshot::Sender<Result<(), String>>,
    },
}

pub(super) struct RunLogRecordCommand {
    pub(super) record: AgentRunTranscriptRecord,
    pub(super) publish: bool,
    pub(super) force_sync: bool,
    pub(super) acknowledgement: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Clone)]
pub(super) struct RunLogPublication {
    pub(super) events: SessionEventSender,
    pub(super) turn_id: zevria_foundation::TurnId,
    pub(super) ensemble_run_id: EnsembleRunId,
    pub(super) agent_run_id: AgentRunId,
}

pub(super) trait RunLogWriter: Send + 'static {
    fn append_buffered(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<usize>;
    fn sync(&mut self) -> anyhow::Result<()>;
}

impl RunLogWriter for AgentRunTranscriptWriter {
    fn append_buffered(&mut self, record: &AgentRunTranscriptRecord) -> anyhow::Result<usize> {
        AgentRunTranscriptWriter::append_buffered(self, record)
    }

    fn sync(&mut self) -> anyhow::Result<()> {
        AgentRunTranscriptWriter::sync(self)
    }
}

use zevria_session_api::EnsembleEvent;

impl RunLog {
    pub(super) async fn open(
        path: PathBuf,
        start: &zevria_workflow::EnsembleStart,
        descriptor: &AgentRunDescriptor,
        resume: bool,
        events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<(Self, AgentRunProjection)> {
        let header = AgentRunTranscriptHeader {
            version: AGENT_RUN_TRANSCRIPT_VERSION,
            ensemble_run_id: start.run_id.clone(),
            workflow: start.workflow,
            descriptor: descriptor.clone(),
            prompt: start.prompt.clone(),
        };
        #[cfg(test)]
        let log_path = path.clone();
        let io_path = path;
        let (writer, projection) = tokio::task::spawn_blocking(move || {
            if resume && io_path.exists() {
                AgentRunTranscriptWriter::reopen(io_path, &header)
            } else {
                let record = AgentRunTranscriptRecord::Header { header };
                let projection = AgentRunProjection::from_records(std::slice::from_ref(&record));
                let AgentRunTranscriptRecord::Header { header } = record else {
                    unreachable!()
                };
                Ok((
                    AgentRunTranscriptWriter::create(io_path, header)?,
                    projection,
                ))
            }
        })
        .await
        .context("agent-run log initialization task failed")??;
        let failure = Arc::new(Mutex::new(None));
        let publication = RunLogPublication {
            events,
            turn_id: turn.id,
            ensemble_run_id: start.run_id.clone(),
            agent_run_id: descriptor.id.clone(),
        };
        let writer =
            spawn_seeded_run_log_writer(writer, publication, failure.clone(), projection.clone());
        Ok((
            Self {
                #[cfg(test)]
                path: log_path,
                writer,
                failure,
                last_permission: Default::default(),
                ignore_recovered_repair: Default::default(),
                review_publication: Arc::new(Mutex::new(None)),
                event_order: Arc::new(tokio::sync::Mutex::new(())),
            },
            projection,
        ))
    }

    pub(super) async fn emit(&self, event: AgentRunEvent) -> anyhow::Result<()> {
        // Keep durable evidence and its root observation in identical order,
        // including concurrently delivered ACP callbacks.
        let _order = self.event_order.lock().await;
        let force_sync = run_log_event_requires_sync(&event);
        let (acknowledgement, receiver) = if force_sync {
            let (sender, receiver) = oneshot::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let review = if matches!(
            &event,
            AgentRunEvent::ReplayBoundary
                | AgentRunEvent::Review { .. }
                | AgentRunEvent::Prompt { .. }
                | AgentRunEvent::SessionEstablished { .. }
                | AgentRunEvent::Plan { .. }
                | AgentRunEvent::NativePlanCaptured { .. }
                | AgentRunEvent::PlanRemoved { .. }
                | AgentRunEvent::Elicitation {
                    outcome: AgentElicitationOutcome::Accepted,
                    ..
                }
        ) {
            self.review_publication
                .lock()
                .expect("review publication poisoned")
                .clone()
        } else {
            None
        };
        let permission = match &event {
            AgentRunEvent::Permission {
                tool_kind,
                decision,
                option_id,
            } => Some(Some(PersistedPermissionDecision {
                tool_kind: tool_kind.clone(),
                decision: decision.clone(),
                option_id: option_id.clone(),
            })),
            // ToolCallUpdate alone can be the terminal response to a denial;
            // only a subsequent semantic action makes that inference stale.
            AgentRunEvent::Prompt { .. }
            | AgentRunEvent::SessionEstablished { .. }
            | AgentRunEvent::UserMessage { .. }
            | AgentRunEvent::AgentMessage { .. }
            | AgentRunEvent::Thought { .. }
            | AgentRunEvent::ToolCall { .. }
            | AgentRunEvent::Plan { .. }
            | AgentRunEvent::NativePlanCaptured { .. }
            | AgentRunEvent::PlanRemoved { .. }
            | AgentRunEvent::Elicitation { .. }
            | AgentRunEvent::ReplayBoundary
            | AgentRunEvent::Unsupported { .. } => Some(None),
            _ => None,
        };
        let new_repair_boundary = matches!(
            &event,
            AgentRunEvent::Prompt {
                continuation: false,
                ..
            } | AgentRunEvent::Prompt {
                repair: Some(_),
                ..
            }
        );
        let event = Arc::new(event);
        let observed = review.map(|review| (review, event.clone()));
        self.writer
            .sender
            .send(RunLogWriteCommand::Event {
                event,
                acknowledgement,
            })
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?;
        if let Some(receiver) = receiver {
            receiver
                .await
                .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?
                .map_err(anyhow::Error::msg)?;
        }
        if new_repair_boundary {
            self.ignore_recovered_repair
                .store(false, AtomicOrdering::Release);
        }
        if let Some(permission) = permission {
            *self
                .last_permission
                .lock()
                .expect("permission evidence lock poisoned") = permission;
        }
        if let Some((review, event)) = observed {
            review.observe(&event).await?;
        }
        Ok(())
    }

    pub(super) async fn append_outcome(&self, outcome: AgentRunOutcome) -> anyhow::Result<()> {
        let (acknowledgement, receiver) = oneshot::channel();
        self.writer
            .sender
            .send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
                record: AgentRunTranscriptRecord::Outcome { outcome },
                publish: false,
                force_sync: true,
                acknowledgement: Some(acknowledgement),
            })))
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?;
        receiver
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?
            .map_err(anyhow::Error::msg)
    }

    pub(super) async fn barrier(&self) -> anyhow::Result<()> {
        let (acknowledgement, receiver) = oneshot::channel();
        self.writer
            .sender
            .send(RunLogWriteCommand::Barrier { acknowledgement })
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?;
        receiver
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?
            .map_err(anyhow::Error::msg)
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn set_failure(&self, error: String) {
        let mut failure = self
            .failure
            .lock()
            .expect("agent-run failure lock poisoned");
        failure.get_or_insert(error);
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .expect("agent-run failure lock poisoned")
            .clone()
    }

    pub(super) fn forget_repair(&self) {
        self.ignore_recovered_repair
            .store(true, AtomicOrdering::Release);
    }

    pub(super) fn repair(&self) -> Option<AgentRunRepair> {
        if self.ignore_recovered_repair.load(AtomicOrdering::Acquire) {
            return None;
        }
        self.writer
            .projection
            .lock()
            .expect("worker projection lock poisoned")
            .repair
            .clone()
    }

    #[cfg(test)]
    pub(super) fn has_plan_proof(&self) -> bool {
        self.writer
            .projection
            .lock()
            .expect("worker projection lock poisoned")
            .plan
            .as_ref()
            .is_some_and(AgentStructuredPlan::has_markdown_proof)
    }

    pub(super) fn denied_permission_repair(&self) -> Option<AgentRunRepair> {
        self.last_permission
            .lock()
            .expect("permission evidence lock poisoned")
            .as_ref()
            .and_then(PersistedPermissionDecision::repair)
    }

    /// Read only the evidence required by an outcome. Callers await a barrier
    /// first; a failed batch is never included, even if its write partly succeeded.
    pub(super) fn outcome_evidence(
        &self,
        descriptor: AgentRunDescriptor,
        status: AgentRunStatus,
        partial: bool,
        failure: Option<String>,
    ) -> AgentRunOutcome {
        let evidence = self
            .writer
            .projection
            .lock()
            .expect("worker projection lock poisoned");
        AgentRunOutcome {
            descriptor,
            status,
            partial,
            failure,
            confirmation: None,
            report: evidence.report.clone(),
            plan: evidence.plan.clone(),
            usage: evidence.usage.clone(),
            acp_session_id: evidence.acp_session_id.clone(),
            user_decisions: evidence.user_decisions.clone(),
            decision_ids: evidence.decision_ids.clone(),
            unavailable_decisions: evidence.unavailable_decisions.clone(),
        }
    }

    pub(super) fn writer_failure(&self, fallback: &str) -> String {
        self.failure().unwrap_or_else(|| fallback.to_string())
    }
}

#[cfg(test)]
pub(super) fn spawn_run_log_writer<W: RunLogWriter>(
    writer: W,
    publication: RunLogPublication,
    failure: Arc<Mutex<Option<String>>>,
) -> RunLogWriterHandle {
    spawn_seeded_run_log_writer(writer, publication, failure, AgentRunProjection::default())
}

fn spawn_seeded_run_log_writer<W: RunLogWriter>(
    mut writer: W,
    publication: RunLogPublication,
    failure: Arc<Mutex<Option<String>>>,
    mut seed: AgentRunProjection,
) -> RunLogWriterHandle {
    let (sender, mut receiver) = mpsc::channel(RUN_LOG_QUEUE_CAPACITY);
    let runtime = tokio::runtime::Handle::current();
    // The transcript writer already validates review transitions. This view
    // only serves report, usage and outcome evidence after durable writes.
    seed.review = None;
    seed.review_error = None;
    let projection = Arc::new(Mutex::new(seed));
    let durable = projection.clone();
    tokio::task::spawn_blocking(move || {
        let mut records = Vec::new();
        let mut acknowledgements = Vec::new();
        while let Some(first) = receiver.blocking_recv() {
            let mut bytes = 0usize;
            let deadline = tokio::time::Instant::now() + RUN_LOG_BATCH_WINDOW;
            let mut command = first;
            loop {
                let force = match command {
                    RunLogWriteCommand::Event {
                        event,
                        acknowledgement,
                    } => {
                        let force = acknowledgement.is_some();
                        acknowledgements.extend(acknowledgement);
                        records.push((
                            AgentRunTranscriptRecord::Event {
                                event: Arc::unwrap_or_clone(event),
                            },
                            true,
                        ));
                        force
                    }
                    RunLogWriteCommand::Record(command) => {
                        let RunLogRecordCommand {
                            record,
                            publish,
                            force_sync,
                            acknowledgement,
                        } = *command;
                        acknowledgements.extend(acknowledgement);
                        records.push((record, publish));
                        force_sync
                    }
                    RunLogWriteCommand::Barrier { acknowledgement } => {
                        acknowledgements.push(acknowledgement);
                        break;
                    }
                };
                match writer.append_buffered(&records.last().expect("record staged").0) {
                    Ok(written) => bytes = bytes.saturating_add(written),
                    Err(error) => {
                        fail_run_log_writer(&failure, &mut acknowledgements, error);
                        return;
                    }
                }
                if force || bytes >= RUN_LOG_BATCH_BYTES {
                    break;
                }
                // The receiver is the wakeup: a barrier or forced record ends a
                // batch immediately instead of waiting out the batching window.
                let next = runtime.block_on(async {
                    tokio::time::timeout_at(deadline, receiver.recv())
                        .await
                        .ok()
                        .flatten()
                });
                let Some(next) = next else { break };
                command = next;
            }
            if !records.is_empty()
                && let Err(error) = writer.sync()
            {
                fail_run_log_writer(&failure, &mut acknowledgements, error);
                break;
            }
            if !records.is_empty() {
                let mut durable = durable.lock().expect("worker projection lock poisoned");
                for (record, _) in &records {
                    durable.apply_display(record);
                }
            }
            for (record, publish) in records.drain(..) {
                if publish && let AgentRunTranscriptRecord::Event { event } = record {
                    let _ = runtime.block_on(publication.events.send(SessionEvent::Ensemble(
                        EnsembleEvent::AgentRunUpdated {
                            turn_id: publication.turn_id,
                            ensemble_run_id: publication.ensemble_run_id.clone(),
                            agent_run_id: publication.agent_run_id.clone(),
                            event: Box::new(event),
                        },
                    )));
                }
            }
            for acknowledgement in acknowledgements.drain(..) {
                let _ = acknowledgement.send(Ok(()));
            }
        }
    });
    RunLogWriterHandle { sender, projection }
}

pub(super) fn fail_run_log_writer(
    failure: &Arc<Mutex<Option<String>>>,
    acknowledgements: &mut Vec<oneshot::Sender<Result<(), String>>>,
    error: anyhow::Error,
) {
    let error = format!("{error:#}");
    failure
        .lock()
        .expect("agent-run failure lock poisoned")
        .get_or_insert_with(|| error.clone());
    for acknowledgement in acknowledgements.drain(..) {
        let _ = acknowledgement.send(Err(error.clone()));
    }
}

pub(super) fn run_log_event_requires_sync(event: &AgentRunEvent) -> bool {
    match event {
        AgentRunEvent::Review { .. }
        | AgentRunEvent::SessionAllocated { .. }
        | AgentRunEvent::SessionEstablished { .. }
        | AgentRunEvent::Prompt { .. }
        | AgentRunEvent::Plan { .. }
        | AgentRunEvent::NativePlanCaptured { .. }
        | AgentRunEvent::PlanRemoved { .. }
        | AgentRunEvent::Permission { .. }
        | AgentRunEvent::ReplayBoundary
        | AgentRunEvent::Failure { .. } => true,
        AgentRunEvent::Status { status, .. } => status.is_terminal(),
        AgentRunEvent::Elicitation {
            outcome: AgentElicitationOutcome::Accepted,
            ..
        } => true,
        AgentRunEvent::ResponseDisplay { .. }
        | AgentRunEvent::UserMessage { .. }
        | AgentRunEvent::UserImage { .. }
        | AgentRunEvent::AgentMessage { .. }
        | AgentRunEvent::Thought { .. }
        | AgentRunEvent::ToolCall { .. }
        | AgentRunEvent::ToolCallUpdate { .. }
        | AgentRunEvent::ToolResultMetadata { .. }
        | AgentRunEvent::ModeChanged { .. }
        | AgentRunEvent::ConfigOptionsChanged { .. }
        | AgentRunEvent::SessionInfo { .. }
        | AgentRunEvent::Usage { .. }
        | AgentRunEvent::Elicitation { .. }
        | AgentRunEvent::Stderr { .. }
        | AgentRunEvent::Protocol { .. }
        | AgentRunEvent::Unsupported { .. } => false,
    }
}
