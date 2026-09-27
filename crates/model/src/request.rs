use crate::{ModelRole, ProviderReplay, TokenUsage};
use rig_core::message::Message;
use std::fmt;

/// One item in an ordered model request.
///
/// Provider-native data can either accompany the canonical message it
/// represents or stand alone when the provider emitted an opaque item (for
/// example a Responses compaction blob) with no valid Rig [`Message`] view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModelRequestItem<'session> {
    Message(&'session Message),
    DeveloperInstruction(&'session crate::DirectiveContent),
    RequestInstruction(&'session zevria_instructions::RequestDirective),
    ReplayBacked(&'session crate::ReplayMessage),
    ReplayOnly(&'session ProviderReplay),
}

impl<'session> ModelRequestItem<'session> {
    pub fn message(message: &'session Message) -> Self {
        Self::Message(message)
    }

    pub fn replay_backed(content: &'session crate::ReplayMessage) -> Self {
        Self::ReplayBacked(content)
    }

    pub fn replay_only(replay: &'session ProviderReplay) -> Self {
        Self::ReplayOnly(replay)
    }

    pub fn message_ref(self) -> Option<&'session Message> {
        match self {
            Self::Message(message) => Some(message),
            Self::ReplayBacked(content) => Some(content.message()),
            Self::ReplayOnly(_) | Self::DeveloperInstruction(_) | Self::RequestInstruction(_) => {
                None
            }
        }
    }

    pub fn replay_ref(self) -> Option<&'session ProviderReplay> {
        match self {
            Self::ReplayBacked(content) => Some(content.replay()),
            Self::ReplayOnly(replay) => Some(replay),
            Self::Message(_) | Self::DeveloperInstruction(_) | Self::RequestInstruction(_) => None,
        }
    }

    /// Snapshot this borrowed request item while preserving opaque native
    /// replay. Trusted pairs are cloned without re-deriving their canonical
    /// message; their constructor already established the invariant.
    pub fn to_owned_item(self) -> anyhow::Result<OwnedModelRequestItem> {
        match self {
            Self::RequestInstruction(directive) => {
                directive.validate()?;
                Ok(OwnedModelRequestItem::RequestInstruction(directive.clone()))
            }
            Self::DeveloperInstruction(text) => {
                text.validate()?;
                Ok(OwnedModelRequestItem::DeveloperInstruction(text.clone()))
            }
            Self::Message(message) => {
                anyhow::ensure!(
                    !matches!(message, Message::System { .. }),
                    "raw system messages must be typed directives"
                );
                Ok(OwnedModelRequestItem::message(message.clone()))
            }
            Self::ReplayBacked(content) => Ok(OwnedModelRequestItem::ReplayBacked(content.clone())),
            Self::ReplayOnly(replay) => OwnedModelRequestItem::replay_only(replay.clone()),
        }
    }
}

/// Serializable owned counterpart of [`ModelRequestItem`], used by durable
/// compaction checkpoints and provider compaction results.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedModelRequestItem {
    Message(Message),
    DeveloperInstruction(crate::DirectiveContent),
    RequestInstruction(zevria_instructions::RequestDirective),
    ReplayBacked(crate::ReplayMessage),
    ReplayOnly(ProviderReplay),
}

impl OwnedModelRequestItem {
    pub fn message(message: Message) -> Self {
        Self::Message(message)
    }

    /// Construct a canonical replay item by deriving its message from the
    /// native envelope. Callers cannot supply mismatched halves.
    pub fn replay_backed(replay: ProviderReplay) -> anyhow::Result<Self> {
        Ok(Self::ReplayBacked(crate::ReplayMessage::new(replay)?))
    }

    /// Preserve a validated native envelope without interpreting its items.
    pub fn replay_only(replay: ProviderReplay) -> anyhow::Result<Self> {
        replay
            .validate()
            .map_err(|reason| anyhow::anyhow!("invalid provider replay envelope: {reason}"))?;
        Ok(Self::ReplayOnly(replay))
    }

    pub fn as_borrowed(&self) -> ModelRequestItem<'_> {
        match self {
            Self::Message(message) => ModelRequestItem::Message(message),
            Self::DeveloperInstruction(text) => ModelRequestItem::DeveloperInstruction(text),
            Self::RequestInstruction(text) => ModelRequestItem::RequestInstruction(text),
            Self::ReplayBacked(item) => ModelRequestItem::ReplayBacked(item),
            Self::ReplayOnly(replay) => ModelRequestItem::ReplayOnly(replay),
        }
    }

    pub fn message_ref(&self) -> Option<&Message> {
        self.as_borrowed().message_ref()
    }

    pub fn replay_ref(&self) -> Option<&ProviderReplay> {
        self.as_borrowed().replay_ref()
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum OwnedModelRequestItemRecord {
    RequestInstruction {
        directive: zevria_instructions::RequestDirective,
    },
    DeveloperInstruction {
        text: crate::DirectiveContent,
    },
    Message {
        #[serde(deserialize_with = "deserialize_checkpoint_message")]
        message: Message,
    },
    ReplayBacked {
        replay: ProviderReplay,
    },
    ReplayOnly {
        replay: ProviderReplay,
    },
}

pub(super) fn deserialize_checkpoint_message<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Message, D::Error> {
    use serde::de::Error as _;
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    if value
        .as_object()
        .is_some_and(|object| object.keys().any(|key| key.starts_with("zevria_")))
    {
        return Err(D::Error::custom(
            "checkpoint messages cannot contain reserved transcript sidecars",
        ));
    }
    let message = serde_json::from_value(value).map_err(D::Error::custom)?;
    if matches!(message, Message::System { .. }) {
        return Err(D::Error::custom(
            "raw system messages must be typed directives",
        ));
    }
    if crate::prompt::message_has_images(&message) {
        crate::UserPrompt::from_message(&message).map_err(D::Error::custom)?;
    }
    Ok(message)
}

impl serde::Serialize for OwnedModelRequestItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error as _;
        #[derive(serde::Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum BorrowedRecord<'a> {
            RequestInstruction {
                directive: &'a zevria_instructions::RequestDirective,
            },
            DeveloperInstruction {
                text: &'a crate::DirectiveContent,
            },
            Message {
                message: &'a Message,
            },
            ReplayBacked {
                replay: &'a ProviderReplay,
            },
            ReplayOnly {
                replay: &'a ProviderReplay,
            },
        }
        let record = match self {
            Self::RequestInstruction(directive) => {
                directive.validate().map_err(S::Error::custom)?;
                BorrowedRecord::RequestInstruction { directive }
            }
            Self::DeveloperInstruction(text) => {
                text.validate().map_err(S::Error::custom)?;
                BorrowedRecord::DeveloperInstruction { text }
            }
            Self::Message(Message::System { .. }) => {
                return Err(S::Error::custom(
                    "raw system messages must be typed directives",
                ));
            }
            Self::Message(message) => BorrowedRecord::Message { message },
            Self::ReplayBacked(item) => BorrowedRecord::ReplayBacked {
                replay: item.replay(),
            },
            Self::ReplayOnly(replay) => BorrowedRecord::ReplayOnly { replay },
        };
        serde::Serialize::serialize(&record, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for OwnedModelRequestItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        match <OwnedModelRequestItemRecord as serde::Deserialize>::deserialize(deserializer)? {
            OwnedModelRequestItemRecord::RequestInstruction { directive } => {
                directive.validate().map_err(D::Error::custom)?;
                Ok(Self::RequestInstruction(directive))
            }
            OwnedModelRequestItemRecord::Message { message } => Ok(Self::Message(message)),
            OwnedModelRequestItemRecord::DeveloperInstruction { text } => {
                text.validate().map_err(D::Error::custom)?;
                Ok(Self::DeveloperInstruction(text))
            }
            OwnedModelRequestItemRecord::ReplayBacked { replay } => {
                Self::replay_backed(replay).map_err(D::Error::custom)
            }
            OwnedModelRequestItemRecord::ReplayOnly { replay } => {
                Self::replay_only(replay).map_err(D::Error::custom)
            }
        }
    }
}

