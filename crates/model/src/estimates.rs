use crate::{ContextTokenEstimate, ModelProfileRef, ModelRequestItem, estimate_message_tokens};
use rig_core::message::{AssistantContent, Message};
pub fn estimate_model_request_item(item: ModelRequestItem<'_>) -> ContextTokenEstimate {
    match item {
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
    match item {
        ModelRequestItem::Message(message @ Message::Assistant { .. }) => {
            let mut portable = message.clone();
            if let Message::Assistant { content, .. } = &mut portable {
                for block in content {
                    if let AssistantContent::Text(text) = block {
                        *text = rig_core::message::Text::new(crate::citations::render_text(text));
                    }
                }
            }
            Ok(estimate_message_tokens(&portable))
        }
        ModelRequestItem::ReplayBacked(content)
            if content.replay().source_profile != *target_profile =>
        {
            Ok(content
                .replay()
                .portable_projection()?
                .message
                .as_ref()
                .map(estimate_message_tokens)
                .unwrap_or_default())
        }
        ModelRequestItem::ReplayBacked(content) => Ok(estimate_message_tokens(content.message())),
        ModelRequestItem::Message(message) => {
            anyhow::ensure!(
                !matches!(message, Message::System { .. }),
                "raw system messages must be typed directives"
            );
            Ok(estimate_message_tokens(message))
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
            Ok(replay.replay_only_token_estimate())
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
