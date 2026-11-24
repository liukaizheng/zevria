//! Transcript item responsibilities.
use crate::ModelRequestItem;
use crate::ProviderReplay;
use crate::compaction::CompactionCheckpoint;
use crate::ensemble::EnsembleRecord;
use crate::models::SessionModels;
use crate::plan::PlanRecord;
use crate::skill::SkillInvocation;
use crate::skill::SkillToolApplication;
use crate::tool_result::ToolResultMetadata;
use rig_core::message::Message;
use serde::de::{Error as _, MapAccess, Visitor, value::MapAccessDeserializer};
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zevria_foundation::{
    CommandJobNotice, RequestMetadata, tool_result::validate_tool_result_message,
};
use zevria_model::MessageRecord;

pub use crate::replay_active_skills;
pub use crate::{ToolDeclarationPlacement, ToolDeclarationRecord, ToolWindow, tool_window};

/// Allows the maximum 20 MiB image prompt, typed control records,
/// base64 expansion, and bounded protocol envelopes.
pub const MAX_ROOT_RECORD_BYTES: usize = 64 * 1024 * 1024;
pub(super) const BATCH_VERSION: u32 = 2;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RecordType {
    Batch,
    SessionModels,
    SessionMode,
    Message,
    RequestPrompt,
    ProviderMessage,
    ToolResults,
    SkillInvocation,
    Directive,
    RequestDirective,
    ToolDeclarations,
    CommandJobNotice,
    Plan,
    Ensemble,
    Compaction,
    IncompleteResponse,
    Error,
}

/// A physical commit boundary. The first key must be `type`, and batch
/// children are items, never another physical line.
pub(super) enum TranscriptLine {
    Batch {
        version: u32,
        records: Vec<TranscriptItem>,
    },
    Item(TranscriptItem),
}

#[derive(Serialize)]
pub(super) struct BatchRef<'a> {
    r#type: RecordType,
    version: u32,
    records: &'a [TranscriptItem],
}

impl<'a> BatchRef<'a> {
    pub(super) fn new(records: &'a [TranscriptItem]) -> Self {
        Self {
            r#type: RecordType::Batch,
            version: BATCH_VERSION,
            records,
        }
    }
}

pub(super) fn read_record_type<'de, A: MapAccess<'de>>(
    map: &mut A,
) -> Result<RecordType, A::Error> {
    #[derive(Deserialize)]
    #[serde(field_identifier, rename_all = "snake_case")]
    enum Key {
        Type,
    }
    map.next_key::<Key>()?
        .ok_or_else(|| A::Error::missing_field("type"))?;
    map.next_value()
}

pub(super) fn read_batch_version<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version != BATCH_VERSION {
        return Err(D::Error::custom(
            "unsupported transcript batch version; expected v2",
        ));
    }
    Ok(version)
}

impl<'de> Deserialize<'de> for TranscriptLine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Line;
        impl<'de> Visitor<'de> for Line {
            type Value = TranscriptLine;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a tagged transcript line starting with type")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                match read_record_type(&mut map)? {
                    RecordType::Batch => {
                        #[derive(Deserialize)]
                        #[serde(deny_unknown_fields)]
                        struct Batch {
                            #[serde(deserialize_with = "read_batch_version")]
                            version: u32,
                            records: Vec<TranscriptItem>,
                        }
                        let Batch { version, records } =
                            Batch::deserialize(MapAccessDeserializer::new(map))?;
                        if records.is_empty() {
                            return Err(A::Error::custom("empty transcript batch"));
                        }
                        Ok(TranscriptLine::Batch { version, records })
                    }
                    tag => TranscriptItem::read_payload(tag, map).map(TranscriptLine::Item),
                }
            }
        }
        deserializer.deserialize_map(Line)
    }
}

