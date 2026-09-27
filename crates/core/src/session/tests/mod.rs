mod adaptive_summary;
mod directives;
mod inline_web;
#[cfg(unix)]
mod instruction_persistence;
mod mode;
mod orchestration;
mod prompt;
mod prompt_admission;
#[cfg(feature = "test-support")]
mod prompt_pipeline;
mod result_details;
mod review_fixtures;
mod runtime;
mod skill_applications;
mod skill_catalog;
mod web_search;
mod worker_review;
use review_fixtures::*;
mod states;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use rig_agent::tool::{Tool, ToolContext, ToolExecutionError, server::ToolServer};
use rig_core::message::{AssistantContent, ToolCall, ToolFunction};
use serde::Deserialize;
use serde_json::json;

use super::*;
use zevria_foundation::SubtaskStatus;
use zevria_foundation::question::QuestionAnswer;
use zevria_foundation::question::QuestionAnswerValue;
use zevria_foundation::question::QuestionOption;
use zevria_foundation::question::QuestionPrompt;
use zevria_foundation::question::QuestionPromptKind;
use zevria_foundation::question::QuestionResponse;
use zevria_foundation::tool_result::FileChange;
use zevria_foundation::tool_result::FileChangeOutput;
use zevria_session_api::EnsembleLaunchFuture;
use zevria_session_api::question::QuestionRequester;
use zevria_session_api::question::question_channels;
use zevria_workflow::AgentRunStatus;

fn test_profile() -> ModelProfileRef {
    ModelProfileRef::new("test-provider", "test-model")
}

fn role_compaction_policy(
    auto_trigger_percent: u64,
    build: (&str, u64, u64),
    plan: (&str, u64, u64),
    review: (&str, u64, u64),
    explore: (&str, u64, u64),
    builder: (&str, u64, u64),
) -> CompactionPolicy {
    let context = |(model, context_window_tokens, retained_user_tokens): (&str, u64, u64)| {
        ModelContextPolicy {
            profile: ModelProfileRef::new("test-provider", model),
            context_window_tokens,
            input_token_limit: context_window_tokens,
            retained_user_tokens,
        }
    };
    CompactionPolicy::new(
        zevria_model::config::CompactionConfig {
            auto_trigger_percent,
            summary_prompt: None,
        },
        [
            context(build),
            context(plan),
            context(review),
            context(explore),
            context(builder),
        ],
    )
    .expect("role compaction policy")
}

fn test_compaction_policy(
    context_window_tokens: u64,
    auto_trigger_percent: u64,
    retained_user_tokens: u64,
) -> CompactionPolicy {
    role_compaction_policy(
        auto_trigger_percent,
        ("test-model", context_window_tokens, retained_user_tokens),
        ("test-model", context_window_tokens, retained_user_tokens),
        ("test-model", context_window_tokens, retained_user_tokens),
        ("test-model", context_window_tokens, retained_user_tokens),
        ("builder-model", context_window_tokens, retained_user_tokens),
    )
}

/// An owned snapshot of one [`ModelRequest`]. Requests borrow the session,
/// so a test double that outlives the call has to copy what it asserts on.
#[derive(Debug, Clone, PartialEq)]
struct CapturedRequest {
    input: Vec<OwnedModelRequestItem>,
    prompt: Message,
    prompt_replay: Option<ProviderReplay>,
    history: Vec<Message>,
    history_replays: Vec<Option<ProviderReplay>>,
    model_role: ModelRole,
    instructions: String,
    skill_context: Option<String>,
    allowed_tool_names: Option<Vec<String>>,
}

impl CapturedRequest {
    fn of(request: &ModelRequest<'_>) -> Self {
        let messages = request
            .input
            .iter()
            .copied()
            .filter(|item| item.message_ref().is_some())
            .collect::<Vec<_>>();
        let prompt = messages
            .last()
            .expect("captured request has conversation input");
        let mut state = zevria_instructions::DirectiveState::default();
        for item in &request.input {
            if let ModelRequestItem::DeveloperInstruction(directive) = item {
                state.apply(directive).unwrap();
            }
        }
        let snapshot = state.snapshot();
        let instructions = request.instructions.to_string();
        let bodies = snapshot
            .directives
            .iter()
            .filter(|directive| {
                matches!(
                    directive.payload,
                    zevria_instructions::DirectivePayload::SkillBody { .. }
                )
            })
            .map(|directive| directive.text.as_str())
            .collect::<Vec<_>>();
        Self {
            input: snapshot_model_input(request.input.clone()).unwrap(),
            prompt: prompt
                .message_ref()
                .expect("legacy captured prompt is message-bearing")
                .clone(),
            prompt_replay: prompt.replay_ref().cloned(),
            history: messages[..messages.len().saturating_sub(1)]
                .iter()
                .filter_map(|item| item.message_ref().cloned())
                .collect(),
            history_replays: messages
                .iter()
                .take(messages.len().saturating_sub(1))
                .map(|item| item.replay_ref().cloned())
                .collect(),
            model_role: request.model_role,
            instructions,
            skill_context: (!bodies.is_empty()).then(|| bodies.join("\n\n")),
            allowed_tool_names: request.allowed_tool_names.map(|names| names.to_vec()),
        }
    }
}

