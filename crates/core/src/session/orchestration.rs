//! Accepted-turn-only obligation. Neither history nor model prose supplies proof.
use super::*;
use zevria_foundation::{RequestBehavior, RequestMetadata};

pub(super) struct OrchestrationGate {
    request: Option<RequestMetadata>,
    qualified: bool,
    corrected: bool,
}

impl OrchestrationGate {
    pub(super) fn new(accepted: &AcceptedTurn) -> Self {
        Self {
            request: accepted
                .request
                .clone()
                .filter(|request| request.behavior == RequestBehavior::Orchestrate),
            qualified: false,
            corrected: false,
        }
    }

    pub(super) fn satisfied(&self) -> bool {
        self.request.is_none() || self.qualified
    }

    /// Called only after the complete correlated batch is durable and replay-valid.
    pub(super) fn observe(&mut self, calls: &[ToolCall], batch: &ToolResultBatch) {
        if self.request.is_none() {
            return;
        }
        let Message::User { content } = &batch.message else {
            return;
        };
        self.qualified |= batch.metadata.iter().any(|metadata| {
            metadata.tool_name == LAUNCH_SUBTASKS_TOOL_NAME
                && calls.iter().any(|call| call.function.name == LAUNCH_SUBTASKS_TOOL_NAME
                    && call.id.as_str() == metadata.id
                    && call.provider.as_ref().map(|provider| &provider.call_id) == metadata.call_id.as_ref())
                && content.iter().any(|block| matches!(block, UserContent::ToolResult(result)
                    if result.call.as_str() == metadata.id && result.name == LAUNCH_SUBTASKS_TOOL_NAME
                        && result.provider.as_ref().map(|provider| &provider.call_id) == metadata.call_id.as_ref()))
                && metadata.subtasks().iter().filter_map(|entry| entry.launch.as_ref().map(|launch| &launch.id))
                    .collect::<std::collections::HashSet<_>>().len() >= 2
        });
    }

    pub(super) fn correction(&mut self) -> Option<TranscriptItem> {
        if self.satisfied() || self.corrected {
            return None;
        }
        self.corrected = true;
        Some(TranscriptItem::RequestDirective(
            zevria_instructions::RequestDirective::correction(
                self.request.clone().expect("required request"),
            ),
        ))
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    /// The composition root supplies exactly the supervisor's captured capacity.
    /// Workers/children leave this unset, even when nominally in Build mode.
    pub fn with_subtask_concurrency(mut self, limit: usize) -> Self {
        self.capabilities.subtask_concurrency = Some(limit);
        self
    }

    pub(super) fn admit_orchestration(
        &self,
        mode: SessionMode,
        policy: &TurnPolicy,
    ) -> Result<(), Rejection> {
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.orchestration_checks += 1);
        if mode != SessionMode::Build
            || !policy.orchestration
            || self.capabilities.subtask_concurrency.is_none()
        {
            return Err("/orchestrate <prompt> is available only for a supported root Build request; it cannot switch modes or approve a Plan".to_string().into());
        }
        if matches!(
            self.plan_state()?,
            PlanWorkflowState::Planning { .. } | PlanWorkflowState::Ready { .. }
        ) {
            return Err("orchestration cannot revise or approve a pending Plan workflow; resolve the Plan explicitly first".to_string().into());
        }
        if !policy.allows_tool(LAUNCH_SUBTASKS_TOOL_NAME)
            || !self
                .tools
                .static_tool_defs()
                .iter()
                .any(|tool| tool.name == LAUNCH_SUBTASKS_TOOL_NAME)
        {
            return Err(
                "orchestration requires the registered and permitted launch_subtasks capability"
                    .to_string()
                    .into(),
            );
        }
        if self.capabilities.subtask_concurrency.unwrap_or(0) < 2 {
            return Err("orchestration requires session.max_concurrent_subtasks >= 2; increase the limit or resubmit without /orchestrate".to_string().into());
        }
        Ok(())
    }
}
