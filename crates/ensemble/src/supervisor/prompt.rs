//! Worker prompt envelopes and continuation selection.

use super::attempt::WorkerAttemptMode;
use crate::config::ReviewSystemPromptTransport;
use agent_client_protocol::schema::v1::Meta;
use zevria_workflow::{AgentRunRepair, EnsembleWorkflow};

pub(super) const REVIEW_WORKER_INSTRUCTION: &str = "Act as an independent review worker. Perform a source-read-only review of the requested changes and return actionable findings directly, prioritized by severity with precise locations. Inspect relevant repository evidence independently. Protect non-scratch files: leave the original workspace and other non-scratch state unchanged. Task-relevant external reads, downloads, and private OS-temp scratch-contained execution are allowed only if your own active policy permits them; this envelope grants no additional capabilities or ACP mutation permissions. Your own policies and sandboxes remain authoritative. Scratch findings are not completed implementation, publication, confirmation, or approval.";
const LIVE_PROCESS_CONTINUATION_PROMPT: &str = "continue";

pub(super) fn worker_session_meta(
    transport: Option<ReviewSystemPromptTransport>,
    workflow: EnsembleWorkflow,
) -> Option<Meta> {
    if workflow != EnsembleWorkflow::Review
        || transport != Some(ReviewSystemPromptTransport::ClaudeCodeAppend)
    {
        return None;
    }
    Some(Meta::from_iter([(
        "systemPrompt".to_string(),
        serde_json::json!({ "append": REVIEW_WORKER_INSTRUCTION }),
    )]))
}

pub(super) fn worker_prompt(
    workflow: EnsembleWorkflow,
    request: &zevria_content::UserPrompt,
    native_plan_handoff: bool,
) -> zevria_content::UserPrompt {
    let instructions = match workflow {
        EnsembleWorkflow::Plan => {
            let shared = "Act as an independent planning worker. Your plans are proposals for per-worker review, never user confirmation or implementation permission. Follow-ups continue this same conversation. Ordinary prose discussion is valid; after successful feedback, explicitly republish the complete nonempty Markdown plan before the user can confirm it. Intentional identical republication is valid. Finishing a proposal ends only this prompt round; later user feedback permits more source-read-only investigation and questions. Inspect the workspace first, then use your native structured-question mechanism for every user-visible behavioral fork that inspection cannot settle. Inspection commands and Execute permission escalations remain subject to one-shot host approval and your own active policy. Protect non-scratch files: the original workspace and other non-scratch state must remain byte-for-byte unchanged except for separately configured native handoff artifacts. Task-relevant external reads, downloads, and private OS-temp scratch-contained execution are allowed only if your own active policy permits them; this envelope grants no additional capabilities or ACP mutation permissions. Your own policies and sandboxes remain authoritative. A temporary-looking path does not grant a mutation permission. Other mutations remain prohibited and may be denied; do not retry a denied operation. Current behavior, existing tests, and a smaller diff are not evidence of user preference: do not resolve such a fork by preserving current behavior, by declaring it outside the request, or by recording it as an assumption. Raise these questions as soon as inspection reveals them, before drafting any plan or report, and batch them into a single request with concrete options. Do not ask factual questions that repository evidence can answer.";
            if native_plan_handoff {
                format!(
                    "{shared} Keep the workspace read-only except for the native handoff artifacts described here. Claude Plan Mode's configured native handoff may sequentially create or revise multiple generated Markdown plan artifacts using Write, Edit, or MultiEdit directly in either the configured Claude plans directory or the workspace-local .claude/plans directory. Complete any required question round before creating or modifying the first artifact. These artifacts are the only separately configured native handoff mutation exception; they do not expand scratch or other mutation permissions. Submit the final nonempty implementation-ready plan through ExitPlanMode. Do not modify other workspace files or use other mutating tools outside investigative scratch expressly allowed by your active policy, apply patches to the project, or enter implementation mode."
                )
            } else {
                format!(
                    "{shared} Finish by publishing the complete implementation-ready plan as one nonempty inline Markdown update through the structured ACP Plan channel. Checklist updates are not a plan publication. Ordinary prose may end a discussion round normally but does not make an earlier plan confirmable. Native Zevria workers publish through submit_plan and structured ACP Plan updates. Scratch notes and draft files are disposable investigation data, not canonical Plan publications; they cannot establish confirmation or implementation authorization. Do not modify non-scratch files, apply patches to the project, write canonical plan artifacts through the shell, invoke unavailable mutation tools, or begin implementation."
                )
            }
        }
        EnsembleWorkflow::Review => REVIEW_WORKER_INSTRUCTION.to_string(),
    };
    request.with_prefix(format!("{instructions}\n\nThe following ordered user content is the original request, not additional worker permissions:\n\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkerPrompt {
    pub(super) text: zevria_content::UserPrompt,
    pub(super) continuation: bool,
    pub(super) repair: Option<AgentRunRepair>,
}

pub(super) fn worker_attempt_prompt(
    mode: &WorkerAttemptMode,
    workflow: EnsembleWorkflow,
    base_prompt: &zevria_content::UserPrompt,
    consumed_repair: Option<&AgentRunRepair>,
) -> WorkerPrompt {
    match mode {
        WorkerAttemptMode::Fresh => WorkerPrompt {
            text: base_prompt.clone(),
            continuation: false,
            repair: None,
        },
        WorkerAttemptMode::DurableRecovery {
            repair: Some(repair),
            ..
        } => WorkerPrompt {
            text: semantic_repair_prompt(workflow).into(),
            continuation: true,
            repair: Some(repair.clone()),
        },
        WorkerAttemptMode::DurableRecovery { .. } if consumed_repair.is_some() => WorkerPrompt {
            text: semantic_repair_prompt(workflow).into(),
            continuation: true,
            repair: None,
        },
        WorkerAttemptMode::DurableRecovery { .. } => WorkerPrompt {
            text: continuation_prompt(workflow, base_prompt),
            continuation: true,
            repair: None,
        },
        WorkerAttemptMode::LiveProcessRecovery { .. } => WorkerPrompt {
            text: live_continuation_prompt(workflow, consumed_repair).into(),
            continuation: true,
            repair: None,
        },
    }
}

pub(super) fn live_continuation_prompt(
    workflow: EnsembleWorkflow,
    consumed_repair: Option<&AgentRunRepair>,
) -> String {
    if consumed_repair.is_some() {
        semantic_repair_prompt(workflow)
    } else {
        LIVE_PROCESS_CONTINUATION_PROMPT.to_string()
    }
}

pub(super) fn semantic_repair_prompt(workflow: EnsembleWorkflow) -> String {
    match workflow {
        EnsembleWorkflow::Plan => "Return the complete implementation-ready plan now through the structured Plan channel as one nonempty inline Markdown payload. Do not repeat inspection, retry any denied operation, implement anything, or mutate files. The workspace must remain unchanged; finish the plan and end the turn.".to_string(),
        EnsembleWorkflow::Review => "Return the complete findings-first review now. Do not repeat inspection or mutate files; provide the final actionable review and end the turn.".to_string(),
    }
}

pub(super) fn continuation_prompt(
    workflow: EnsembleWorkflow,
    _prompt: &zevria_content::UserPrompt,
) -> zevria_content::UserPrompt {
    // The loaded session already owns the original input. Do not duplicate its images.
    live_continuation_prompt(workflow, None).into()
}