/// One conversation item, persisted as one JSONL record. Skill directives retain
/// their exact ordered model-input positions; invocations and tool applications
/// intentionally retain their full historical activation pins.
#[derive(Debug, PartialEq)]
pub enum TranscriptItem {
    /// Model structure only: top-level window anchor or ordered inline additions.
    ToolDeclarations(Box<ToolDeclarationRecord>),
    /// Immutable display-only evidence of an abandoned response.
    IncompleteResponse(Box<crate::IncompleteResponse>),
    /// Last-wins Build/Plan selections; never model-visible.
    /// Never conversation content, model input, or response provenance.
    SessionModels(Box<SessionModels>),
    /// Model-inert, last-wins mode selection.
    SessionMode(crate::SessionMode),
    /// Validated skill instructions, persisted without their derived rendered text.
    Directive(Box<crate::DirectiveContent>),
    RequestDirective(Box<zevria_instructions::RequestDirective>),
    /// One editable prompt, with model-inert validated request intent.
    RequestPrompt {
        message: Message,
        request: zevria_foundation::RequestMetadata,
    },
    Message(Message),
    /// A provider-native replay plus its internally cached canonical message.
    /// The private cache prevents callers from constructing mismatched halves.
    ProviderMessage(ReplayBackedMessage),
    ToolResults {
        message: Message,
        metadata: Vec<ToolResultMetadata>,
        /// Hidden lifecycle data. Never copied into frontend event metadata.
        skill_applications: Vec<SkillToolApplication>,
    },
    /// An owning direct invocation. Its bodyless canonical model message is
    /// derived privately from name and ordered arguments, never its application.
    SkillInvocation(Box<SkillInvocation>),
    /// Append-only untrusted command data with verbatim model-visible text.
    CommandJobNotice(Box<CommandJobNotice>),
    /// Engine-owned Plan workflow state. Only `PlanRecord::Handoff` projects
    /// a message into model history; every other record is metadata-only.
    Plan(Box<PlanRecord>),
    /// Engine-owned ensemble workflow state. Only `ReportsReady` projects its
    /// bounded, untrusted synthesis input into model history.
    Ensemble(Box<EnsembleRecord>),
    /// A durable context checkpoint used only by model projection.
    Compaction(Box<CompactionCheckpoint>),
    Error {
        error: String,
    },
}

impl Clone for TranscriptItem {
    fn clone(&self) -> Self {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| counts.item_clones += 1);
        match self {
            Self::ToolDeclarations(record) => Self::ToolDeclarations(record.clone()),
            Self::IncompleteResponse(record) => Self::IncompleteResponse(record.clone()),
            Self::SessionModels(models) => Self::SessionModels(models.clone()),
            Self::SessionMode(mode) => Self::SessionMode(*mode),
            Self::Directive(record) => Self::Directive(record.clone()),
            Self::RequestDirective(record) => Self::RequestDirective(record.clone()),
            Self::RequestPrompt { message, request } => Self::RequestPrompt {
                message: message.clone(),
                request: request.clone(),
            },
            Self::Message(message) => Self::Message(message.clone()),
            Self::ProviderMessage(message) => Self::ProviderMessage(message.clone()),
            Self::ToolResults {
                message,
                metadata,
                skill_applications,
            } => Self::ToolResults {
                message: message.clone(),
                metadata: metadata.clone(),
                skill_applications: skill_applications.clone(),
            },
            Self::SkillInvocation(record) => Self::SkillInvocation(record.clone()),
            Self::CommandJobNotice(record) => Self::CommandJobNotice(record.clone()),
            Self::Plan(record) => Self::Plan(record.clone()),
            Self::Ensemble(record) => Self::Ensemble(record.clone()),
            Self::Compaction(record) => Self::Compaction(record.clone()),
            Self::Error { error } => Self::Error {
                error: error.clone(),
            },
        }
    }
}

/// The inseparable native and provider-neutral views of one provider response.
///
/// The private content shares [`crate::ReplayMessage`]'s trusted-pair invariant.
/// Construction derives it from replay once or moves it from a completed
/// [`MessageRecord`]; display metadata never belongs to the native envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayBackedMessage {
    content: std::sync::Arc<crate::ReplayMessage>,
}

