//! Shared, deterministic Plan tool results. No projection path enters model text.

use rig_agent::tool::ToolContext;
use zevria_foundation::{FileChange, PlanDraftDetail, ToolResultDetail};
use zevria_workflow::{
    PlanDraft, PlanDraftScope, PlanValidationError, derived_title, submission_problems,
};

pub(crate) fn scope(
    context: &ToolContext,
) -> Result<std::sync::Arc<PlanDraftScope>, PlanValidationError> {
    context.scope::<PlanDraftScope>().ok_or_else(|| PlanValidationError(
        "Plan authoring requires an active Planning scope; these tools are unavailable outside Plan-mode turns".into()
    ))
}

pub(crate) fn working_copy(scope: &PlanDraftScope) -> Result<&str, PlanValidationError> {
    scope.markdown.as_deref().ok_or_else(|| {
        PlanValidationError(
            "there is no stored Plan working copy; save an initial draft with create_plan first"
                .into(),
        )
    })
}

pub(crate) fn detail(
    markdown: &str,
    scope: &PlanDraftScope,
    sequence: Option<u32>,
    change: Option<FileChange>,
) -> ToolResultDetail {
    ToolResultDetail::PlanDraft(PlanDraftDetail {
        title: derived_title(markdown).expect("validated title").into(),
        target_revision: scope.target.revision,
        sequence,
        bytes: markdown.len(),
        change,
        projection: None,
    })
}

pub(crate) fn save(
    context: &mut ToolContext,
    scope: &PlanDraftScope,
    markdown: String,
    change: Option<FileChange>,
    max: usize,
) -> String {
    let problems = submission_problems(&markdown, max);
    let bytes = markdown.len();
    context
        .insert_result(detail(&markdown, scope, Some(scope.next_sequence), change))
        .expect("Plan details serialize");
    context
        .insert_result(PlanDraft {
            target: scope.target,
            sequence: scope.next_sequence,
            markdown,
            source_turn_id: scope.turn_id,
        })
        .expect("Plan draft serializes");
    format!(
        "Plan draft {} for revision {} saved ({bytes} bytes). Submission check: {}.",
        scope.next_sequence,
        scope.target.revision,
        if problems.is_empty() {
            "ready to submit".into()
        } else {
            problems.join("; ")
        }
    )
}
