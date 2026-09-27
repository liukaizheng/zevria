//! Tools orchestration.

use super::*;

#[cfg(test)]
#[path = "tools_metadata_tests.rs"]
mod metadata_tests;

pub(super) fn assistant_tool_calls(message: &Message) -> Vec<ToolCall> {
    let Message::Assistant { content, .. } = message else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn tool_arguments_for_dispatch(arguments: &serde_json::Value) -> String {
    match arguments {
        // Adapters preserve malformed streamed JSON as a string. Dispatch the
        // original bytes so Rig returns its useful argument error to the model.
        serde_json::Value::String(arguments) => arguments.clone(),
        arguments => arguments.to_string(),
    }
}

#[derive(Debug)]
pub(super) struct ToolResultBatch {
    pub(super) message: Message,
    pub(super) metadata: Vec<ToolResultMetadata>,
    pub(super) candidate: Option<PlanCandidate>,
    pub(super) reconciliation: Option<ValidatedReportReconciliation>,
    pub(super) question_disposition: Option<QuestionTerminalDisposition>,
    pub(super) inspection_attempted: bool,
    pub(super) skill_applications: Vec<zevria_instructions::skill::SkillToolApplication>,
}

/// One executed (or denied) call's correlated result and display metadata.
pub(super) struct ToolCallSlot {
    pub(super) result: UserContent,
    pub(super) metadata: ToolResultMetadata,
    pub(super) candidate: Option<PlanCandidate>,
    pub(super) reconciliation: Option<ValidatedReportReconciliation>,
    pub(super) question_disposition: Option<QuestionTerminalDisposition>,
    pub(super) skill_application: Option<zevria_instructions::skill::SkillToolApplication>,
}

pub(super) fn denied_slot(call: &ToolCall, mode: SessionMode) -> ToolCallSlot {
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

pub(super) fn candidate_locked_slot(call: &ToolCall) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(call, ToolCallOutcome::Denied, "status: denied\nreason: ", "a Plan artifact was already accepted for this turn; make no more tool calls and provide a short final confirmation".into())
}

pub(super) fn cancelled_slot(call: &ToolCall) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Cancelled,
        "status: cancelled\nreason: ",
        "the parent turn was cancelled before this call started".into(),
    )
}

/// Strict parsing is engine-owned; raw tool dispatch cannot admit an application.
pub(super) fn skill_call_request(call: &ToolCall) -> anyhow::Result<SkillRequest> {
    Ok(serde_json::from_str(&tool_arguments_for_dispatch(
        &call.function.arguments,
    ))?)
}

pub(super) fn skill_lifecycle_error_slot(
    call: &ToolCall,
    error: impl fmt::Display,
) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Error,
        "status: error\nerror: ",
        format!("skill application was rejected: {error}"),
    )
}

pub(super) async fn dispatch_tool_call(
    tools: &ToolServerHandle,
    call: &ToolCall,
    turn: &TurnContext,
    reconciliation_catalog: Option<&ReportReconciliationCatalog>,
    skill_context: Option<&zevria_instructions::skill::SkillContext>,
) -> ToolCallSlot {
    let arguments = tool_arguments_for_dispatch(&call.function.arguments);
    // Per-call context carries the assistant call id into the tool, so tools
    // that announce work to the frontend (the subtask tool) can correlate
    // their events with this pending call's row.
    let mut context = ToolContext::new();
    context.insert(ToolCallId(call.id.to_string()));
    context.insert(turn.clone());
    if let Some(skill_context) = skill_context {
        context.insert(skill_context.clone());
    }
    if let Some(catalog) = reconciliation_catalog {
        context.insert(catalog.clone());
    }
    let execution = tools
        .execute(&call.function.name, &arguments, &mut context)
        .await;
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
    let detail = context.result::<ToolResultDetail>().cloned();
    if context.result::<ToolCancelled>().is_some() {
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
    let candidate = (execution.is_success() && call.function.name == SUBMIT_PLAN_TOOL_NAME)
        .then(|| context.result::<PlanCandidate>().cloned())
        .flatten();
    let reconciliation = (execution.is_success()
        && call.function.name == RECONCILE_REPORTS_TOOL_NAME)
        .then(|| {
            let accepted = context.result::<ValidatedReportReconciliation>()?.clone();
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
            provider: call.provider.clone(),
            name: call.function.name.clone(),
            content: vec![ToolResultContent::text(output)],
        }),
        metadata: ToolResultMetadata {
            diagnostic,
            id: call.id.to_string(),
            call_id: call
                .provider
                .as_ref()
                .map(|provider| provider.call_id.clone()),
            tool_name: call.function.name.clone(),
            outcome,
            detail,
        },
        candidate,
        reconciliation,
        question_disposition,
        skill_application: None,
    }
}