impl Serialize for TranscriptItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        fn single<S: Serializer>(
            serializer: S,
            tag: RecordType,
            key: &'static str,
            value: &impl Serialize,
        ) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("type", &tag)?;
            map.serialize_entry(key, value)?;
            map.end()
        }
        match self {
            Self::SessionModels(models) => {
                single(serializer, RecordType::SessionModels, "models", models)
            }
            Self::SessionMode(mode) => {
                single(serializer, RecordType::SessionMode, "selected", mode)
            }
            Self::Message(message) => {
                if matches!(message, Message::System { .. }) {
                    return Err(serde::ser::Error::custom(
                        "raw system messages must be typed directives",
                    ));
                }
                single(serializer, RecordType::Message, "message", message)
            }
            Self::RequestPrompt { message, request } => {
                request.validate().map_err(serde::ser::Error::custom)?;
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", &RecordType::RequestPrompt)?;
                map.serialize_entry("message", message)?;
                map.serialize_entry("request", request)?;
                map.end()
            }
            Self::ProviderMessage(message) => single(
                serializer,
                RecordType::ProviderMessage,
                "replay",
                message.content.replay(),
            ),
            Self::ToolResults {
                message,
                metadata,
                skill_applications,
            } => {
                validate_tool_result_message(message).map_err(serde::ser::Error::custom)?;
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("type", &RecordType::ToolResults)?;
                map.serialize_entry("message", message)?;
                map.serialize_entry("metadata", metadata)?;
                map.serialize_entry("skill_applications", skill_applications)?;
                map.end()
            }
            Self::SkillInvocation(record) => single(
                serializer,
                RecordType::SkillInvocation,
                "invocation",
                record,
            ),
            Self::Directive(directive) => {
                directive.validate().map_err(serde::ser::Error::custom)?;
                #[derive(Serialize)]
                struct DirectiveRef<'a> {
                    version: u32,
                    payload: &'a crate::DirectivePayload,
                }
                single(
                    serializer,
                    RecordType::Directive,
                    "directive",
                    &DirectiveRef {
                        version: directive.version,
                        payload: &directive.payload,
                    },
                )
            }
            Self::RequestDirective(record) => {
                record.validate().map_err(serde::ser::Error::custom)?;
                single(
                    serializer,
                    RecordType::RequestDirective,
                    "directive",
                    record,
                )
            }
            Self::ToolDeclarations(record) => {
                record.validate().map_err(serde::ser::Error::custom)?;
                single(
                    serializer,
                    RecordType::ToolDeclarations,
                    "declarations",
                    record,
                )
            }
            Self::CommandJobNotice(record) => {
                single(serializer, RecordType::CommandJobNotice, "notice", record)
            }
            Self::Plan(record) => single(serializer, RecordType::Plan, "plan", record),
            Self::Ensemble(record) => single(serializer, RecordType::Ensemble, "ensemble", record),
            Self::Compaction(record) => {
                single(serializer, RecordType::Compaction, "checkpoint", record)
            }
            Self::IncompleteResponse(record) => {
                record.validate().map_err(serde::ser::Error::custom)?;
                single(
                    serializer,
                    RecordType::IncompleteResponse,
                    "attempt",
                    record,
                )
            }
            Self::Error { error } => single(serializer, RecordType::Error, "error", error),
        }
    }
}

impl<'de> Deserialize<'de> for TranscriptItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Item;
        impl<'de> Visitor<'de> for Item {
            type Value = TranscriptItem;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a tagged transcript item starting with type")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let tag = read_record_type(&mut map)?;
                TranscriptItem::read_payload(tag, map)
            }
        }
        deserializer.deserialize_map(Item)
    }
}

impl TranscriptItem {
    /// Dispatch directly to typed field deserializers. No flattening or Serde
    /// Content buffer at the record boundary, including for batch children.
    fn read_payload<'de, A: MapAccess<'de>>(tag: RecordType, map: A) -> Result<Self, A::Error> {
        let fields = MapAccessDeserializer::new(map);
        macro_rules! field {
            ($key:ident: $ty:ty $(, $decode:literal)?) => {{
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields {
                    $(#[serde(deserialize_with = $decode)])?
                    $key: $ty,
                }
                Fields::deserialize(fields)?.$key
            }};
        }
        Ok(match tag {
            RecordType::Batch => {
                return Err(A::Error::custom(
                    "nested transcript batches are not supported",
                ));
            }
            RecordType::SessionModels => Self::SessionModels(field!(models: Box<SessionModels>)),
            RecordType::SessionMode => Self::SessionMode(field!(selected: crate::SessionMode)),
            RecordType::Message => Self::Message(field!(message: Message, "read_message")),
            RecordType::RequestPrompt => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields {
                    #[serde(deserialize_with = "read_prompt_message")]
                    message: Message,
                    request: RequestMetadata,
                }
                let Fields { message, request } = Fields::deserialize(fields)?;
                Self::RequestPrompt { message, request }
            }
            RecordType::ProviderMessage => {
                Self::ProviderMessage(field!(replay: ReplayBackedMessage, "read_replay"))
            }
            RecordType::ToolResults => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields {
                    #[serde(deserialize_with = "read_tool_result_message")]
                    message: Message,
                    metadata: Vec<ToolResultMetadata>,
                    #[serde(default)]
                    skill_applications: Vec<SkillToolApplication>,
                }
                let Fields {
                    message,
                    metadata,
                    skill_applications,
                } = Fields::deserialize(fields)?;
                Self::ToolResults {
                    message,
                    metadata,
                    skill_applications,
                }
            }
            RecordType::SkillInvocation => {
                Self::SkillInvocation(field!(invocation: Box<SkillInvocation>))
            }
            RecordType::Directive => {
                Self::Directive(field!(directive: Box<crate::DirectiveContent>, "read_directive"))
            }
            RecordType::RequestDirective => Self::RequestDirective(
                field!(directive: Box<zevria_instructions::RequestDirective>, "read_request_directive"),
            ),
            RecordType::ToolDeclarations => {
                Self::ToolDeclarations(field!(declarations: Box<ToolDeclarationRecord>))
            }
            RecordType::CommandJobNotice => {
                Self::CommandJobNotice(field!(notice: Box<CommandJobNotice>))
            }
            RecordType::Plan => Self::Plan(field!(plan: Box<PlanRecord>)),
            RecordType::Ensemble => {
                Self::Ensemble(field!(ensemble: Box<EnsembleRecord>, "read_ensemble"))
            }
            RecordType::Compaction => {
                Self::Compaction(field!(checkpoint: Box<CompactionCheckpoint>))
            }
            RecordType::IncompleteResponse => Self::IncompleteResponse(
                field!(attempt: Box<crate::IncompleteResponse>, "read_incomplete_response"),
            ),
            RecordType::Error => Self::Error {
                error: field!(error: String),
            },
        })
    }
}

