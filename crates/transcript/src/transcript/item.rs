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
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;
use zevria_foundation::CommandJobNotice;
use zevria_model::MessageRecord;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

pub(super) const TOOL_RESULT_METADATA_KEY: &str = "zevria_tool_result_metadata";
pub(super) const SUBTASK_RESULTS_KEY: &str = "zevria_subtask_results";
pub(super) const SKILL_INVOCATION_KEY: &str = "zevria_skill_invocation";
pub(super) const COMMAND_JOB_NOTICE_KEY: &str = "zevria_command_job_notice";
pub(super) const TOOL_DECLARATIONS_KEY: &str = "zevria_tool_declarations";
pub use crate::{ToolDeclarationPlacement, ToolDeclarationRecord, ToolWindow, tool_window};
pub(super) const SKILL_APPLICATIONS_KEY: &str = "zevria_skill_applications";
pub(super) const SKILL_DIRECTIVE_KEY: &str = "zevria_skill_directive";
pub(super) const REQUEST_METADATA_KEY: &str = "zevria_request";
pub(super) const REQUEST_DIRECTIVE_KEY: &str = "zevria_request_directive";
pub use crate::replay_active_skills;
/// Allows the maximum 20 MiB image prompt, typed control records,
/// base64 expansion, and bounded protocol envelopes.
pub const MAX_ROOT_RECORD_BYTES: usize = 64 * 1024 * 1024;
pub(super) const PROVIDER_REPLAY_KEY: &str = "zevria_provider_replay";
pub(super) const PLAN_RECORD_KEY: &str = "zevria_plan";
pub(super) const ENSEMBLE_RECORD_KEY: &str = "zevria_ensemble";
pub(super) const COMPACTION_RECORD_KEY: &str = "zevria_compaction";
pub(super) const SESSION_MODELS_KEY: &str = "zevria_session_models";
pub(super) const SESSION_MODE_KEY: &str = "zevria_session_mode";
// Retired envelopes: recognized only to reject unsupported complete or torn records.
pub(super) const INSTRUCTION_PREFIX_KEY: &str = "zevria_instruction_prefix";
pub(super) const DIRECTIVE_KEY: &str = "zevria_directive";
pub(super) const INCOMPLETE_RESPONSE_KEY: &str = "zevria_incomplete_response";
pub(super) const BATCH_KEY: &str = "zevria_batch";

/// A single physical commit boundary. Nested envelopes are deliberately not
/// TranscriptItems, so batches cannot recursively contain batches.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BatchEnvelope {
    pub(super) zevria_batch: BatchRecord,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BatchRecord {
    pub(super) version: u32,
    pub(super) records: Vec<TranscriptItem>,
}

// The hot single-record decode stays inline instead of allocating one box per line.
#[allow(clippy::large_enum_variant)]
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum TranscriptLine {
    Batch(BatchEnvelope),
    Item(TranscriptItem),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionModelsEnvelope {
    zevria_session_models: SessionModels,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionModeEnvelope {
    zevria_session_mode: SessionModeRecord,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionModeRecord {
    version: u32,
    #[serde(deserialize_with = "deserialize_selected_mode")]
    selected: crate::SessionMode,
}

fn deserialize_selected_mode<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<crate::SessionMode, D::Error> {
    let selected = String::deserialize(deserializer)?;
    if selected == "orchestrate" {
        return Err(D::Error::custom(
            "legacy Orchestrate mode is no longer supported; start a fresh Build session and submit /orchestrate <prompt>; the saved transcript has not been modified",
        ));
    }
    serde_json::from_value(serde_json::Value::String(selected)).map_err(D::Error::custom)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillDirectiveRecord {
    version: u32,
    payload: crate::DirectivePayload,
}

/// Decode an owned envelope without discarding duplicate authoritative fields.
/// Nested provider-native JSON remains uninterpreted. Semantic validation follows
/// in the contract's typed decoder, without deriving replay messages twice.
struct UniqueEnvelope(serde_json::Value);
impl<'de> Deserialize<'de> for UniqueEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = UniqueEnvelope;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an owned envelope with unique fields")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut object = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if object.contains_key(&key) {
                        return Err(A::Error::custom("duplicate owned envelope field"));
                    }
                    object.insert(key, map.next_value::<serde_json::Value>()?);
                }
                Ok(UniqueEnvelope(serde_json::Value::Object(object)))
            }
        }
        deserializer.deserialize_map(Fields)
    }
}