struct ScriptedProvider {
    responses: VecDeque<anyhow::Result<ModelResponse>>,
    input_counts: VecDeque<anyhow::Result<InputTokenCount>>,
    input_count_calls: Arc<AtomicUsize>,
    counted_requests: Arc<Mutex<Vec<CapturedRequest>>>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    resets: usize,
}

struct StubEnsembleLauncher {
    reports: Vec<Option<String>>,
    user_decisions: Vec<Vec<AgentUserDecisionBatch>>,
    worker_error: Option<String>,
    plan_proof: bool,
    worker_queries: Arc<AtomicUsize>,
    launches: Arc<AtomicUsize>,
}

impl StubEnsembleLauncher {
    fn successful(reports: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let reports = reports
            .into_iter()
            .map(|report| Some(report.into()))
            .collect::<Vec<_>>();
        Self {
            user_decisions: vec![Vec::new(); reports.len()],
            reports,
            worker_error: None,
            plan_proof: true,
            worker_queries: Arc::new(AtomicUsize::new(0)),
            launches: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn successful_with_decision(
        reports: impl IntoIterator<Item = impl Into<String>>,
        worker_index: usize,
        decision: AgentUserDecisionBatch,
    ) -> Self {
        let mut launcher = Self::successful(reports);
        launcher.user_decisions[worker_index].push(decision);
        launcher
    }

    fn all_failed(count: usize) -> Self {
        Self {
            reports: vec![None; count],
            user_decisions: vec![Vec::new(); count],
            worker_error: None,
            plan_proof: true,
            worker_queries: Arc::new(AtomicUsize::new(0)),
            launches: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn proofless_successful(reports: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut launcher = Self::successful(reports);
        launcher.plan_proof = false;
        launcher
    }

    fn invalid(error: impl Into<String>) -> Self {
        Self {
            reports: Vec::new(),
            user_decisions: Vec::new(),
            worker_error: Some(error.into()),
            plan_proof: true,
            worker_queries: Arc::new(AtomicUsize::new(0)),
            launches: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl EnsembleLauncher for StubEnsembleLauncher {
    fn start_review(
        &self,
        _request: EnsembleLaunchRequest,
        states: Vec<WorkerReviewState>,
        _events: SessionEventSender,
        turn: TurnContext,
    ) -> anyhow::Result<EnsembleReviewExecution> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(start_stub_review(
            states,
            &self.reports,
            self.plan_proof,
            &self.user_decisions,
            turn,
        ))
    }
    fn workers(
        &self,
        _workflow: EnsembleWorkflow,
    ) -> anyhow::Result<Vec<zevria_workflow::AgentRunDescriptor>> {
        self.worker_queries.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.worker_error {
            anyhow::bail!(error.clone());
        }
        Ok(self
            .reports
            .iter()
            .enumerate()
            .map(|(index, _)| zevria_workflow::AgentRunDescriptor {
                id: AgentRunId::new(),
                agent: format!("agent-{index}"),
                label: format!("Agent {index}"),
                safe_mode: "read-only".to_string(),
            })
            .collect())
    }

    fn max_synthesis_bytes_per_agent(&self) -> usize {
        4_096
    }

    fn launch<'a>(
        &'a self,
        request: EnsembleLaunchRequest,
        _events: SessionEventSender,
        _turn: TurnContext,
    ) -> EnsembleLaunchFuture<'a> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let workflow = request.start.workflow;
            Ok(request
                .start
                .agents
                .into_iter()
                .zip(&self.reports)
                .zip(&self.user_decisions)
                .map(|((descriptor, report), user_decisions)| AgentRunOutcome {
                    confirmation: None,
                    descriptor,
                    status: if report.is_some() {
                        AgentRunStatus::Completed
                    } else {
                        AgentRunStatus::Failed
                    },
                    report: report.clone().unwrap_or_default(),
                    plan: (workflow == EnsembleWorkflow::Plan
                        && report.is_some()
                        && self.plan_proof)
                        .then(|| zevria_workflow::AgentStructuredPlan {
                            plan_id: Some("stub-plan".to_string()),
                            markdown: Some("# Stub implementation plan".to_string()),
                            entries: Vec::new(),
                        }),
                    partial: report.is_none(),
                    failure: report
                        .is_none()
                        .then(|| "scripted worker failure".to_string()),
                    usage: None,
                    acp_session_id: None,
                    user_decisions: user_decisions.clone(),
                    decision_ids: user_decisions
                        .iter()
                        .flat_map(AgentUserDecisionBatch::decision_ids)
                        .cloned()
                        .collect(),
                    unavailable_decisions: Vec::new(),
                })
                .collect())
        })
    }
}

fn model_response(message: Message) -> ModelResponse {
    ModelResponse::plain(message).expect("valid scripted response")
}

impl ScriptedProvider {
    fn new(responses: impl IntoIterator<Item = anyhow::Result<Message>>) -> Self {
        Self::with_model_responses(
            responses
                .into_iter()
                .map(|response| response.and_then(ModelResponse::plain)),
        )
    }

    fn with_model_responses(
        responses: impl IntoIterator<Item = anyhow::Result<ModelResponse>>,
    ) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            input_counts: VecDeque::new(),
            input_count_calls: Arc::new(AtomicUsize::new(0)),
            counted_requests: Arc::new(Mutex::new(Vec::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            resets: 0,
        }
    }

    fn with_input_counts(
        mut self,
        counts: impl IntoIterator<Item = anyhow::Result<InputTokenCount>>,
    ) -> Self {
        self.input_counts = counts.into_iter().collect();
        self
    }
}

impl ModelProvider for ScriptedProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("requests lock")
                .push(CapturedRequest::of(&request));
            let response = self
                .responses
                .pop_front()
                .expect("script should contain another response")?;
            progress.stream_updated(response.message().clone());
            if let Some(usage) = response.usage {
                progress.usage_updated(usage).await;
            }
            Ok(response)
        })
    }

    fn reset(&mut self) {
        self.resets += 1;
    }

    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move {
            self.counted_requests
                .lock()
                .unwrap()
                .push(CapturedRequest::of(&request));
            self.input_count_calls.fetch_add(1, Ordering::SeqCst);
            self.input_counts
                .pop_front()
                .unwrap_or(Ok(InputTokenCount::Unsupported))
        })
    }
}