fn read_message<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Message, D::Error> {
    let message = Message::deserialize(deserializer)?;
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

fn read_prompt_message<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Message, D::Error> {
    let message = Message::deserialize(deserializer)?;
    crate::UserPrompt::from_message(&message).map_err(D::Error::custom)?;
    Ok(message)
}

fn read_tool_result_message<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Message, D::Error> {
    let message = Message::deserialize(deserializer)?;
    validate_tool_result_message(&message).map_err(D::Error::custom)?;
    Ok(message)
}

fn read_replay<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ReplayBackedMessage, D::Error> {
    let replay = ProviderReplay::deserialize(deserializer)?;
    let content = crate::ReplayMessage::new(replay).map_err(D::Error::custom)?;
    Ok(ReplayBackedMessage {
        content: content.into(),
    })
}

fn read_directive<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<crate::DirectiveContent>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DirectiveRecord {
        version: u32,
        payload: crate::DirectivePayload,
    }
    let DirectiveRecord { version, payload } = DirectiveRecord::deserialize(deserializer)?;
    if version != zevria_instructions::directive::INSTRUCTION_VERSION {
        return Err(D::Error::custom(
            "unsupported directive version; start a fresh session",
        ));
    }
    crate::DirectiveContent::new(payload)
        .map(Box::new)
        .map_err(D::Error::custom)
}

fn read_request_directive<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<zevria_instructions::RequestDirective>, D::Error> {
    let directive = Box::<zevria_instructions::RequestDirective>::deserialize(deserializer)?;
    directive.validate().map_err(D::Error::custom)?;
    Ok(directive)
}

fn read_ensemble<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<EnsembleRecord>, D::Error> {
    let record = Box::<EnsembleRecord>::deserialize(deserializer)?;
    if matches!(record.as_ref(), EnsembleRecord::ReviewStarted { version, .. } if *version != zevria_workflow::ENSEMBLE_REVIEW_VERSION)
    {
        return Err(D::Error::custom(
            "unsupported ensemble review version; expected v1",
        ));
    }
    Ok(record)
}

fn read_incomplete_response<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<crate::IncompleteResponse>, D::Error> {
    let record = Box::<crate::IncompleteResponse>::deserialize(deserializer)?;
    record.validate().map_err(D::Error::custom)?;
    Ok(record)
}

impl From<MessageRecord> for TranscriptItem {
    fn from(record: MessageRecord) -> Self {
        record.into_parts(Self::Message, |content| {
            Self::ProviderMessage(ReplayBackedMessage {
                content: content.into(),
            })
        })
    }
}

impl TranscriptItem {
    pub fn plan_record(&self) -> Option<&PlanRecord> {
        match self {
            Self::Plan(record) => Some(record),
            _ => None,
        }
    }

