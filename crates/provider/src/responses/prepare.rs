//! Responses prepare responsibilities.
use crate::diagnostics::Diagnostics;
use zevria_model::ModelRequest;
use zevria_responses::tools::ToolRendererOptions;

use crate::{config::validate_additional_params, connection::OpenAiProvider, replay::ReplaySource};

pub(crate) struct ProjectionCache {
    pub(super) profile: zevria_foundation::ModelProfileRef,
    pub(super) compatibility: [bool; 3],
    pub(super) input_count: usize,
    pub(super) input_digest: [u8; 32],
    pub(super) projection: zevria_responses::replay::InputProjection,
    pub(super) input: std::sync::Arc<Vec<serde_json::Value>>,
}

pub(crate) struct PreparedRequest {
    pub(crate) cache_diagnostics: crate::diagnostics::RequestDiagnostics,
    pub(crate) request_properties: serde_json::Value,
    pub(crate) full_input: std::sync::Arc<Vec<serde_json::Value>>,
    pub(super) input_count: usize,
    pub(super) input_digest: [u8; 32],
    pub(super) input_matches_continuation: bool,
    pub(super) history_message_count: usize,
    pub(super) replay_source: ReplaySource,
}

pub(crate) async fn prepare_turn_request(
    model_request: &ModelRequest<'_>,
    openai: &mut OpenAiProvider,
) -> anyhow::Result<PreparedRequest> {
    let tool_options = ToolRendererOptions {
        strict_tools: openai.compatibility.strict_tools,
    };
    let declarations = zevria_model::ToolDeclarations::new(model_request.tools.to_vec())?;
    let additions = model_request
        .input
        .iter()
        .filter_map(|item| match item {
            zevria_model::ModelRequestItem::AdditionalTools(tools) => Some(*tools),
            _ => None,
        })
        .collect::<Vec<_>>();
    let folded = if openai.compatibility.additional_tools {
        declarations
    } else {
        declarations.fold(additions.iter().copied())
    };
    let mut tools = zevria_responses::tools::render(&folded, tool_options)?;

    let search_advertised = openai.web_search.enabled
        && model_request.allowed_tool_names.is_none_or(|names| {
            names
                .iter()
                .any(|name| name == zevria_foundation::WEB_SEARCH_TOOL_NAME)
        });
    if search_advertised {
        tools.push(serde_json::to_value(openai.web_search.tool()?)?);
    }

    let mut request_properties = openai
        .prepared_responses_parameters()
        .unwrap_or_else(|| serde_json::json!({}));
    request_properties["model"] = openai.profile.model.clone().into();
    request_properties["instructions"] = model_request.instructions.into();
    if !tools.is_empty() {
        request_properties["tools"] = tools.into();
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
    } else {
        let mut declared = properties
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if openai.compatibility.additional_tools {
            for addition in &additions {
                declared.extend(zevria_responses::tools::render(addition, tool_options)?);
            }
        }
        validate_tool_choice(properties, &declared).map_err(|error| anyhow::anyhow!(
            "profile {}: providers.{}.additional_params.tool_choice conflicts with declared tools or hosted capabilities (check providers.{}.web_search.enabled and hosted-search task permissions): {error}",
            openai.profile, openai.profile.provider, openai.profile.provider
        ))?;
    }

    let fallback_identity;
    let identity = match model_request.input_identity {
        Some(identity) if identity.len() == model_request.input.len() => identity,
        _ => {
            fallback_identity = zevria_model::ModelInputIdentity::from_items(&model_request.input)?;
            &fallback_identity
        }
    };
    let input_matches_continuation = openai
        .ws
        .continuation
        .as_ref()
        .map(|continuation| continuation.lineage)
        .is_some_and(|lineage| {
            identity.len() > lineage.input_count
                && identity.prefix(lineage.input_count) == Some(lineage.input_digest)
        });
    let compatibility = [
        openai.compatibility.developer_messages,
        tool_options.strict_tools,
        openai.compatibility.additional_tools,
    ];
    let cached = openai.input_projection.take().filter(|cache| {
        cache.profile == openai.profile
            && cache.compatibility == compatibility
            && identity.prefix(cache.input_count) == Some(cache.input_digest)
    });
    let (mut projection, start, projection_extended) = match cached {
        Some(cache)
            if cache
                .projection
                .can_extend(&model_request.input[cache.input_count..], &openai.profile)? =>
        {
            let mut projection = cache.projection;
            projection.restore_input(
                std::sync::Arc::try_unwrap(cache.input)
                    .unwrap_or_else(|shared| shared.as_ref().clone()),
            );
            (projection, cache.input_count, true)
        }
        _ => (
            zevria_responses::replay::InputProjection::default(),
            0,
            false,
        ),
    };
    projection.extend(
        &model_request.input[start..],
        &openai.profile,
        openai.compatibility.developer_messages,
        tool_options,
        openai.compatibility.additional_tools,
    )?;
    let replay_source = projection.source();
    let full_input = std::sync::Arc::new(projection.take_input());
    anyhow::ensure!(
        !full_input.is_empty(),
        "the OpenAI Responses request contained no input items"
    );
    openai.input_projection = Some(ProjectionCache {
        profile: openai.profile.clone(),
        compatibility,
        input_count: identity.len(),
        input_digest: identity.digest(),
        projection,
        input: full_input.clone(),
    });
    Ok(PreparedRequest {
        cache_diagnostics: Diagnostics::empty_request(),
        request_properties,
        full_input,
        input_count: identity.len(),
        input_digest: identity.digest(),
        // Rebuilding may rename portable call IDs to reserve new native
        // handles, or change projection compatibility. Never chain that prefix.
        input_matches_continuation: input_matches_continuation && projection_extended,
        history_message_count: model_request.input.len().saturating_sub(1),
        replay_source,
    })
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
