//! Tools orchestration.

use crate::session::SessionEngine;
use crate::session::kernel::request_plan::RequestPlan;
use crate::session::kernel::timing::LifecycleTiming;
use crate::session::workflow::plan::PlanSubmissionGate;
use rig_agent::tool::ToolContext;
use rig_agent::tool::server::ToolServerHandle;
use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use std::fmt;
use std::sync::Arc;
use zevria_foundation::policy::{SessionMode, TurnPolicy};
use zevria_foundation::question::{QUESTION_TOOL_NAME, QuestionTerminalDisposition};
use zevria_foundation::subtask::LAUNCH_SUBTASKS_TOOL_NAME;
use zevria_foundation::tool_result::{
    ToolCallId, ToolCallOutcome, ToolCancelled, ToolResultDetail, ToolResultMetadata,
};
use zevria_foundation::{
    CREATE_PLAN_TOOL_NAME, READ_PLAN_TOOL_NAME, RECONCILE_REPORTS_TOOL_NAME, SKILL_TOOL_NAME,
    SUBMIT_PLAN_TOOL_NAME, UPDATE_PLAN_TOOL_NAME,
};
use zevria_instructions::skill::{ActiveSkills, SkillRequest};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::turn::TurnContext;
use zevria_workflow::ensemble::{ReportReconciliationCatalog, ValidatedReportReconciliation};
use zevria_workflow::plan::{
    PlanCandidate, PlanDraft, PlanDraftScope, PlanRecord, PlanWorkflowState, apply_plan_record,
};

use zevria_foundation::ToolContextRuntimeExt;

#[cfg(test)]
#[path = "tools_metadata_tests.rs"]
mod metadata_tests;

/// Resolved once at batch admission; names remain verbatim in wire results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum EngineTool {
    Command,
    Skill,
    SkillRead,
    CreatePlan,
    UpdatePlan,
    ReadPlan,
    SubmitPlan,
    ReconcileReports,
    Question,
    LaunchSubtasks,
    Other,
}
impl EngineTool {
    pub(in crate::session) fn resolve(name: &str) -> Self {
        match name {
            "command" => Self::Command,
            SKILL_TOOL_NAME => Self::Skill,
            zevria_foundation::SKILL_READ_TOOL_NAME => Self::SkillRead,
            CREATE_PLAN_TOOL_NAME => Self::CreatePlan,
            UPDATE_PLAN_TOOL_NAME => Self::UpdatePlan,
            READ_PLAN_TOOL_NAME => Self::ReadPlan,
            SUBMIT_PLAN_TOOL_NAME => Self::SubmitPlan,
            RECONCILE_REPORTS_TOOL_NAME => Self::ReconcileReports,
            QUESTION_TOOL_NAME => Self::Question,
            LAUNCH_SUBTASKS_TOOL_NAME => Self::LaunchSubtasks,
            _ => Self::Other,
        }
    }
    pub(in crate::session) fn is_skill(self) -> bool {
        matches!(self, Self::Skill | Self::SkillRead)
    }
    pub(in crate::session) fn authors_draft(self) -> bool {
        matches!(self, Self::CreatePlan | Self::UpdatePlan)
    }
    fn is_plan(self) -> bool {
        self.authors_draft() || matches!(self, Self::ReadPlan | Self::SubmitPlan)
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::session) struct EngineCall<'a> {
    raw: &'a ToolCall,
    pub(in crate::session) tool: EngineTool,
}
impl<'a> EngineCall<'a> {
    pub(in crate::session) fn new(raw: &'a ToolCall) -> Self {
        Self {
            raw,
            tool: EngineTool::resolve(raw.function.name.as_str()),
        }
    }
}
impl std::ops::Deref for EngineCall<'_> {
    type Target = ToolCall;
    fn deref(&self) -> &Self::Target {
        self.raw
    }
}

