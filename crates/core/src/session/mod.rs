//! Provider-neutral conversation orchestration.
//!
//! [`SessionEngine`] is the single owner of local conversation state. Model
//! adapters only complete one model request at a time; tools and frontends are
//! connected here through Rig's shared tool server and session events.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures_util::FutureExt as _;
use rig_agent::tool::{ToolContext, server::ToolServerHandle};
use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use sha2::{Digest as _, Sha256};
#[cfg(test)]
use std::future::Future;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use zevria_foundation::RECONCILE_REPORTS_TOOL_NAME;
use zevria_foundation::SKILL_TOOL_NAME;
use zevria_foundation::SUBMIT_PLAN_TOOL_NAME;
use zevria_foundation::config::ModelContextPolicy;
use zevria_foundation::config::ModelProfileRef;
use zevria_foundation::question::QUESTION_TOOL_NAME;
#[cfg(test)]
use zevria_foundation::question::QuestionRequest;
#[cfg(test)]
use zevria_foundation::question::QuestionRequestId;
use zevria_foundation::question::QuestionTerminalDisposition;
use zevria_foundation::subtask::LAUNCH_SUBTASKS_TOOL_NAME;
#[cfg(test)]
use zevria_foundation::subtask::SubtaskId;
use zevria_foundation::task::TaskList;
use zevria_foundation::tool_result::ToolCallId;
use zevria_foundation::tool_result::ToolCallOutcome;
use zevria_foundation::tool_result::ToolCancelled;
use zevria_foundation::tool_result::ToolResultDetail;
use zevria_foundation::tool_result::ToolResultMetadata;
use zevria_instructions::prompts::{
    ENSEMBLE_PLAN_SYNTHESIS_INSTRUCTIONS, ENSEMBLE_REVIEW_SYNTHESIS_INSTRUCTIONS,
};
use zevria_instructions::skill::ActiveSkills;
use zevria_instructions::skill::SkillApplication;
use zevria_instructions::skill::SkillCatalog;
use zevria_instructions::skill::SkillInvocation;
use zevria_instructions::skill::SkillName;
use zevria_instructions::skill::SkillRequest;
#[cfg(test)]
use zevria_model::ProviderReplay;
use zevria_model::compaction::CompactionBackend;
use zevria_model::compaction::CompactionCheckpoint;
use zevria_model::compaction::CompactionTrigger;
use zevria_model::compaction::ContextTokenEstimate;
use zevria_model::compaction::local_replacement_history;
use zevria_model::compaction::select_recent_user_messages;
use zevria_model::config::CompactionPolicy;
use zevria_session_api::EnsembleLaunchRequest;
use zevria_session_api::EnsembleLauncher;
use zevria_session_api::question::QuestionResponder;
use zevria_session_api::worker::*;
use zevria_transcript::transcript::Conversation;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;
use zevria_transcript::transcript::latest_successful_task_snapshot;
use zevria_transcript::transcript::replay_active_skills;
#[cfg(test)]
use zevria_workflow::ensemble::AgentRunId;
use zevria_workflow::ensemble::AgentRunOutcome;
use zevria_workflow::ensemble::AgentRunStatus;
use zevria_workflow::ensemble::AgentRunSummary;
use zevria_workflow::ensemble::EnsembleRecord;
use zevria_workflow::ensemble::EnsembleRecovery;
use zevria_workflow::ensemble::EnsembleRunId;
use zevria_workflow::ensemble::EnsembleStart;
use zevria_workflow::ensemble::EnsembleWorkflow;
use zevria_workflow::ensemble::ReconciliationNextStep;
use zevria_workflow::ensemble::ReportReconciliation;
use zevria_workflow::ensemble::ReportReconciliationCatalog;
use zevria_workflow::ensemble::ValidatedReportReconciliation;
use zevria_workflow::ensemble::latest_ensemble_recovery;
use zevria_workflow::ensemble_review::*;
use zevria_workflow::plan::DirectWorkerPlan;
use zevria_workflow::plan::MAX_PLAN_ARTIFACT_BYTES;
use zevria_workflow::plan::PlanArtifact;
use zevria_workflow::plan::PlanCandidate;
use zevria_workflow::plan::PlanDecision;
use zevria_workflow::plan::PlanHandoff;
use zevria_workflow::plan::PlanId;
use zevria_workflow::plan::PlanPublicationProvenance;
use zevria_workflow::plan::PlanRecord;
use zevria_workflow::plan::PlanResolution;
use zevria_workflow::plan::PlanVersion;
use zevria_workflow::plan::PlanWorkflowState;
use zevria_workflow::plan::apply_plan_record;
use zevria_workflow::plan::replay_plan_state;