struct PendingProvider {
    cancelled: Arc<AtomicBool>,
}

impl ModelProvider for PendingProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        _progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move { std::future::pending::<anyhow::Result<ModelResponse>>().await })
    }

    fn reset(&mut self) {}

    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

struct PendingInputCountProvider;

impl ModelProvider for PendingInputCountProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        _progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async { anyhow::bail!("completion is not used by this test provider") })
    }

    fn reset(&mut self) {}

    fn count_input_tokens<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(std::future::pending())
    }
}

struct FirstResponseThenPendingProvider {
    first: Option<Message>,
    cancelled: Arc<AtomicBool>,
}

impl ModelProvider for FirstResponseThenPendingProvider {
    fn complete<'a>(
        &'a mut self,
        _request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            if let Some(message) = self.first.take() {
                progress.stream_updated(message.clone());
                return ModelResponse::plain(message);
            }
            std::future::pending::<anyhow::Result<ModelResponse>>().await
        })
    }

    fn reset(&mut self) {}

    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

#[derive(Deserialize)]
struct EchoArgs {
    value: String,
}

struct EchoTool {
    calls: Arc<Mutex<Vec<String>>>,
}

struct CommandTestTool {
    calls: Arc<Mutex<Vec<String>>>,
}

struct WriteTestTool {
    calls: Arc<Mutex<Vec<String>>>,
}

struct SubmitPlanStubTool;

struct ReconcileReportsStubTool;

struct QuestionStubTool {
    requester: QuestionRequester,
}

#[derive(Deserialize)]
struct SubmitPlanStubArgs {
    title: String,
    markdown: String,
}

#[derive(Deserialize)]
struct NoArgs {}

#[derive(Debug)]
struct ExpectedToolFailure;

impl std::fmt::Display for ExpectedToolFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("expected execution failure")
    }
}

impl std::error::Error for ExpectedToolFailure {}

struct FailingTool;

struct MetadataTool;

impl Tool for MetadataTool {
    const NAME: &'static str = "metadata";
    type Error = std::convert::Infallible;
    type Args = NoArgs;
    type Output = String;

