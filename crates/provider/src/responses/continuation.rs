//! Responses continuation responsibilities.
use crate::diagnostics::Diagnostics;
use zevria_model::ModelResponse;

use crate::connection::{ContinuationState, OpenAiProvider};

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
    pub(super) projection_generation: u64,
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
    let decision = openai
        .responses_transport
        .ws
        .continuation
        .as_ref()
        .map(|continuation| {
            if continuation.socket_generation != openai.responses_transport.ws.socket_generation {
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
            openai
                .responses_transport
                .ws
                .invalidate_continuation(reason);
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
                projection_generation: prepared.projection_generation,
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
