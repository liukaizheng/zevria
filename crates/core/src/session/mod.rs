//! Provider-neutral conversation orchestration.
//!
//! [`SessionEngine`] is the single owner of local conversation state. Model
//! adapters only complete one model request at a time; tools and frontends are
//! connected here through Rig's shared tool server and session events.

mod builder;
pub use builder::SessionEngineBuilder;

use crate::session::kernel::accounting::ContextAccounting;
use crate::session::kernel::request_plan::RequestPlan;
use crate::session::kernel::state::{EnginePhase, GuidanceState, RootCapabilities, SkillState};
use crate::session::turn::execution::TurnWork;
use crate::session::turn::runtime::CommandAction;
use futures_util::FutureExt as _;
use rig_agent::tool::server::ToolServerHandle;
use rig_core::message::Message;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use zevria_foundation::policy::{SessionMode, SessionPolicies, TurnId, TurnPolicy};
use zevria_instructions::skill::{ActiveSkills, SkillCatalog};
use zevria_model::ToolDeclarations;
use zevria_model::config::CompactionPolicy;
use zevria_model::estimates::estimate_model_input;
use zevria_model::request::ModelRequestItem;
use zevria_session_api::EnsembleLauncher;
use zevria_session_api::command::{ManagementCommand, SessionCommand, TurnCommand};
use zevria_session_api::event::SessionEventSender;
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::question::QuestionResponder;
use zevria_session_api::turn::TurnContext;
use zevria_transcript::transcript::{Conversation, TranscriptItem, TranscriptWriter};
use zevria_transcript::{
    SessionReplayError, ToolDeclarationPlacement, ToolDeclarationRecord, tool_window,
};
use zevria_workflow::plan::{PlanId, PlanWorkflowState};

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
    incremental_tools: bool,
    phase: EnginePhase,
    context: ContextAccounting,
    request_plans: Mutex<Vec<Arc<RequestPlan>>>,
    skills: SkillState,
    capabilities: RootCapabilities,
    next_turn_id: u64,
    plan_projection_warnings: HashSet<PlanId>,
}

impl<P: ModelProvider> SessionEngine<P> {
    pub fn new(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        transcript: TranscriptWriter,
        skills: Arc<SkillCatalog>,
    ) -> anyhow::Result<Self> {
        let conversation = Self::opening_conversation(transcript)?;
        Self::from_conversation(provider, tools, policies, conversation, skills)
    }

    fn opening_conversation(transcript: TranscriptWriter) -> anyhow::Result<Conversation> {
        if transcript.is_pending() {
            Ok(Conversation::new(transcript))
        } else {
            let outcome = zevria_transcript::transcript::load_report(transcript.path())?;
            Conversation::from_loaded(transcript, outcome)
        }
    }

    /// Configure an opening engine before its one initial accounting pass.
    pub fn builder(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        transcript: TranscriptWriter,
        skills: Arc<SkillCatalog>,
    ) -> anyhow::Result<SessionEngineBuilder<P>> {
        Ok(SessionEngineBuilder::unaccounted(
            Self::unaccounted_from_conversation(
                provider,
                tools,
                policies,
                Self::opening_conversation(transcript)?,
                skills,
            )?,
        ))
    }

    /// Configure a resume using startup's authoritative load report.
    pub fn builder_from_loaded(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        transcript: TranscriptWriter,
        skills: Arc<SkillCatalog>,
        loaded: zevria_transcript::TranscriptLoadOutcome,
    ) -> anyhow::Result<SessionEngineBuilder<P>> {
        Ok(SessionEngineBuilder::unaccounted(
            Self::unaccounted_from_conversation(
                provider,
                tools,
                policies,
                Conversation::from_loaded(transcript, loaded)?,
                skills,
            )?,
        ))
    }

    /// Resume from startup's one validated load, preserving its exact evidence.
    pub fn from_loaded(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        transcript: TranscriptWriter,
        skills: Arc<SkillCatalog>,
        loaded: zevria_transcript::transcript::TranscriptLoadOutcome,
    ) -> anyhow::Result<Self> {
        Self::from_conversation(
            provider,
            tools,
            policies,
            Conversation::from_loaded(transcript, loaded)?,
            skills,
        )
    }

    fn from_conversation(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        conversation: Conversation,
        skills: Arc<SkillCatalog>,
    ) -> anyhow::Result<Self> {
        let mut engine =
            Self::unaccounted_from_conversation(provider, tools, policies, conversation, skills)?;
        engine.reestimate_context_usage();
        Ok(engine)
    }

