//! Validated ordinary message ownership, independent of the root storage codec.

use rig_core::message::Message;

use crate::{ModelRequestItem, ProviderReplay, ReplayMessage};

#[derive(Debug, Clone, PartialEq)]
enum MessageContent {
    Plain(Message),
    Replay(ReplayMessage),
}

/// A non-system ordinary message, optionally backed by a trusted native ledger.
/// Display metadata is validated separately and never enters model projection.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRecord {
    content: MessageContent,
    display_attempt_id: Option<String>,
}

impl MessageRecord {
    pub fn plain(message: Message) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !matches!(message, Message::System { .. }),
            "raw system messages must be typed directives"
        );
        if crate::prompt::message_has_images(&message) {
            crate::UserPrompt::from_message(&message)?;
        }
        Ok(Self {
            content: MessageContent::Plain(message),
            display_attempt_id: None,
        })
    }

    pub fn from_replay(replay: ProviderReplay) -> anyhow::Result<Self> {
        Ok(Self::from_trusted_replay(ReplayMessage::new(replay)?))
    }

    pub fn from_trusted_replay(content: ReplayMessage) -> Self {
        Self {
            content: MessageContent::Replay(content),
            display_attempt_id: None,
        }
    }

    pub fn with_display_attempt(mut self, id: Option<String>) -> anyhow::Result<Self> {
        if let Some(id) = id {
            crate::web_search::validate_display_id(&id)?;
            anyhow::ensure!(
                matches!(self.message(), Message::Assistant { .. }),
                "display binding requires an assistant record"
            );
            self.display_attempt_id = Some(id);
        }
        Ok(self)
    }

    pub fn display_attempt_id(&self) -> Option<&str> {
        self.display_attempt_id.as_deref()
    }

    pub fn message(&self) -> &Message {
        match &self.content {
            MessageContent::Plain(message) => message,
            MessageContent::Replay(content) => content.message(),
        }
    }

    pub fn provider_replay(&self) -> Option<&ProviderReplay> {
        match &self.content {
            MessageContent::Plain(_) => None,
            MessageContent::Replay(content) => Some(content.replay()),
        }
    }

    pub fn model_request_item(&self) -> ModelRequestItem<'_> {
        match &self.content {
            MessageContent::Plain(message) => ModelRequestItem::message(message),
            MessageContent::Replay(content) => ModelRequestItem::replay_backed(content),
        }
    }

    /// Transfer content to an owned request/checkpoint without display metadata.
    pub fn into_model_request_item(self) -> crate::OwnedModelRequestItem {
        match self.content {
            MessageContent::Plain(message) => crate::OwnedModelRequestItem::Message(message),
            MessageContent::Replay(content) => crate::OwnedModelRequestItem::ReplayBacked(content),
        }
    }

    /// Owned display restoration can take the message while dropping the native
    /// ledger. The boolean preserves response-commit semantics for native content.
    pub fn into_display(self) -> (Message, Option<String>, bool) {
        let (message, replay_backed) = match self.content {
            MessageContent::Plain(message) => (message, false),
            MessageContent::Replay(content) => (content.into_message(), true),
        };
        (message, self.display_attempt_id, replay_backed)
    }

    /// Consume validated content without exposing independently mutable message/replay halves.
    pub fn into_parts<T>(
        self,
        plain: impl FnOnce(Message, Option<String>) -> T,
        replay: impl FnOnce(ReplayMessage, Option<String>) -> T,
    ) -> T {
        match self.content {
            MessageContent::Plain(message) => plain(message, self.display_attempt_id),
            MessageContent::Replay(content) => replay(content, self.display_attempt_id),
        }
    }
}