    fn description(&self) -> String {
        "return isolated file metadata".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        context.insert_result(ToolResultDetail::FileChanges(vec![FileChangeOutput {
            path: "secret/path.rs".into(),
            change: FileChange::Add {
                content: "metadata-only content".to_string(),
            },
        }]));
        Ok("plain model output".to_string())
    }
}

impl Tool for FailingTool {
    const NAME: &'static str = "explode";
    type Error = ExpectedToolFailure;
    type Args = NoArgs;
    type Output = String;

    fn description(&self) -> String {
        "always fail".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Err(ExpectedToolFailure)
    }
}

impl Tool for EchoTool {
    const NAME: &'static str = "echo";
    type Error = std::convert::Infallible;
    type Args = EchoArgs;
    type Output = String;

    fn description(&self) -> String {
        "echo a value".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(args.value.clone());
        Ok(args.value)
    }
}

/// Stand-in for the stateless request tool from `zevria-tools`.
struct SkillStubTool {
    calls: Arc<Mutex<Vec<String>>>,
}

impl Tool for SkillStubTool {
    const NAME: &'static str = SKILL_TOOL_NAME;
    type Error = std::convert::Infallible;
    type Args = SkillRequest;
    type Output = String;

    fn description(&self) -> String {
        "activate a skill".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        request: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(request.skill.to_string());
        let application = if request.arguments().is_empty() {
            "the current request"
        } else {
            request.arguments()
        };
        Ok(format!(
            "status: activated\nskill: {}\napplication: {application}",
            request.skill
        ))
    }
}

struct MissingRequestSkillTool;

impl Tool for MissingRequestSkillTool {
    const NAME: &'static str = SKILL_TOOL_NAME;
    type Error = std::convert::Infallible;
    type Args = SkillRequest;
    type Output = String;

    fn description(&self) -> String {
        "invalid test skill tool".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        request: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok(format!(
            "status: activated\nskill: {}\napplication: the current request",
            request.skill
        ))
    }
}

impl Tool for CommandTestTool {
    const NAME: &'static str = "command";
    type Error = std::convert::Infallible;
    type Args = EchoArgs;
    type Output = String;

    fn description(&self) -> String {
        "record a command call".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(args.value.clone());
        Ok(args.value)
    }
}

impl Tool for WriteTestTool {
    const NAME: &'static str = "write";
    type Error = std::convert::Infallible;
    type Args = EchoArgs;
    type Output = String;

    fn description(&self) -> String {
        "record a write call".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(args.value.clone());
        Ok(args.value)
    }
}

impl Tool for SubmitPlanStubTool {
    const NAME: &'static str = SUBMIT_PLAN_TOOL_NAME;
    type Error = std::convert::Infallible;
    type Args = SubmitPlanStubArgs;
    type Output = String;

    fn description(&self) -> String {
        "submit a validated Plan candidate".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let candidate = PlanCandidate::validate(args.title, args.markdown, 128 * 1024)
            .expect("test candidate must be valid");
        context.insert_result(candidate);
        Ok("accepted".to_string())
    }
}

impl Tool for ReconcileReportsStubTool {
    const NAME: &'static str = RECONCILE_REPORTS_TOOL_NAME;
    type Error = std::convert::Infallible;
    type Args = ReportReconciliation;
    type Output = String;

    fn description(&self) -> String {
        "validate one test reconciliation declaration".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let catalog = context
            .get::<ReportReconciliationCatalog>()
            .expect("engine supplies the ReportsReady decision catalog");
        let accepted = args.validate(catalog).expect("valid test reconciliation");
        let next_step = accepted.next_step;
        context.insert_result(accepted);
        Ok(format!("next: {next_step}"))
    }
}

impl Tool for QuestionStubTool {
    const NAME: &'static str = QUESTION_TOOL_NAME;
    type Error = zevria_foundation::question::QuestionRequestError;
    type Args = NoArgs;
    type Output = String;

    fn description(&self) -> String {
        "ask one test question".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let turn = context
            .get::<TurnContext>()
            .cloned()
            .expect("engine supplies turn context");
        let response = self
            .requester
            .ask(
                vec![QuestionPrompt {
                    id: "scope".to_string(),
                    header: "Scope".to_string(),
                    question: "Which scope?".to_string(),
                    options: vec![
                        QuestionOption {
                            label: "Focused".to_string(),
                            description: "Keep it narrow.".to_string(),
                        },
                        QuestionOption {
                            label: "Broad".to_string(),
                            description: "Include adjacent work.".to_string(),
                        },
                    ],
                    kind: QuestionPromptKind::SingleSelect { allow_other: true },
                    required: true,
                    default: None,
                }],
                turn,
            )
            .await?;
        context.insert_result(ToolResultDetail::QuestionDisposition(match &response {
            QuestionResponse::Answered { .. } => QuestionTerminalDisposition::Answered,
            QuestionResponse::Dismissed => QuestionTerminalDisposition::Dismissed,
        }));
        Ok(serde_json::to_string(&response).expect("serializable response"))
    }
}

