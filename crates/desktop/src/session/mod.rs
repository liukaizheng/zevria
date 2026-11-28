//! Desktop bindings for the shared workspace model.
use zevria_content::{PromptBlock, PromptImage, UserPrompt};
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DraftContent {
    pub text: String,
    pub images: Vec<PromptImage>,
}

/// The native textarea and attachment strip flatten a prompt for editing.
/// Keep the source blocks so that resending it unchanged is lossless.
pub struct RecalledOrigin {
    draft: DraftContent,
    prompt: UserPrompt,
}

impl DraftContent {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty() && self.images.is_empty()
    }

    pub fn from_user_prompt(prompt: &UserPrompt) -> (Self, RecalledOrigin) {
        let mut text = prompt.text_projection();
        // A leading image made the sigil literal in the source prompt. The
        // textarea flattens text ahead of attachments when the draft changes,
        // so keep that meaning with the editor's normal leading-space escape.
        if matches!(prompt.blocks().first(), Some(PromptBlock::Image(_)))
            && text.starts_with(['/', '$'])
        {
            text.insert(0, ' ');
        }
        let draft = Self {
            text,
            images: prompt.images().cloned().collect(),
        };
        let origin = RecalledOrigin {
            draft: draft.clone(),
            prompt: prompt.clone(),
        };
        (draft, origin)
    }

    pub fn to_user_prompt(&self, origin: Option<&RecalledOrigin>) -> UserPrompt {
        if let Some(origin) = origin.filter(|origin| origin.draft == *self) {
            return origin.prompt.clone();
        }
        let text = (!self.text.is_empty()).then(|| PromptBlock::Text(self.text.clone()));
        UserPrompt::new(
            text.into_iter()
                .chain(self.images.iter().cloned().map(PromptBlock::Image))
                .collect(),
        )
        .expect("desktop draft images come from a validated prompt and can only be removed")
    }
}

pub fn restore(restored: RestoredSession) -> WorkspaceModel {
    let skills = restored.skill_context.completions();
    let side_chats = restored.side_chats;
    let mut model = WorkspaceModel::restore(WorkspaceRestoration {
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
    });
    model.side_chats = side_chats
        .into_iter()
        .map(|side| {
            zevria_presentation::side_chats::SideChatPane::restore(
                zevria_presentation::side_chats::RestoredSidePane {
                    descriptor: side.descriptor,
                    quoted_excerpt: side.origin.quoted_excerpt,
                    items: side.items,
                },
            )
        })
        .collect();
    model
}
