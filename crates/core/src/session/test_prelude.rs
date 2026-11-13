//! Unit-test conveniences only; production modules import their own dependencies.
#![allow(unused_imports)]
pub(super) use crate::session::kernel::accounting::{
    CapacityAssessment, CapacityBand, CapacityCandidates, ContextAccounting, ContextUsageTracker,
    TurnInputCountState, count_input_tokens_for_turn,
};
pub(super) use crate::session::kernel::capacity::{
    CapacityCompactionState, prepared_fixed_tokens, prepared_request_identity,
    prepared_request_shape,
};
pub(super) use crate::session::kernel::directives::PreparedInstructionUpdates;
pub(super) use crate::session::kernel::records::{
    Committed, CompletedItems, RecordChange, RecordError, TurnAnchor, recoverable,
};
pub(super) use crate::session::kernel::request_plan::{HistoryView, RequestPlan};
pub(super) use crate::session::kernel::state::{
    EnginePhase, GuidanceState, ModelManagement, ModelManagementContext, RequestShapeFingerprint,
    RootCapabilities, SkillState, request_shape_fingerprint,
};
pub(super) use crate::session::kernel::timing::{LifecycleTiming, RoundPhase, RoundTimings};
pub(super) use crate::session::manage::skills::{
    PreparedSkillApplication, SkillApplicationPreparation,
};
pub(super) use crate::session::turn::execution::{
    AcceptedKind, AcceptedTurn, Failure, Rejection, TurnCompletion, TurnWork, terminal_records,
};
pub(super) use crate::session::turn::job_notices::JobFollowState;
pub(super) use crate::session::turn::model_loop::{
    DispatchState, FinalResponsePersistence, ModelTurnOutput, TurnFinalization,
};
pub(super) use crate::session::turn::orchestration::OrchestrationGate;
pub(super) use crate::session::turn::runtime::{CommandAction, command_control_reply};
pub(super) use crate::session::turn::tools::{
    BatchPlan, BatchShape, EngineCall, EngineTool, ToolCallSlot, ToolEffect, ToolExecutionScope,
    ToolResultBatch, assemble_tool_result_batch, assistant_tool_calls, cancelled_slot,
    candidate_locked_slot, denied_slot, dispatch_tool_call, ensemble_gate_denied_slot,
    execute_ordinary_tool_call, execute_planned_tool_calls, execute_tool_calls, skill_call_request,
    skill_lifecycle_error_slot, tool_arguments_for_dispatch,
};
pub(super) use crate::session::workflow::compaction::{
    CheckpointPlacement, CompactionRequest, CompactionSource, append_plan_context,
    append_task_snapshot_context, automatic_compaction_armed_for, snapshot_conversation_input,
    snapshot_model_input,
};
pub(super) use crate::session::workflow::ensemble::{
    RecoveredEnsembleSynthesis, invalid_plan_outcomes, invalid_plan_summaries,
    recovered_ensemble_synthesis,
};
pub(super) use crate::session::workflow::plan::{
    EnsemblePlanSubmissionGate, PLAN_SLUG_MAX_CHARS, PLAN_SLUG_MAX_WORDS, PendingPlanGateCall,
    PlanSubmissionGate, ensemble_plan_terminal_error, plan_submission_gate_after_reports,
    slug_words, workflow_transition_for_mode,
};
pub(super) use crate::session::workflow::prompt::{
    PreparedPrompt, PromptAnchorState, PromptContext, PromptMeasurement, PromptPreflight,
    PromptTurnInput, PromptTurnKind,
};
pub(super) use crate::session::workflow::worker_review::validate_frozen_workers;
pub(super) use futures_util::FutureExt;
pub(super) use rig_agent::tool::ToolContext;
pub(super) use rig_agent::tool::server::ToolServerHandle;
pub(super) use rig_core::message::{
    AssistantContent, Message, ToolCall, ToolResult, ToolResultContent, UserContent,
};
pub(super) use sha2::{Digest as _, Sha256};
pub(super) use std::collections::{HashMap, HashSet, VecDeque};
pub(super) use std::fmt;
pub(super) use std::future::Future;
pub(super) use std::path::PathBuf;
pub(super) use std::sync::{Arc, Mutex};
pub(super) use tokio::sync::mpsc;
pub(super) use tokio_util::sync::CancellationToken;
pub(super) use zevria_content::assistant_plain_text;
pub(super) use zevria_foundation::config::{ModelContextPolicy, ModelProfileRef};
pub(super) use zevria_foundation::policy::*;
pub(super) use zevria_foundation::question::{
    QUESTION_TOOL_NAME, QuestionRequest, QuestionRequestId, QuestionTerminalDisposition,
};
pub(super) use zevria_foundation::subtask::{LAUNCH_SUBTASKS_TOOL_NAME, SubtaskId};
pub(super) use zevria_foundation::task::TaskList;
pub(super) use zevria_foundation::tool_result::{
    ToolCallId, ToolCallOutcome, ToolCancelled, ToolResultDetail, ToolResultMetadata,
};
pub(super) use zevria_foundation::{
    CREATE_PLAN_TOOL_NAME, PLAN_AUTHORING_TOOL_NAMES, READ_PLAN_TOOL_NAME,
    RECONCILE_REPORTS_TOOL_NAME, SKILL_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME, UPDATE_PLAN_TOOL_NAME,
};
pub(super) use zevria_instructions::prompts::{
    ENSEMBLE_PLAN_SYNTHESIS_INSTRUCTIONS, ENSEMBLE_REVIEW_SYNTHESIS_INSTRUCTIONS,
};
pub(super) use zevria_instructions::skill::{
    ActiveSkills, SkillApplication, SkillCatalog, SkillInvocation, SkillName, SkillRequest,
};
pub(super) use zevria_model::compaction::{
    CompactionBackend, CompactionCheckpoint, CompactionTrigger, ContextTokenEstimate,
    local_replacement_history, select_recent_user_messages,
};
pub(super) use zevria_model::config::CompactionPolicy;
pub(super) use zevria_model::estimates::*;
pub(super) use zevria_model::request::*;
pub(super) use zevria_model::telemetry::*;
pub(super) use zevria_model::{ProviderReplay, ToolDeclarations};
pub(super) use zevria_session_api::command::*;
pub(super) use zevria_session_api::event::*;
pub(super) use zevria_session_api::provider::*;
pub(super) use zevria_session_api::question::QuestionResponder;
pub(super) use zevria_session_api::turn::*;
pub(super) use zevria_session_api::worker::*;
pub(super) use zevria_session_api::{EnsembleLaunchRequest, EnsembleLauncher};
pub(super) use zevria_transcript::transcript::{
    Conversation, TranscriptItem, TranscriptWriter, latest_successful_task_snapshot,
    replay_active_skills,
};
pub(super) use zevria_transcript::{
    SessionReplayError, ToolDeclarationPlacement, ToolDeclarationRecord, tool_window,
};
pub(super) use zevria_workflow::ensemble::{
    AgentRunId, AgentRunOutcome, AgentRunStatus, AgentRunSummary, AgentUserDecisionAnswer,
    AgentUserDecisionBatch, AgentUserDecisionId, AgentUserDecisionValue, EnsembleRecord,
    EnsembleRecovery, EnsembleRunId, EnsembleStart, EnsembleWorkflow, ReconciliationNextStep,
    RecordedDecisionAccounting, RecordedDecisionDisposition, ReportDisagreement,
    ReportDisagreementClassification, ReportDisagreementResolution, ReportPosition,
    ReportReconciliation, ReportReconciliationCatalog, ValidatedReportReconciliation,
    latest_ensemble_recovery,
};
pub(super) use zevria_workflow::ensemble_review::*;
pub(super) use zevria_workflow::plan::{
    DirectWorkerPlan, MAX_PLAN_ARTIFACT_BYTES, PlanArtifact, PlanCandidate, PlanDecision,
    PlanDraft, PlanDraftScope, PlanHandoff, PlanId, PlanPublicationProvenance, PlanRecord,
    PlanResolution, PlanVersion, PlanWorkflowState, apply_plan_record, derived_title,
    replay_plan_state,
};

pub(super) use crate::session::workflow::prompt::PROMPT_PLAN_PREFIX_REPLAYS;