fn tool_call(id: &str, value: &str) -> AssistantContent {
    named_tool_call(id, "echo", json!({"value": value}))
}

fn named_tool_call(id: &str, name: &str, arguments: serde_json::Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::new(
        rig_core::message::ToolCallId::new_or_mint(id),
        ToolFunction::new(name.to_string(), arguments),
    ))
}

async fn collect_events(receiver: &mut SessionEventReceiver) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        if let SessionUpdate::Lifecycle(event) = update {
            events.push(event);
        }
    }
    events
}

async fn recv_event(receiver: &mut SessionEventReceiver) -> Option<SessionEvent> {
    loop {
        match receiver.recv().await? {
            SessionUpdate::Lifecycle(event) => return Some(event),
            SessionUpdate::Streams(_) => {}
        }
    }
}

fn test_policies() -> SessionPolicies {
    SessionPolicies::new(
        TurnPolicy::new("Build test instructions", None, ModelRole::Build, true),
        TurnPolicy::new(
            "Plan test instructions",
            Some(vec!["command".to_string()]),
            ModelRole::Plan,
            false,
        ),
    )
}

// Tiny-budget fixtures advertise only their mock tools, not unregistered
// command/search capabilities and their substantial real instruction modules.
fn test_policies_for_tools(tools: &[&str]) -> SessionPolicies {
    let mut policies = test_policies();
    for mode in SessionMode::ALL {
        let policy = policies.policy_mut(mode);
        policy.allowed_tool_names = Some(
            tools
                .iter()
                .filter(|name| policy.allows_tool(name))
                .map(|name| (*name).to_string())
                .collect(),
        );
    }
    policies
}

fn plan_submission_policies() -> SessionPolicies {
    SessionPolicies::new(
        TurnPolicy::new(
            "Build test instructions",
            Some(vec!["command".to_string()]),
            ModelRole::Build,
            false,
        ),
        TurnPolicy::new(
            "Plan test instructions",
            Some(vec![SUBMIT_PLAN_TOOL_NAME.to_string()]),
            ModelRole::Plan,
            false,
        ),
    )
}

fn valid_plan_markdown(title: &str, goal: &str) -> String {
    format!(
        "# {title}\n\n## Goal\n{goal}\n\n## Decisions\nUse typed state.\n\n\
         ## Implementation\nImplement it.\n\n## Validation\nTest it.\n\n## Risks\nPersistence failures."
    )
}

fn submit_plan_call(title: &str, markdown: &str) -> Message {
    Message::Assistant {
        id: None,
        content: vec![named_tool_call(
            "submit-plan",
            SUBMIT_PLAN_TOOL_NAME,
            json!({"title": title, "markdown": markdown}),
        )],
    }
}

fn command_call(id: &str, command: &str) -> Message {
    Message::Assistant {
        id: None,
        content: vec![named_tool_call(id, "command", json!({"value": command}))],
    }
}

fn successful_tool_result(id: &str, tool_name: &str, output: &str) -> TranscriptItem {
    TranscriptItem::ToolResults {
        skill_applications: Vec::new(),
        message: Message::User {
            content: vec![UserContent::tool_result(
                id,
                tool_name,
                vec![ToolResultContent::text(output)],
            )],
        },
        metadata: vec![ToolResultMetadata {
            diagnostic: None,
            id: id.to_string(),
            call_id: None,
            tool_name: tool_name.to_string(),
            outcome: ToolCallOutcome::Success,
            detail: None,
        }],
    }
}

fn reconciliation_call(id: &str, declaration: ReportReconciliation) -> Message {
    Message::Assistant {
        id: None,
        content: vec![named_tool_call(
            id,
            RECONCILE_REPORTS_TOOL_NAME,
            serde_json::to_value(declaration).expect("reconciliation arguments"),
        )],
    }
}

fn no_disagreement_reconciliation() -> ReportReconciliation {
    ReportReconciliation {
        disagreements: Vec::new(),
        decisions: Vec::new(),
        unavailable_decisions: Vec::new(),
    }
}