#[cfg(test)]
use zevria_workflow::ensemble::AgentUserDecisionAnswer;
#[cfg(test)]
use zevria_workflow::ensemble::AgentUserDecisionBatch;
#[cfg(test)]
use zevria_workflow::ensemble::AgentUserDecisionId;
#[cfg(test)]
use zevria_workflow::ensemble::AgentUserDecisionValue;
#[cfg(test)]
use zevria_workflow::ensemble::RecordedDecisionAccounting;
#[cfg(test)]
use zevria_workflow::ensemble::RecordedDecisionDisposition;
#[cfg(test)]
use zevria_workflow::ensemble::ReportDisagreement;
#[cfg(test)]
use zevria_workflow::ensemble::ReportDisagreementClassification;
#[cfg(test)]
use zevria_workflow::ensemble::ReportDisagreementResolution;
#[cfg(test)]
use zevria_workflow::ensemble::ReportPosition;

/// Owns one resumable conversation and drives provider/tool iterations.
pub struct SessionEngine<P> {
    provider: P,
    tools: ToolServerHandle,
    policies: SessionPolicies,
    application_prompt: String,
    guidance: GuidanceState,
    /// The only conversation copy; model input is a projection of these items.
    conversation: Conversation,
    compaction: CompactionPolicy,
    replay: SessionReplayState,
    phase: EnginePhase,
    context: ContextState,
    skills: SkillState,
    capabilities: RootCapabilities,
    next_turn_id: u64,
}

impl<P: ModelProvider> SessionEngine<P> {
    pub fn new(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        transcript: TranscriptWriter,
        skills: Arc<SkillCatalog>,
    ) -> anyhow::Result<Self> {
        let application_prompt = provider.application_prompt().to_string();
        let initial = zevria_transcript::transcript::load(transcript.path())?;
        let replay = validate_session_replay(&initial)?;
        let mut conversation = Conversation::new(transcript);
        conversation.adopt_persisted(initial);
        let mut engine = Self {
            application_prompt,
            guidance: GuidanceState::default(),
            provider,
            tools,
            skills: SkillState {
                mode_permissions: std::array::from_fn(|index| {
                    policies.policy(SessionMode::ALL[index]).skills_enabled
                }),
                management: None,
                catalog: skills,
            },
            policies,
            conversation,
            compaction: CompactionPolicy::default(),
            replay,
            phase: EnginePhase::Idle,
            context: ContextState::default(),
            capabilities: RootCapabilities::default(),
            next_turn_id: 1,
        };
        engine.reestimate_context_usage();
        Ok(engine)
    }

    /// Seed the conversation from bare messages, which mirror as plain items.
    pub fn with_history(self, history: Vec<Message>) -> Result<Self, SessionReplayError> {
        let mut items =
            self.conversation.items()[..self.conversation.leading_metadata_len()].to_vec();
        items.extend(history.into_iter().map(TranscriptItem::Message));
        self.with_transcript_items(items)
    }

    /// Seed the conversation from a resumed session's raw items, preserving
    /// their compound shapes.
    pub fn with_transcript_items(
        mut self,
        items: Vec<TranscriptItem>,
    ) -> Result<Self, SessionReplayError> {
        self.ensure_replay_valid()?;
        let replay = validate_session_replay(&items)?;
        self.conversation.adopt_persisted(items);
        self.install_replay(replay)?;
        self.invalidate_model_preview();
        self.context.invalidate_prepared_count();
        self.context.last_snapshots.clear();
        self.reestimate_context_usage();
        self.context.recompute_arming(self.conversation.items());
        Ok(self)
    }

