//! Responses maintenance responsibilities.
use crate::http::bounded_http_error_body;
use zevria_model::CompactResult;
use zevria_model::InputTokenCount;
use zevria_model::ModelRequest;
use zevria_model::ProviderReplay;

use crate::{
    connection::OpenAiProvider,
    credential::{authenticated, http_unauthorized, rejected_after_refresh},
};

use super::{
    prepare::{ResponsesCountProbe, prepare_count_request, prepare_turn_request},
    serialize::RequestBody,
};
use bytes::Bytes;
pub(super) async fn count_full_input_tokens(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    probe: Option<ResponsesCountProbe>,
) -> anyhow::Result<InputTokenCount> {
    if openai.input_token_count_unsupported {
        return Ok(InputTokenCount::Unsupported);
    }
    let Some(url) = openai.input_token_count_url.clone() else {
        return Ok(InputTokenCount::Unsupported);
    };

    let prepared = prepare_count_request(model_request, openai, probe)?;
    let body = Bytes::from(serde_json::to_vec(&RequestBody::count_tokens(&prepared)?)?);

    let log_url = crate::connection::redacted_url_for_logging(url.as_str());
    let response = authenticated(
        &openai.api_key,
        &openai.profile.provider,
        |lease| {
            openai
                .http
                .post(url.clone())
                .bearer_auth(lease.expose())
                .timeout(openai.input_token_count_timeout)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
        },
        http_unauthorized,
    )
    .await?
    .map_err(|error| {
        anyhow::anyhow!(
            "OpenAI input-token request to {log_url} failed: {}",
            error.without_url()
        )
    })?;
    let status = response.status();
    if matches!(status.as_u16(), 404 | 405 | 501) {
        openai.input_token_count_unsupported = true;
        tracing::info!(url = %log_url, %status, "input-token counting is unsupported");
        return Ok(InputTokenCount::Unsupported);
    }
    if !status.is_success() {
        let detail = bounded_http_error_body(response).await;
        let error = anyhow::anyhow!(
            "OpenAI input-token request to {log_url} failed with HTTP {status}: {detail}"
        );
        return Err(
            if status == reqwest::StatusCode::UNAUTHORIZED && openai.api_key.is_refreshable() {
                error.context(rejected_after_refresh(&openai.profile.provider))
            } else {
                error
            },
        );
    }
    let value: serde_json::Value = response.json().await.map_err(|error| {
        anyhow::anyhow!(
            "malformed OpenAI input-token response from {log_url}: {}",
            error.without_url()
        )
    })?;
    let input_tokens = value
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "malformed OpenAI input-token response from {log_url}: missing nonnegative integer input_tokens"
            )
        })?;
    Ok(InputTokenCount::Exact(input_tokens))
}

pub(super) async fn run_remote_compaction(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<CompactResult> {
    let Some(url) = openai.compaction_url.clone() else {
        return Ok(CompactResult::Unsupported);
    };
    let prepared = prepare_turn_request(model_request, openai).await?;
    let body = Bytes::from(serde_json::to_vec(&RequestBody::compact(&prepared)?)?);

    let log_url = crate::connection::redacted_url_for_logging(url.as_str());
    let response = authenticated(
        &openai.api_key,
        &openai.profile.provider,
        |lease| {
            openai
                .maintenance_http
                .post(url.clone())
                .bearer_auth(lease.expose())
                .timeout(openai.compaction_timeout)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
        },
        http_unauthorized,
    )
    .await?
    .map_err(|error| {
        anyhow::anyhow!(
            "OpenAI compaction request to {log_url} failed: {}",
            error.without_url()
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        let detail = bounded_http_error_body(response).await;
        let error =
            anyhow::anyhow!("OpenAI compaction request failed with HTTP {status}: {detail}");
        return Err(
            if status == reqwest::StatusCode::UNAUTHORIZED && openai.api_key.is_refreshable() {
                error.context(rejected_after_refresh(&openai.profile.provider))
            } else {
                error
            },
        );
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| anyhow::anyhow!("malformed OpenAI compaction response: {error}"))?;
    let output = value
        .get("output")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("OpenAI compaction response omitted an output array"))?;
    if output.is_empty() {
        anyhow::bail!("OpenAI compaction response contained an empty output array");
    }
    if output.iter().any(|item| !item.is_object()) {
        anyhow::bail!("OpenAI compaction output items must be JSON objects");
    }
    let output = output
        .iter()
        .filter(|item| {
            item.get("role").and_then(serde_json::Value::as_str) != Some("developer")
                && item.get("type").and_then(serde_json::Value::as_str) != Some("additional_tools")
        })
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !output.is_empty(),
        "OpenAI compaction output was empty after filtering instruction and tool declarations"
    );
    let replay = ProviderReplay::openai_responses(openai.profile.clone(), output);
    Ok(CompactResult::Replacement(vec![
        zevria_model::OwnedModelRequestItem::replay_only(replay)?,
    ]))
}
