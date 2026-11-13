//! Opening-only composition. No engine escapes before its complete history,
//! guidance, policy and capabilities receive one initial accounting pass.
use super::{SessionEngine, kernel::state::GuidanceState};
use rig_core::message::Message;
use std::{path::PathBuf, sync::Arc};
use zevria_foundation::SessionMode;
use zevria_instructions::{
    GuidanceRoots, GuidanceSnapshot, SkillCatalog, skill::SkillManagementService,
};
use zevria_model::{CompactionPolicy, models::ModelSettingsService};
use zevria_session_api::{CommandJobs, EnsembleLauncher, ModelProvider, QuestionResponder};
use zevria_transcript::{SessionReplayError, TranscriptItem};

pub struct SessionEngineBuilder<P> {
    engine: SessionEngine<P>,
    recompute_arming: bool,
}

impl<P: ModelProvider> SessionEngineBuilder<P> {
    pub(super) fn unaccounted(engine: SessionEngine<P>) -> Self {
        Self {
            engine,
            recompute_arming: false,
        }
    }

    pub fn with_history(self, history: Vec<Message>) -> Result<Self, SessionReplayError> {
        let mut items = self.engine.conversation.items()
            [..self.engine.conversation.leading_metadata_len()]
            .to_vec();
        items.extend(history.into_iter().map(TranscriptItem::Message));
        self.with_transcript_items(items)
    }

    pub fn with_transcript_items(
        mut self,
        items: Vec<TranscriptItem>,
    ) -> Result<Self, SessionReplayError> {
        self.engine.ensure_replay_valid()?;
        let replay = zevria_transcript::validate_session_replay(&items)?;
        self.engine.conversation.adopt_validated(items, replay);
        self.recompute_arming = true;
        Ok(self)
    }

    pub fn with_guidance_roots(mut self, roots: GuidanceRoots) -> Self {
        let mut snapshot = zevria_instructions::load_guidance(&roots);
        let startup_diagnostics = snapshot.take_diagnostics();
        self.engine.guidance = GuidanceState {
            snapshot: Some(snapshot),
            startup_diagnostics,
        };
        self
    }

    pub fn with_guidance_snapshot(mut self, mut snapshot: GuidanceSnapshot) -> Self {
        snapshot.take_diagnostics();
        self.engine.guidance = GuidanceState {
            snapshot: Some(snapshot),
            startup_diagnostics: Vec::new(),
        };
        self
    }

    pub fn with_compaction_policy(mut self, policy: CompactionPolicy) -> Self {
        self.engine.compaction = policy;
        self
    }

    pub fn with_skill_catalog(
        mut self,
        catalog: Arc<SkillCatalog>,
    ) -> Result<Self, SessionReplayError> {
        self.engine.ensure_replay_valid()?;
        self.engine.skills.catalog = catalog;
        Ok(self)
    }

    pub fn with_skill_management(
        mut self,
        service: Arc<dyn SkillManagementService>,
        permissions: [bool; SessionMode::COUNT],
    ) -> Result<Self, SessionReplayError> {
        self.engine.ensure_replay_valid()?;
        self.engine.skills.management = Some(service);
        self.engine.skills.mode_permissions = permissions;
        Ok(self)
    }

    fn capability(mut self, apply: impl FnOnce(SessionEngine<P>) -> SessionEngine<P>) -> Self {
        self.engine = apply(self.engine);
        self
    }

    pub fn with_incremental_tools(self, enabled: bool) -> Self {
        self.capability(|engine| engine.with_incremental_tools(enabled))
    }
    pub fn with_mode_management(self) -> Self {
        self.capability(SessionEngine::with_mode_management)
    }
    pub fn with_idle_notifications(self) -> Self {
        self.capability(SessionEngine::with_idle_notifications)
    }
    pub fn with_subtask_concurrency(self, limit: usize) -> Self {
        self.capability(|engine| engine.with_subtask_concurrency(limit))
    }
    pub fn with_plans_dir(self, path: PathBuf) -> Self {
        self.capability(|engine| engine.with_plans_dir(path))
    }
    pub fn with_question_responder(self, responder: QuestionResponder) -> Self {
        self.capability(|engine| engine.with_question_responder(responder))
    }
    pub fn with_ensemble_launcher(self, launcher: Arc<dyn EnsembleLauncher>) -> Self {
        self.capability(|engine| engine.with_ensemble_launcher(launcher))
    }
    pub fn with_command_jobs(self, jobs: CommandJobs) -> Self {
        self.capability(|engine| engine.with_command_jobs(jobs))
    }
    pub fn with_job_continuations(self, limit: u32) -> Self {
        self.capability(|engine| engine.with_job_continuations(limit))
    }
    pub fn with_model_management(
        self,
        service: Arc<dyn ModelSettingsService>,
        revision: String,
    ) -> Self {
        self.capability(|engine| engine.with_model_management(service, revision))
    }

    pub fn build(mut self) -> Result<SessionEngine<P>, SessionReplayError> {
        self.engine.replay_changed()?;
        self.engine.reestimate_context_usage();
        if self.recompute_arming {
            self.engine
                .context
                .recompute_arming(self.engine.conversation.items());
        }
        Ok(self.engine)
    }
}