/// One conversation item, persisted as one JSONL record. Skill directives retain
/// their exact ordered model-input positions; invocations and tool applications
/// intentionally retain their full historical activation pins.
/// Removed reserved sidecars are rejected before ordinary message decoding.
#[derive(Debug, PartialEq)]
pub enum TranscriptItem {
    /// Model structure only: top-level window anchor or ordered inline additions.
    ToolDeclarations(ToolDeclarationRecord),
    /// Immutable display-only evidence of an abandoned response.
    IncompleteResponse(crate::IncompleteResponse),
    /// Canonical current Build/Plan selections, only at the start of a root log.
    /// Never conversation content, model input, or response provenance.
    SessionModels(SessionModels),
    /// Model-inert authoritative root selection after optional session models.
    SessionMode(crate::SessionMode),
    /// Validated skill instructions, persisted without their derived rendered text.
    Directive(crate::DirectiveContent),
    RequestDirective(zevria_instructions::RequestDirective),
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
    SkillInvocation(SkillInvocation),
    /// Append-only untrusted command data with verbatim model-visible text.
    CommandJobNotice(CommandJobNotice),
    /// Engine-owned Plan workflow state. Only `PlanRecord::Handoff` projects
    /// a message into model history; every other record is metadata-only.
    Plan(PlanRecord),
    /// Engine-owned ensemble workflow state. Only `ReportsReady` projects its
    /// bounded, untrusted synthesis input into model history.
    Ensemble(EnsembleRecord),
    /// A durable context checkpoint used only by model projection.
    Compaction(CompactionCheckpoint),
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
    content: crate::ReplayMessage,
}

#[derive(Serialize, Deserialize)]
struct ErrorRecord {
    error: String,
}

use zevria_foundation::tool_result::validate_tool_result_message;