    fn unaccounted_from_conversation(
        provider: P,
        tools: ToolServerHandle,
        policies: SessionPolicies,
        conversation: Conversation,
        skills: Arc<SkillCatalog>,
    ) -> anyhow::Result<Self> {
        let application_prompt = provider.application_prompt().to_string();
        conversation.replay()?;
        Ok(Self {
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
            incremental_tools: true,
            phase: EnginePhase::Idle,
            context: ContextAccounting::default(),
            request_plans: Mutex::new(Vec::new()),
            capabilities: RootCapabilities::default(),
            next_turn_id: 1,
            plan_projection_warnings: HashSet::new(),
        })
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
        let replay = zevria_transcript::validate_session_replay(&items)?;
        self.conversation.adopt_validated(items, replay);
        self.replay_changed()?;
        self.invalidate_request_plans();
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
        self.invalidate_request_plans();
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
        self.invalidate_mode_accounting();
        self
    }

    /// Project Plan artifacts as Markdown at Ready transitions beneath this
    /// directory (conventionally `{workspace}/.zevria/plans`). Restoration leaves
    /// existing or missing projections untouched; the transcript is authoritative.
    pub fn with_plans_dir(mut self, plans_dir: PathBuf) -> Self {
        self.capabilities.plans_dir = Some(plans_dir);
        self
    }

    pub fn with_command_jobs(mut self, jobs: zevria_session_api::CommandJobs) -> Self {
        self.capabilities.command_jobs = Some(jobs);
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

    pub fn with_incremental_tools(mut self, enabled: bool) -> Self {
        self.request_plans
            .get_mut()
            .expect("policy cache poisoned")
            .clear();
        self.incremental_tools = enabled;
        self
    }

    pub(super) fn desired_tool_declarations(&self, policy: &TurnPolicy) -> ToolDeclarations {
        ToolDeclarations::from_definitions(self.tools.snapshot().definitions(), |tool| {
            policy.allows_tool(tool.name.as_str())
        })
        .expect("registered tools have unique names and object schemas")
    }

    fn uncached_tool_declaration_update(
        &self,
        items: &[TranscriptItem],
        policy: &TurnPolicy,
    ) -> Option<TranscriptItem> {
        let desired = self.desired_tool_declarations(policy);
        let window = tool_window(items);
        let (placement, tools) = if window.initial.is_some() {
            let additions = desired
                .iter()
                .filter(|tool| {
                    window
                        .latest
                        .get(tool.name.as_str())
                        .is_none_or(|latest| *latest != *tool)
                })
                .cloned()
                .collect::<Vec<_>>();
            if additions.is_empty() {
                return None;
            }
            (
                ToolDeclarationPlacement::Additional,
                ToolDeclarations::new(additions).expect("filtered declarations"),
            )
        } else if self.incremental_tools {
            (ToolDeclarationPlacement::Initial, desired)
        } else {
            return None;
        };
        Some(TranscriptItem::ToolDeclarations(
            ToolDeclarationRecord::new(placement, tools).expect("valid declaration update"),
        ))
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
        Ok(self.conversation.replay()?.instructions.skills())
    }

    /// The session's conversation and its log.
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Authoritative workflow state, available synchronously after construction
    /// or restoration. Hosts seed presentation from this snapshot before consuming
    /// live transition events; `run` does not publish a startup snapshot.
    pub fn plan_state(&self) -> Result<&PlanWorkflowState, SessionReplayError> {
        Ok(&self.conversation.replay()?.plan)
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
        self.replay_changed()?;
        let (action, reply) = self.route_idle_command(command, true);
        if let Some(reply) = reply {
            let _ = events.send(reply).await;
        }
        let command = match action {
            CommandAction::Dispatch(command) => command,
            CommandAction::Continue | CommandAction::Shutdown => return Ok(()),
            CommandAction::Queue(_) => unreachable!("direct command API is idle"),
        };
        match command {
            SessionCommand::Turn(command) => {
                let turn = self.next_turn(command.mode());
                self.handle_turn(command, &turn, events).await
            }
            SessionCommand::Manage(ManagementCommand::SetMode { request_id, mode }) => {
                self.manage_mode(request_id, mode, events).await?;
                self.refresh_continuation_offer(events).await;
                Ok(())
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
            SessionCommand::Control(_) => unreachable!("control plane consumed the command"),
        }
    }

    /// Execute a child turn under the caller's identity and cancellation scope.
    pub async fn handle_turn(
        &mut self,
        command: TurnCommand,
        turn: &TurnContext,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.replay_changed()?;
        self.enter_turn()?;
        let result = self
            .execute_turn(TurnWork::Command(command), events, turn)
            .boxed()
            .await;
        self.exit_turn();
        if result.is_ok() {
            self.refresh_continuation_offer(events).await;
        }
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

#[cfg(feature = "test-support")]
mod pipeline_probe;

#[cfg(test)]
mod tests;

mod kernel;
mod manage;
#[cfg(test)]
mod test_prelude;
mod turn;
mod workflow;
#[cfg(test)]
use test_prelude::*;
