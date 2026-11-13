//! State orchestration.

use sha2::{Digest as _, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zevria_foundation::policy::{SessionMode, TurnPolicy, WorkspaceContract};
use zevria_session_api::EnsembleLauncher;
use zevria_session_api::question::QuestionResponder;
use zevria_session_api::worker::WorkerControlRouter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) struct RequestShapeFingerprint(pub(in crate::session) [u8; 32]);

pub(in crate::session) fn request_shape_fingerprint(
    policy: &TurnPolicy,
) -> RequestShapeFingerprint {
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
    use crate::session::kernel::accounting::ContextUsageTracker;
    use zevria_foundation::policy::{ModelRole, WorkspaceBinding};

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

#[derive(Clone)]
pub(in crate::session) struct ModelManagementContext {
    pub(in crate::session) service: Arc<dyn zevria_model::models::ModelSettingsService>,
    pub(in crate::session) revision: String,
    pub(in crate::session) generation: u64,
    pub(in crate::session) session_generation: String,
}

/// Mutable orchestration phase; model previews are independent idle capabilities.
pub(in crate::session) enum EnginePhase {
    Idle,
    Turn {
        skill_queries: Arc<Mutex<zevria_instructions::skill::SkillContext>>,
    },
}

pub(in crate::session) struct ModelManagement {
    pub(in crate::session) context: ModelManagementContext,
    pub(in crate::session) preview: Option<zevria_model::models::ModelSelectionPreview>,
}

#[derive(Default)]
pub(in crate::session) struct RootCapabilities {
    pub(in crate::session) command_jobs: Option<zevria_session_api::CommandJobs>,
    pub(in crate::session) job_follow: crate::session::turn::job_notices::JobFollowState,
    pub(in crate::session) mode_management: bool,
    pub(in crate::session) idle_notifications: bool,
    /// Captured from the same configuration as the subtask supervisor. None
    /// means this engine is not an orchestration-capable root.
    pub(in crate::session) subtask_concurrency: Option<usize>,
    pub(in crate::session) plans_dir: Option<PathBuf>,
    pub(in crate::session) question_responder: Option<QuestionResponder>,
    pub(in crate::session) ensemble_launcher: Option<Arc<dyn EnsembleLauncher>>,
    pub(in crate::session) worker_controls: WorkerControlRouter,
    pub(in crate::session) models: Option<ModelManagement>,
}

/// Immutable opening values, with diagnostics delivered only by the opening
/// composition path. Independent of root-only skills and management grants.
#[derive(Default)]
pub(in crate::session) struct GuidanceState {
    pub(in crate::session) snapshot: Option<zevria_instructions::GuidanceSnapshot>,
    pub(in crate::session) startup_diagnostics: Vec<zevria_instructions::GuidanceDiagnostic>,
}

pub(in crate::session) struct SkillState {
    pub(in crate::session) catalog: Arc<zevria_instructions::skill::SkillCatalog>,
    pub(in crate::session) management:
        Option<Arc<dyn zevria_instructions::skill::SkillManagementService>>,
    pub(in crate::session) mode_permissions: [bool; SessionMode::COUNT],
}
