use crate::{ContextTokenEstimate, ModelProfileRef, ModelRequestItem, estimate_message_tokens};
use rig_core::message::{AssistantContent, Message};
use zevria_foundation::image_accounting::ImageAccountingPolicy;
pub fn estimate_model_request_item(item: ModelRequestItem<'_>) -> ContextTokenEstimate {
    match item {
        ModelRequestItem::AdditionalTools(tools) => {
            let tokens = tools.approximate_tokens();
            ContextTokenEstimate::new(tokens, tokens.saturating_add(16))
        }
        ModelRequestItem::Message(message) => estimate_message_tokens(message),
        ModelRequestItem::ReplayBacked(content) => estimate_message_tokens(content.message()),
        ModelRequestItem::ReplayOnly(replay) => replay.replay_only_token_estimate(),
        ModelRequestItem::RequestInstruction(directive) => {
            let tokens = crate::compaction::approximate_tokens(&directive.render());
            ContextTokenEstimate::new(tokens, tokens.saturating_add(16))
        }
        ModelRequestItem::DeveloperInstruction(directive) => {
            let tokens = crate::compaction::approximate_tokens(&directive.text);
            ContextTokenEstimate::new(tokens, tokens.saturating_add(16))
        }
    }
}

pub fn estimate_model_input(input: Vec<ModelRequestItem<'_>>) -> ContextTokenEstimate {
    input.into_iter().map(estimate_model_request_item).fold(
        ContextTokenEstimate::default(),
        ContextTokenEstimate::saturating_add,
    )
}

pub fn estimate_model_request_item_for_profile(
    item: ModelRequestItem<'_>,
    target_profile: &ModelProfileRef,
) -> anyhow::Result<ContextTokenEstimate> {
    estimate_model_request_item_with_policy(
        item,
        target_profile,
        &ImageAccountingPolicy::Unresolved,
    )
}

pub fn estimate_model_request_item_with_policy(
    item: ModelRequestItem<'_>,
    target_profile: &ModelProfileRef,
    policy: &ImageAccountingPolicy,
) -> anyhow::Result<ContextTokenEstimate> {
    match item {
        ModelRequestItem::AdditionalTools(tools) => {
            tools.validate()?;
            Ok(estimate_model_request_item(item))
        }
        ModelRequestItem::Message(message @ Message::Assistant { .. }) => {
            let mut estimate =
                crate::compaction::estimate_message_tokens_with_policy(message, policy);
            if let Message::Assistant(assistant) = message {
                for block in &assistant.content {
                    if let AssistantContent::Text(text) = block {
                        let old = crate::compaction::approximate_tokens(&text.text);
                        let new = crate::compaction::approximate_tokens(
                            &crate::citations::render_text(text),
                        );
                        estimate.payload_tokens = estimate
                            .payload_tokens
                            .saturating_sub(old)
                            .saturating_add(new);
                        estimate.estimated_tokens = estimate
                            .estimated_tokens
                            .saturating_sub(old)
                            .saturating_add(new);
                    }
                }
            }
            Ok(estimate)
        }
        ModelRequestItem::ReplayBacked(content)
            if content.replay().source_profile != *target_profile =>
        {
            Ok(content
                .replay()
                .portable_projection()?
                .message
                .as_ref()
                .map(|message| {
                    crate::compaction::estimate_message_tokens_with_policy(message, policy)
                })
                .unwrap_or_default())
        }
        ModelRequestItem::ReplayBacked(content) => Ok(
            crate::compaction::estimate_message_tokens_with_policy(content.message(), policy),
        ),
        ModelRequestItem::Message(message) => {
            anyhow::ensure!(
                !matches!(message, Message::System { .. }),
                "raw system messages must be typed directives"
            );
            Ok(crate::compaction::estimate_message_tokens_with_policy(
                message, policy,
            ))
        }
        ModelRequestItem::RequestInstruction(directive) => {
            directive.validate()?;
            Ok(estimate_model_request_item(item))
        }
        ModelRequestItem::DeveloperInstruction(directive) => {
            directive.validate()?;
            Ok(estimate_model_request_item(item))
        }
        ModelRequestItem::ReplayOnly(replay) => {
            if replay.source_profile != *target_profile {
                anyhow::bail!(
                    "cannot project replay-only {provider} history from source profile {source} into foreign target profile {target}",
                    provider = replay.provider,
                    source = replay.source_profile,
                    target = target_profile,
                );
            }
            Ok(replay.replay_only_token_estimate_with_policy(policy))
        }
    }
}

pub fn estimate_model_input_for_profile(
    input: Vec<ModelRequestItem<'_>>,
    target_profile: &ModelProfileRef,
) -> anyhow::Result<ContextTokenEstimate> {
    input
        .into_iter()
        .try_fold(ContextTokenEstimate::default(), |total, item| {
            estimate_model_request_item_for_profile(item, target_profile)
                .map(|estimate| total.saturating_add(estimate))
        })
}

pub fn estimate_model_input_with_policy(
    input: Vec<ModelRequestItem<'_>>,
    target_profile: &ModelProfileRef,
    policy: &ImageAccountingPolicy,
) -> anyhow::Result<ContextTokenEstimate> {
    input
        .into_iter()
        .try_fold(ContextTokenEstimate::default(), |total, item| {
            estimate_model_request_item_with_policy(item, target_profile, policy)
                .map(|estimate| total.saturating_add(estimate))
        })
        .map(|estimate| estimate.enforce_request_constraints(policy))
}
