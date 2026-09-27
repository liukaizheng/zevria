//! Ordered worker-log persistence and publication.

use super::*;

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
        let io_path = path.clone();
        let (writer, projection) = tokio::task::spawn_blocking(move || {
            if resume && io_path.exists() {
                let projection = load_agent_run_projection(&io_path)?;
                if projection.header.as_ref() != Some(&header) {
                    anyhow::bail!(
                        "worker transcript identity does not match the ensemble recovery record"
                    );
                }
                Ok((AgentRunTranscriptWriter::append_to(io_path)?, projection))
            } else {
                Ok((
                    AgentRunTranscriptWriter::create(io_path, header)?,
                    AgentRunProjection::default(),
                ))
            }
        })
        .await
        .context("agent-run log initialization task failed")??;
        let evidence = Arc::new(Mutex::new(WorkerEvidenceState::from_projection(
            &projection,
        )));
        let failure = Arc::new(Mutex::new(None));
        let publication = RunLogPublication {
            events,
            turn_id: turn.id,
            ensemble_run_id: start.run_id.clone(),
            agent_run_id: descriptor.id.clone(),
        };
        let writer = spawn_run_log_writer(writer, publication, failure.clone());
        Ok((
            Self {
                path,
                writer,
                failure,
                evidence,
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
        self.writer
            .send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
                record: AgentRunTranscriptRecord::Event {
                    event: event.clone(),
                },
                publication: Some(event.clone()),
                force_sync,
                acknowledgement,
            })))
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?;
        if let Some(receiver) = receiver {
            receiver
                .await
                .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?
                .map_err(anyhow::Error::msg)?;
        }
        self.evidence
            .lock()
            .expect("worker evidence lock poisoned")
            .apply(&event);
        let review = self
            .review_publication
            .lock()
            .expect("review publication poisoned")
            .clone();
        if let Some(review) = review {
            review.observe(&event).await?;
        }
        Ok(())
    }

    pub(super) async fn append_outcome(&self, outcome: AgentRunOutcome) -> anyhow::Result<()> {
        let (acknowledgement, receiver) = oneshot::channel();
        self.writer
            .send(RunLogWriteCommand::Record(Box::new(RunLogRecordCommand {
                record: AgentRunTranscriptRecord::Outcome { outcome },
                publication: None,
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
            .send(RunLogWriteCommand::Barrier { acknowledgement })
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?;
        receiver
            .await
            .map_err(|_| anyhow::anyhow!(self.writer_failure("agent-run writer stopped")))?
            .map_err(anyhow::Error::msg)
    }

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

    pub(super) fn repair(&self) -> Option<AgentRunRepair> {
        self.evidence
            .lock()
            .expect("worker evidence lock poisoned")
            .repair
            .clone()
    }

    pub(super) fn has_plan_proof(&self) -> bool {
        self.evidence
            .lock()
            .expect("worker evidence lock poisoned")
            .has_plan_proof()
    }

    pub(super) fn denied_permission_repair(&self) -> Option<AgentRunRepair> {
        self.evidence
            .lock()
            .expect("worker evidence lock poisoned")
            .denied_permission_repair()
    }

    pub(super) fn writer_failure(&self, fallback: &str) -> String {
        self.failure().unwrap_or_else(|| fallback.to_string())
    }
}

pub(super) fn spawn_run_log_writer<W: RunLogWriter>(
    mut writer: W,
    publication: RunLogPublication,
    failure: Arc<Mutex<Option<String>>>,
) -> mpsc::Sender<RunLogWriteCommand> {
    let (sender, mut receiver) = mpsc::channel(RUN_LOG_QUEUE_CAPACITY);
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        while let Some(first) = receiver.blocking_recv() {
            let mut publications = Vec::new();
            let mut acknowledgements = Vec::new();
            let mut bytes = 0usize;
            let mut force_sync = false;
            let first_result = process_run_log_command(
                &mut writer,
                first,
                &mut publications,
                &mut acknowledgements,
                &mut bytes,
                &mut force_sync,
            );
            if let Err(error) = first_result {
                fail_run_log_writer(&failure, &mut acknowledgements, error);
                break;
            }

            if !force_sync && bytes < RUN_LOG_BATCH_BYTES {
                std::thread::sleep(RUN_LOG_BATCH_WINDOW);
                while bytes < RUN_LOG_BATCH_BYTES && !force_sync {
                    let command = match receiver.try_recv() {
                        Ok(command) => command,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    };
                    if let Err(error) = process_run_log_command(
                        &mut writer,
                        command,
                        &mut publications,
                        &mut acknowledgements,
                        &mut bytes,
                        &mut force_sync,
                    ) {
                        fail_run_log_writer(&failure, &mut acknowledgements, error);
                        return;
                    }
                }
            }

            if bytes > 0
                && let Err(error) = writer.sync()
            {
                fail_run_log_writer(&failure, &mut acknowledgements, error);
                break;
            }
            for event in publications {
                let _ = runtime.block_on(publication.events.send(SessionEvent::AgentRunUpdated {
                    turn_id: publication.turn_id,
                    ensemble_run_id: publication.ensemble_run_id.clone(),
                    agent_run_id: publication.agent_run_id.clone(),
                    event,
                }));
            }
            for acknowledgement in acknowledgements {
                let _ = acknowledgement.send(Ok(()));
            }
        }
    });
    sender
}

pub(super) fn process_run_log_command<W: RunLogWriter>(
    writer: &mut W,
    command: RunLogWriteCommand,
    publications: &mut Vec<AgentRunEvent>,
    acknowledgements: &mut Vec<oneshot::Sender<Result<(), String>>>,
    bytes: &mut usize,
    force_sync: &mut bool,
) -> anyhow::Result<()> {
    match command {
        RunLogWriteCommand::Record(command) => {
            let RunLogRecordCommand {
                record,
                publication,
                force_sync: record_force_sync,
                acknowledgement,
            } = *command;
            if let Some(acknowledgement) = acknowledgement {
                acknowledgements.push(acknowledgement);
            }
            *bytes = bytes.saturating_add(writer.append_buffered(&record)?);
            if let Some(publication) = publication {
                publications.push(publication);
            }
            *force_sync |= record_force_sync;
        }
        RunLogWriteCommand::Barrier { acknowledgement } => {
            acknowledgements.push(acknowledgement);
            *force_sync = true;
        }
    }
    Ok(())
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
