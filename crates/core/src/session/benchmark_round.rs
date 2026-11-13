//! Complete synthetic two-call turns, without network or shell startup.
use crate::session::SessionEngine;
#[cfg(test)]
use crate::session::workflow::compaction::snapshot_model_input;
use rig_agent::tool::ToolContext;
use rig_core::message::{AssistantContent, Message, ToolCall};
use std::collections::VecDeque;
use std::sync::Arc;
use zevria_foundation::config::ModelContextPolicy;
use zevria_foundation::policy::{ModelRole, SessionMode, SessionPolicies, TurnPolicy};
use zevria_foundation::tool_result::ToolCallOutcome;
use zevria_instructions::skill::SkillCatalog;
#[cfg(test)]
use zevria_model::ToolDeclarations;
use zevria_model::config::CompactionPolicy;
use zevria_model::request::{ModelRequest, ModelResponse};
use zevria_session_api::command::{SessionCommand, TurnCommand};
use zevria_session_api::event::{ProgressReporter, SessionEvent, SessionUpdate};
use zevria_session_api::provider::{ModelProvider, ProviderFuture};
use zevria_transcript::transcript::TranscriptWriter;

use rig_agent::tool::Tool;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(test)]
struct CapturedRoundRequest {
    instructions: String,
    tools: ToolDeclarations,
    input: Vec<zevria_model::OwnedModelRequestItem>,
}

struct RoundProvider {
    responses: VecDeque<ModelResponse>,
    calls: usize,
    #[cfg(test)]
    requests: Vec<CapturedRoundRequest>,
}

impl ModelProvider for RoundProvider {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        _: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.calls += 1;
            // Request snapshots belong only to correctness tests. Cloning the
            // full history in a test double would contaminate the benchmark.
            #[cfg(test)]
            self.requests.push(CapturedRoundRequest {
                instructions: request.instructions.to_owned(),
                tools: ToolDeclarations::new(request.tools.to_vec()).unwrap(),
                input: snapshot_model_input(request.input).unwrap(),
            });
            #[cfg(not(test))]
            let _ = request;
            Ok(self
                .responses
                .pop_front()
                .expect("exactly two scripted calls"))
        })
    }

    fn reset(&mut self) {}
}

#[derive(serde::Deserialize)]
struct EchoArgs {
    value: String,
}

struct EchoTool(Arc<AtomicUsize>);
impl Tool for EchoTool {
    const NAME: &'static str = "benchmark_echo";
    type Error = std::convert::Infallible;
    type Args = EchoArgs;
    type Output = String;

    fn description(&self) -> String {
        "Echo a synthetic benchmark value".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object", "properties":{"value":{"type":"string"}}, "required":["value"]})
    }
    async fn call(&self, _: &mut ToolContext, args: EchoArgs) -> Result<String, Self::Error> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(args.value)
    }
}

/// One-shot fixture. Construction, initial durable publication, replay and
/// policy reconciliation happen outside Criterion's timed operation.
pub struct EngineRoundFixture {
    engine: SessionEngine<RoundProvider>,
    tool_calls: Arc<AtomicUsize>,
    _directory: tempfile::TempDir,
}