    pub fn ensemble_record(&self) -> Option<&EnsembleRecord> {
        match self {
            Self::Ensemble(record) => Some(record),
            _ => None,
        }
    }

    /// Build a provider response whose cached message is derived from replay.
    pub fn provider_message(replay: ProviderReplay) -> anyhow::Result<Self> {
        Ok(Self::ProviderMessage(ReplayBackedMessage {
            content: crate::ReplayMessage::new(replay)?.into(),
        }))
    }

    /// The ordinary Rig message represented by this item, if any.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(message)
            | Self::RequestPrompt { message, .. }
            | Self::ToolResults { message, .. } => Some(message),
            Self::SkillInvocation(invocation) => Some(invocation.model_message()),
            Self::CommandJobNotice(notice) => Some(notice.model_message()),
            Self::ProviderMessage(provider_message) => Some(provider_message.content.message()),
            Self::Plan(record) => record.model_message(),
            Self::Ensemble(record) => record.model_message(),
            Self::SessionModels(_)
            | Self::SessionMode(_)
            | Self::Directive(_)
            | Self::RequestDirective(_)
            | Self::ToolDeclarations(_)
            | Self::Compaction(_)
            | Self::IncompleteResponse(_)
            | Self::Error { .. } => None,
        }
    }

    /// Frontend-only projection; request prefixes never enter model input.
    pub fn display_message(&self) -> Option<Message> {
        if let Self::RequestPrompt { message, request } = self {
            let prompt = zevria_content::UserPrompt::from_message(message).ok()?;
            return Some(match request.behavior {
                zevria_foundation::RequestBehavior::Orchestrate => {
                    prompt.with_prefix("/orchestrate ").to_message()
                }
                zevria_foundation::RequestBehavior::Standard if matches!(prompt.blocks().first(), Some(zevria_content::PromptBlock::Text(text)) if text.starts_with('/') || text.starts_with('$')) => {
                    prompt.with_prefix(" ").to_message()
                }
                _ => message.clone(),
            });
        }
        self.message().cloned()
    }

    /// Provider-native replay data associated with this message, if any.
    pub fn provider_replay(&self) -> Option<&ProviderReplay> {
        match self {
            Self::ProviderMessage(provider_message) => Some(provider_message.content.replay()),
            Self::Message(_)
            | Self::RequestPrompt { .. }
            | Self::ToolResults { .. }
            | Self::SessionModels(_)
            | Self::SessionMode(_)
            | Self::Directive(_)
            | Self::RequestDirective(_)
            | Self::SkillInvocation(_)
            | Self::CommandJobNotice(_)
            | Self::ToolDeclarations(_)
            | Self::Plan(_)
            | Self::Ensemble(_)
            | Self::Compaction(_)
            | Self::IncompleteResponse(_)
            | Self::Error { .. } => None,
        }
    }

    /// Exact model-request representation for an ordinary transcript item.
    /// Checkpoints themselves are expanded by [`Conversation::model_input`].
    pub fn model_request_item(&self) -> Option<ModelRequestItem<'_>> {
        match self {
            Self::ToolDeclarations(record) => match record.placement {
                ToolDeclarationPlacement::Initial => None,
                ToolDeclarationPlacement::Additional => {
                    Some(ModelRequestItem::AdditionalTools(&record.tools))
                }
            },
            Self::Directive(directive) => Some(ModelRequestItem::DeveloperInstruction(directive)),
            Self::RequestDirective(directive) => {
                Some(ModelRequestItem::RequestInstruction(directive))
            }
            Self::ProviderMessage(provider_message) => {
                Some(ModelRequestItem::replay_backed(&provider_message.content))
            }
            Self::Message(message)
            | Self::RequestPrompt { message, .. }
            | Self::ToolResults { message, .. } => Some(ModelRequestItem::message(message)),
            Self::SkillInvocation(invocation) => {
                Some(ModelRequestItem::message(invocation.model_message()))
            }
            Self::CommandJobNotice(notice) => {
                Some(ModelRequestItem::message(notice.model_message()))
            }
            Self::Plan(record) => record.model_message().map(ModelRequestItem::message),
            Self::Ensemble(record) => record.model_message().map(ModelRequestItem::message),
            Self::SessionModels(_)
            | Self::SessionMode(_)
            | Self::Compaction(_)
            | Self::IncompleteResponse(_)
            | Self::Error { .. } => None,
        }
    }
}

#[cfg(test)]
#[path = "item_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "record.rs"]
mod record;
