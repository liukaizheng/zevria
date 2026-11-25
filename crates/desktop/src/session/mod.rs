//! Desktop bindings for the shared workspace model.
use zevria_foundation::ModelRole;
use zevria_host::RestoredSession;
use zevria_presentation::{
    RestorationInput,
    workspace::{RestoredChild, WorkspaceRestoration},
};
pub use zevria_presentation::{
    question,
    workspace::{ChildPane, InputRoute, WorkspaceModel},
};

pub fn restore(restored: RestoredSession) -> WorkspaceModel {
    let skills = restored.skill_context.completions();
    WorkspaceModel::restore(WorkspaceRestoration {
        session_id: restored.session_id,
        transcript_path: restored.transcript_path,
        session: RestorationInput {
            items: restored.transcript_items,
            workflow: restored.plan_state,
            selected_mode: restored.selected_mode,
            model_profiles: ModelRole::ALL
                .into_iter()
                .map(|role| (role, restored.model_contexts[role.index()].profile.clone()))
                .collect(),
            contexts: restored.model_snapshots,
            reasoning: restored.reasoning_levels,
            persistence_error: None,
        },
        notices: restored.startup_notices,
        skills,
        mcp_prompts: restored.mcp_prompts,
        subtasks: restored
            .subtasks
            .into_iter()
            .map(|child| RestoredChild {
                id: child.id,
                launch: child.launch,
                items: child.items,
            })
            .collect(),
    })
}