/// A completed provider response and any native data needed to replay it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    record: crate::MessageRecord,
    pub usage: Option<TokenUsage>,
}

impl ModelResponse {
    pub fn from_record(record: crate::MessageRecord) -> Self {
        Self {
            record,
            usage: None,
        }
    }

    pub fn plain(message: Message) -> anyhow::Result<Self> {
        Ok(Self::from_record(crate::MessageRecord::plain(message)?))
    }

    pub fn from_replay(replay: ProviderReplay) -> anyhow::Result<Self> {
        Ok(Self::from_record(crate::MessageRecord::from_replay(
            replay,
        )?))
    }

    pub fn record(&self) -> &crate::MessageRecord {
        &self.record
    }

    pub fn message(&self) -> &Message {
        self.record.message()
    }

    /// Move the validated completed record directly into transcript admission.
    pub fn into_record(self) -> crate::MessageRecord {
        self.record
    }

    pub fn with_display_attempt(mut self, id: Option<String>) -> anyhow::Result<Self> {
        self.record = self.record.with_display_attempt(id)?;
        Ok(self)
    }

    pub fn with_usage(mut self, usage: Option<TokenUsage>) -> Self {
        self.usage = usage;
        self
    }
}

impl TryFrom<Message> for ModelResponse {
    type Error = anyhow::Error;