fn root_question_reconciliation() -> ReportReconciliation {
    ReportReconciliation {
        disagreements: vec![ReportDisagreement {
            id: "insert_turn_start".to_string(),
            summary: "Workers propose different user-visible Insert-mode behavior.".to_string(),
            positions: vec![
                ReportPosition {
                    label: "preserve_insert".to_string(),
                    position: "Preserve Insert mode when a turn starts.".to_string(),
                },
                ReportPosition {
                    label: "auto_exit_insert".to_string(),
                    position: "Auto-exit Insert mode when a turn starts.".to_string(),
                },
            ],
            classification: ReportDisagreementClassification::PreferenceTradeoff,
            resolution: ReportDisagreementResolution::RootQuestion {
                reason: "Both behaviors are viable and materially change user experience."
                    .to_string(),
            },
        }],
        decisions: Vec::new(),
        unavailable_decisions: Vec::new(),
    }
}

fn insert_scope_decision_batch() -> AgentUserDecisionBatch {
    let request_id = QuestionRequestId::new("worker-insert-scope");
    let answer =
        |question_id: &str, header: &str, question: &str, value: &str| AgentUserDecisionAnswer {
            decision_id: AgentUserDecisionId::from_question(&request_id, question_id),
            question_id: question_id.to_string(),
            header: header.to_string(),
            question: question.to_string(),
            answer: AgentUserDecisionValue::String {
                value: value.to_string(),
            },
        };
    let answers = vec![
        answer(
            "insert_turn_start",
            "Insert behavior",
            "Should turn start also exit Insert mode?",
            "Also auto-exit Insert on turn start",
        ),
        answer(
            "select_mode",
            "Select behavior",
            "Should Select mode change too?",
            "Leave Select mode as-is (Recommended)",
        ),
    ];
    AgentUserDecisionBatch {
        request_id,
        answers,
    }
}

fn captured_insert_reconciliation(batch: &AgentUserDecisionBatch) -> ReportReconciliation {
    let insert_decision = batch.answers[0].decision_id.clone();
    ReportReconciliation {
        disagreements: vec![ReportDisagreement {
            id: "insert_turn_start".to_string(),
            summary: "Workers disagree whether turn start preserves or exits Insert mode."
                .to_string(),
            positions: vec![
                ReportPosition {
                    label: "preserve_insert".to_string(),
                    position: "Preserve Insert mode.".to_string(),
                },
                ReportPosition {
                    label: "auto_exit_insert".to_string(),
                    position: "Auto-exit Insert mode.".to_string(),
                },
            ],
            classification: ReportDisagreementClassification::PreferenceTradeoff,
            resolution: ReportDisagreementResolution::RecordedUserDecisions {
                decision_ids: vec![insert_decision.clone()],
                application: "Apply the captured auto-exit selection without asking again."
                    .to_string(),
            },
        }],
        decisions: batch
            .answers
            .iter()
            .map(|answer| RecordedDecisionAccounting {
                decision_id: answer.decision_id.clone(),
                disposition: RecordedDecisionDisposition::Applied {
                    explanation: format!("Apply the captured answer for {}.", answer.question_id),
                },
            })
            .collect(),
        unavailable_decisions: Vec::new(),
    }
}

fn ready_plan_fixture() -> (PlanArtifact, Vec<TranscriptItem>) {
    let id = PlanId::new();
    let title = "Durable approval workflow";
    let artifact = PlanArtifact {
        version: PlanVersion { id, revision: 1 },
        title: title.to_string(),
        markdown: valid_plan_markdown(title, "Keep approval durable."),
        source_turn_id: TurnId::new(9),
    };
    let items = vec![
        TranscriptItem::Plan(PlanRecord::Started { id }),
        TranscriptItem::Plan(PlanRecord::Ready {
            artifact: artifact.clone(),
        }),
    ];
    (artifact, items)
}

fn revising_plan_fixture() -> (PlanArtifact, Vec<TranscriptItem>) {
    let (artifact, mut items) = ready_plan_fixture();
    items.extend([
        TranscriptItem::Plan(PlanRecord::RevisionRequested {
            artifact: artifact.clone(),
        }),
        TranscriptItem::Message(Message::user("Consider another edge case.")),
        TranscriptItem::Message(Message::assistant(
            "I am still working through that revision.",
        )),
    ]);
    (artifact, items)
}

fn test_skills() -> Arc<SkillCatalog> {
    Arc::new(
        SkillCatalog::new([
            zevria_instructions::skill::SkillDefinition::new(
                SkillName::parse("commit").expect("name"),
                "Commit changes",
                "Commit instructions",
                zevria_instructions::skill::SkillSource::Programmatic("test".to_string()),
            )
            .expect("definition"),
            zevria_instructions::skill::SkillDefinition::new(
                SkillName::parse("review").expect("name"),
                "Review changes",
                "Review instructions",
                zevria_instructions::skill::SkillSource::Programmatic("test".to_string()),
            )
            .expect("definition"),
        ])
        .expect("registry"),
    )
}