/// Question disposition is a typed result-detail sidecar and may accompany a
/// successful Plan effect. Retain that historical combination explicitly.
pub(in crate::session) enum ToolEffect {
    None,
    Candidate(Box<PlanCandidate>, Option<QuestionTerminalDisposition>),
    Draft(Box<PlanDraft>, Option<QuestionTerminalDisposition>),
    Reconciliation(
        Box<ValidatedReportReconciliation>,
        Option<QuestionTerminalDisposition>,
    ),
    Question(QuestionTerminalDisposition),
    Skill(Box<zevria_instructions::skill::SkillToolApplication>),
}
impl ToolEffect {
    fn candidate(&self) -> Option<&PlanCandidate> {
        if let Self::Candidate(value, _) = self {
            Some(value)
        } else {
            None
        }
    }
    fn draft(&self) -> Option<&PlanDraft> {
        if let Self::Draft(value, _) = self {
            Some(value)
        } else {
            None
        }
    }
    pub(in crate::session) fn question_disposition(&self) -> Option<QuestionTerminalDisposition> {
        match self {
            Self::Question(value) => Some(*value),
            Self::Candidate(_, value) | Self::Draft(_, value) | Self::Reconciliation(_, value) => {
                *value
            }
            Self::None | Self::Skill(_) => None,
        }
    }
}

pub(in crate::session) fn assistant_tool_calls(message: &Message) -> Vec<ToolCall> {
    let Message::Assistant(assistant) = message else {
        return Vec::new();
    };
    assistant
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

pub(in crate::session) fn tool_arguments_for_dispatch(
    function: &rig_core::message::ToolFunction,
) -> String {
    // Never turn Rig's partially recovered object into an executable call.
    // Invalid arguments must reach dispatch as the original model bytes.
    function.raw_arguments()
}

#[derive(Debug)]
pub(in crate::session) struct ToolResultBatch {
    pub(in crate::session) message: Message,
    pub(in crate::session) metadata: Vec<ToolResultMetadata>,
    pub(in crate::session) candidate: Option<PlanCandidate>,
    pub(in crate::session) drafts: Vec<PlanDraft>,
    pub(in crate::session) reconciliation: Option<ValidatedReportReconciliation>,
    pub(in crate::session) question_disposition: Option<QuestionTerminalDisposition>,
    pub(in crate::session) inspection_attempted: bool,
    pub(in crate::session) skill_applications:
        Vec<zevria_instructions::skill::SkillToolApplication>,
}

/// One executed (or denied) call's correlated result and display metadata.
pub(in crate::session) struct ToolCallSlot {
    pub(in crate::session) result: UserContent,
    pub(in crate::session) metadata: ToolResultMetadata,
    pub(in crate::session) tool: EngineTool,
    pub(in crate::session) effect: ToolEffect,
}

pub(in crate::session) fn denied_slot(call: &EngineCall<'_>, mode: SessionMode) -> ToolCallSlot {
    let reason = format!(
        "the tool `{}` is unavailable in {mode} mode",
        call.function.name
    );
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Denied,
        "status: denied\nreason: ",
        reason,
    )
}

pub(in crate::session) fn candidate_locked_slot(call: &EngineCall<'_>) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(call, ToolCallOutcome::Denied, "status: denied\nreason: ", "a Plan artifact was already accepted for this turn; make no more tool calls and provide a short final confirmation".into())
}

pub(in crate::session) fn cancelled_slot(call: &EngineCall<'_>) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Cancelled,
        "status: cancelled\nreason: ",
        "the parent turn was cancelled before this call started".into(),
    )
}

/// Strict parsing is engine-owned; raw tool dispatch cannot admit an application.
pub(in crate::session) fn skill_call_request(
    call: &EngineCall<'_>,
) -> anyhow::Result<SkillRequest> {
    Ok(serde_json::from_str(&tool_arguments_for_dispatch(
        &call.function,
    ))?)
}

pub(in crate::session) fn skill_lifecycle_error_slot(
    call: &EngineCall<'_>,
    error: impl fmt::Display,
) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Error,
        "status: error\nerror: ",
        format!("skill application was rejected: {error}"),
    )
}

