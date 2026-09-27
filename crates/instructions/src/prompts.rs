/// Stable engine protocol and sole instruction authority boundary.
pub const ENGINE_PROTOCOL_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/engine-protocol.md");
/// Default product identity and engineering practice; replaceable by session.preamble.
pub const DEFAULT_PREAMBLE: &str = include_str!("../../../docs/instructions/system-prompt.md");
/// Shell conventions, selected whenever command is permitted.
pub const COMMAND_CONVENTIONS_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/command-conventions.md");
/// Provider-hosted external search semantics.
pub const HOSTED_SEARCH_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/hosted-search.md");
/// Source-read-only investigation and private OS-temp execution contract.
pub const INSPECTION_POLICY_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/inspection-policy.md");
/// Skill selection and lifecycle, independent of tool call mechanics.
pub const SKILL_SELECTION_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/skill-selection.md");
pub const SKILL_SELECTION_UNAVAILABLE: &str = "Skill selection is unavailable.";
/// Parent implementation workflow.
pub const BUILD_MODE_INSTRUCTIONS: &str = include_str!("../../../docs/instructions/build-mode.md");
/// Approval-ready planning workflow.
pub const PLAN_MODE_INSTRUCTIONS: &str = include_str!("../../../docs/instructions/plan-mode.md");
/// Independent investigation role.
pub const EXPLORE_AGENT_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/explore-agent.md");
/// Isolated Build-child role.
pub const BUILD_SUBTASK_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/build-subtask.md");
/// Independent Plan worker; shared role fragment followed by publication semantics.
pub const ENSEMBLE_WORKER_PLAN_INSTRUCTIONS: &str = concat!(
    include_str!("../../../docs/instructions/ensemble-worker.md"),
    "\n",
    include_str!("../../../docs/instructions/ensemble-worker-plan.md")
);
/// Independent Review worker; shared role fragment followed by reporting semantics.
pub const ENSEMBLE_WORKER_REVIEW_INSTRUCTIONS: &str = concat!(
    include_str!("../../../docs/instructions/ensemble-worker.md"),
    "\n",
    include_str!("../../../docs/instructions/ensemble-worker-review.md")
);
/// Confirmed-plan synthesis, not an ordinary Plan overlay.
pub const ENSEMBLE_PLAN_SYNTHESIS_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/ensemble-plan-synthesis.md");
/// Evidence-verified review synthesis.
pub const ENSEMBLE_REVIEW_SYNTHESIS_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/ensemble-review-synthesis.md");
/// Tool-free maintenance summarization.
pub const MAINTENANCE_INSTRUCTIONS: &str =
    include_str!("../../../docs/instructions/maintenance-mode.md");

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