fn test_transcript() -> (tempfile::TempDir, TranscriptWriter) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let writer = TranscriptWriter::create(directory.path()).expect("transcript writer");
    (directory, writer)
}

fn conversation_records(items: &[TranscriptItem]) -> Vec<TranscriptItem> {
    items.to_vec()
}

impl<P: ModelProvider> SessionEngine<P> {
    fn prepare_test_prompt(
        &self,
        anchor: TurnAnchor,
        input: PromptTurnInput,
        mode: SessionMode,
    ) -> Result<PreparedPrompt, Rejection> {
        let checked = self.preflight_prompt(anchor, input, mode)?;
        self.prepare_prompt(checked).map(|(_, prepared)| prepared)
    }

    fn required_instruction_tokens(&self, policy: &TurnPolicy, active: &ActiveSkills) -> u64 {
        self.instruction_preparation(policy)
            .unwrap()
            .prepare_updates(self.directive_state().unwrap(), active)
            .unwrap()
            .instruction_tokens
    }

    fn prospective_skill_overhead(&self, policy: &TurnPolicy, active: &ActiveSkills) -> u64 {
        self.required_instruction_tokens(policy, active)
            .saturating_add(self.fixed_input_tokens(policy))
    }

    fn with_fixture(self, items: Vec<TranscriptItem>) -> Result<Self, SessionReplayError> {
        self.with_transcript_items(items)
    }
}

fn ensemble_start_fixture(run_id: &str, workflow: EnsembleWorkflow, prompt: &str) -> EnsembleStart {
    EnsembleStart {
        run_id: EnsembleRunId::from_string(run_id),
        workflow,
        prompt: prompt.into(),
        agents: vec![zevria_workflow::AgentRunDescriptor {
            id: AgentRunId::from_string(format!("{run_id}-agent")),
            agent: "fixture-agent".to_string(),
            label: "Fixture agent".to_string(),
            safe_mode: "read-only".to_string(),
        }],
    }
}

fn persist_fixture(writer: &mut TranscriptWriter, items: &[TranscriptItem]) {
    writer.rewrite(items).expect("fixture persisted projection");
}

fn recorded_skill_applications(item: &TranscriptItem) -> Vec<&SkillApplication> {
    match item {
        TranscriptItem::SkillInvocation(invocation) => vec![invocation.application()],
        TranscriptItem::ToolResults {
            skill_applications, ..
        } => skill_applications
            .iter()
            .map(|accepted| &accepted.application)
            .collect(),
        _ => Vec::new(),
    }
}

fn prompt_message_edit(
    prompt_ordinal: usize,
    text: impl Into<zevria_content::UserPrompt>,
    mode: SessionMode,
) -> SessionCommand {
    SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
        target: TranscriptEditTarget::PromptOrdinal(prompt_ordinal),
        replacement: TranscriptEditReplacement::Message {
            behavior: zevria_foundation::RequestBehavior::Standard,
            text: text.into(),
            mode,
        },
    }))
}

fn ensemble_edit(run_id: EnsembleRunId, replacement: TranscriptEditReplacement) -> SessionCommand {
    SessionCommand::Turn(TurnCommand::EditTranscript(TranscriptEdit {
        target: TranscriptEditTarget::EnsembleRun(run_id),
        replacement,
    }))
}

struct TestSkillManagement;

impl zevria_instructions::skill::SkillManagementService for TestSkillManagement {
    fn update<'a>(
        &'a self,
        request: zevria_instructions::skill::SkillManagementRequest,
        installed: Arc<zevria_instructions::skill::SkillCatalog>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<Arc<zevria_instructions::skill::SkillCatalog>>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use zevria_instructions::skill::*;
            let mut config = installed.config().clone();
            if let SkillManagementRequest::SetEnabled { name, enabled, .. } = request {
                config.rules.push(SkillEnableRule { name, enabled });
            }
            Ok(Arc::new(
                test_skills().as_ref().clone().with_config(config)?,
            ))
        })
    }
}

mod capacity;
mod compaction;
mod edit;
mod ensemble;
mod events;
mod lifecycle;
mod plan;
mod skills;
mod subtasks;
mod tools;

// --- subtask integration ---

use zevria_foundation::subtask::SubtaskKind;
use zevria_foundation::subtask::SubtaskOutcome;
use zevria_session_api::subtask::SubtaskLaunchRequest;
use zevria_session_api::subtask::SubtaskLauncher;
use zevria_session_api::subtask::subtask_channels;