pub(in crate::session) async fn dispatch_tool_call(
    tools: &ToolServerHandle,
    call: &EngineCall<'_>,
    turn: &TurnContext,
    reconciliation_catalog: Option<&ReportReconciliationCatalog>,
    skill_context: Option<&zevria_instructions::skill::SkillContext>,
    plan_scope: Option<&PlanDraftScope>,
) -> ToolCallSlot {
    let arguments = tool_arguments_for_dispatch(&call.function);
    // Per-call context carries the assistant call id into the tool, so tools
    // that announce work to the frontend (the subtask tool) can correlate
    // their events with this pending call's row.
    let mut context = ToolContext::new();
    context
        .insert(ToolCallId(call.id.to_string()))
        .expect("call IDs are serializable");
    context.insert_scope(turn.clone());
    if let Some(skill_context) = skill_context {
        context.insert_scope(skill_context.clone());
    }
    if let Some(catalog) = reconciliation_catalog {
        context.insert_scope(catalog.clone());
    }
    if let Some(scope) = plan_scope {
        context.insert_scope(scope.clone());
    }
    let catalog = tools.snapshot();
    let execution = match catalog.get(call.function.name.as_str()) {
        Some(tool) => tool.execute(arguments, &mut context).await,
        None => rig_core::tool::ToolResult::failed(rig_core::tool::ToolExecutionError::not_found(
            format!("tool `{}` not found", call.function.name),
        )),
    };
    let mut outcome = ToolCallOutcome::from(&execution);
    let model_output = execution.output().render();
    let diagnostic = (!execution.is_success()).then(|| model_output.clone());
    let output = if execution.is_success() {
        model_output
    } else {
        format!("status: error\nerror: {model_output}")
    };
    // Mutation metadata is truthful even when a later apply or rollback step
    // failed. Do not erase it merely because Rig classified the call as an
    // error.
    let detail = context
        .result::<ToolResultDetail>()
        .expect("published tool details must decode");
    if context
        .result::<ToolCancelled>()
        .expect("published cancellation must decode")
        .is_some()
    {
        outcome = ToolCallOutcome::Cancelled;
    } else if !outcome.is_success()
        && detail
            .as_ref()
            .is_some_and(|detail| !detail.file_changes().is_empty())
    {
        outcome = ToolCallOutcome::Partial;
    } else if turn.is_cancelled() && !outcome.is_success() {
        outcome = ToolCallOutcome::Cancelled;
    }
    let candidate = (execution.is_success() && call.tool == EngineTool::SubmitPlan)
        .then(|| {
            context
                .result::<PlanCandidate>()
                .expect("published Plan candidates must validate")
        })
        .flatten();
    let draft = (execution.is_success() && call.tool.authors_draft())
        .then(|| {
            context
                .result::<PlanDraft>()
                .expect("published Plan drafts must decode")
        })
        .flatten();
    let reconciliation = (execution.is_success() && call.tool == EngineTool::ReconcileReports)
        .then(|| {
            let accepted = context
                .result::<ValidatedReportReconciliation>()
                .expect("published reconciliation must decode")?;
            let catalog = reconciliation_catalog?;
            accepted.declaration.clone().validate(catalog).ok()
        })
        .flatten();
    let question_disposition = detail
        .as_ref()
        .and_then(ToolResultDetail::question_disposition);
    ToolCallSlot {
        result: UserContent::ToolResult(ToolResult {
            call: call.id.clone(),
            name: call.function.name.clone(),
            content: vec![ToolResultContent::text(output)],
            is_error: !outcome.is_success(),
        }),
        metadata: ToolResultMetadata {
            diagnostic,
            id: call.id.to_string(),
            call_id: call
                .id
                .provider()
                .map(|provider| provider.as_str().to_owned()),
            tool_name: call.function.name.to_string(),
            outcome,
            detail,
        },
        tool: call.tool,
        effect: if let Some(value) = candidate {
            ToolEffect::Candidate(Box::new(value), question_disposition)
        } else if let Some(value) = draft {
            ToolEffect::Draft(Box::new(value), question_disposition)
        } else if let Some(value) = reconciliation {
            ToolEffect::Reconciliation(Box::new(value), question_disposition)
        } else if let Some(value) = question_disposition {
            ToolEffect::Question(value)
        } else {
            ToolEffect::None
        },
    }
}

pub(in crate::session) struct ToolExecutionScope<'a> {
    pub(in crate::session) tools: &'a ToolServerHandle,
    pub(in crate::session) mode: SessionMode,
    pub(in crate::session) policy: &'a TurnPolicy,
    pub(in crate::session) catalog: &'a Arc<zevria_instructions::skill::SkillCatalog>,
    pub(in crate::session) fixed_tokens: u64,
    pub(in crate::session) instruction_state: Option<zevria_instructions::DirectiveState>,
    pub(in crate::session) instructions: anyhow::Result<Arc<RequestPlan>>,
    pub(in crate::session) skill_activation_available: bool,
    pub(in crate::session) input_token_limit: u64,
    pub(in crate::session) turn: &'a TurnContext,
    pub(in crate::session) reconciliation_catalog: Option<&'a ReportReconciliationCatalog>,
    pub(in crate::session) planning: Option<PlanWorkflowState>,
}

