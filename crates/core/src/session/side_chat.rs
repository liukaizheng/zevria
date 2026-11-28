//! Forking is a small metadata publication, never a clone of the live history.
use super::SessionEngine;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, watch};
use zevria_foundation::{SessionMode, TurnPolicy};
use zevria_instructions::SkillCatalog;
use zevria_model::{
    ToolDeclarations,
    models::{ModelSelection, SessionModels},
};
use zevria_session_api::{ModelProvider, SideChatCommand};

pub type ParentForkHandle = watch::Receiver<Option<Arc<ParentForkMeta>>>;

pub struct ParentForkMeta {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub safe_len: usize,
    pub inherited_turn_count: usize,
    pub mode: SessionMode,
    pub policy: TurnPolicy,
    pub model: Option<ModelSelection>,
    pub session_models: Option<SessionModels>,
    pub skills_enabled: bool,
    pub instructions: Arc<str>,
    pub initial_tools: Arc<ToolDeclarations>,
    pub skill_catalog: Arc<SkillCatalog>,
    pub persistence_degraded: bool,
    generation: u64,
    rewrites: Arc<AtomicU64>,
}

impl ParentForkMeta {
    /// An edit/repair invalidates the old snapshot before replacing the file.
    /// Ordinary appends never invalidate a committed prefix.
    pub fn still_current(&self) -> bool {
        self.rewrites.load(Ordering::Acquire) == self.generation
    }
}

pub(super) struct ParentForkPublisher {
    pub(super) watch: watch::Sender<Option<Arc<ParentForkMeta>>>,
    rewrites: Arc<AtomicU64>,
}

pub struct SideChatReadOnlyConfig {
    pub quoted_excerpt: Option<String>,
    pub skills_enabled: bool,
    pub mode: SessionMode,
    pub instructions: Arc<str>,
    pub frozen_tools: Arc<ToolDeclarations>,
    pub mcp_tools: Option<rig_agent::tool::server::ToolServerHandle>,
    pub mcp: Option<Arc<dyn zevria_session_api::mcp::McpSessionService>>,
}

impl<P: ModelProvider> SessionEngine<P> {
    /// Wire the control plane before spawning either the parent or the supervisor.
    pub fn enable_side_chats(
        &mut self,
        commands: mpsc::UnboundedSender<SideChatCommand>,
    ) -> ParentForkHandle {
        let (watch, handle) = watch::channel(None);
        self.capabilities.side_chat_tx = Some(commands);
        self.capabilities.fork = Some(ParentForkPublisher {
            watch,
            rewrites: self.conversation.observe_rewrites(),
        });
        self.publish_fork();
        handle
    }

    pub fn side_chat_mcp_tools(&self) -> rig_agent::tool::server::ToolServerHandle {
        self.tools.clone()
    }

    pub(super) fn publish_fork(&self) {
        let Some(publisher) = &self.capabilities.fork else {
            return;
        };
        let mode = self.selected_mode();
        let policy = self.policies.policy(mode);
        let plan = self.request_plan(policy);
        let replay = self.conversation.replay();
        let (safe_len, inherited_turn_count) = if self.conversation.is_pending() {
            (0, 0)
        } else {
            replay
                .as_ref()
                .map_or((0, 0), |state| state.instructions.fork_cursor())
        };
        publisher.watch.send_replace(Some(Arc::new(ParentForkMeta {
            session_id: self.conversation.session_id().into(),
            transcript_path: self.conversation.path().into(),
            safe_len,
            inherited_turn_count,
            mode,
            policy: policy.clone(),
            model: self.provider.model_selection(policy.model_role),
            session_models: self.conversation.session_models().cloned(),
            skills_enabled: policy.skills_enabled,
            instructions: plan.instructions.clone(),
            initial_tools: plan.tools.clone(),
            skill_catalog: self.skills.catalog.clone(),
            persistence_degraded: replay.is_err()
                || self.conversation.persistence_error().is_some(),
            generation: publisher.rewrites.load(Ordering::Acquire),
            rewrites: publisher.rewrites.clone(),
        })));
    }
}
