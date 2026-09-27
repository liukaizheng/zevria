//! State orchestration.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestShapeFingerprint(pub(super) [u8; 32]);

pub(super) fn request_shape_fingerprint(policy: &TurnPolicy) -> RequestShapeFingerprint {
    fn update_component(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }

    let mut hasher = Sha256::new();
    update_component(&mut hasher, policy.model_role.name().as_bytes());
    update_component(&mut hasher, policy.scope.as_bytes());
    update_component(&mut hasher, policy.instructions.as_bytes());
    hasher.update([u8::from(policy.skills_enabled)]);
    hasher.update([u8::from(policy.orchestration)]);
    hasher.update([match policy.contract {
        WorkspaceContract::Mutable => 0,
        WorkspaceContract::SourceReadOnlyScratch => 1,
    }]);
    match &policy.workspace {
        None => hasher.update([0]),
        Some(workspace) => {
            hasher.update([1]);
            update_component(&mut hasher, workspace.root.as_bytes());
            update_component(&mut hasher, workspace.startup.as_bytes());
        }
    }
    match &policy.allowed_tool_names {
        None => hasher.update([0]),
        Some(names) => {
            hasher.update([1]);
            hasher.update((names.len() as u64).to_be_bytes());
            for name in names {
                update_component(&mut hasher, name.as_bytes());
            }
        }
    }
    RequestShapeFingerprint(hasher.finalize().into())
}

#[cfg(test)]
mod policy_fingerprint_tests {
    use super::*;