pub(in crate::session) async fn execute_ordinary_tool_call(
    scope: &ToolExecutionScope<'_>,
    call: &EngineCall<'_>,
    active: &mut ActiveSkills,
    plan_scope: Option<&PlanDraftScope>,
) -> ToolCallSlot {
    if scope.turn.is_cancelled() {
        return cancelled_slot(call);
    }
    if (call.tool == EngineTool::Skill && !scope.skill_activation_available)
        || !scope.policy.allows_tool(&call.function.name)
        || (call.tool.is_skill() && !scope.policy.skills_enabled)
    {
        return denied_slot(call, scope.mode);
    }
    let skill_context = call
        .tool
        .is_skill()
        .then(|| zevria_instructions::skill::SkillContext {
            catalog: scope.catalog.clone(),
            pins: active.clone(),
            mode_enabled: scope.skill_activation_available,
        });
    if call.tool != EngineTool::Skill {
        return dispatch_tool_call(
            scope.tools,
            call,
            scope.turn,
            scope.reconciliation_catalog,
            skill_context.as_ref(),
            plan_scope,
        )
        .await;
    }

    let request = match skill_call_request(call) {
        Ok(request) => request,
        Err(error) => return skill_lifecycle_error_slot(call, error),
    };
    let Some(state) = &scope.instruction_state else {
        return skill_lifecycle_error_slot(call, "instruction state is unavailable");
    };
    let instructions = match &scope.instructions {
        Ok(instructions) => instructions,
        Err(error) => return skill_lifecycle_error_slot(call, error),
    };
    let preparation = crate::session::manage::skills::SkillApplicationPreparation {
        context: skill_context.as_ref().expect("skill context"),
        instructions,
        state,
        fixed_tokens: scope.fixed_tokens,
        input_token_limit: scope.input_token_limit,
    };
    let crate::session::manage::skills::PreparedSkillApplication {
        application,
        prospective,
        ..
    } = match preparation.prepare(
        &request.skill,
        zevria_instructions::skill::SkillInvocationOrigin::Model,
        None,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return skill_lifecycle_error_slot(call, error),
    };
    if scope.turn.is_cancelled() {
        return cancelled_slot(call);
    }
    // Only accepted engine applications may produce success acknowledgements.
    let mut accepted = ToolCallSlot::local(
        call,
        ToolCallOutcome::Success,
        application.acknowledgement(&request),
    );
    accepted.effect =
        ToolEffect::Skill(Box::new(zevria_instructions::skill::SkillToolApplication {
            call_id: call.id.to_string(),
            application,
        }));
    *active = prospective;
    accepted
}

pub(in crate::session) fn ensemble_gate_denied_slot(
    call: &EngineCall<'_>,
    reason: impl fmt::Display,
) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Denied,
        "status: denied\nreason: ",
        reason.to_string(),
    )
}

