//! Responses prepare responsibilities.
use crate::diagnostics::Diagnostics;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use zevria_model::{ModelInputIdentity, ModelRequest, ModelRequestItem};
use zevria_responses::{
    replay::{PreflightOptions, ProjectedInput, ResponsesSlotCache},
    tools::ToolRendererOptions,
};

use crate::{config::validate_additional_params, connection::OpenAiProvider, replay::ReplaySource};

#[derive(Default)]
pub(crate) struct ResponsesCache {
    pub(crate) input: ResponsesSlotCache,
    tools: Option<ToolCache>,
}

struct ToolCache {
    digest: [u8; 32],
    compatibility: [bool; 2],
    rendered: Arc<RenderedTools>,
}

pub(crate) struct RenderedTools {
    values: Vec<Value>,
    pub(crate) tokens: u64,
}

pub(crate) struct PropertiesCache {
    base: Vec<u8>,
    tools: Arc<RenderedTools>,
    properties: Arc<Value>,
}

pub(crate) struct PreparedRequest {
    pub(crate) cache_diagnostics: crate::diagnostics::RequestDiagnostics,
    pub(crate) request_properties: Arc<Value>,
    pub(crate) full_input: Arc<Vec<Value>>,
    pub(super) http_body: OnceLock<bytes::Bytes>,
    pub(super) projection_generation: u64,
    pub(super) input_count: usize,
    pub(super) input_digest: [u8; 32],
    pub(super) input_matches_continuation: bool,
    pub(super) history_message_count: usize,
    pub(super) replay_source: ReplaySource,
}

impl ResponsesCache {
    /// Render the exact declarations, reusing both their wire array and token
    /// estimate between admission and dispatch. Fold revisions in place.
    pub(crate) fn rendered_tools(
        &mut self,
        request: &ModelRequest<'_>,
        compatibility: &crate::config::ResponsesCompatibilityConfig,
    ) -> anyhow::Result<Arc<RenderedTools>> {
        struct HashWriter(Sha256);
        impl std::io::Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let fold = !compatibility.additional_tools;
        let mut digest = HashWriter(Sha256::new());
        serde_json::to_writer(&mut digest, request.tools)?;
        if fold {
            for addition in tool_additions(request) {
                serde_json::to_writer(&mut digest, addition)?;
            }
        }
        let digest: [u8; 32] = digest.0.finalize().into();
        let key = [compatibility.strict_tools, compatibility.additional_tools];
        if let Some(cache) = &self.tools
            && cache.digest == digest
            && cache.compatibility == key
        {
            return Ok(cache.rendered.clone());
        }
        let declarations = zevria_model::ToolDeclarations::new(request.tools.to_vec())?;
        let declarations = if fold {
            declarations.fold(tool_additions(request))
        } else {
            declarations
        };
        let values = zevria_responses::tools::render(
            &declarations,
            ToolRendererOptions {
                strict_tools: compatibility.strict_tools,
            },
        )?;
        let tokens = if values.is_empty() {
            0
        } else {
            zevria_model::compaction::approximate_tokens_from_bytes(
                serde_json::to_vec(&values)?.len(),
            )
        };
        let rendered = Arc::new(RenderedTools { values, tokens });
        self.tools = Some(ToolCache {
            digest,
            compatibility: key,
            rendered: rendered.clone(),
        });
        Ok(rendered)
    }
}

fn tool_additions<'a>(
    request: &'a ModelRequest<'a>,
) -> impl Iterator<Item = &'a zevria_model::ToolDeclarations> {
    request.input.iter().filter_map(|item| match item {
        zevria_model::ModelRequestItem::AdditionalTools(tools) => Some(*tools),
        _ => None,
    })
}

/// An identity-less count is a temporary view, not a continuation prefix.
/// Carry its validated wire projection through lazy runtime initialization so
/// counts reject bad replay locally without projecting the history twice.
pub(crate) struct ResponsesCountProbe {
    identity: ModelInputIdentity,
    projected: ProjectedInput,
}

