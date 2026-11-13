//! Responses continuation responsibilities.
use crate::diagnostics::Diagnostics;
use zevria_model::ModelResponse;
use zevria_model::ProviderReplay;

use crate::{
    accumulator::AttemptState,
    connection::{ContinuationState, OpenAiProvider},
};

use super::driver::RequestMode;
use super::prepare::PreparedRequest;
/// A complete logical request. Every retry and stale-ID fallback derives its
/// transmitted payload from this snapshot, so recovery cannot accidentally
/// rebuild a different history or tool set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ContinuationLineage {
    pub(super) input_count: usize,
    pub(super) input_digest: [u8; 32],
    pub(super) projected_prefix_len: usize,
}

pub(super) struct Transmission<'prepared> {
    pub(super) full_reason: crate::diagnostics::TransmissionDiagnostics,
    pub(super) previous_response_id: Option<String>,
    pub(super) input: &'prepared [serde_json::Value],
    pub(super) mode: RequestMode,
}

pub(super) fn select_transmission<'prepared>(
    prepared: &'prepared PreparedRequest,
    openai: &mut OpenAiProvider,
) -> Transmission<'prepared> {
    let decision = openai.ws.continuation.as_ref().map(|continuation| {
        if continuation.socket_generation != openai.ws.socket_generation {
            return Err("reconnect");
        }
        if !crate::prompt_cache::compatible(
            &continuation.request_properties,
            &prepared.request_properties,
        ) {
            return Err("request_properties_changed");
        }
        let prefix_len = continuation.lineage.projected_prefix_len;
        let has_exact_prefix =
            prepared.input_matches_continuation && prepared.full_input.len() > prefix_len;
        if !has_exact_prefix {
            return Err("history_mismatch");
        }

        Ok((
            continuation.response_id.clone(),
            &prepared.full_input[prefix_len..],
        ))
    });

    match decision {
        Some(Ok((response_id, suffix))) => Transmission {
            full_reason: Diagnostics::incremental(),
            previous_response_id: Some(response_id),
            input: suffix,
            mode: RequestMode::Incremental,
        },
        Some(Err(reason)) => {
            openai.ws.invalidate_continuation(reason);
            Transmission {
                full_reason: Diagnostics::selection(reason),
                previous_response_id: None,
                input: &prepared.full_input,
                mode: RequestMode::Full,
            }
        }
        None => Transmission {
            full_reason: Diagnostics::no_continuation(openai),
            previous_response_id: None,
            input: &prepared.full_input,
            mode: RequestMode::Full,
        },
    }
}

pub(super) fn full_transmission(prepared: &PreparedRequest) -> Transmission<'_> {
    Transmission {
        full_reason: Diagnostics::stale_id(),
        previous_response_id: None,
        input: &prepared.full_input,
        mode: RequestMode::Full,
    }
}

pub(super) fn successful_model_response(
    openai: &mut OpenAiProvider,
    state: &mut AttemptState,
    prepared: &PreparedRequest,
    response_id: String,
    usage: Option<zevria_model::TokenUsage>,
) -> anyhow::Result<ModelResponse> {
    openai.ws.touch();
    let Some(response_output) = state.take_native_output() else {
        openai.ws.invalidate_continuation("missing_native_output");
        tracing::warn!(
            reason = "missing_native_output",
            socket_generation = openai.ws.socket_generation,
            "rejecting OpenAI response without complete native output"
        );
        anyhow::bail!("OpenAI response completed without complete native output");
    };

    let replay = ProviderReplay::openai_responses(openai.profile.clone(), response_output);
    let response = match ModelResponse::from_replay(replay) {
        Ok(response) => response,
        Err(error) => {
            openai.ws.invalidate_continuation("invalid_native_output");
            tracing::warn!(
                reason = "invalid_native_output",
                socket_generation = openai.ws.socket_generation,
                error = %error,
                "rejecting OpenAI response whose native output cannot be converted"
            );
            return Err(
                error.context("failed to convert captured OpenAI output into an assistant message")
            );
        }
    };
    let response = response.with_usage(usage);
    openai.ws.continuation = Some(ContinuationState::after_response(
        prepared,
        openai.ws.socket_generation,
        response_id,
        &response,
    )?);
    Ok(response)
}

impl ContinuationState {
    /// The production boundary constructor is also used by synthetic fixtures;
    /// neither detached projected vectors nor a missing lineage can seed a chain.
    pub(crate) fn after_response(
        prepared: &PreparedRequest,
        socket_generation: u64,
        response_id: String,
        response: &ModelResponse,
    ) -> anyhow::Result<Self> {
        let native = response
            .record()
            .provider_replay()
            .ok_or_else(|| anyhow::anyhow!("continuation requires a native response record"))?;
        Ok(Self {
            socket_generation,
            response_id,
            request_properties: prepared.request_properties.clone(),
            lineage: ContinuationLineage {
                input_count: prepared.input_count + 1,
                input_digest: zevria_model::ModelInputIdentity::appended_digest(
                    prepared.input_digest,
                    response.record().model_request_item(),
                )?,
                projected_prefix_len: prepared.full_input.len() + native.items.len(),
            },
        })
    }
}