pub(in crate::session) struct BatchShape {
    pub(in crate::session) standalone_question: bool,
    pub(in crate::session) contains_reconciliation: bool,
    pub(in crate::session) contains_question: bool,
}
impl BatchShape {
    pub(in crate::session) fn new(calls: &[EngineCall<'_>]) -> Self {
        Self {
            standalone_question: calls.len() == 1 && calls[0].tool == EngineTool::Question,
            contains_reconciliation: calls
                .iter()
                .any(|call| call.tool == EngineTool::ReconcileReports),
            contains_question: calls.iter().any(|call| call.tool == EngineTool::Question),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum BatchPolicy {
    SkillOnly,
    Serialized,
    Default,
    MixedRejected,
}
pub(in crate::session) struct BatchPlan<'a> {
    pub(in crate::session) calls: Vec<EngineCall<'a>>,
    policy: BatchPolicy,
    shape: BatchShape,
}
impl<'a> BatchPlan<'a> {
    pub(in crate::session) fn new(calls: &'a [ToolCall], gate: &PlanSubmissionGate) -> Self {
        let calls = calls.iter().map(EngineCall::new).collect::<Vec<_>>();
        let skill = calls.iter().any(|call| call.tool == EngineTool::Skill);
        let policy = if skill && calls.iter().any(|call| call.tool != EngineTool::Skill) {
            BatchPolicy::MixedRejected
        } else if gate.ensemble.is_some()
            || gate.candidate_accepted()
            || calls.iter().any(|call| call.tool.is_plan())
        {
            BatchPolicy::Serialized
        } else if skill {
            BatchPolicy::SkillOnly
        } else {
            BatchPolicy::Default
        };
        let shape = BatchShape::new(&calls);
        Self {
            calls,
            policy,
            shape,
        }
    }
}
fn ensemble_gate_decision(
    call: &EngineCall<'_>,
    shape: &BatchShape,
    gate: &PlanSubmissionGate,
) -> Option<&'static str> {
    let ensemble = gate.ensemble.as_ref()?;
    match call.tool {
        EngineTool::ReconcileReports if !ensemble.inspection_completed => Some(
            "complete at least one terminal `command` inspection attempt before calling reconcile_reports",
        ),
        EngineTool::ReconcileReports => None,
        EngineTool::Question if !shape.standalone_question => {
            Some("the required root question must be the only tool call in its assistant response")
        }
        EngineTool::Question if ensemble.reconciliation.is_none() => {
            Some("call reconcile_reports after inspection before opening a root question")
        }
        EngineTool::Question if !ensemble.requires_question() => Some(
            "the accepted reconciliation does not require a root question; apply recorded decisions and continue to submit_plan",
        ),
        EngineTool::Question if ensemble.question_disposition.is_some() => Some(
            "the single root question attempt already reached a terminal disposition; do not repeat it",
        ),
        EngineTool::Question => None,
        EngineTool::SubmitPlan if shape.contains_reconciliation => Some(
            "submit_plan must occur in a later assistant round after reconcile_reports has completed",
        ),
        EngineTool::SubmitPlan if shape.contains_question => Some(
            "submit_plan cannot share an assistant response with question; wait for the standalone question result",
        ),
        EngineTool::SubmitPlan if !ensemble.inspection_completed => {
            Some("complete at least one terminal `command` inspection attempt before submit_plan")
        }
        EngineTool::SubmitPlan if ensemble.reconciliation.is_none() => {
            Some("call reconcile_reports and obtain an accepted declaration before submit_plan")
        }
        EngineTool::SubmitPlan
            if ensemble.requires_question() && ensemble.question_disposition.is_none() =>
        {
            Some(
                "the accepted reconciliation requires one standalone root question before submit_plan",
            )
        }
        _ => None,
    }
}

/// Execute one assistant call batch. Each `launch_subtasks` call internally
/// enqueues all its entries before collecting reports, even under serialized
/// dispatch. Multiple launch batches also run concurrently — with each other and with the
/// ordinary tools, which keep executing sequentially in assistant order.
/// Result slots keep the original assistant-call order for correlation and
/// transcripts. Historical pins seed the batch: first valid calls pin, while
/// same-batch and later duplicates reapply. Both produce typed applications and
/// engine-generated bodyless acknowledgements.
#[cfg(test)]
pub(in crate::session) async fn execute_tool_calls(
    scope: &ToolExecutionScope<'_>,
    calls: &[ToolCall],
    active: ActiveSkills,
    gate: &PlanSubmissionGate,
) -> ToolResultBatch {
    execute_planned_tool_calls(scope, &BatchPlan::new(calls, gate), active, gate).await
}

pub(in crate::session) async fn execute_planned_tool_calls(
    scope: &ToolExecutionScope<'_>,
    plan: &BatchPlan<'_>,
    active_skills: ActiveSkills,
    submission_gate: &PlanSubmissionGate,
) -> ToolResultBatch {
    let calls = &plan.calls;
    let _timing = LifecycleTiming::new("execute_tool_calls", calls.len());
    // Validate the entire response before constructing concurrent futures or
    // launching any call. A failed boundary may not partially apply a skill.
    if plan.policy == BatchPolicy::MixedRejected {
        return assemble_tool_result_batch(calls.iter().map(|call| {
            if scope.turn.is_cancelled() {
                cancelled_slot(call)
            } else {
                ToolCallSlot::local_diagnostic(call, ToolCallOutcome::Denied, "status: denied\nreason: ",
                    "activation must complete in a skill-only response; this entire mixed batch was rejected without executing any calls. Invoke/reapply only skill calls, wait for their instructions, then issue task tools in a later response.".into())
            }
        }).collect(), None);
    }
    let is_launch = |call: &EngineCall<'_>| call.tool == EngineTool::LaunchSubtasks;
    if plan.policy == BatchPolicy::Serialized {
        let gate = submission_gate;
        let shape = &plan.shape;
        let mut accepted = gate.candidate_accepted();
        let mut candidate = None;
        let mut active = active_skills;
        let mut slots = Vec::with_capacity(calls.len());
        let mut prospective = scope.planning.clone();
        let mut edit_failed = false;
        for call in calls {
            let draft_scope = prospective
                .as_ref()
                .and_then(|state| state.draft_scope(scope.turn.id));
            let mut slot = if scope.turn.is_cancelled() {
                cancelled_slot(call)
            } else if accepted {
                candidate_locked_slot(call)
            } else if let Some(reason) = ensemble_gate_decision(call, shape, gate) {
                ensemble_gate_denied_slot(call, reason)
            } else if call.tool == EngineTool::SubmitPlan && edit_failed {
                ToolCallSlot::local_diagnostic(call, ToolCallOutcome::Denied, "status: denied\nreason: ", "submit_plan cannot follow a failed create_plan or update_plan in the same response; call read_plan and fix the draft first".into())
            } else {
                execute_ordinary_tool_call(scope, call, &mut active, draft_scope.as_ref()).await
            };
            if let Some(draft) = slot.effect.draft() {
                let validated = prospective
                    .clone()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Plan draft was returned without a Planning scope")
                    })
                    .and_then(|state| {
                        apply_plan_record(
                            state,
                            &PlanRecord::DraftSaved {
                                draft: draft.clone(),
                            },
                        )
                    });
                match validated {
                    Ok(state) => prospective = Some(state),
                    Err(error) => {
                        slot = ToolCallSlot::local_diagnostic(
                            call,
                            ToolCallOutcome::Error,
                            "status: error\nerror: ",
                            error.to_string(),
                        )
                    }
                }
            } else if slot.metadata.outcome.is_success() && call.tool.authors_draft() {
                slot = ToolCallSlot::local_diagnostic(
                    call,
                    ToolCallOutcome::Error,
                    "status: error\nerror: ",
                    "Plan authoring tool did not return a stored draft".into(),
                );
            }
            if call.tool == EngineTool::SubmitPlan
                && slot.metadata.outcome.is_success()
                && !slot.effect.candidate().is_some_and(|candidate| {
                    prospective
                        .as_ref()
                        .and_then(PlanWorkflowState::working_copy)
                        .is_some_and(|copy| copy.markdown() == candidate.markdown())
                })
            {
                slot = ToolCallSlot::local_diagnostic(
                    call,
                    ToolCallOutcome::Error,
                    "status: error\nerror: ",
                    "submitted candidate must match the stored Plan working copy unchanged".into(),
                );
            }
            if call.tool.authors_draft() && !slot.metadata.outcome.is_success() {
                edit_failed = true;
            }
            if let Some(submitted) = slot.effect.candidate() {
                accepted = true;
                candidate = Some(submitted.clone());
            }
            slots.push(slot);
        }
        return assemble_tool_result_batch(slots, candidate);
    }

