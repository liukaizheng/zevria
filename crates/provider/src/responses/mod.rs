//! One OpenAI model request, response chaining, and bounded recovery.

use crate::diagnostics::Diagnostics;
#[cfg(test)]
use rig_core::message::Message;
use zevria_model::ModelRequest;
use zevria_session_api::CompactFuture;
use zevria_session_api::InputTokenCountFuture;
use zevria_session_api::ModelProvider;
use zevria_session_api::ProgressReporter;
use zevria_session_api::ProviderFuture;

use crate::{
    accumulator::AttemptState,
    connection::{OpenAiProvider, OpenAiTransport},
    websocket_session::OpenAiWebSocketTerminalCategory,
};

mod continuation;
mod driver;
mod errors;
mod http;
mod maintenance;
mod prepare;
mod recovery_loop;
mod serialize;
mod websocket;
#[cfg(test)]
use crate::{
    connection::ContinuationState,
    recovery::{AttemptProgress, Recovery, RecoveryPolicy},
};
pub(crate) use continuation::ContinuationLineage;
#[cfg(test)]
use continuation::{full_transmission, select_transmission};
#[cfg(test)]
use driver::RequestMode;
#[cfg(test)]
pub(crate) use errors::{is_websocket_disconnect, message_reports_upstream_disconnect};
use maintenance::count_full_input_tokens;
use maintenance::run_remote_compaction;
pub(crate) use prepare::PreparedRequest;
pub(crate) use prepare::ProjectionCache;
#[cfg(test)]
pub(crate) use prepare::prepare_turn_request;
use recovery_loop::run_model_request_with_reconnect;
#[cfg(test)]
use recovery_loop::{run_model_request_with_recovery, run_prepared_with_recovery};
#[cfg(test)]
use websocket::run_model_request;
#[cfg(test)]
use zevria_model::ProviderReplay;
#[cfg(test)]
use zevria_session_api::event::NetworkTransport;
/// Single-response helper used by the protocol-focused tests.
#[cfg(test)]
pub(crate) async fn run_turn(
    user_message: impl Into<String>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<()> {
    // Cloned into locals so the borrowed request does not alias `state`,
    // which the helper below needs mutably.
    let prompt = Message::user(user_message.into());
    let history = state.history.clone();
    let history_replays = state.history_replays.clone();
    let mut input = history
        .iter()
        .enumerate()
        .map(
            |(index, message)| match history_replays.get(index).and_then(Option::as_ref) {
                Some(replay) => zevria_model::ModelRequestItem::replay_backed(replay),
                None => zevria_model::ModelRequestItem::message(message),
            },
        )
        .collect::<Vec<_>>();
    input.push(zevria_model::ModelRequestItem::message(&prompt));
    run_turn_request(
        ModelRequest {
            input_identity: None,
            tools: &[],
            forbid_tool_calls: false,
            instructions: crate::tests::test_instructions(),
            input,
            model_role: zevria_foundation::ModelRole::Build,
            allowed_tool_names: None,
        },
        openai,
        state,
        progress,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn run_turn_request(
    model_request: ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<()> {
    let prompt = model_request
        .input
        .iter()
        .rev()
        .find_map(|item| item.message_ref())
        .cloned()
        .expect("test request prompt");
    let prepared = prepare_turn_request(&model_request, openai).await?;
    let policy = RecoveryPolicy::default();
    let response = if openai.transport == OpenAiTransport::Http {
        run_prepared_with_recovery(&prepared, openai, state, progress, &policy).await?
    } else {
        let recovery = Recovery::new(&policy);
        let plan = recovery.start_attempt().unwrap();
        let attempt_progress = AttemptProgress {
            reporter: progress,
            recovery: &recovery,
            attempt: plan.attempt,
            transport: NetworkTransport::WebSocket,
        };
        run_model_request(
            &prepared,
            openai,
            state,
            progress,
            &policy,
            attempt_progress,
        )
        .await?
    };
    state.history.push(prompt);
    state.history_replays.push(None);
    let content = response.into_record().into_model_request_item();
    state
        .history
        .push(content.message_ref().expect("completed message").clone());
    state.history_replays.push(match content {
        zevria_model::OwnedModelRequestItem::ReplayBacked(content) => Some(content),
        _ => None,
    });
    Ok(())
}

#[cfg(test)]
pub(crate) async fn run_turn_request_with_reconnect(
    model_request: ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
) -> anyhow::Result<()> {
    run_turn_request_with_recovery(
        model_request,
        openai,
        state,
        progress,
        &RecoveryPolicy::default(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn run_turn_request_with_recovery(
    model_request: ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    progress: &ProgressReporter,
    policy: &RecoveryPolicy,
) -> anyhow::Result<()> {
    let prompt = model_request
        .input
        .iter()
        .rev()
        .find_map(|item| item.message_ref())
        .cloned()
        .expect("test request prompt");
    let response =
        run_model_request_with_recovery(&model_request, openai, state, progress, policy).await?;
    state.history.push(prompt);
    state.history_replays.push(None);
    let content = response.into_record().into_model_request_item();
    state
        .history
        .push(content.message_ref().expect("completed message").clone());
    state.history_replays.push(match content {
        zevria_model::OwnedModelRequestItem::ReplayBacked(content) => Some(content),
        _ => None,
    });
    Ok(())
}

impl ModelProvider for OpenAiProvider {
    fn application_prompt(&self) -> &str {
        &self.preamble
    }

    fn prewarm(
        &mut self,
        _role: zevria_foundation::ModelRole,
    ) -> zevria_session_api::PrewarmFuture<'_> {
        Box::pin(self.prewarm_connection())
    }

    fn complete<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
        progress: ProgressReporter,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let mut state = AttemptState::default();
            run_model_request_with_reconnect(&request, self, &mut state, &progress)
                .await
                .map_err(|error| {
                    let context = format!(
                        "profile {} (providers.{}.web_search.enabled={}): {error}",
                        self.profile, self.profile.provider, self.web_search.enabled
                    );
                    error.context(context)
                })
        })
    }

    fn compact<'a>(&'a mut self, request: ModelRequest<'a>) -> CompactFuture<'a> {
        Box::pin(async move { run_remote_compaction(&request, self).await })
    }

    fn count_input_tokens<'a>(
        &'a mut self,
        request: ModelRequest<'a>,
    ) -> InputTokenCountFuture<'a> {
        Box::pin(async move { count_full_input_tokens(&request, self).await })
    }

    fn reset(&mut self) {
        Diagnostics::reset(self, "engine_reset");
        if self.transport == OpenAiTransport::WebSocket {
            self.ws.invalidate_continuation("engine_reset");
            self.ws.pending_done_response_id = None;
        }
    }

    fn cancel(&mut self) {
        if self.transport == OpenAiTransport::WebSocket {
            self.ws
                .session
                .terminate(OpenAiWebSocketTerminalCategory::LocalCancellation);
            self.ws.invalidate_continuation("turn_cancelled");
            self.ws.pending_done_response_id = None;
        }
    }
}

#[cfg(test)]
#[path = "../instruction_set_tests.rs"]
mod instruction_set_tests;

// Temporary test-only preparation probe; included in both feature configurations.
#[cfg(test)]
#[path = "../cache_diagnostics/probe_support.rs"]
pub(crate) mod cache_preparation_probe_support;