    #[test]
    fn structured_capabilities_invalidate_request_shape_and_usage_baselines() {
        let base = TurnPolicy::new(
            "same instructions",
            Some(vec!["command".into(), "launch_subtasks".into()]),
            ModelRole::Build,
            false,
        );
        let shape = request_shape_fingerprint(&base);
        let mut usage = ContextUsageTracker::default();
        usage.provider_reported(100, 10, shape);
        assert_eq!(usage.compactable_tokens_for(shape), Some(90));
        let bound = base.clone().with_workspace(WorkspaceBinding {
            root: "/child".into(),
            startup: "/startup".into(),
        });
        for changed in [
            base.clone().with_orchestration(),
            base.clone()
                .with_contract(WorkspaceContract::SourceReadOnlyScratch),
            bound.clone(),
        ] {
            let changed_shape = request_shape_fingerprint(&changed);
            assert_ne!(shape, changed_shape);
            assert_eq!(usage.compactable_tokens_for(changed_shape), None);
        }
        for workspace in [
            WorkspaceBinding {
                root: "/other".into(),
                startup: "/startup".into(),
            },
            WorkspaceBinding {
                root: "/child".into(),
                startup: "/other".into(),
            },
        ] {
            assert_ne!(
                request_shape_fingerprint(&bound),
                request_shape_fingerprint(&base.clone().with_workspace(workspace))
            );
        }
        assert_eq!(shape, request_shape_fingerprint(&base));
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ContextUsageTracker {
    pub(super) last_provider_total: Option<u64>,
    pub(super) last_provider_fixed_overhead: u64,
    pub(super) last_request_shape: Option<RequestShapeFingerprint>,
    pub(super) appended_estimate: ContextTokenEstimate,
}

impl ContextUsageTracker {
    pub(super) fn compactable_tokens_for(self, shape: RequestShapeFingerprint) -> Option<u64> {
        self.last_provider_total
            .filter(|_| self.last_request_shape == Some(shape))
            .map(|total| {
                total
                    .saturating_sub(self.last_provider_fixed_overhead)
                    .saturating_add(self.appended_estimate.payload_tokens)
            })
    }

    pub(super) fn projected_tokens_for(
        self,
        shape: RequestShapeFingerprint,
        fixed_overhead: u64,
    ) -> Option<u64> {
        self.compactable_tokens_for(shape)
            .map(|tokens| tokens.saturating_add(fixed_overhead))
    }

    pub(super) fn provider_reported(
        &mut self,
        total_tokens: u64,
        fixed_overhead: u64,
        shape: RequestShapeFingerprint,
    ) {
        self.last_provider_total = Some(total_tokens);
        self.last_provider_fixed_overhead = fixed_overhead.min(total_tokens);
        self.last_request_shape = Some(shape);
        self.appended_estimate = ContextTokenEstimate::default();
    }

    pub(super) fn appended_tokens(&mut self, estimate: ContextTokenEstimate) {
        self.appended_estimate = self.appended_estimate.saturating_add(estimate);
    }
}

use zevria_transcript::SessionReplayError;

pub(super) fn validate_session_replay(
    items: &[TranscriptItem],
) -> Result<SessionReplayState, SessionReplayError> {
    zevria_transcript::validate_session_replay(items).map(SessionReplayState::from)
}

// Instruction and Plan replay succeed together, or neither payload is available.
pub(super) enum SessionReplayState {
    Valid {
        instructions: Box<zevria_transcript::InstructionReplayState>,
        plan: PlanWorkflowState,
    },
    Failed(SessionReplayError),
}

impl From<zevria_transcript::ValidatedSessionReplay> for SessionReplayState {
    fn from(validated: zevria_transcript::ValidatedSessionReplay) -> Self {
        Self::Valid {
            instructions: Box::new(validated.instructions),
            plan: validated.plan,
        }
    }
}

#[derive(Clone)]
pub(super) struct ModelManagementContext {
    pub(super) service: Arc<dyn zevria_model::models::ModelSettingsService>,
    pub(super) revision: String,
    pub(super) generation: u64,
    pub(super) session_generation: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnInputCountState {
    #[default]
    Empty,
    Prepared {
        turn_id: TurnId,
        role: ModelRole,
        identity: RequestShapeFingerprint,
        tokens: u64,
    },
    Failed {
        turn_id: TurnId,
    },
}

impl TurnInputCountState {
    pub(super) fn invalidate_prepared(&mut self) {
        if matches!(self, Self::Prepared { .. }) {
            *self = Self::Empty;
        }
    }

    pub(super) fn take_prepared(
        &mut self,
        turn_id: TurnId,
        role: ModelRole,
        identity: RequestShapeFingerprint,
    ) -> Option<u64> {
        let prepared = match *self {
            Self::Prepared {
                turn_id: counted_turn,
                role: counted_role,
                identity: counted_identity,
                tokens,
            } if counted_turn == turn_id
                && counted_role == role
                && counted_identity == identity =>
            {
                Some(tokens)
            }
            _ => None,
        };
        self.invalidate_prepared();
        prepared
    }
}

/// Mutable orchestration phase; model previews are independent idle capabilities.
pub(super) enum EnginePhase {
    Idle,
    Turn {
        skill_queries: Arc<Mutex<zevria_instructions::skill::SkillContext>>,
    },
}

pub(super) struct ModelManagement {
    pub(super) context: ModelManagementContext,
    pub(super) preview: Option<zevria_model::models::ModelSelectionPreview>,
}

#[derive(Default)]
pub(super) struct RootCapabilities {
    pub(super) mode_management: bool,
    /// Captured from the same configuration as the subtask supervisor. None
    /// means this engine is not an orchestration-capable root.
    pub(super) subtask_concurrency: Option<usize>,
    pub(super) plans_dir: Option<PathBuf>,
    pub(super) question_responder: Option<QuestionResponder>,
    pub(super) ensemble_launcher: Option<Arc<dyn EnsembleLauncher>>,
    pub(super) worker_controls: WorkerControlRouter,
    pub(super) models: Option<ModelManagement>,
}

/// Immutable opening values, with diagnostics delivered only by the opening
/// composition path. Independent of root-only skills and management grants.
#[derive(Default)]
pub(super) struct GuidanceState {
    pub(super) snapshot: Option<zevria_instructions::GuidanceSnapshot>,
    pub(super) startup_diagnostics: Vec<zevria_instructions::GuidanceDiagnostic>,
}

pub(super) struct SkillState {
    pub(super) catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    pub(super) management: Option<Arc<dyn zevria_instructions::skill::SkillManagementService>>,
    pub(super) mode_permissions: [bool; SessionMode::COUNT],
}

pub(super) struct ContextState {
    pub(super) usage: HashMap<ModelProfileRef, ContextUsageTracker>,
    pub(super) last_snapshots: HashMap<ModelRole, ContextTokenSnapshot>,
    pub(super) input_count: TurnInputCountState,
    pub(super) count_failures: std::collections::HashSet<ModelProfileRef>,
    pub(super) automatic_compaction_armed: [bool; ModelRole::COUNT],
}
impl Default for ContextState {
    fn default() -> Self {
        Self {
            usage: HashMap::new(),
            last_snapshots: HashMap::new(),
            input_count: TurnInputCountState::Empty,
            count_failures: std::collections::HashSet::new(),
            automatic_compaction_armed: [true; ModelRole::COUNT],
        }
    }
}
impl ContextState {
    pub(super) fn arm_all(&mut self) {
        self.automatic_compaction_armed.fill(true);
    }
    pub(super) fn arm(&mut self, role: ModelRole) {
        self.automatic_compaction_armed[role.index()] = true;
    }
    pub(super) fn recompute_arming(&mut self, items: &[TranscriptItem]) {
        self.automatic_compaction_armed
            .fill(automatic_compaction_armed_for(items));
    }
    pub(super) fn is_armed(&self, role: ModelRole) -> bool {
        self.automatic_compaction_armed[role.index()]
    }
    pub(super) fn invalidate_prepared_count(&mut self) {
        self.input_count.invalidate_prepared();
    }
    pub(super) fn reset_for_model_update(&mut self, role: ModelRole) {
        self.usage.clear();
        self.last_snapshots.clear();
        self.count_failures.clear();
        self.input_count = TurnInputCountState::Empty;
        self.arm(role);
    }
}