    /// Capture file guidance synchronously once at session opening/resume.
    /// Ordinary turns and compaction never reread these roots.
    pub fn with_guidance_roots(mut self, roots: zevria_instructions::GuidanceRoots) -> Self {
        let mut snapshot = zevria_instructions::load_guidance(&roots);
        let diagnostics = snapshot.take_diagnostics();
        self = self.with_guidance_snapshot(snapshot);
        self.guidance.startup_diagnostics = diagnostics;
        self
    }

    /// Inherit already-captured values without filesystem reads or repeating
    /// the parent's diagnostics. Does not modify the application preamble.
    pub fn with_guidance_snapshot(
        mut self,
        mut snapshot: zevria_instructions::GuidanceSnapshot,
    ) -> Self {
        snapshot.take_diagnostics();
        self.guidance = GuidanceState {
            snapshot: Some(snapshot),
            startup_diagnostics: Vec::new(),
        };
        self.invalidate_model_preview();
        self.context.invalidate_prepared_count();
        self.context.last_snapshots.clear();
        self.reestimate_context_usage();
        self
    }

    pub fn guidance_snapshot(&self) -> Option<&zevria_instructions::GuidanceSnapshot> {
        self.guidance.snapshot.as_ref()
    }

    /// Apply the same automatic/manual context policy used by the root or
    /// child composition path.
    pub fn with_compaction_policy(mut self, compaction: CompactionPolicy) -> Self {
        self.compaction = compaction;
        self.reestimate_context_usage();
        self
    }

    /// Project Plan artifacts as Markdown at Ready transitions beneath this
    /// directory (conventionally `{workspace}/.zevria/plans`). Restoration leaves
    /// existing or missing projections untouched; the transcript is authoritative.
    pub fn with_plans_dir(mut self, plans_dir: PathBuf) -> Self {
        self.capabilities.plans_dir = Some(plans_dir);
        self
    }

    /// Attach the response endpoint paired with the root `question` tool.
    pub fn with_question_responder(mut self, responder: QuestionResponder) -> Self {
        self.capabilities.question_responder = Some(responder);
        self
    }

    /// Attach the ACP worker supervisor used by the typed ensemble commands.
    pub fn with_ensemble_launcher(mut self, launcher: Arc<dyn EnsembleLauncher>) -> Self {
        self.capabilities.ensemble_launcher = Some(launcher);
        self
    }

    /// Message-bearing compatibility view of the active model projection.
    /// Use [`Self::model_input`] when opaque replay-only entries matter.
    pub fn history(&self) -> Vec<Message> {
        self.conversation.messages().cloned().collect()
    }