impl Serialize for TranscriptItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        fn message_with_sidecars<S>(
            message: &Message,
            sidecars: &[(&str, serde_json::Value)],
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut value = serde_json::to_value(message).map_err(serde::ser::Error::custom)?;
            let object = value.as_object_mut().ok_or_else(|| {
                serde::ser::Error::custom("a Rig message must serialize as a JSON object")
            })?;
            for (key, sidecar) in sidecars {
                object.insert((*key).to_string(), sidecar.clone());
            }
            value.serialize(serializer)
        }

        match self {
            Self::ToolDeclarations(record) => {
                record.validate().map_err(serde::ser::Error::custom)?;
                serde_json::json!({TOOL_DECLARATIONS_KEY: record}).serialize(serializer)
            }
            Self::IncompleteResponse(attempt) => {
                attempt.validate().map_err(serde::ser::Error::custom)?;
                serde_json::json!({INCOMPLETE_RESPONSE_KEY: attempt}).serialize(serializer)
            }
            Self::SessionMode(mode) => {
                serde_json::json!({SESSION_MODE_KEY: {"version": 1, "selected": mode}})
                    .serialize(serializer)
            }
            Self::Directive(directive) => {
                directive.validate().map_err(serde::ser::Error::custom)?;
                serde_json::json!({SKILL_DIRECTIVE_KEY: SkillDirectiveRecord {
                    version: directive.version,
                    payload: directive.payload.clone(),
                }})
                .serialize(serializer)
            }
            Self::RequestDirective(directive) => {
                directive.validate().map_err(serde::ser::Error::custom)?;
                serde_json::json!({REQUEST_DIRECTIVE_KEY: directive}).serialize(serializer)
            }
            Self::RequestPrompt { message, request } => {
                request.validate().map_err(serde::ser::Error::custom)?;
                crate::UserPrompt::from_message(message).map_err(serde::ser::Error::custom)?;
                message_with_sidecars(
                    message,
                    &[(
                        REQUEST_METADATA_KEY,
                        serde_json::to_value(request).map_err(serde::ser::Error::custom)?,
                    )],
                    serializer,
                )
            }
            Self::SessionModels(models) => {
                let mut record = serde_json::Map::new();
                record.insert(
                    SESSION_MODELS_KEY.to_string(),
                    serde_json::to_value(models).map_err(serde::ser::Error::custom)?,
                );
                record.serialize(serializer)
            }
            Self::Message(message) => {
                if matches!(message, Message::System { .. }) {
                    return Err(serde::ser::Error::custom(
                        "raw system messages must be typed directives",
                    ));
                }
                message.serialize(serializer)
            }
            Self::ProviderMessage(provider_message) => {
                let mut record = serde_json::Map::new();
                record.insert(
                    PROVIDER_REPLAY_KEY.to_string(),
                    serde_json::to_value(provider_message.content.replay())
                        .map_err(serde::ser::Error::custom)?,
                );
                record.serialize(serializer)
            }
            Self::ToolResults {
                message,
                metadata,
                skill_applications,
            } => {
                validate_tool_result_message(message).map_err(serde::ser::Error::custom)?;
                let mut sidecars = vec![(
                    TOOL_RESULT_METADATA_KEY,
                    serde_json::to_value(metadata).map_err(serde::ser::Error::custom)?,
                )];
                if !skill_applications.is_empty() {
                    sidecars.push((
                        SKILL_APPLICATIONS_KEY,
                        serde_json::to_value(skill_applications)
                            .map_err(serde::ser::Error::custom)?,
                    ));
                }
                message_with_sidecars(message, &sidecars, serializer)
            }
            Self::SkillInvocation(invocation) => {
                serde_json::json!({SKILL_INVOCATION_KEY: invocation}).serialize(serializer)
            }
            Self::CommandJobNotice(notice) => {
                serde_json::json!({COMMAND_JOB_NOTICE_KEY: notice}).serialize(serializer)
            }
            Self::Plan(record) => {
                let mut value = serde_json::Map::new();
                value.insert(
                    PLAN_RECORD_KEY.to_string(),
                    serde_json::to_value(record).map_err(serde::ser::Error::custom)?,
                );
                value.serialize(serializer)
            }
            Self::Ensemble(record) => {
                let mut value = serde_json::Map::new();
                value.insert(
                    ENSEMBLE_RECORD_KEY.to_string(),
                    serde_json::to_value(record).map_err(serde::ser::Error::custom)?,
                );
                value.serialize(serializer)
            }
            Self::Compaction(checkpoint) => {
                let mut value = serde_json::Map::new();
                value.insert(
                    COMPACTION_RECORD_KEY.to_string(),
                    serde_json::to_value(checkpoint).map_err(serde::ser::Error::custom)?,
                );
                value.serialize(serializer)
            }
            Self::Error { error } => ErrorRecord {
                error: error.clone(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TranscriptItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Decode typed mode/display envelopes before materializing generic JSON,
        // which would otherwise silently discard duplicate authoritative fields.
        struct RecordValue;
        impl<'de> serde::de::Visitor<'de> for RecordValue {
            type Value = serde_json::Value;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a transcript record object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut object = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value = if key == SESSION_MODE_KEY {
                        let mode = map.next_value::<SessionModeRecord>()?;
                        if mode.version != 1 {
                            return Err(A::Error::custom(
                                "unsupported session mode version; expected 1",
                            ));
                        }
                        serde_json::json!({"version": mode.version, "selected": mode.selected})
                    } else if key == REQUEST_METADATA_KEY {
                        let request = map.next_value::<zevria_foundation::RequestMetadata>()?;
                        serde_json::to_value(request).map_err(A::Error::custom)?
                    } else if key == REQUEST_DIRECTIVE_KEY {
                        let directive =
                            map.next_value::<zevria_instructions::RequestDirective>()?;
                        directive.validate().map_err(A::Error::custom)?;
                        serde_json::to_value(directive).map_err(A::Error::custom)?
                    } else if key == TOOL_DECLARATIONS_KEY {
                        let record = map.next_value::<ToolDeclarationRecord>()?;
                        serde_json::to_value(record).map_err(A::Error::custom)?
                    } else if key == COMMAND_JOB_NOTICE_KEY {
                        let notice = map.next_value::<CommandJobNotice>()?;
                        serde_json::to_value(notice).map_err(A::Error::custom)?
                    } else if key == INCOMPLETE_RESPONSE_KEY {
                        let attempt = map.next_value::<crate::IncompleteResponse>()?;
                        attempt.validate().map_err(A::Error::custom)?;
                        serde_json::to_value(attempt).map_err(A::Error::custom)?
                    } else if matches!(
                        key.as_str(),
                        COMPACTION_RECORD_KEY
                            | PROVIDER_REPLAY_KEY
                            | SKILL_DIRECTIVE_KEY
                            | ENSEMBLE_RECORD_KEY
                            | SESSION_MODELS_KEY
                    ) {
                        map.next_value::<UniqueEnvelope>()?.0
                    } else {
                        map.next_value::<serde_json::Value>()?
                    };
                    if object.insert(key.clone(), value).is_some() && key.starts_with("zevria_") {
                        return Err(A::Error::custom("duplicate reserved transcript key"));
                    }
                }
                Ok(serde_json::Value::Object(object))
            }
        }
        let mut value = deserializer.deserialize_map(RecordValue)?;
        validate_reserved_lifecycle_keys(&value).map_err(D::Error::custom)?;
        if let Some(record) = value.get(TOOL_DECLARATIONS_KEY) {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(D::Error::custom(
                    "tool declarations must be a standalone record",
                ));
            }
            return serde_json::from_value(record.clone())
                .map(Self::ToolDeclarations)
                .map_err(D::Error::custom);
        }
        if let Some(directive) = value.get(REQUEST_DIRECTIVE_KEY) {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(D::Error::custom(
                    "request directive must be a standalone record",
                ));
            }
            let directive: zevria_instructions::RequestDirective =
                serde_json::from_value(directive.clone()).map_err(D::Error::custom)?;
            directive.validate().map_err(D::Error::custom)?;
            return Ok(Self::RequestDirective(directive));
        }
        if let Some(request) = value
            .as_object_mut()
            .and_then(|object| object.remove(REQUEST_METADATA_KEY))
        {
            if value
                .as_object()
                .is_some_and(|object| object.keys().any(|key| key.starts_with("zevria_")))
            {
                return Err(D::Error::custom(
                    "request metadata belongs only to an ordinary user prompt",
                ));
            }
            let request = serde_json::from_value(request).map_err(D::Error::custom)?;
            let message = serde_json::from_value(value).map_err(D::Error::custom)?;
            crate::UserPrompt::from_message(&message).map_err(D::Error::custom)?;
            return Ok(Self::RequestPrompt { message, request });
        }
        if let Some(attempt) = value.get(INCOMPLETE_RESPONSE_KEY) {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(D::Error::custom(
                    "incomplete response must be a standalone record",
                ));
            }
            let attempt: crate::IncompleteResponse =
                serde_json::from_value(attempt.clone()).map_err(D::Error::custom)?;
            attempt.validate().map_err(D::Error::custom)?;
            return Ok(Self::IncompleteResponse(attempt));
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(SESSION_MODE_KEY))
        {
            let envelope: SessionModeEnvelope =
                serde_json::from_value(value).map_err(D::Error::custom)?;
            if envelope.zevria_session_mode.version != 1 {
                return Err(D::Error::custom(
                    "unsupported session mode version; expected 1",
                ));
            }
            return Ok(Self::SessionMode(envelope.zevria_session_mode.selected));
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(SESSION_MODELS_KEY))
        {
            let envelope: SessionModelsEnvelope =
                serde_json::from_value(value).map_err(D::Error::custom)?;
            return Ok(Self::SessionModels(envelope.zevria_session_models));
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(SKILL_DIRECTIVE_KEY))
        {
            let object = value.as_object_mut().expect("checked as an object");
            if object.len() != 1 {
                return Err(D::Error::custom(
                    "a skill directive record must contain only `zevria_skill_directive`",
                ));
            }
            let record = object
                .remove(SKILL_DIRECTIVE_KEY)
                .expect("checked skill directive key");
            let record =
                serde_json::from_value::<SkillDirectiveRecord>(record).map_err(D::Error::custom)?;
            if record.version != zevria_instructions::directive::INSTRUCTION_VERSION {
                return Err(D::Error::custom(
                    "unsupported directive version; start a fresh session",
                ));
            }
            return crate::DirectiveContent::new(record.payload)
                .map(Self::Directive)
                .map_err(D::Error::custom);
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(COMPACTION_RECORD_KEY))
        {
            let object = value.as_object_mut().expect("checked as an object");
            if object.len() != 1 {
                return Err(D::Error::custom(
                    "a compaction transcript record must contain only `zevria_compaction`",
                ));
            }
            let checkpoint = object
                .remove(COMPACTION_RECORD_KEY)
                .expect("checked compaction record key");
            let checkpoint = serde_json::from_value::<CompactionCheckpoint>(checkpoint)
                .map_err(D::Error::custom)?;
            checkpoint.validate().map_err(D::Error::custom)?;
            return Ok(Self::Compaction(checkpoint));
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(PLAN_RECORD_KEY))
        {
            let object = value.as_object_mut().expect("checked as an object");
            if object.len() != 1 {
                return Err(D::Error::custom(
                    "a Plan transcript record must contain only `zevria_plan`",
                ));
            }
            let record = object
                .remove(PLAN_RECORD_KEY)
                .expect("checked Plan record key");
            return serde_json::from_value::<PlanRecord>(record)
                .map(Self::Plan)
                .map_err(D::Error::custom);
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(ENSEMBLE_RECORD_KEY))
        {
            let object = value.as_object_mut().expect("checked as an object");
            if object.len() != 1 {
                return Err(D::Error::custom(
                    "an ensemble transcript record must contain only `zevria_ensemble`",
                ));
            }
            let record = object
                .remove(ENSEMBLE_RECORD_KEY)
                .expect("checked ensemble record key");
            let record =
                serde_json::from_value::<EnsembleRecord>(record).map_err(D::Error::custom)?;
            if matches!(&record, EnsembleRecord::ReviewStarted { version, .. } if *version != zevria_workflow::ENSEMBLE_REVIEW_VERSION)
            {
                return Err(D::Error::custom(
                    "unsupported ensemble review version; expected v1",
                ));
            }
            return Ok(Self::Ensemble(record));
        }
        if value
            .as_object()
            .is_some_and(|object| object.contains_key(PROVIDER_REPLAY_KEY))
        {
            let object = value.as_object_mut().expect("checked as an object");
            if object.len() != 1 {
                return Err(D::Error::custom(
                    "a provider replay transcript record must contain only \
                     `zevria_provider_replay`",
                ));
            }
            let envelope = object
                .remove(PROVIDER_REPLAY_KEY)
                .expect("checked provider replay key");
            let replay =
                serde_json::from_value::<ProviderReplay>(envelope).map_err(D::Error::custom)?;
            return Self::provider_message(replay).map_err(D::Error::custom);
        }
        if let Some(metadata) = value
            .as_object_mut()
            .and_then(|object| object.remove(TOOL_RESULT_METADATA_KEY))
        {
            let metadata = serde_json::from_value(metadata).map_err(D::Error::custom)?;
            let applications = value
                .as_object_mut()
                .and_then(|object| object.remove(SKILL_APPLICATIONS_KEY));
            let skill_applications = applications
                .map(serde_json::from_value)
                .transpose()
                .map_err(D::Error::custom)?
                .unwrap_or_default();
            let message = serde_json::from_value(value).map_err(D::Error::custom)?;
            validate_tool_result_message(&message).map_err(D::Error::custom)?;
            return Ok(Self::ToolResults {
                message,
                metadata,
                skill_applications,
            });
        }
        if let Some(notice) = value.get(COMMAND_JOB_NOTICE_KEY) {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(D::Error::custom(
                    "a command job notice must be a standalone record",
                ));
            }
            return serde_json::from_value::<CommandJobNotice>(notice.clone())
                .map(Self::CommandJobNotice)
                .map_err(D::Error::custom);
        }
        if let Some(invocation) = value.get(SKILL_INVOCATION_KEY) {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(D::Error::custom(
                    "a skill invocation must be a dedicated record without a message mirror",
                ));
            }
            return serde_json::from_value::<SkillInvocation>(invocation.clone())
                .map(Self::SkillInvocation)
                .map_err(D::Error::custom);
        }
        if value.get("role").is_some() {
            let message: Message = serde_json::from_value(value).map_err(D::Error::custom)?;
            if matches!(message, Message::System { .. }) {
                return Err(D::Error::custom(
                    "raw system messages must be typed directives",
                ));
            }
            if crate::prompt::message_has_images(&message) {
                crate::UserPrompt::from_message(&message).map_err(D::Error::custom)?;
            }
            return Ok(Self::Message(message));
        }
        serde_json::from_value::<ErrorRecord>(value)
            .map(|record| Self::Error {
                error: record.error,
            })
            .map_err(D::Error::custom)
    }
}