pub(super) struct ToolExecutionScope<'a> {
    pub(super) tools: &'a ToolServerHandle,
    pub(super) mode: SessionMode,
    pub(super) policy: &'a TurnPolicy,
    pub(super) catalog: &'a Arc<zevria_instructions::skill::SkillCatalog>,
    pub(super) fixed_tokens: u64,
    pub(super) instruction_state: Option<zevria_instructions::DirectiveState>,
    pub(super) instructions: anyhow::Result<super::directives::InstructionPreparation>,
    pub(super) skill_activation_available: bool,
    pub(super) input_token_limit: u64,
    pub(super) turn: &'a TurnContext,
    pub(super) reconciliation_catalog: Option<&'a ReportReconciliationCatalog>,
}

pub(super) async fn execute_ordinary_tool_call(
    scope: &ToolExecutionScope<'_>,
    call: &ToolCall,
    active: &mut ActiveSkills,
) -> ToolCallSlot {
    if scope.turn.is_cancelled() {
        return cancelled_slot(call);
    }
    if (call.function.name == SKILL_TOOL_NAME && !scope.skill_activation_available)
        || !scope.policy.allows_tool(&call.function.name)
        || (zevria_foundation::SKILL_TOOL_NAMES.contains(&call.function.name.as_str())
            && !scope.policy.skills_enabled)
    {
        return denied_slot(call, scope.mode);
    }
    let skill_context = zevria_foundation::SKILL_TOOL_NAMES
        .contains(&call.function.name.as_str())
        .then(|| zevria_instructions::skill::SkillContext {
            catalog: scope.catalog.clone(),
            pins: active.clone(),
            mode_enabled: scope.skill_activation_available,
        });
    if call.function.name != SKILL_TOOL_NAME {
        return dispatch_tool_call(
            scope.tools,
            call,
            scope.turn,
            scope.reconciliation_catalog,
            skill_context.as_ref(),
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
    let preparation = super::skills::SkillApplicationPreparation {
        context: skill_context.as_ref().expect("skill context"),
        instructions,
        state,
        fixed_tokens: scope.fixed_tokens,
        input_token_limit: scope.input_token_limit,
    };
    let super::skills::PreparedSkillApplication {
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
    accepted.skill_application = Some(zevria_instructions::skill::SkillToolApplication {
        call_id: call.id.to_string(),
        application,
    });
    *active = prospective;
    accepted
}

pub(super) fn ensemble_gate_denied_slot(
    call: &ToolCall,
    reason: impl fmt::Display,
) -> ToolCallSlot {
    ToolCallSlot::local_diagnostic(
        call,
        ToolCallOutcome::Denied,
        "status: denied\nreason: ",
        reason.to_string(),
    )
}

struct BatchShape {
    standalone_question: bool,
    contains_reconciliation: bool,
    contains_question: bool,
}
impl BatchShape {
    fn new(calls: &[ToolCall]) -> Self {
        Self {
            standalone_question: calls.len() == 1 && calls[0].function.name == QUESTION_TOOL_NAME,
            contains_reconciliation: calls
                .iter()
                .any(|call| call.function.name == RECONCILE_REPORTS_TOOL_NAME),
            contains_question: calls
                .iter()
                .any(|call| call.function.name == QUESTION_TOOL_NAME),
        }
    }
}
enum BatchPolicy<'a> {
    Concurrent,
    Serialized { gate: &'a PlanSubmissionGate },
}
fn ensemble_gate_decision(
    call: &ToolCall,
    shape: &BatchShape,
    gate: &PlanSubmissionGate,
) -> Option<&'static str> {
    let ensemble = gate.ensemble.as_ref()?;
    match call.function.name.as_str() {
        RECONCILE_REPORTS_TOOL_NAME if !ensemble.inspection_completed => Some(
            "complete at least one terminal `command` inspection attempt before calling reconcile_reports",
        ),
        RECONCILE_REPORTS_TOOL_NAME => None,
        QUESTION_TOOL_NAME if !shape.standalone_question => {
            Some("the required root question must be the only tool call in its assistant response")
        }
        QUESTION_TOOL_NAME if ensemble.reconciliation.is_none() => {
            Some("call reconcile_reports after inspection before opening a root question")
        }
        QUESTION_TOOL_NAME if !ensemble.requires_question() => Some(
            "the accepted reconciliation does not require a root question; apply recorded decisions and continue to submit_plan",
        ),
        QUESTION_TOOL_NAME if ensemble.question_disposition.is_some() => Some(
            "the single root question attempt already reached a terminal disposition; do not repeat it",
        ),
        QUESTION_TOOL_NAME => None,
        SUBMIT_PLAN_TOOL_NAME if shape.contains_reconciliation => Some(
            "submit_plan must occur in a later assistant round after reconcile_reports has completed",
        ),
        SUBMIT_PLAN_TOOL_NAME if shape.contains_question => Some(
            "submit_plan cannot share an assistant response with question; wait for the standalone question result",
        ),
        SUBMIT_PLAN_TOOL_NAME if !ensemble.inspection_completed => {
            Some("complete at least one terminal `command` inspection attempt before submit_plan")
        }
        SUBMIT_PLAN_TOOL_NAME if ensemble.reconciliation.is_none() => {
            Some("call reconcile_reports and obtain an accepted declaration before submit_plan")
        }
        SUBMIT_PLAN_TOOL_NAME
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
pub(super) async fn execute_tool_calls(
    scope: &ToolExecutionScope<'_>,
    calls: &[ToolCall],
    active_skills: ActiveSkills,
    submission_gate: &PlanSubmissionGate,
) -> ToolResultBatch {
    // Validate the entire response before constructing concurrent futures or
    // launching any call. A failed boundary may not partially apply a skill.
    if calls
        .iter()
        .any(|call| call.function.name == SKILL_TOOL_NAME)
        && calls
            .iter()
            .any(|call| call.function.name != SKILL_TOOL_NAME)
    {
        return assemble_tool_result_batch(calls.iter().map(|call| {
            if scope.turn.is_cancelled() {
                cancelled_slot(call)
            } else {
                ToolCallSlot::local_diagnostic(call, ToolCallOutcome::Denied, "status: denied\nreason: ",
                    "activation must complete in a skill-only response; this entire mixed batch was rejected without executing any calls. Invoke/reapply only skill calls, wait for their instructions, then issue task tools in a later response.".into())
            }
        }).collect(), None);
    }
    let is_launch = |call: &ToolCall| call.function.name == LAUNCH_SUBTASKS_TOOL_NAME;
    let policy = if submission_gate.ensemble.is_some()
        || submission_gate.candidate_accepted()
        || calls
            .iter()
            .any(|call| call.function.name == SUBMIT_PLAN_TOOL_NAME)
    {
        BatchPolicy::Serialized {
            gate: submission_gate,
        }
    } else {
        BatchPolicy::Concurrent
    };
    if let BatchPolicy::Serialized { gate } = policy {
        let shape = BatchShape::new(calls);
        let mut accepted = gate.candidate_accepted();
        let mut candidate = None;
        let mut active = active_skills;
        let mut slots = Vec::with_capacity(calls.len());
        for call in calls {
            let slot = if scope.turn.is_cancelled() {
                cancelled_slot(call)
            } else if accepted {
                candidate_locked_slot(call)
            } else if let Some(reason) = ensemble_gate_decision(call, &shape, gate) {
                ensemble_gate_denied_slot(call, reason)
            } else {
                execute_ordinary_tool_call(scope, call, &mut active).await
            };
            if let Some(submitted) = &slot.candidate {
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
            let slot = execute_ordinary_tool_call(scope, call, &mut active).await;
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

pub(super) fn assemble_tool_result_batch(
    slots: Vec<ToolCallSlot>,
    mut candidate: Option<PlanCandidate>,
) -> ToolResultBatch {
    let mut results = Vec::with_capacity(slots.len());
    let mut metadata = Vec::with_capacity(slots.len());
    let mut reconciliation = None;
    let mut question_disposition = None;
    let mut inspection_attempted = false;
    let mut skill_applications = Vec::new();
    for slot in slots {
        if candidate.is_none() {
            candidate = slot.candidate;
        }
        if slot.metadata.tool_name == "command"
            && !matches!(slot.metadata.outcome, ToolCallOutcome::Denied)
        {
            inspection_attempted = true;
        }
        if slot.reconciliation.is_some() {
            reconciliation = slot.reconciliation;
        }
        if slot.question_disposition.is_some() {
            question_disposition = slot.question_disposition;
        }
        if let Some(application) = slot.skill_application {
            skill_applications.push(application);
        }
        results.push(slot.result);
        metadata.push(slot.metadata);
    }

    ToolResultBatch {
        message: Message::User { content: results },
        metadata,
        candidate,
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
        call: &ToolCall,
        outcome: ToolCallOutcome,
        prefix: &str,
        diagnostic: String,
    ) -> Self {
        let mut slot = Self::local(call, outcome, format!("{prefix}{diagnostic}"));
        slot.metadata.diagnostic = Some(diagnostic);
        slot
    }

    fn local(call: &ToolCall, outcome: ToolCallOutcome, output: String) -> Self {
        Self {
            result: UserContent::ToolResult(ToolResult {
                call: call.id.clone(),
                provider: call.provider.clone(),
                name: call.function.name.clone(),
                content: vec![ToolResultContent::text(output)],
            }),
            metadata: ToolResultMetadata {
                diagnostic: None,
                id: call.id.to_string(),
                call_id: call
                    .provider
                    .as_ref()
                    .map(|provider| provider.call_id.clone()),
                tool_name: call.function.name.clone(),
                outcome,
                detail: None,
            },
            candidate: None,
            reconciliation: None,
            question_disposition: None,
            skill_application: None,
        }
    }
}
impl<'a> ToolExecutionScope<'a> {
    pub(super) fn new<P: ModelProvider>(
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
            fixed_tokens: engine.fixed_input_tokens(policy),
            instruction_state: engine.directive_state().ok().cloned(),
            instructions: engine.instruction_preparation(policy),
            skill_activation_available: engine.skill_activation_available(policy),
            input_token_limit: engine.context_policy(policy.model_role).input_token_limit,
            turn,
            reconciliation_catalog: gate.reconciliation_catalog(),
        }
    }
}