    let launches = futures_util::future::join_all(
        calls
            .iter()
            .enumerate()
            .filter(|(_, call)| is_launch(call))
            .map(|(index, call)| async move {
                let slot = if scope.turn.is_cancelled() {
                    cancelled_slot(call)
                } else if scope.policy.allows_tool(&call.function.name) {
                    dispatch_tool_call(
                        scope.tools,
                        call,
                        scope.turn,
                        scope.reconciliation_catalog,
                        None,
                        None,
                    )
                    .await
                } else {
                    denied_slot(call, scope.mode)
                };
                (index, slot)
            }),
    );
    let ordinary = async {
        let mut active = active_skills;
        let mut completed = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if is_launch(call) {
                continue;
            }
            let slot = execute_ordinary_tool_call(scope, call, &mut active, None).await;
            completed.push((index, slot));
        }
        completed
    };
    let (launch_slots, ordinary_slots) = tokio::join!(launches, ordinary);

    let mut slots: Vec<Option<ToolCallSlot>> =
        std::iter::repeat_with(|| None).take(calls.len()).collect();
    for (index, slot) in launch_slots.into_iter().chain(ordinary_slots) {
        slots[index] = Some(slot);
    }

    assemble_tool_result_batch(slots.into_iter().flatten().collect(), None)
}