    pub fn model_input(&self) -> Vec<ModelRequestItem<'_>> {
        self.conversation.model_input()
    }

    pub(super) fn instruction_set(
        &self,
        policy: &TurnPolicy,
    ) -> zevria_instructions::InstructionSet {
        zevria_instructions::InstructionSet {
            application: self.application_prompt.clone(),
            system: self
                .guidance_components()
                .map(|(key, text)| (key.into(), text.into()))
                .collect(),
            workflow: zevria_instructions::DirectivePolicy::new(&policy.scope, policy),
            catalog: self.skill_activation_available(policy).then(|| {
                self.captured_skill_context(&ActiveSkills::default(), true)
                    .prompt_catalog()
            }),
        }
    }

    pub(super) fn rendered_instructions(&self, policy: &TurnPolicy) -> String {
        self.instruction_set(policy).render()
    }

    pub fn context_tokens(&self) -> Result<u64, SessionReplayError> {
        let policy = self.policies.policy(self.selected_mode());
        let current = self.projected_context_tokens(policy)?;
        let updates = self
            .reconcile_instruction_state(self.directive_state()?, policy, self.active_skills()?)
            .map_err(|error| SessionReplayError::Directives(error.to_string()))?;
        Ok(current.saturating_add(
            estimate_model_input(
                updates
                    .iter()
                    .map(ModelRequestItem::DeveloperInstruction)
                    .collect(),
            )
            .conservative_tokens,
        ))
    }

    pub fn active_skills(&self) -> Result<&ActiveSkills, SessionReplayError> {
        match &self.replay {
            SessionReplayState::Valid { instructions, .. } => Ok(instructions.skills()),
            SessionReplayState::Failed(error) => Err(error.clone()),
        }
    }

    /// The session's conversation and its log.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Authoritative workflow state, available synchronously after construction
    /// or restoration. Hosts seed presentation from this snapshot before consuming
    /// live transition events; `run` does not publish a startup snapshot.
    pub fn plan_state(&self) -> Result<&PlanWorkflowState, SessionReplayError> {
        match &self.replay {
            SessionReplayState::Valid { plan, .. } => Ok(plan),
            SessionReplayState::Failed(error) => Err(error.clone()),
        }
    }

    fn ensure_replay_valid(&self) -> Result<(), SessionReplayError> {
        self.plan_state().map(|_| ())
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    /// Process one command. Provider failures are reported as events and do
    /// not stop the engine, so a later prompt can recover with local history.
    pub async fn handle_command(
        &mut self,
        command: SessionCommand,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.refresh_replay()?;
        match command {
            SessionCommand::Turn(command) => {
                let turn = self.next_turn(command.mode());
                self.handle_turn(command, &turn, events).await
            }
            SessionCommand::Control(ControlCommand::Worker(control)) => {
                if let Err(result) = self.capabilities.worker_controls.route(control) {
                    let _ = events
                        .send(SessionEvent::WorkerControlResult { result })
                        .await;
                }
                Ok(())
            }
            SessionCommand::Control(ControlCommand::AnswerQuestion {
                request_id,
                response,
            }) => {
                if let Some(responder) = &self.capabilities.question_responder {
                    responder.respond(&request_id, response);
                }
                Ok(())
            }
            SessionCommand::Control(
                ControlCommand::CancelTurn { .. } | ControlCommand::Shutdown,
            ) => Ok(()),
            SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode }) => {
                self.manage_mode(request_id, mode, events).await
            }
            SessionCommand::Manage(ManagementCommand::Models {
                request_id,
                request,
            }) => {
                self.manage_models(request_id, request, events, &CancellationToken::new())
                    .await
            }
            SessionCommand::Manage(ManagementCommand::Skills {
                request_id,
                request,
            }) => self.manage_skills(request_id, request, events).await,
        }
    }

    /// Execute a child turn under the caller's identity and cancellation scope.
    pub async fn handle_turn(
        &mut self,
        command: TurnCommand,
        turn: &TurnContext,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.refresh_replay()?;
        self.enter_turn()?;
        let result = self
            .execute_turn(TurnWork::Command(command), events, turn)
            .boxed()
            .await;
        self.exit_turn();
        if result.is_err() {
            events.stream_cleared(turn.id);
        }
        result
    }

    fn enter_turn(
        &mut self,
    ) -> Result<Arc<Mutex<zevria_instructions::skill::SkillContext>>, SessionReplayError> {
        self.ensure_replay_valid()?;
        self.invalidate_model_preview();
        let skill_queries = Arc::new(Mutex::new(self.management_skill_context()?));
        self.phase = EnginePhase::Turn {
            skill_queries: skill_queries.clone(),
        };
        Ok(skill_queries)
    }

    fn exit_turn(&mut self) {
        self.phase = EnginePhase::Idle;
    }

    fn next_turn(&mut self, mode: SessionMode) -> TurnContext {
        let id = TurnId::new(self.next_turn_id);
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        TurnContext::new(id, mode, CancellationToken::new())
    }
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod benchmark;

use zevria_content::assistant_plain_text;
use zevria_foundation::policy::*;
use zevria_model::request::*;
use zevria_model::telemetry::*;
use zevria_session_api::command::*;
use zevria_session_api::event::*;
use zevria_session_api::provider::*;
use zevria_session_api::turn::*;

mod orchestration;
mod state;
use state::*;

mod capacity;
mod directives;
use capacity::*;
use zevria_model::estimates::*;

mod compaction;
use compaction::*;

mod records;
use records::*;

#[cfg(feature = "test-support")]
mod pipeline_probe;
mod prompt;
use prompt::*;

mod plan;
use plan::*;

mod ensemble;
mod worker_review;

mod model_loop;
use model_loop::*;

mod tools;
use tools::*;

mod skills;

mod turn;
use turn::*;

use zevria_transcript::SessionReplayError;
mod mode;
mod models;
mod runtime;
mod summary;
#[cfg(test)]
mod tests;