    fn try_from(message: Message) -> anyhow::Result<Self> {
        Self::plain(message)
    }
}

/// A borrowed request for a single provider completion.
///
/// `input` is the complete model-visible conversation in exact order,
/// including provider-native replay-only values that have no message view.
/// `model_role` and `allowed_tool_names` are the immutable
/// policy snapshot for the whole user submission, including provider retries
/// and tool continuations. `None` allows every registered tool; `Some(names)`
/// is an explicit allow-list. Requests carry nothing ephemeral: instructions
/// stay byte-stable within a workflow so providers keep a cacheable prefix, and
/// the conversation fields hold exactly the messages the session holds.
///
/// Every conversation field borrows from the session, so building the core
/// request does not deep-copy messages or their native ledgers. A provider can
/// then prepare the complete wire lineage needed for exact continuation and
/// reconnect checks.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest<'session> {
    /// The complete engine instruction set for this request; byte-stable within
    /// a workflow so providers keep a cacheable prefix.
    pub instructions: &'session str,
    pub input: Vec<ModelRequestItem<'session>>,
    pub model_role: ModelRole,
    pub allowed_tool_names: Option<&'session [String]>,
}

impl ModelRequest<'_> {
    /// The input's message-bearing compatibility view.
    pub fn owned_messages(&self) -> Vec<Message> {
        self.input
            .iter()
            .filter_map(|item| item.message_ref().cloned())
            .collect()
    }
}

/// A completion rejected specifically because its input exceeds provider capacity.
/// Adapters attach this marker while structured error information is available;
/// callers must inspect the error chain, never classify diagnostic prose.
#[derive(Debug)]
pub struct ModelInputTooLarge {
    source: anyhow::Error,
}

impl ModelInputTooLarge {
    pub fn new(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

impl fmt::Display for ModelInputTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model input exceeds provider capacity: {}", self.source)
    }
}

impl std::error::Error for ModelInputTooLarge {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Result of asking a provider for native conversation compaction.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactResult {
    Unsupported,
    Replacement(Vec<OwnedModelRequestItem>),
}

/// Provider result for a complete logical request's exact input-token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputTokenCount {
    Exact(u64),
    Unsupported,
}