pub(in crate::session) fn assemble_tool_result_batch(
    slots: Vec<ToolCallSlot>,
    mut candidate: Option<PlanCandidate>,
) -> ToolResultBatch {
    let mut results = Vec::with_capacity(slots.len());
    let mut metadata = Vec::with_capacity(slots.len());
    let mut reconciliation = None;
    let mut question_disposition = None;
    let mut inspection_attempted = false;
    let mut skill_applications = Vec::new();
    let mut drafts = Vec::new();
    for slot in slots {
        if slot.tool == EngineTool::Command
            && !matches!(slot.metadata.outcome, ToolCallOutcome::Denied)
        {
            inspection_attempted = true;
        }
        if let Some(disposition) = slot.effect.question_disposition() {
            question_disposition = Some(disposition);
        }
        match slot.effect {
            ToolEffect::Candidate(value, _) => {
                if candidate.is_none() {
                    candidate = Some(*value);
                }
            }
            ToolEffect::Draft(value, _) => drafts.push(*value),
            ToolEffect::Reconciliation(value, _) => reconciliation = Some(*value),
            ToolEffect::Skill(value) => skill_applications.push(*value),
            ToolEffect::Question(_) | ToolEffect::None => {}
        }
        results.push(slot.result);
        metadata.push(slot.metadata);
    }

    ToolResultBatch {
        message: Message::User { content: results },
        metadata,
        candidate,
        drafts,
        reconciliation,
        question_disposition,
        inspection_attempted,
        skill_applications,
    }
}

impl ToolCallSlot {
    /// Capture the readable diagnostic before constructing the unchanged model
    /// envelope. Renderers never need to reverse-parse provider result text.
    fn local_diagnostic(
        call: &EngineCall<'_>,
        outcome: ToolCallOutcome,
        prefix: &str,
        diagnostic: String,
    ) -> Self {
        let mut slot = Self::local(call, outcome, format!("{prefix}{diagnostic}"));
        slot.metadata.diagnostic = Some(diagnostic);
        slot
    }

    fn local(call: &EngineCall<'_>, outcome: ToolCallOutcome, output: String) -> Self {
        Self {
            result: UserContent::ToolResult(ToolResult {
                call: call.id.clone(),
                name: call.function.name.clone(),
                content: vec![ToolResultContent::text(output)],
                is_error: !outcome.is_success(),
            }),
            metadata: ToolResultMetadata {
                diagnostic: None,
                id: call.id.to_string(),
                call_id: call
                    .id
                    .provider()
                    .map(|provider| provider.as_str().to_owned()),
                tool_name: call.function.name.to_string(),
                outcome,
                detail: None,
            },
            tool: call.tool,
            effect: ToolEffect::None,
        }
    }
}
impl<'a> ToolExecutionScope<'a> {
    pub(in crate::session) fn new<P: ModelProvider>(
        engine: &'a SessionEngine<P>,
        mode: SessionMode,
        policy: &'a TurnPolicy,
        turn: &'a TurnContext,
        gate: &'a PlanSubmissionGate,
    ) -> Self {
        Self {
            tools: &engine.tools,
            mode,
            policy,
            catalog: &engine.skills.catalog,
            fixed_tokens: engine.request_plan(policy).irreducible_tokens,
            instruction_state: engine.directive_state().ok().cloned(),
            instructions: engine.instruction_preparation(policy),
            skill_activation_available: engine.skill_activation_available(policy),
            input_token_limit: engine.context_policy(policy.model_role).input_token_limit,
            turn,
            reconciliation_catalog: gate.reconciliation_catalog(),
            planning: match (mode, engine.plan_state()) {
                (SessionMode::Plan, Ok(state @ PlanWorkflowState::Planning { .. })) => {
                    Some(state.clone())
                }
                _ => None,
            },
        }
    }
}
