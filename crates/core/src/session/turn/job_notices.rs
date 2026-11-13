//! Append-only completion delivery and engine-owned continuation eligibility.
use crate::session::SessionEngine;
use crate::session::kernel::records::{RecordChange, TurnAnchor, recoverable};
use crate::session::kernel::state::EnginePhase;
use crate::session::turn::execution::{AcceptedKind, AcceptedTurn, Failure, Rejection};
use crate::session::workflow::plan::{PlanSubmissionGate, workflow_transition_for_mode};
use zevria_foundation::policy::{SessionMode, TurnId, TurnPolicy};
use zevria_session_api::event::{ProgressReporter, SessionEvent, SessionEventSender};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::turn::TurnContext;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;

use zevria_foundation::{
    COMMAND_STATUS_TOOL_NAME, CommandJobId, CommandJobNotice, CommandJobNoticeKind,
    CommandJobSnapshot, CommandJobStatus,
};
use zevria_session_api::{PendingCompletion, command_render};

pub(in crate::session) struct JobFollowState {
    pub(in crate::session) limit: u32,
    pub(in crate::session) consecutive: u32,
    pub(in crate::session) floor: TurnId,
    pub(in crate::session) advertised: Option<Option<SessionMode>>,
}
impl Default for JobFollowState {
    fn default() -> Self {
        Self {
            limit: 0,
            consecutive: 0,
            floor: TurnId::new(0),
            advertised: None,
        }
    }
}
impl JobFollowState {
    pub(in crate::session) fn raise_floor(&mut self, floor: TurnId) {
        self.floor = self.floor.max(floor);
    }
    fn is_followed(&self, completion: &PendingCompletion) -> bool {
        completion.origin.is_some_and(|origin| origin >= self.floor)
            && matches!(
                completion.snapshot.status,
                CommandJobStatus::Exited { .. }
                    | CommandJobStatus::TimedOut { .. }
                    | CommandJobStatus::Failed { .. }
            )
    }
    fn render_notice(
        &self,
        kind: CommandJobNoticeKind,
        completions: &[PendingCompletion],
        running: Vec<CommandJobSnapshot>,
    ) -> (CommandJobNotice, Vec<(CommandJobId, bool)>) {
        let mut text = match kind {
            CommandJobNoticeKind::Update => "Command job update: these background jobs finished.",
            CommandJobNoticeKind::Continuation => "Command job update: no new user message was sent. Zevria resumed this session because background jobs you started have finished.",
        }.to_owned();
        let mut remaining = 32 * 1024usize;
        let mut first_followed = true;
        let mut acknowledgements = Vec::new();
        for completion in completions {
            text.push_str("\n\n");
            text.push_str(&command_render::job_line(&completion.snapshot));
            let mut consumed = false;
            if self.is_followed(completion) {
                let (stdout, stderr) = completion.unread_bytes();
                // Lossy UTF-8 can expand each raw byte to three bytes. Include
                // tags, newlines, and the single omission marker in the bound.
                let bound = |bytes: usize| {
                    if bytes == 0 {
                        0
                    } else {
                        bytes
                            .saturating_mul(3)
                            .min(command_render::MAX_OUTPUT_STREAM_BYTES)
                            + 192
                    }
                };
                let upper = bound(stdout).saturating_add(bound(stderr));
                if first_followed || upper <= remaining {
                    let streams = completion.peek_unread();
                    let blocks = command_render::stream_blocks(&streams.0, &streams.1);
                    remaining = remaining.saturating_sub(blocks.len());
                    if !blocks.is_empty() {
                        text.push('\n');
                        text.push_str(&blocks);
                    }
                    consumed = true;
                } else {
                    text.push_str("\nnote: output not included; use command_status to read it");
                }
                first_followed = false;
            }
            acknowledgements.push((completion.snapshot.id.clone(), consumed));
        }
        if !running.is_empty() {
            text.push_str("\n\nStill running:");
            for job in &running {
                text.push('\n');
                text.push_str(&command_render::job_line(job));
            }
        }
        if kind == CommandJobNoticeKind::Continuation {
            text.push_str("\n\nEvaluate these results against the conversation so far. Continue any work they affect; otherwise report them briefly. Do not start unrelated work.");
        }
        (
            CommandJobNotice::new(
                kind,
                completions.iter().map(|c| c.snapshot.clone()).collect(),
                running,
                text,
            ),
            acknowledgements,
        )
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub fn with_job_continuations(mut self, limit: u32) -> Self {
        self.capabilities.job_follow.limit = limit;
        self
    }

    pub(in crate::session) async fn deliver_job_notices(
        &mut self,
        policy: &TurnPolicy,
        gate: &PlanSubmissionGate,
        progress: &ProgressReporter,
    ) -> Result<(), Failure> {
        if !policy.allows_tool(COMMAND_STATUS_TOOL_NAME)
            || gate.candidate.is_some()
            || progress.turn().is_cancelled()
        {
            return Ok(());
        }
        let Some(jobs) = self.capabilities.command_jobs.clone() else {
            return Ok(());
        };
        let completions = jobs.pending_completions();
        if completions.is_empty() {
            return Ok(());
        }
        let (notice, ack) = self.capabilities.job_follow.render_notice(
            CommandJobNoticeKind::Update,
            &completions,
            jobs.running_model_jobs(),
        );
        let persistence =
            recoverable(self.record_completed(TranscriptItem::CommandJobNotice(notice.clone())))?;
        // push_completed keeps completed evidence in memory on degraded writes.
        self.context.invalidate_prepared_count();
        jobs.acknowledge_completions(&ack);
        if let Some(error) = persistence {
            self.persistence_failed(&error, progress.events()).await?;
        }
        let _ = progress
            .events()
            .send(SessionEvent::CommandJobsReported {
                turn_id: progress.turn().id,
                notice,
            })
            .await;
        Ok(())
    }

    fn eligible_continuation_mode(&self) -> Option<SessionMode> {
        let jobs = self.capabilities.command_jobs.as_ref()?;
        let follow = &self.capabilities.job_follow;
        if jobs.is_closed()
            || follow.consecutive >= follow.limit
            || self.conversation.is_read_only()
        {
            return None;
        }
        let mode = self.selected_mode();
        if !self
            .policies
            .policy(mode)
            .allows_tool(COMMAND_STATUS_TOOL_NAME)
            || !matches!(
                workflow_transition_for_mode(self.plan_state().ok()?, mode),
                Ok(None)
            )
            || !jobs
                .pending_completions()
                .iter()
                .any(|completion| follow.is_followed(completion))
        {
            return None;
        }
        Some(mode)
    }

    pub(in crate::session) fn continuation_offer(&self) -> Option<SessionMode> {
        if !matches!(self.phase, EnginePhase::Idle) {
            return None;
        }
        self.eligible_continuation_mode()
    }

    pub(in crate::session) async fn refresh_continuation_offer(
        &mut self,
        events: &SessionEventSender,
    ) {
        if self.capabilities.command_jobs.is_none() {
            return;
        }
        let mode = self.continuation_offer();
        if self.capabilities.job_follow.advertised != Some(mode) {
            self.capabilities.job_follow.advertised = Some(mode);
            let _ = events
                .send(SessionEvent::JobContinuationChanged { mode })
                .await;
        }
    }

    pub(in crate::session) async fn continue_after_jobs(
        &mut self,
        mode: SessionMode,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        let acceptance = self.accept_job_continuation(mode, events, turn).await;
        match acceptance {
            Err(rejection) => {
                self.capabilities
                    .job_follow
                    .raise_floor(TurnId::new(turn.id.get().saturating_add(1)));
                self.finish_rejected(turn, events, rejection).await
            }
            Ok(accepted) => {
                let outcome = self.run_turn(&accepted, events, turn).await;
                self.finish_accepted(&accepted, turn, events, outcome).await
            }
        }
    }

    async fn accept_job_continuation(
        &mut self,
        mode: SessionMode,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<AcceptedTurn, Rejection> {
        self.gate_persistence(events).await?;
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        if self.eligible_continuation_mode() != Some(mode) {
            return Err("background job continuation is no longer eligible"
                .to_string()
                .into());
        }
        let jobs = self
            .capabilities
            .command_jobs
            .clone()
            .expect("eligible jobs");
        let completions = jobs.pending_completions();
        let (notice, ack) = self.capabilities.job_follow.render_notice(
            CommandJobNoticeKind::Continuation,
            &completions,
            jobs.running_model_jobs(),
        );
        let mut records = vec![TranscriptItem::CommandJobNotice(notice.clone())];
        let boundary = self.standard_request_boundary();
        let request = boundary.as_ref().and_then(|record| match record {
            TranscriptItem::RequestDirective(directive) => Some(directive.request.clone()),
            _ => None,
        });
        records.extend(boundary);
        records.extend(self.instruction_updates(
            self.conversation.items(),
            self.policies.policy(mode),
            self.active_skills()?,
        )?);
        if let Err(error) = self.commit(RecordChange::anchored(TurnAnchor::Append, records, mode)) {
            return Err(self
                .acceptance_failed("background job continuation", error, events)
                .await);
        }
        jobs.acknowledge_completions(&ack);
        self.capabilities.job_follow.consecutive += 1;
        self.emit_selected_mode(events).await;
        let _ = events
            .send(SessionEvent::JobContinuationStarted {
                turn_id: turn.id,
                mode,
                notice,
            })
            .await;
        let mut accepted = AcceptedTurn::new(AcceptedKind::Continuation { mode });
        accepted.request = request;
        Ok(accepted)
    }
}