impl ResponsesCountProbe {
    pub(crate) fn new(
        input: &[ModelRequestItem<'_>],
        options: PreflightOptions<'_>,
    ) -> anyhow::Result<Self> {
        let identity = ModelInputIdentity::from_items(input)?;
        let projected = project_input(
            &mut ResponsesSlotCache::default(),
            input,
            &identity,
            options,
        )?;
        Ok(Self {
            identity,
            projected,
        })
    }
}

pub(crate) async fn prepare_turn_request(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<PreparedRequest> {
    prepare_request(model_request, openai, None)
}

pub(super) fn prepare_count_request(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    probe: Option<ResponsesCountProbe>,
) -> anyhow::Result<PreparedRequest> {
    let probe = match (probe, model_request.input_identity) {
        (None, None) => Some(ResponsesCountProbe::new(
            &model_request.input,
            openai
                .compatibility
                .preflight_options(&openai.profile, &openai.image_accounting),
        )?),
        (probe, _) => probe,
    };
    prepare_request(model_request, openai, probe)
}

fn prepare_request(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
    probe: Option<ResponsesCountProbe>,
) -> anyhow::Result<PreparedRequest> {
    let use_live_cache = probe.is_none();
    let (probe_identity, probe_projection) = match probe {
        Some(ResponsesCountProbe {
            identity,
            projected,
        }) => (Some(identity), Some(projected)),
        None => (None, None),
    };
    let fallback_identity;
    let identity = match probe_identity.as_ref().or(model_request.input_identity) {
        Some(identity) if identity.len() == model_request.input.len() => identity,
        _ => {
            fallback_identity = ModelInputIdentity::from_items(&model_request.input)?;
            &fallback_identity
        }
    };
    let mut scratch;
    let mut live;
    let cache = if use_live_cache {
        live = openai
            .slot_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Responses slot cache lock poisoned"))?;
        &mut live.responses
    } else {
        scratch = ResponsesCache::default();
        &mut scratch
    };
    let tool_options = ToolRendererOptions {
        strict_tools: openai.compatibility.strict_tools,
    };
    let tools = cache.rendered_tools(model_request, &openai.compatibility)?;

    let mut request_properties = openai
        .prepared_responses_parameters()
        .unwrap_or_else(|| serde_json::json!({}));
    request_properties["model"] = openai.profile.model.clone().into();
    request_properties["instructions"] = model_request.instructions.into();
    if !tools.values.is_empty() {
        // Reserve the original property position, without copying the tool
        // schemas just to check whether the properties can be shared.
        request_properties["tools"] = Value::Null;
    }
    if openai.compatibility.send_store {
        request_properties["store"] = false.into();
    }
    let properties = request_properties
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the OpenAI request did not serialize as an object"))?;
    validate_additional_params(&openai.profile.provider, &openai.additional_params)?;
    for (key, value) in &openai.additional_params {
        properties.insert(key.clone(), value.clone());
    }
    properties.remove("input");
    properties.remove("previous_response_id");
    if model_request.forbid_tool_calls {
        properties.insert("tool_choice".into(), serde_json::json!("none"));
    } else if properties.contains_key("tool_choice") {
        let mut declared = tools.values.clone();
        if openai.compatibility.additional_tools {
            for addition in tool_additions(model_request) {
                declared.extend(zevria_responses::tools::render(addition, tool_options)?);
            }
        }
        validate_tool_choice(properties, &declared).map_err(|error| anyhow::anyhow!(
            "profile {}: providers.{}.additional_params.tool_choice conflicts with declared tools: {error}",
            openai.profile, openai.profile.provider
        ))?;
    }

    let base = use_live_cache
        .then(|| serde_json::to_vec(&request_properties))
        .transpose()?;
    let request_properties = match (&openai.request_properties_cache, base) {
        (Some(cache), Some(base)) if cache.base == base && Arc::ptr_eq(&cache.tools, &tools) => {
            cache.properties.clone()
        }
        (_, base) => {
            if !tools.values.is_empty() {
                request_properties["tools"] = Value::Array(tools.values.clone());
            }
            let properties = Arc::new(request_properties);
            if let Some(base) = base {
                openai.request_properties_cache = Some(PropertiesCache {
                    base,
                    tools,
                    properties: properties.clone(),
                });
            }
            properties
        }
    };

    let projected = match probe_projection {
        Some(projected) => projected,
        None => project_input(
            &mut cache.input,
            &model_request.input,
            identity,
            openai
                .compatibility
                .preflight_options(&openai.profile, &openai.image_accounting),
        )?,
    };
    anyhow::ensure!(
        !projected.input.is_empty(),
        "the OpenAI Responses request contained no input items"
    );
    let input_matches_continuation = use_live_cache
        && openai
            .responses_transport
            .ws
            .continuation
            .as_ref()
            .is_some_and(|continuation| {
                let lineage = continuation.lineage;
                projected.generation == lineage.projection_generation
                    && identity.len() > lineage.input_count
                    && identity.prefix(lineage.input_count) == Some(lineage.input_digest)
            });
    Ok(PreparedRequest {
        cache_diagnostics: Diagnostics::empty_request(),
        request_properties,
        full_input: projected.input,
        http_body: OnceLock::new(),
        projection_generation: projected.generation,
        input_count: identity.len(),
        input_digest: identity.digest(),
        // Rebuilding may rename portable call IDs to reserve new native
        // handles, or change projection compatibility. Never chain that prefix.
        input_matches_continuation,
        history_message_count: model_request.input.len().saturating_sub(1),
        replay_source: projected.source,
    })
}

fn project_input(
    cache: &mut ResponsesSlotCache,
    input: &[ModelRequestItem<'_>],
    identity: &ModelInputIdentity,
    options: PreflightOptions<'_>,
) -> anyhow::Result<ProjectedInput> {
    let assessment = cache.assess(input, identity, options)?;
    if let zevria_model::models::ReplayPreflight::ConversionRequired { .. } = assessment {
        let replay = input
            .iter()
            .find_map(|item| match item {
                ModelRequestItem::ReplayOnly(replay)
                    if &replay.source_profile != options.projection.target
                        || replay.protocol() != Some(zevria_model::ReplayProtocol::Responses) =>
                {
                    Some(replay)
                }
                _ => None,
            })
            .expect("conversion requires a foreign replay-only item");
        anyhow::bail!(
            "cannot send replay-only {} history from source profile {} to foreign target profile {}: conversion required",
            replay.provider,
            replay.source_profile,
            options.projection.target
        );
    }
    Ok(cache.projected_input().expect("compatible projection"))
}

pub(super) fn validate_tool_choice(
    properties: &serde_json::Map<String, serde_json::Value>,
    tools: &[serde_json::Value],
) -> anyhow::Result<()> {
    let Some(choice) = properties.get("tool_choice") else {
        return Ok(());
    };
    let permitted = |choice: &serde_json::Value| {
        tools.iter().any(|tool| {
            tool.get("type") == choice.get("type")
                && (tool.get("type").and_then(serde_json::Value::as_str) != Some("function")
                    || tool.get("name") == choice.get("name"))
        })
    };
    match choice.as_str() {
        Some("auto" | "none") => return Ok(()),
        Some("required") => {
            anyhow::ensure!(
                !tools.is_empty(),
                "required tool choice has no permitted tools"
            );
            return Ok(());
        }
        _ => {}
    }
    if choice.get("type").and_then(serde_json::Value::as_str) == Some("allowed_tools") {
        let choices = choice
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("allowed_tools must contain tools"))?;
        anyhow::ensure!(
            !choices.is_empty() && choices.iter().all(permitted),
            "allowed_tools contains an unavailable or disallowed tool"
        );
    } else {
        anyhow::ensure!(
            choice.is_object() && permitted(choice),
            "forced tool is unavailable or disallowed"
        );
    }
    Ok(())
}