impl From<MessageRecord> for TranscriptItem {
    fn from(record: MessageRecord) -> Self {
        record.into_parts(Self::Message, |content| {
            Self::ProviderMessage(ReplayBackedMessage { content })
        })
    }
}

impl TranscriptItem {
    /// Build a provider response whose cached message is derived from replay.
    pub fn provider_message(replay: ProviderReplay) -> anyhow::Result<Self> {
        Ok(Self::ProviderMessage(ReplayBackedMessage {
            content: crate::ReplayMessage::new(replay)?,
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

pub(super) fn validate_reserved_lifecycle_keys(value: &serde_json::Value) -> anyhow::Result<()> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    anyhow::ensure!(
        !object.contains_key(SUBTASK_RESULTS_KEY),
        "removed subtask sidecar; expected ordinary correlated tool results"
    );
    for key in object.keys() {
        anyhow::ensure!(
            !key.starts_with("zevria_") || current_reserved_key(key),
            "unsupported reserved record key; expected current transcript envelope"
        );
        anyhow::ensure!(
            !key.starts_with(SESSION_MODELS_KEY) || key == SESSION_MODELS_KEY,
            "unsupported reserved session models key {key:?}"
        );
        if key.starts_with("zevria_skill_")
            && key != SKILL_APPLICATIONS_KEY
            && key != SKILL_INVOCATION_KEY
            && key != SKILL_DIRECTIVE_KEY
        {
            anyhow::bail!(
                "unsupported reserved skill lifecycle key {:?}",
                key.chars().take(128).collect::<String>()
            );
        }
    }
    if object.contains_key(TOOL_DECLARATIONS_KEY) {
        anyhow::ensure!(
            object.len() == 1,
            "tool declarations must be a standalone record"
        );
    }
    if object.contains_key(COMMAND_JOB_NOTICE_KEY) {
        anyhow::ensure!(
            object.len() == 1,
            "a command job notice must be a standalone record"
        );
    }
    if object.contains_key(SKILL_DIRECTIVE_KEY) {
        anyhow::ensure!(
            object.len() == 1,
            "a skill directive record must contain only `zevria_skill_directive`"
        );
    }
    if object.contains_key(SKILL_APPLICATIONS_KEY) {
        anyhow::ensure!(
            object.contains_key(TOOL_RESULT_METADATA_KEY),
            "skill applications require tool result metadata"
        );
    }
    if object.contains_key(SKILL_INVOCATION_KEY) {
        anyhow::ensure!(
            !object
                .keys()
                .any(|key| key.starts_with("zevria_") && key != SKILL_INVOCATION_KEY),
            "a skill invocation cannot be combined with another reserved sidecar"
        );
    }
    Ok(())
}

pub(super) fn current_reserved_key(key: &str) -> bool {
    matches!(
        key,
        BATCH_KEY
            | SKILL_APPLICATIONS_KEY
            | TOOL_DECLARATIONS_KEY
            | COMMAND_JOB_NOTICE_KEY
            | SKILL_INVOCATION_KEY
            | SKILL_DIRECTIVE_KEY
            | TOOL_RESULT_METADATA_KEY
            | COMPACTION_RECORD_KEY
            | PROVIDER_REPLAY_KEY
            | PLAN_RECORD_KEY
            | ENSEMBLE_RECORD_KEY
            | SESSION_MODELS_KEY
            | SESSION_MODE_KEY
            | INCOMPLETE_RESPONSE_KEY
            | REQUEST_METADATA_KEY
            | REQUEST_DIRECTIVE_KEY
    )
}

#[cfg(test)]
#[path = "item_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "record.rs"]
mod record;