#[derive(Debug)]
struct LaunchTestFailure(String);

impl std::fmt::Display for LaunchTestFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LaunchTestFailure {}

#[derive(Deserialize)]
struct LaunchTestArgs {
    tasks: Vec<LaunchTestSpec>,
}
#[derive(Deserialize)]
struct LaunchTestSpec {
    title: String,
}

/// Test double for the real `launch_subtasks` tool: launches through the
/// core channels, blocks on the outcome oneshot, and attaches the
/// metadata extension, recording the dispatch order into a shared log.
struct LaunchTestTool {
    launcher: SubtaskLauncher,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Tool for LaunchTestTool {
    const NAME: &'static str = LAUNCH_SUBTASKS_TOOL_NAME;
    type Error = LaunchTestFailure;
    type Args = LaunchTestArgs;
    type Output = String;

    fn description(&self) -> String {
        "run a test subtask to completion".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let call_id = context
            .get::<ToolCallId>()
            .map(|id| id.0.clone())
            .unwrap_or_default();
        let turn = context.get::<TurnContext>().cloned().expect("turn context");
        let launches = futures_util::future::join_all(args.tasks.into_iter().enumerate().map(
            |(index, task)| {
                let call_id = call_id.clone();
                let turn = turn.clone();
                async move {
                    let launch = self
                        .launcher
                        .launch(
                            call_id,
                            index,
                            &task.title,
                            SubtaskKind::Explore,
                            format!("{} with full context", task.title),
                            turn,
                            None,
                        )
                        .await;
                    self.calls
                        .lock()
                        .unwrap()
                        .push(format!("launch:{}", task.title));
                    launch
                }
            },
        ))
        .await;
        let outcomes = futures_util::future::join_all(launches.into_iter().enumerate().map(
            |(index, launch)| async move {
                let (identity, outcome) = match launch {
                    Ok((metadata, outcome)) => (
                        Some(metadata),
                        outcome.await.unwrap_or_else(|_| SubtaskOutcome::Failed {
                            error: "the subtask supervisor shut down before the subtask reported"
                                .into(),
                        }),
                    ),
                    Err(error) => (
                        None,
                        SubtaskOutcome::Failed {
                            error: error.to_string(),
                        },
                    ),
                };
                let (status, output) = match outcome {
                    SubtaskOutcome::Completed { report } => (
                        SubtaskStatus::Completed,
                        format!("status: completed\nreport:\n{report}"),
                    ),
                    SubtaskOutcome::Failed { error } => (SubtaskStatus::Failed, error),
                    SubtaskOutcome::Cancelled => {
                        (SubtaskStatus::Cancelled, "subtask cancelled".into())
                    }
                };
                (
                    zevria_foundation::SubtaskEntryMetadata {
                        index,
                        status,
                        launch: identity,
                    },
                    output,
                )
            },
        ))
        .await;
        let failed = outcomes
            .iter()
            .any(|(entry, _)| entry.status != SubtaskStatus::Completed);
        let (metadata, output): (Vec<_>, Vec<_>) = outcomes.into_iter().unzip();
        context.insert_result(ToolResultDetail::Subtasks(metadata));
        if failed {
            Err(LaunchTestFailure(output.join("\n")))
        } else {
            Ok(output.join("\n"))
        }
    }
}

/// Fake supervisor: resolve every incoming request with a report derived
/// from its title.
fn autocomplete_requests(mut requests: tokio::sync::mpsc::Receiver<SubtaskLaunchRequest>) {
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let report = format!("report for {}", request.descriptor.title);
            let _ = request.outcome.send(SubtaskOutcome::Completed { report });
        }
    });
}

fn launch_call(id: &str, title: &str) -> AssistantContent {
    named_tool_call(
        id,
        LAUNCH_SUBTASKS_TOOL_NAME,
        json!({"tasks":[{"title": title}]}),
    )
}

fn user_tool_result_count(message: &Message) -> usize {
    let Message::User { content } = message else {
        return 0;
    };
    content
        .iter()
        .filter(|item| matches!(item, UserContent::ToolResult(_)))
        .count()
}

fn into_turn(command: SessionCommand) -> TurnCommand {
    let SessionCommand::Turn(command) = command else {
        panic!("expected turn command")
    };
    command
}

fn test_skill_tools() -> ToolServerHandle {
    ToolServer::new()
        .tool(SkillStubTool {
            calls: Arc::new(Mutex::new(Vec::new())),
        })
        .run()
}