impl EngineRoundFixture {
    /// Requires an entered Tokio runtime. Counts include headers and the
    /// reconciled policy/tool scaffolding, not an extra hidden prefix.
    pub fn new(records: usize) -> Self {
        let mut history = crate::transcript_bench::fixture(
            crate::transcript_bench::Workload::Replay,
            records,
            128,
        )
        .items;
        let suffix = history.split_off(2);
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create_with_id(directory.path(), "benchmark").unwrap();
        writer.rewrite(&history).unwrap();
        let call = AssistantContent::ToolCall(ToolCall::new(
            rig_core::message::CallId::from_wire("call-benchmark"),
            rig_core::message::ToolFunction::new(
                rig_core::message::ToolName::new(EchoTool::NAME).unwrap(),
                serde_json::json!({"value":"benchmark"}),
            ),
        ));
        let provider = RoundProvider {
            responses: [
                ModelResponse::plain(Message::Assistant(
                    rig_core::message::AssistantMessage::new(vec![call]),
                ))
                .unwrap(),
                ModelResponse::plain(Message::assistant("done")).unwrap(),
            ]
            .into(),
            calls: 0,
            #[cfg(test)]
            requests: Vec::new(),
        };
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let tools = rig_agent::tool::server::ToolServer::new()
            .tool(EchoTool(tool_calls.clone()))
            .run();
        let policy = TurnPolicy::new("Synthetic engine round", None, ModelRole::Build, true);
        let context = ModelContextPolicy {
            profile: crate::transcript_bench::profile(),
            context_window_tokens: 10_000_000,
            input_token_limit: 10_000_000,
            retained_user_tokens: 20_000,
        };
        let compaction = CompactionPolicy::new(
            zevria_model::config::CompactionConfig::default(),
            std::array::from_fn(|_| context.clone()),
        )
        .unwrap();
        let mut engine = SessionEngine::new(
            provider,
            tools,
            SessionPolicies::new(policy.clone(), policy.clone()),
            writer,
            Arc::new(SkillCatalog::default()),
        )
        .unwrap()
        .with_transcript_items(history)
        .unwrap()
        .with_compaction_policy(compaction);
        engine.reconcile_before_dispatch(&policy).unwrap();
        let remaining = records
            .checked_sub(engine.conversation.items().len())
            .expect("history exceeds policy scaffolding");
        engine
            .commit(suffix.into_iter().take(remaining).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(engine.conversation.items().len(), records);
        Self {
            engine,
            tool_calls,
            _directory: directory,
        }
    }

    /// Prompt acceptance -> model call -> tool -> model final -> terminal event.
    /// Includes real durable appends and drains the bounded lifecycle channel
    /// concurrently in the same task, so no fixture capacity can hide a stall.
    pub async fn run(&mut self) {
        assert_eq!(self.engine.provider.calls, 0, "fixture is one-shot");
        let (events, mut receiver) = zevria_session_api::session_event_channel(1);
        let dispatch = async {
            self.engine
                .handle_command(
                    SessionCommand::Turn(TurnCommand::Submit {
                        behavior: zevria_foundation::RequestBehavior::Standard,
                        text: "Run the synthetic tool once".into(),
                        mode: SessionMode::Build,
                    }),
                    &events,
                )
                .await
                .unwrap();
            drop(events);
        };
        let drain = async {
            let (mut started, mut calls, mut results, mut completed) = (0, 0, 0, 0);
            while let Some(update) = receiver.recv().await {
                if let SessionUpdate::Lifecycle(event) = update {
                    match event {
                        SessionEvent::TurnStarted { .. } => started += 1,
                        SessionEvent::ModelCallStarted { .. } => calls += 1,
                        SessionEvent::ToolResults { metadata, .. } => {
                            assert_eq!(metadata.len(), 1);
                            assert_eq!(metadata[0].id, "call-benchmark");
                            assert_eq!(metadata[0].tool_name, EchoTool::NAME);
                            assert_eq!(metadata[0].outcome, ToolCallOutcome::Success);
                            results += 1;
                        }
                        SessionEvent::TurnCompleted { message, .. } => {
                            assert_eq!(message, Message::assistant("done"));
                            completed += 1;
                        }
                        SessionEvent::TurnFailed { .. }
                        | SessionEvent::TurnRejected { .. }
                        | SessionEvent::TurnCancelled { .. }
                        | SessionEvent::CompactionStarted { .. }
                        | SessionEvent::PersistenceChanged { error: Some(_), .. } => {
                            panic!("synthetic round failed: {event:?}")
                        }
                        _ => {}
                    }
                }
            }
            assert_eq!((started, calls, results, completed), (1, 2, 1, 1));
        };
        tokio::join!(dispatch, drain);
        assert_eq!(self.engine.provider.calls, 2);
        assert!(self.engine.provider.responses.is_empty());
        assert_eq!(self.tool_calls.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_round_is_durable_and_preserves_the_request_prefix_at_all_sizes() {
        for records in [100, 1000, 5000] {
            let mut fixture = EngineRoundFixture::new(records);
            assert_eq!(fixture.engine.conversation.items().len(), records);
            fixture.run().await;
            let requests = &fixture.engine.provider.requests;
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].instructions, requests[1].instructions);
            assert_eq!(requests[0].tools, requests[1].tools);
            assert!(requests[1].input.len() > requests[0].input.len());
            assert!(requests[1].input.starts_with(&requests[0].input));
            let loaded =
                zevria_transcript::transcript::load(fixture.engine.conversation.path()).unwrap();
            assert_eq!(loaded, fixture.engine.conversation.items());
        }
    }
}
