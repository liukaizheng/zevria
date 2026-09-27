//! JSONL persistence for conversation transcripts.
//!
//! Every conversation owns a session id; its items are appended live to
//! `{workspace}/.zevria/sessions/{id}.jsonl`, one JSON object per line.
//! Ordinary messages keep their bare wire shape (`{"role": ...}`), compound
//! records add reserved metadata to that same object, provider responses store
//! only their replay envelope, and turn failures are stored as
//! `{"error": ...}`. Each line remains both a resumable history record and a
//! readable log.

#[cfg(test)]
mod record;
use zevria_model::MessageRecord;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rig_core::message::{AssistantContent, Message, ProviderCallId, ToolResult, UserContent};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use uuid::Uuid;

use crate::instruction_replay::SkillLifecycleLocation;
use crate::models::SessionModels;
use crate::tool_result::ToolResultMetadata;
use crate::{
    ModelRequestItem, ProviderReplay,
    compaction::{CompactionCheckpoint, is_summary_message},
    skill::{SkillInvocation, SkillToolApplication},
    task::{TASK_TOOL_NAME, TaskList},
};
use crate::{
    ensemble::{EnsembleRecord, EnsembleRunId},
    plan::PlanRecord,
};

const TOOL_RESULT_METADATA_KEY: &str = "zevria_tool_result_metadata";
const SUBTASK_RESULTS_KEY: &str = "zevria_subtask_results";
const SKILL_INVOCATION_KEY: &str = "zevria_skill_invocation";
const SKILL_APPLICATIONS_KEY: &str = "zevria_skill_applications";
const SKILL_DIRECTIVE_KEY: &str = "zevria_skill_directive";
const REQUEST_METADATA_KEY: &str = "zevria_request";
const REQUEST_DIRECTIVE_KEY: &str = "zevria_request_directive";
pub use crate::replay_active_skills;
/// Allows the maximum 20 MiB image prompt, typed control records,
/// base64 expansion, and bounded protocol envelopes.
pub const MAX_ROOT_RECORD_BYTES: usize = 64 * 1024 * 1024;
const PROVIDER_REPLAY_KEY: &str = "zevria_provider_replay";
const PLAN_RECORD_KEY: &str = "zevria_plan";
const ENSEMBLE_RECORD_KEY: &str = "zevria_ensemble";
const COMPACTION_RECORD_KEY: &str = "zevria_compaction";
const SESSION_MODELS_KEY: &str = "zevria_session_models";
const SESSION_MODE_KEY: &str = "zevria_session_mode";
// Retired envelopes: recognized only to reject unsupported complete or torn records.
const INSTRUCTION_PREFIX_KEY: &str = "zevria_instruction_prefix";
const DIRECTIVE_KEY: &str = "zevria_directive";
const WEB_SEARCH_KEY: &str = "zevria_web_search_attempt";
const DISPLAY_ATTEMPT_KEY: &str = "zevria_display_attempt";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionModelsEnvelope {
    zevria_session_models: SessionModels,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionModeEnvelope {
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
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    /// Versioned display-only activity, flushed at model-call exit.
    WebSearchAttempt(crate::WebSearchAttemptRecord),
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
    AssistantMessage {
        message: Message,
        display_attempt_id: String,
    },
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

/// The inseparable native and provider-neutral views of one provider response.
///
/// The private content shares [`crate::ReplayMessage`]'s trusted-pair invariant.
/// Construction derives it from replay once or moves it from a completed
/// [`MessageRecord`]; display metadata never belongs to the native envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayBackedMessage {
    content: crate::ReplayMessage,
    display_attempt_id: Option<String>,
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
            Self::WebSearchAttempt(attempt) => {
                attempt.validate().map_err(serde::ser::Error::custom)?;
                serde_json::json!({WEB_SEARCH_KEY: attempt}).serialize(serializer)
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
            Self::AssistantMessage {
                message,
                display_attempt_id,
            } => {
                if !matches!(message, Message::Assistant { .. }) {
                    return Err(serde::ser::Error::custom(
                        "display binding requires an assistant record",
                    ));
                }
                crate::web_search::validate_display_id(display_attempt_id)
                    .map_err(serde::ser::Error::custom)?;
                message_with_sidecars(
                    message,
                    &[(DISPLAY_ATTEMPT_KEY, serde_json::json!(display_attempt_id))],
                    serializer,
                )
            }
            Self::ProviderMessage(provider_message) => {
                let mut record = serde_json::Map::new();
                record.insert(
                    PROVIDER_REPLAY_KEY.to_string(),
                    serde_json::to_value(provider_message.content.replay())
                        .map_err(serde::ser::Error::custom)?,
                );
                if let Some(id) = &provider_message.display_attempt_id {
                    record.insert(DISPLAY_ATTEMPT_KEY.into(), serde_json::json!(id));
                }
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
                    } else if key == WEB_SEARCH_KEY {
                        let attempt = map.next_value::<crate::WebSearchAttemptRecord>()?;
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
        let display_attempt_id = value
            .as_object_mut()
            .and_then(|object| object.remove(DISPLAY_ATTEMPT_KEY))
            .map(|id| serde_json::from_value::<String>(id).map_err(D::Error::custom))
            .transpose()?;
        if let Some(id) = &display_attempt_id {
            crate::web_search::validate_display_id(id).map_err(D::Error::custom)?;
            if !value.get(PROVIDER_REPLAY_KEY).is_some()
                && value.get("role").and_then(serde_json::Value::as_str) != Some("assistant")
            {
                return Err(D::Error::custom(
                    "display binding requires an assistant record",
                ));
            }
        }
        if let Some(attempt) = value.get(WEB_SEARCH_KEY) {
            if value.as_object().is_none_or(|object| object.len() != 1) {
                return Err(D::Error::custom(
                    "web search attempt must be a standalone record",
                ));
            }
            let mut attempt: crate::WebSearchAttemptRecord =
                serde_json::from_value(attempt.clone()).map_err(D::Error::custom)?;
            attempt.validate().map_err(D::Error::custom)?;
            attempt.finish(crate::WebSearchAttemptOutcome::Interrupted);
            return Ok(Self::WebSearchAttempt(attempt));
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
            return Self::provider_message(replay)
                .and_then(|item| item.with_display_attempt(display_attempt_id))
                .map_err(D::Error::custom);
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
            return Self::Message(message)
                .with_display_attempt(display_attempt_id)
                .map_err(D::Error::custom);
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
        record.into_parts(
            |message, display_attempt_id| match display_attempt_id {
                Some(display_attempt_id) => Self::AssistantMessage {
                    message,
                    display_attempt_id,
                },
                None => Self::Message(message),
            },
            |content, display_attempt_id| {
                Self::ProviderMessage(ReplayBackedMessage {
                    content,
                    display_attempt_id,
                })
            },
        )
    }
}

impl TranscriptItem {
    /// Build a provider response whose cached message is derived from replay.
    pub fn provider_message(replay: ProviderReplay) -> anyhow::Result<Self> {
        Ok(Self::ProviderMessage(ReplayBackedMessage {
            content: crate::ReplayMessage::new(replay)?,
            display_attempt_id: None,
        }))
    }

    pub fn display_attempt_id(&self) -> Option<&str> {
        match self {
            Self::ProviderMessage(item) => item.display_attempt_id.as_deref(),
            Self::AssistantMessage {
                display_attempt_id, ..
            } => Some(display_attempt_id),
            _ => None,
        }
    }

    pub fn with_display_attempt(self, id: Option<String>) -> anyhow::Result<Self> {
        let Some(id) = id else {
            return Ok(self);
        };
        crate::web_search::validate_display_id(&id)?;
        match self {
            Self::ProviderMessage(mut item) => {
                item.display_attempt_id = Some(id);
                Ok(Self::ProviderMessage(item))
            }
            Self::Message(message @ Message::Assistant { .. })
            | Self::AssistantMessage {
                message: message @ Message::Assistant { .. },
                ..
            } => Ok(Self::AssistantMessage {
                message,
                display_attempt_id: id,
            }),
            _ => anyhow::bail!("display binding requires an assistant record"),
        }
    }

    /// The ordinary Rig message represented by this item, if any.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Message(message)
            | Self::RequestPrompt { message, .. }
            | Self::AssistantMessage { message, .. }
            | Self::ToolResults { message, .. } => Some(message),
            Self::SkillInvocation(invocation) => Some(invocation.model_message()),
            Self::ProviderMessage(provider_message) => Some(provider_message.content.message()),
            Self::Plan(record) => record.model_message(),
            Self::Ensemble(record) => record.model_message(),
            Self::SessionModels(_)
            | Self::SessionMode(_)
            | Self::Directive(_)
            | Self::RequestDirective(_)
            | Self::Compaction(_)
            | Self::WebSearchAttempt(_)
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
            | Self::AssistantMessage { .. }
            | Self::ToolResults { .. }
            | Self::SessionModels(_)
            | Self::SessionMode(_)
            | Self::Directive(_)
            | Self::RequestDirective(_)
            | Self::SkillInvocation(_)
            | Self::Plan(_)
            | Self::Ensemble(_)
            | Self::Compaction(_)
            | Self::WebSearchAttempt(_)
            | Self::Error { .. } => None,
        }
    }

    /// Exact model-request representation for an ordinary transcript item.
    /// Checkpoints themselves are expanded by [`Conversation::model_input`].
    pub fn model_request_item(&self) -> Option<ModelRequestItem<'_>> {
        match self {
            Self::Directive(directive) => Some(ModelRequestItem::DeveloperInstruction(directive)),
            Self::RequestDirective(directive) => {
                Some(ModelRequestItem::RequestInstruction(directive))
            }
            Self::ProviderMessage(provider_message) => {
                Some(ModelRequestItem::replay_backed(&provider_message.content))
            }
            Self::Message(message)
            | Self::RequestPrompt { message, .. }
            | Self::AssistantMessage { message, .. }
            | Self::ToolResults { message, .. } => Some(ModelRequestItem::message(message)),
            Self::SkillInvocation(invocation) => {
                Some(ModelRequestItem::message(invocation.model_message()))
            }
            Self::Plan(record) => record.model_message().map(ModelRequestItem::message),
            Self::Ensemble(record) => record.model_message().map(ModelRequestItem::message),
            Self::SessionModels(_)
            | Self::SessionMode(_)
            | Self::Compaction(_)
            | Self::WebSearchAttempt(_)
            | Self::Error { .. } => None,
        }
    }
}

/// The sessions directory of a workspace: `{workspace}/.zevria/sessions`.
pub fn sessions_dir(workspace: &Path) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace).join("sessions")
}

/// The subsession directory holding one root session's child transcripts:
/// `{workspace}/.zevria/subsessions/{root_session_id}`. Living outside the
/// sessions directory keeps [`latest_session_file`] and `--continue` untouched
/// by child files.
pub fn subsessions_dir(workspace: &Path, root_session_id: &str) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace)
        .join("subsessions")
        .join(root_session_id)
}

/// The plans directory of a workspace: `{workspace}/.zevria/plans`. Submitted
/// artifacts are projected here by the session engine; transcripts remain
/// canonical.
pub fn plans_dir(workspace: &Path) -> PathBuf {
    zevria_foundation::runtime_paths::workspace_state_root(workspace).join("plans")
}

/// Pick a fresh session id without touching the filesystem, so composition
/// can name a session before its transcript file exists.
pub fn pick_session_id() -> String {
    Uuid::new_v4().to_string()
}

/// Discover the child transcripts recorded under a root's subsession
/// directory as `(child_id, path)` pairs, ordered by modification time. A
/// missing directory simply means the root never launched a subtask.
pub fn subsession_files(subsessions_dir: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    Ok(session_files_by_mtime(subsessions_dir)?
        .into_iter()
        .map(|(_, session_id, path)| (session_id, path))
        .collect())
}

/// The `.jsonl` transcript files in one directory as
/// `(modified, session_id, path)` triples, ordered oldest-first. A missing
/// directory simply means no sessions were recorded there.
fn session_files_by_mtime(
    directory: &Path,
) -> anyhow::Result<Vec<(std::time::SystemTime, String, PathBuf)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(anyhow::Error::from(error).context(format!(
                "failed to list the session files in {}",
                directory.display()
            )));
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| {
            format!(
                "failed to list the session files in {}",
                directory.display()
            )
        })?;
        let path = entry.path();
        let is_session_file = entry.file_type().is_ok_and(|kind| kind.is_file())
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl");
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !is_session_file {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("failed to read the metadata of {}", path.display()))?;
        files.push((modified, stem.to_string(), path));
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    Ok(files)
}

/// One resumable session discovered on disk, with the metadata a picker
/// displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    /// The session's first user prompt: one line, truncated for display.
    pub preview: Option<String>,
}

/// How many leading transcript lines [`list_sessions`] scans for a preview.
const PREVIEW_SCAN_LINES: usize = 16;
/// Character cap for a session preview line.
const PREVIEW_MAX_CHARS: usize = 80;

/// Every resumable session in the directory, newest-first. Empty files and
/// legacy models-plus-prefix-only roots are skipped. A saved mode is meaningful
/// session state even without a message and must remain reopenable.
pub fn list_sessions(sessions_dir: &Path) -> anyhow::Result<Vec<SessionSummary>> {
    let mut summaries = Vec::new();
    for (modified, session_id, path) in session_files_by_mtime(sessions_dir)?.into_iter().rev() {
        if is_abandoned_root(&path) {
            continue;
        }
        let preview = session_preview(&path);
        summaries.push(SessionSummary {
            id: session_id,
            path,
            modified,
            preview,
        });
    }
    Ok(summaries)
}

fn bounded_record_lines<R: std::io::BufRead>(
    mut reader: R,
) -> impl Iterator<Item = std::io::Result<Vec<u8>>> {
    use std::io::{BufRead as _, Read as _};
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut line = Vec::new();
        match (&mut reader)
            .take(MAX_ROOT_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => {
                done = true;
                None
            }
            Ok(_) if line.len() > MAX_ROOT_RECORD_BYTES => {
                done = true;
                Some(Err(std::io::Error::other(
                    "root record exceeds the 64 MiB limit",
                )))
            }
            Ok(_) => Some(Ok(line)),
            Err(error) => {
                done = true;
                Some(Err(error))
            }
        }
    })
}

/// The first plain user prompt within the file's leading lines, flattened to
/// one bounded display line. Unreadable, malformed, or absent content simply
/// yields no preview — a session must remain listable regardless.
fn session_preview(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in bounded_record_lines(reader).take(PREVIEW_SCAN_LINES) {
        let line = String::from_utf8(line.ok()?).ok()?;
        if line.trim().is_empty() {
            continue;
        }
        // Only user prompts qualify: compound tool-result and subtask-result
        // records are engine traffic, not the user's prompt. A direct skill
        // invocation previews as its compact `$name` form.
        let value: serde_json::Value = serde_json::from_str(&line).ok()?;
        // Picker previews never instantiate a validated image or decode pixels.
        if let Some(start) = value.get(ENSEMBLE_RECORD_KEY).and_then(|e| e.get("start")) {
            let workflow: crate::EnsembleWorkflow =
                serde_json::from_value(start.get("workflow")?.clone()).ok()?;
            let prefix = workflow.slash_command();
            return Some(preview_line(&format!(
                "{prefix} {}",
                preview_blocks(start.get("prompt")?, true)
            )));
        }
        if let Some(invocation) = value.get(SKILL_INVOCATION_KEY) {
            return Some(preview_line(&format!(
                "${} {}",
                invocation.get("name")?.as_str()?,
                preview_blocks(invocation.get("arguments")?, true)
            )));
        }
        if value.get("role").and_then(serde_json::Value::as_str) == Some("user")
            && !value.as_object()?.contains_key(TOOL_RESULT_METADATA_KEY)
            && !value.as_object()?.contains_key(SUBTASK_RESULTS_KEY)
        {
            let preview = preview_blocks(value.get("content")?, false);
            if !preview.is_empty() {
                return Some(preview_line(&preview));
            }
        }
    }
    None
}

fn preview_blocks(value: &serde_json::Value, typed: bool) -> String {
    let mut preview = String::new();
    if let Some(blocks) = value.as_array() {
        for block in blocks {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    if let Some(text) = block
                        .get(if typed { "value" } else { "text" })
                        .and_then(serde_json::Value::as_str)
                    {
                        preview.extend(text.chars().take(PREVIEW_MAX_CHARS + 1));
                    }
                }
                Some("image") => preview.push_str("[image]"),
                _ => {}
            }
            if preview.chars().count() > PREVIEW_MAX_CHARS {
                break;
            }
        }
    }
    preview
}

/// Flatten prompt text to its first line, char-truncated with an ellipsis.
fn preview_line(prompt_text: &str) -> String {
    let first_line = prompt_text.lines().next().unwrap_or_default();
    let mut preview: String = first_line.chars().take(PREVIEW_MAX_CHARS).collect();
    if first_line.chars().count() > PREVIEW_MAX_CHARS {
        preview.push('…');
    }
    preview
}

/// Appends transcript items to a session's `{id}.jsonl` file.
pub struct TranscriptWriter {
    session_id: String,
    file: std::fs::File,
    path: PathBuf,
    recovered_malformed_lines: usize,
    read_only: bool,
    needs_newline: bool,
}

impl TranscriptWriter {
    /// Start a new session: pick a fresh id and create its file.
    pub fn create(sessions_dir: &Path) -> anyhow::Result<Self> {
        // A v4 collision is practically impossible, but `create_new` keeps a
        // collision from silently appending to another session's file.
        for _ in 0..8 {
            if let Some(writer) = Self::try_create_with_id(sessions_dir, &pick_session_id())? {
                return Ok(writer);
            }
        }

        Err(anyhow::anyhow!(
            "failed to pick an unused session id in {}",
            sessions_dir.display()
        ))
    }

    /// Create the transcript file for an already-chosen session id, refusing
    /// to touch a file that exists.
    pub fn create_with_id(sessions_dir: &Path, session_id: &str) -> anyhow::Result<Self> {
        Self::try_create_with_id(sessions_dir, session_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "a session file for {session_id} already exists in {}",
                sessions_dir.display()
            )
        })
    }

    /// `Ok(None)` means a file for this id already exists.
    fn try_create_with_id(sessions_dir: &Path, session_id: &str) -> anyhow::Result<Option<Self>> {
        std::fs::create_dir_all(sessions_dir).with_context(|| {
            format!(
                "failed to create the sessions directory at {}",
                sessions_dir.display()
            )
        })?;

        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => Ok(Some(Self {
                session_id: session_id.to_string(),
                file,
                path,
                recovered_malformed_lines: 0,
                read_only: false,
                needs_newline: false,
            })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(anyhow::Error::from(error).context(format!(
                "failed to create the session file at {}",
                path.display()
            ))),
        }
    }

    /// Reopen an existing session file to keep appending to it.
    pub fn append_to(path: PathBuf) -> anyhow::Result<Self> {
        // Validate the complete record boundary and lifecycle before acquiring
        // any writable handle. In particular, never erase unsupported records.
        let outcome = load_report(&path)?;
        outcome.ensure_resumable()?;
        let mut writer = Self::open_existing(path, false, outcome.needs_newline)?;
        if outcome.recoverable_lines > 0 {
            writer.rewrite(&outcome.items)?;
            writer.recovered_malformed_lines = outcome.recoverable_lines;
        }
        Ok(writer)
    }

    /// Open only for inspection. Every mutation path fails, including repair
    /// and atomic replacement; the original bytes are never normalized.
    pub fn read_only(path: PathBuf) -> anyhow::Result<Self> {
        load_report(&path)?;
        Self::open_existing(path, true, false)
    }

    fn open_existing(path: PathBuf, read_only: bool, needs_newline: bool) -> anyhow::Result<Self> {
        let session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("the session file {} has no readable name", path.display()))?
            .to_string();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(!read_only)
            .open(&path)
            .with_context(|| format!("failed to open the session file at {}", path.display()))?;
        Ok(Self {
            session_id,
            file,
            path,
            recovered_malformed_lines: 0,
            read_only,
            needs_newline,
        })
    }

    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn ensure_writable(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.read_only,
            "the session transcript was opened read-only; mutations and repair are disabled"
        );
        Ok(())
    }

    /// Append an item as a JSON line and flush it to disk.
    pub fn append(&mut self, item: &TranscriptItem) -> anyhow::Result<()> {
        self.ensure_writable()?;
        if matches!(item, TranscriptItem::SessionModels(_)) {
            anyhow::ensure!(
                self.file.metadata()?.len() == 0,
                "session models must be a unique first record; replace the canonical header instead"
            );
        }
        if matches!(item, TranscriptItem::SessionMode(_)) {
            let mut proposed = load(&self.path)?;
            proposed.push(item.clone());
            session_mode(&proposed)?;
        }
        self.append_record(item)
    }

    // Only public checked append and the private validated transaction call this.
    fn append_record(&mut self, item: &TranscriptItem) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(item).context("failed to serialize a transcript item")?;
        anyhow::ensure!(
            line.len() < MAX_ROOT_RECORD_BYTES,
            "root transcript record exceeds the 64 MiB limit"
        );
        line.push(b'\n');
        if self.needs_newline {
            line.insert(0, b'\n');
        }
        self.file
            .write_all(&line)
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.sync_data())
            .with_context(|| {
                format!(
                    "failed to append to the session file at {}",
                    self.path.display()
                )
            })?;
        self.needs_newline = false;
        Ok(())
    }

    /// Replace the file's contents with these items, keeping the session id.
    /// Used when an edited message rewrites the conversation.
    ///
    /// The replacement is staged beside the session file and renamed over it,
    /// so a failure — or a crash — leaves the original log whole rather than
    /// half-rewritten. Callers may therefore treat an error as "nothing
    /// changed on disk".
    pub fn rewrite(&mut self, items: &[TranscriptItem]) -> anyhow::Result<()> {
        self.ensure_writable()?;
        validate_header(items.iter())?;
        self.rewrite_records(items)
    }

    // Private persistence primitive: callers either validate raw headers above,
    // or hold the exact immutable PreparedChange validated across all domains.
    fn rewrite_records(&mut self, items: &[TranscriptItem]) -> anyhow::Result<()> {
        let parent = self.path.parent().with_context(|| {
            format!(
                "session file {} has no parent directory",
                self.path.display()
            )
        })?;
        let mut staged = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "failed to create a staged session file beside {}",
                self.path.display()
            )
        })?;
        if let Ok(metadata) = self.file.metadata() {
            staged
                .as_file()
                .set_permissions(metadata.permissions())
                .with_context(|| {
                    format!("failed to preserve permissions for {}", self.path.display())
                })?;
        }
        write_items(staged.as_file_mut(), &self.path, items)?;
        staged.as_file().sync_all().with_context(|| {
            format!(
                "failed to sync the staged session file for {}",
                self.path.display()
            )
        })?;

        // Keep the staged handle open through the rename. Once the rename
        // succeeds this very handle names the replacement file, so there is
        // no fallible reopen step after the on-disk commit point.
        let (staged_file, staged_path) =
            staged
                .keep()
                .map_err(|error| error.error)
                .with_context(|| {
                    format!(
                        "failed to retain the staged session file for {}",
                        self.path.display()
                    )
                })?;
        if let Err(error) = std::fs::rename(&staged_path, &self.path) {
            let _ = std::fs::remove_file(&staged_path);
            return Err(anyhow::Error::from(error).context(format!(
                "failed to replace the session file at {}",
                self.path.display()
            )));
        }
        self.file = staged_file;
        self.needs_newline = false;
        if let Err(error) = sync_directory(parent) {
            // The replacement and writer handle already agree. Directory
            // fsync strengthens crash durability, but its failure cannot be
            // reported as an uncommitted rewrite without lying to callers.
            tracing::warn!(
                target: "zevria_core::transcript", path = %self.path.display(),
                %error,
                "session replacement committed but the parent directory could not be synced"
            );
        }
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of malformed records removed while reopening this transcript.
    pub fn recovered_malformed_lines(&self) -> usize {
        self.recovered_malformed_lines
    }
}

/// Write every item to an already-created staged file.
fn write_items(
    file: &mut std::fs::File,
    destination: &Path,
    items: &[TranscriptItem],
) -> anyhow::Result<()> {
    for item in items {
        let mut line = serde_json::to_vec(item).context("failed to serialize a transcript item")?;
        anyhow::ensure!(
            line.len() < MAX_ROOT_RECORD_BYTES,
            "root transcript record exceeds the 64 MiB limit"
        );
        line.push(b'\n');
        file.write_all(&line).with_context(|| {
            format!(
                "failed to write the staged session file for {}",
                destination.display()
            )
        })?;
    }
    file.flush().with_context(|| {
        format!(
            "failed to flush the session file at {}",
            destination.display()
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Whether `item` is a *prompt item*: one a frontend renders as a user
/// message row. These are exactly the plain user messages and `$skill`
/// invocations, in the order they were recorded, so a frontend and the engine
/// number the same turns without exchanging any message content.
///
/// Tool-result batches carry no prompt and are deliberately excluded — as are
/// assistant messages, subtask result blocks, and error notices. Typed Plan
/// handoffs supply model instructions but do not consume editable prompt rows.
pub fn is_prompt_item(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::SkillInvocation(_)
            | TranscriptItem::Message(Message::User { .. })
            | TranscriptItem::RequestPrompt { .. }
    )
}

/// Validate the authoritative selection's uniqueness and exact header position.
/// Missing metadata is supported, but never malformed or misplaced metadata.
pub fn session_mode(items: &[TranscriptItem]) -> anyhow::Result<Option<crate::SessionMode>> {
    #[cfg(feature = "test-support")]
    crate::replay_probe::record(|counts| counts.header_scans += 1);
    validate_header(items.iter())
}

fn validate_header<'a>(
    items: impl Iterator<Item = &'a TranscriptItem>,
) -> anyhow::Result<Option<crate::SessionMode>> {
    let mut header = SessionHeader::default();
    for (index, item) in items.enumerate() {
        header.apply(index, item)?;
    }
    Ok(header.selected)
}

/// Incremental form of the same header rules used by persistence and replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionHeader {
    selected: Option<crate::SessionMode>,
    mode_index: usize,
}

impl SessionHeader {
    pub(crate) fn selected(&self) -> Option<crate::SessionMode> {
        self.selected
    }

    pub(crate) fn apply(&mut self, index: usize, item: &TranscriptItem) -> anyhow::Result<()> {
        match item {
            TranscriptItem::SessionModels(_) => {
                anyhow::ensure!(index == 0, "session models must be a unique first record");
                self.mode_index = 1;
            }
            TranscriptItem::SessionMode(mode) => {
                anyhow::ensure!(self.selected.is_none(), "session mode must be unique");
                anyhow::ensure!(
                    index == self.mode_index,
                    "session mode must be first or immediately follow session models"
                );
                self.selected = Some(*mode);
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn install_session_mode(
    items: &mut Vec<TranscriptItem>,
    mode: crate::SessionMode,
) -> anyhow::Result<()> {
    session_mode(items)?;
    let index = usize::from(matches!(
        items.first(),
        Some(TranscriptItem::SessionModels(_))
    ));
    if matches!(items.get(index), Some(TranscriptItem::SessionMode(_))) {
        items[index] = TranscriptItem::SessionMode(mode);
    } else {
        items.insert(index, TranscriptItem::SessionMode(mode));
    }
    Ok(())
}

pub fn is_leading_metadata(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::SessionModels(_) | TranscriptItem::SessionMode(_)
    )
}

/// Whether an item supplies user-role model input that can anchor context
/// compaction. Typed Plan handoffs and ensemble evidence are included without
/// becoming editable/numbered frontend prompt rows. Not every anchor supplies
/// retained user instructions: ensemble evidence remains summary material.
pub fn is_compaction_prompt_item(item: &TranscriptItem) -> bool {
    is_prompt_item(item)
        || matches!(
            item,
            TranscriptItem::Plan(PlanRecord::Handoff { .. })
                | TranscriptItem::Ensemble(EnsembleRecord::ReportsReady { .. })
        )
}

/// Elide only completed v1 presentations recoverable from a later exact-ID
/// native replay. Plain assistant bindings have no native ledger to rebuild
/// from, and unlinked checkpoints/retries must retain their readable evidence.
/// Call only as part of the atomic commit of both records.
pub fn compact_linked_attempts(items: &mut [TranscriptItem]) {
    let mut linked = std::collections::HashSet::new();
    for item in items.iter_mut().rev() {
        if item.provider_replay().is_some()
            && let Some(id) = item.display_attempt_id()
        {
            linked.insert(id.to_owned());
        } else if let TranscriptItem::WebSearchAttempt(attempt) = item
            && attempt.version == zevria_content::web_search::WEB_SEARCH_ATTEMPT_VERSION
            && attempt.outcome == crate::WebSearchAttemptOutcome::Completed
            && linked.contains(attempt.id.as_str())
        {
            attempt.compact_presentation();
        }
    }
}

/// One live conversation and the writer for its complete persisted sequence.
///
/// The item list is the *single* in-memory representation of a session's
/// conversation. Model input projects from it; disk equals the live items,
/// including ordered directives. Item indices equal file-line indices when
/// blank lines are excluded. Required changes install memory after persistence
/// commits; completed work can remain in memory under explicitly degraded
/// persistence.
pub struct Conversation {
    items: Vec<TranscriptItem>,
    writer: TranscriptWriter,
    persistence_error: Option<String>,
}

/// Created and consumed synchronously inside Conversation. Neither arbitrary
/// records nor detached replay facts can enter the validated persistence path.
struct PreparedChange {
    items: Vec<TranscriptItem>,
    replay: crate::ValidatedSessionReplay,
    single_append: bool,
}

impl Conversation {
    /// Begin an empty conversation logging to `writer`.
    pub fn new(writer: TranscriptWriter) -> Self {
        Self {
            items: Vec::new(),
            writer,
            persistence_error: None,
        }
    }

    /// Adopt items already present in the log — the resume path, where the
    /// writer was opened with [`TranscriptWriter::append_to`] on the very file
    /// they were loaded from. Nothing is written.
    pub fn adopt_persisted(&mut self, items: Vec<TranscriptItem>) {
        self.items = items;
    }

    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn session_models(&self) -> Option<&SessionModels> {
        match self.items.first() {
            Some(TranscriptItem::SessionModels(models)) => Some(models),
            _ => None,
        }
    }

    /// Commit a canonical snapshot before changing memory or an active route.
    /// Equal selections do not rewrite history. Missing metadata on a saved
    /// root is unsupported and must never trigger implicit model inference.
    pub fn replace_session_models(&mut self, models: SessionModels) -> anyhow::Result<()> {
        self.ensure_durable()?;
        if self.session_models() == Some(&models) {
            return Ok(());
        }
        let mut replacement = self.items.clone();
        if self.session_models().is_some() {
            replacement[0] = TranscriptItem::SessionModels(models);
        } else {
            anyhow::ensure!(
                self.items.iter().all(is_leading_metadata),
                "unsupported history: session model metadata is missing; start a fresh session"
            );
            replacement.insert(0, TranscriptItem::SessionModels(models));
        }
        self.writer.rewrite(&replacement)?;
        self.items = replacement;
        Ok(())
    }

    /// Atomically select a mode without creating model-visible conversation input.
    /// Rejected proposals and failed writes leave both memory and disk unchanged.
    pub fn replace_session_mode(&mut self, mode: crate::SessionMode) -> anyhow::Result<bool> {
        self.writer.ensure_writable()?;
        anyhow::ensure!(
            self.persistence_error.is_none(),
            "session transcript persistence is degraded"
        );
        if session_mode(&self.items)? == Some(mode) {
            return Ok(false);
        }
        self.commit_anchored(None, Vec::new(), Some(mode))?;
        Ok(true)
    }

    /// Assemble, validate, and persist one authoritative anchored proposal.
    /// The returned replay belongs to the exact items installed after durability;
    /// failed validation or persistence never installs prospective state.
    pub fn commit_anchored(
        &mut self,
        index: Option<usize>,
        records: Vec<TranscriptItem>,
        mode: Option<crate::SessionMode>,
    ) -> anyhow::Result<crate::ValidatedSessionReplay> {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| counts.proposals += 1);
        let end = index
            .unwrap_or(self.items.len())
            .max(self.leading_metadata_len());
        anyhow::ensure!(
            end <= self.items.len(),
            "replacement index is outside the transcript"
        );
        let single_append = index.is_none() && mode.is_none() && records.len() == 1;
        let mut items = self.items[..end].to_vec();
        if let Some(mode) = mode {
            // Only canonical header positions can be replaced. Final replay
            // rejects duplicate/misplaced metadata, including any in records.
            // Do this before adding the suffix so it cannot hide an invalid header.
            let position = usize::from(matches!(
                items.first(),
                Some(TranscriptItem::SessionModels(_))
            ));
            if matches!(items.get(position), Some(TranscriptItem::SessionMode(_))) {
                items[position] = TranscriptItem::SessionMode(mode);
            } else {
                items.insert(position, TranscriptItem::SessionMode(mode));
            }
        }
        items.extend(records);
        let replay = crate::validate_session_replay(&items)?;
        let change = PreparedChange {
            items,
            replay,
            single_append,
        };
        self.persist_prepared_change(change, mode.is_some())
    }

    fn persist_prepared_change(
        &mut self,
        change: PreparedChange,
        selects_mode: bool,
    ) -> anyhow::Result<crate::ValidatedSessionReplay> {
        self.writer.ensure_writable()?;
        if selects_mode {
            anyhow::ensure!(
                self.persistence_error.is_none(),
                "session transcript persistence is degraded"
            );
        } else if !change.single_append {
            self.ensure_durable()?;
        }
        if change.items != self.items {
            if change.single_append {
                if let Err(error) = self
                    .writer
                    .append_record(change.items.last().expect("one appended item"))
                {
                    let error = error.context("the session transcript is degraded");
                    self.persistence_error = Some(format!("{error:#}"));
                    return Err(error);
                }
            } else {
                self.writer.rewrite_records(&change.items)?;
            }
            self.items = change.items;
        }
        Ok(change.replay)
    }

    /// The exact ordered active model projection.
    pub fn model_input(&self) -> Vec<ModelRequestItem<'_>> {
        model_input(&self.items)
    }

    /// Message-bearing compatibility view of the active model projection.
    /// Opaque provider-only checkpoint entries are intentionally absent.
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.model_input()
            .into_iter()
            .filter_map(ModelRequestItem::message_ref)
    }

    pub fn latest_compaction(&self) -> Option<(usize, &CompactionCheckpoint)> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, item)| match item {
                TranscriptItem::Compaction(checkpoint) => Some((index, checkpoint)),
                _ => None,
            })
    }

    /// User/direct-skill prompts and typed Plan handoff instructions eligible
    /// for bounded retention in the next checkpoint.
    pub fn retained_user_candidates(&self) -> Vec<String> {
        retained_user_candidates(&self.items)
    }

    pub fn has_real_user_prompt(&self) -> bool {
        self.items.iter().any(is_prompt_item)
    }

    pub fn has_compaction_prompt(&self) -> bool {
        self.items.iter().any(is_compaction_prompt_item)
    }

    /// Append a pre-effect item. A failure keeps memory unchanged and marks
    /// the writer degraded because the failed write may have left a partial
    /// trailing line that must be repaired before another append.
    pub fn push_required(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        if let Err(error) = self.writer.append(&item) {
            let error = error.context("the session transcript is degraded");
            self.persistence_error = Some(format!("{error:#}"));
            return Err(error);
        }
        self.items.push(item);
        Ok(())
    }

    /// Atomically append several pre-effect records. Workflow transitions
    /// pair a prompt with its Plan record through this path so a crash or
    /// write failure can expose neither half on its own.
    pub fn push_required_batch(&mut self, new_items: Vec<TranscriptItem>) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        if new_items.is_empty() {
            return Ok(());
        }
        self.ensure_durable()?;
        let mut replacement = self.items.clone();
        replacement.extend(new_items);
        self.writer.rewrite(&replacement)?;
        self.items = replacement;
        Ok(())
    }

    /// Append work that has already completed externally. If persistence
    /// fails, retain the item in memory so the live conversation remains
    /// truthful, mark the writer degraded, and require a full repair before
    /// any further pre-effect work starts.
    pub fn push_completed(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        match self.writer.append(&item) {
            Ok(()) => {
                self.items.push(item);
                Ok(())
            }
            Err(error) => {
                self.items.push(item);
                let error = error.context("the session transcript is degraded");
                self.persistence_error = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }

    /// Commit a display-linked response and compact its checkpoint in the same
    /// rewrite. Ordinary completed records keep the cheaper append path.
    pub fn push_completed_linked(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        if item.display_attempt_id().is_some() {
            self.push_completed_batch(vec![item])
        } else {
            self.push_completed(item)
        }
    }

    /// Atomically persist several records for work that has already completed.
    /// A failed replacement keeps the complete batch in memory and marks the
    /// conversation degraded, matching [`Self::push_completed`] semantics while
    /// ensuring a successful crash boundary can expose either every record or
    /// none of them.
    pub fn push_completed_batch(&mut self, new_items: Vec<TranscriptItem>) -> anyhow::Result<()> {
        self.writer.ensure_writable()?;
        if new_items.is_empty() {
            return Ok(());
        }
        let mut replacement = self.items.clone();
        replacement.extend(new_items);
        compact_linked_attempts(&mut replacement);
        let result = self
            .ensure_durable()
            .and_then(|_| self.writer.rewrite(&replacement));
        self.items = replacement;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let error = error.context("the session transcript is degraded");
                self.persistence_error = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }

    /// Repair any partial/failed append by rewriting the persisted projection
    /// of the complete live conversation. Returns `true` after a successful repair.
    pub fn ensure_durable(&mut self) -> anyhow::Result<bool> {
        self.writer.ensure_writable()?;
        if self.persistence_error.is_none() {
            return Ok(false);
        }
        if let Err(error) = self.writer.rewrite(&self.items) {
            let error = error.context("failed to repair the degraded session transcript");
            self.persistence_error = Some(format!("{error:#}"));
            return Err(error);
        }
        self.persistence_error = None;
        Ok(true)
    }

    pub fn persistence_error(&self) -> Option<&str> {
        self.persistence_error.as_deref()
    }

    pub fn is_read_only(&self) -> bool {
        self.writer.is_read_only()
    }

    /// The position of the `ordinal`-th [`prompt item`](is_prompt_item), or
    /// `None` when the conversation holds fewer prompts than that.
    pub fn prompt_position(&self, ordinal: usize) -> Option<usize> {
        let (index, _) = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| is_prompt_item(item))
            .nth(ordinal)?;
        Some(index)
    }

    pub fn leading_metadata_len(&self) -> usize {
        self.items
            .iter()
            .take_while(|item| is_leading_metadata(item))
            .count()
    }

    /// The position of the durable start record for `run_id`, if it remains
    /// in the active root transcript. Later records for the same run are not
    /// valid tail-edit anchors.
    pub fn ensemble_start_position(&self, run_id: &EnsembleRunId) -> Option<usize> {
        self.items.iter().position(|item| {
            matches!(
                item,
                TranscriptItem::Ensemble(EnsembleRecord::Started { start })
                    if &start.run_id == run_id
            )
        })
    }

    /// Drop every item from `index` on and rewrite the retained sequence.
    /// The fallible write runs first, so a failure leaves both sides untouched
    /// and the caller keeps a conversation whose items match its log.
    pub fn truncate(&mut self, index: usize) -> anyhow::Result<()> {
        self.ensure_durable()?;
        let index = index.max(self.leading_metadata_len());
        self.writer.rewrite(&self.items[..index])?;
        self.items.truncate(index);
        Ok(())
    }

    /// Replace `index..` with one new transcript item as a single commit.
    /// A frontend receives confirmation only after the old tail and its typed
    /// replacement have moved together on disk and in memory.
    pub fn replace_from(&mut self, index: usize, item: TranscriptItem) -> anyhow::Result<()> {
        self.replace_from_items(index, vec![item])
    }

    /// Atomically replace the transcript tail with several typed records.
    /// Message, skill, ensemble, compaction, and Plan-transition records can
    /// therefore share one durable revision boundary.
    pub fn replace_from_items(
        &mut self,
        index: usize,
        new_items: Vec<TranscriptItem>,
    ) -> anyhow::Result<()> {
        self.ensure_durable()?;
        let index = index.max(self.leading_metadata_len());
        let mut replacement = self.items[..index].to_vec();
        replacement.extend(new_items);
        self.writer.rewrite(&replacement)?;
        self.items = replacement;
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        self.writer.session_id()
    }

    pub fn path(&self) -> &Path {
        self.writer.path()
    }
}

/// Classification of an inspection diagnostic. Only an incomplete final
/// JSON line is eligible for automatic crash recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptDamage {
    IncompleteTail,
    InvalidRecord,
    IncompatibleLifecycle,
    InvalidSessionModels,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDiagnostic {
    /// Original one-based physical line, or None for a cross-record failure.
    pub line: Option<usize>,
    pub kind: TranscriptDamage,
    pub message: String,
}

/// Validated projection of a current transcript, including recoverable tail
/// diagnostics. Unsupported histories return an error instead of a projection.
#[derive(Debug)]
pub struct TranscriptLoadOutcome {
    pub path: PathBuf,
    pub items: Vec<TranscriptItem>,
    /// Original physical line for each projected item.
    pub source_lines: Vec<usize>,
    pub diagnostics: Vec<TranscriptDiagnostic>,
    pub omitted_diagnostics: usize,
    pub recoverable_lines: usize,
    pub blocked: bool,
    // Invalid metadata must never be mistaken for absence.
    session_models_error: Option<String>,
    needs_newline: bool,
}

impl TranscriptLoadOutcome {
    fn diagnose(&mut self, line: Option<usize>, kind: TranscriptDamage, message: impl AsRef<str>) {
        self.blocked |= kind != TranscriptDamage::IncompleteTail;
        if kind == TranscriptDamage::InvalidSessionModels && self.session_models_error.is_none() {
            let detail = message.as_ref().chars().take(1024).collect::<String>();
            self.session_models_error =
                Some(line.map_or_else(|| detail.clone(), |line| format!("line {line}: {detail}")));
        }
        if self.diagnostics.len() == 256 {
            self.omitted_diagnostics += 1;
        } else {
            self.diagnostics.push(TranscriptDiagnostic {
                line,
                kind,
                message: message.as_ref().chars().take(1024).collect(),
            });
        }
    }

    /// Read validated selection metadata. Production root and native-worker
    /// resume additionally require Some; ordinary child logs do not.
    pub fn session_models(&self) -> anyhow::Result<Option<&SessionModels>> {
        if let Some(error) = &self.session_models_error {
            anyhow::bail!(
                "invalid session model metadata: {error}; restore a valid backup or use an appropriate compatible Zevria version; metadata must not be automatically replaced"
            );
        }
        Ok(match self.items.first() {
            Some(TranscriptItem::SessionModels(models)) => Some(models),
            _ => None,
        })
    }

    pub fn ensure_resumable(&self) -> anyhow::Result<()> {
        if self.blocked {
            let detail = self
                .diagnostics
                .iter()
                .filter(|entry| entry.kind != TranscriptDamage::IncompleteTail)
                .take(8)
                .map(|entry| match entry.line {
                    Some(line) => format!("line {line}: {}", entry.message),
                    None => entry.message.clone(),
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(UnsupportedHistory::new(&self.path, None, detail).into());
        }
        Ok(())
    }
}

/// Load a current resumable history. Unsupported records reject the entire load;
/// [`load_report`] additionally returns diagnostics for recoverable trailing damage.
pub fn load(path: &Path) -> anyhow::Result<Vec<TranscriptItem>> {
    let outcome = load_report(path)?;
    outcome.ensure_resumable()?;
    Ok(outcome.items)
}

pub fn load_report(path: &Path) -> anyhow::Result<TranscriptLoadOutcome> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to read the session file at {}", path.display()))?;
    let mut outcome = TranscriptLoadOutcome {
        path: path.to_path_buf(),
        items: Vec::new(),
        source_lines: Vec::new(),
        diagnostics: Vec::new(),
        omitted_diagnostics: 0,
        recoverable_lines: 0,
        blocked: false,
        session_models_error: None,
        needs_newline: false,
    };
    let mut lines = bounded_record_lines(std::io::BufReader::new(file))
        .enumerate()
        .peekable();
    while let Some((index, line)) = lines.next() {
        let line = line?;
        let line = line.as_slice();
        outcome.needs_newline = !line.ends_with(b"\n");
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Detect a reserved top-level key even if its value is truncated. Do
        // not search message text: literal marker strings are ordinary data.
        if partial_json_field(line, &[SESSION_MODE_KEY, "selected"])
            .as_ref()
            .and_then(serde_json::Value::as_str)
            == Some("orchestrate")
        {
            return Err(UnsupportedHistory::new(path, Some(index + 1), "Build or Plan session mode")
                .with_failure(HistoryFailure::Validation { detail: "legacy Orchestrate mode is no longer supported; start a fresh Build session and submit /orchestrate <prompt>. The saved transcript has not been modified".into() }).into());
        }
        let model_metadata = has_session_models_key(line);
        if top_level_keys(line)
            .iter()
            .any(|key| key == SESSION_MODE_KEY)
            && let Err(error) = serde_json::from_slice::<SessionModeEnvelope>(line)
        {
            outcome.diagnose(
                Some(index + 1),
                TranscriptDamage::IncompatibleLifecycle,
                error.to_string(),
            );
            continue;
        }
        if model_metadata {
            let header = serde_json::from_slice::<SessionModelsEnvelope>(line);
            let error = if index != 0 {
                Some(
                    "session model metadata must be unique and on the first physical line"
                        .to_string(),
                )
            } else {
                header.as_ref().err().map(ToString::to_string)
            };
            if let Some(error) = error {
                outcome.diagnose(
                    Some(index + 1),
                    TranscriptDamage::InvalidSessionModels,
                    error,
                );
                continue;
            }
        }
        let value = match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) => value,
            Err(error) => {
                let recoverable = lines.peek().is_none()
                    && outcome.needs_newline
                    && error.is_eof()
                    && !has_unsupported_reserved_tail(line);
                let kind = if recoverable {
                    outcome.recoverable_lines += 1;
                    TranscriptDamage::IncompleteTail
                } else {
                    TranscriptDamage::InvalidRecord
                };
                outcome.diagnose(Some(index + 1), kind, error.to_string());
                continue;
            }
        };
        // Inspect reserved keys at the record boundary, never marker-like
        // strings inside ordinary messages or a skill's instructions.
        let validation = validate_reserved_lifecycle_keys(&value);
        if let Err(error) = validation {
            outcome.diagnose(
                Some(index + 1),
                TranscriptDamage::IncompatibleLifecycle,
                format!("{error:#}"),
            );
            continue;
        }
        let reserved = value.as_object().is_some_and(|object| {
            object.keys().any(|key| {
                key.starts_with("zevria_skill_")
                    || key == COMPACTION_RECORD_KEY
                    || key == PROVIDER_REPLAY_KEY
                    || key == ENSEMBLE_RECORD_KEY
                    || key == PLAN_RECORD_KEY
                    || key == INSTRUCTION_PREFIX_KEY
                    || key == SESSION_MODE_KEY
                    || key == DIRECTIVE_KEY
                    || key == REQUEST_METADATA_KEY
                    || key == REQUEST_DIRECTIVE_KEY
            })
        });
        match serde_json::from_slice::<TranscriptItem>(line) {
            Ok(item) => {
                outcome.items.push(item);
                outcome.source_lines.push(index + 1);
            }
            Err(error) => outcome.diagnose(
                Some(index + 1),
                if reserved {
                    TranscriptDamage::IncompatibleLifecycle
                } else {
                    TranscriptDamage::InvalidRecord
                },
                error.to_string(),
            ),
        }
    }
    // Authority-bearing records must be validated before writable repair.
    if !outcome.blocked
        && let Err(error) = crate::replay_directives(&outcome.items)
    {
        outcome.diagnose(
            None,
            TranscriptDamage::IncompatibleLifecycle,
            error.to_string(),
        );
    }
    // Validate current lifecycle linkage before projection or writable open.
    if !outcome.blocked
        && let Err(error) = replay_active_skills(&outcome.items)
    {
        let line = error
            .downcast_ref::<SkillLifecycleLocation>()
            .and_then(|location| outcome.source_lines.get(location.0))
            .copied();
        outcome.diagnose(
            line,
            TranscriptDamage::IncompatibleLifecycle,
            "expected valid current skill lifecycle linkage and checkpoint assertions",
        );
    }
    if !outcome.blocked
        && let Err(error) = crate::validate_ensemble_review_history(&outcome.items)
    {
        outcome.diagnose(None, TranscriptDamage::IncompatibleLifecycle, error);
    }
    if !outcome.blocked {
        crate::replay::validate_web_search_replay(&outcome.items).map_err(|error| {
            UnsupportedHistory::new(
                path,
                None,
                "compacted web search attempts with a later exact-ID linked provider replay",
            )
            .with_failure(HistoryFailure::Validation {
                detail: error.to_string(),
            })
        })?;
    }
    if outcome.blocked {
        let diagnostic = outcome
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.kind != TranscriptDamage::IncompleteTail);
        return Err(UnsupportedHistory::new(path, diagnostic.and_then(|diagnostic| diagnostic.line),
            "current transcript records, dedicated name-based skill invocation and application records, skill directive v1 records with version and payload only, compaction v1 without instruction state, no retired instruction prefix or directive records, optional leading session models followed immediately by optional session mode v1 with an exact ID (mode first without models), provider replay v1 with nonblank source_profile, display-only web search attempt v1 with optional assistant display bindings, ensemble review v1 with explicit confirmations, and valid lifecycle linkage").into());
    }
    Ok(outcome)
}

/// Safe structured context for a persisted-record failure. Decoder messages can
/// contain arbitrary record values, so only categories and fixed descriptions
/// are retained, never the raw Serde error or JSON payload.
#[derive(Debug)]
pub enum HistoryFailure {
    Decode {
        category: serde_json::error::Category,
        record_line: usize,
        record_column: usize,
        detail: &'static str,
    },
    Validation {
        /// Locally generated constraint explanation, not a protocol payload.
        detail: String,
    },
    Version {
        expected: u32,
        found: u32,
    },
}

/// A persisted format failure, never recoverable crash debris or a model conversion request.
#[derive(Debug)]
pub struct UnsupportedHistory {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub expected: String,
    pub failure: Option<HistoryFailure>,
}

impl UnsupportedHistory {
    pub fn new(path: &Path, line: Option<usize>, expected: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            line,
            expected: expected.into(),
            failure: None,
        }
    }

    pub fn with_failure(mut self, failure: HistoryFailure) -> Self {
        self.failure = Some(failure);
        self
    }

    pub fn decoding(path: &Path, line: usize, error: &serde_json::Error) -> Self {
        let message = error.to_string();
        let detail = if message.starts_with("invalid web action terminal index") {
            "terminal index must be canonical unsigned decimal u64"
        } else if message.starts_with("duplicate web action terminal index") {
            "duplicate terminal index"
        } else if message.starts_with("invalid type:") {
            "invalid value type"
        } else if message.starts_with("unknown variant") {
            "unknown record, event, or enum variant"
        } else if message.starts_with("unknown field") {
            "unknown field"
        } else if message.starts_with("missing field") {
            "missing required field"
        } else if message.starts_with("duplicate field") {
            "duplicate field"
        } else {
            "record does not match the current schema"
        };
        Self::new(path, Some(line), "a decodable current record").with_failure(
            HistoryFailure::Decode {
                category: error.classify(),
                record_line: error.line(),
                record_column: error.column(),
                detail,
            },
        )
    }
}

impl std::fmt::Display for UnsupportedHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported history at {}", self.path.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        match &self.failure {
            Some(HistoryFailure::Decode {
                category,
                record_line,
                record_column,
                detail,
            }) => {
                write!(f, "; decoding failure ({category:?}: {detail}")?;
                if *record_line != 0 || *record_column != 0 {
                    write!(f, ", record line {record_line}, column {record_column}")?;
                }
                write!(f, ")")?;
            }
            Some(HistoryFailure::Validation { detail }) => {
                // Validation explanations are authored by the local validators.
                // Bound and sanitize them even if a future constraint is longer.
                let detail = zevria_content::web_search::sanitize_readable(detail)
                    .chars()
                    .take(256)
                    .collect::<String>();
                write!(f, "; validation failure: {detail}")?;
            }
            Some(HistoryFailure::Version { expected, found }) => {
                write!(
                    f,
                    "; worker version mismatch: found v{found}, expected v{expected}"
                )?;
            }
            None => {}
        }
        write!(
            f,
            "; expected {}. Original bytes were preserved; preserve the logs and resolve the reported history issue before resuming",
            self.expected
        )
    }
}
impl std::error::Error for UnsupportedHistory {}

fn validate_reserved_lifecycle_keys(value: &serde_json::Value) -> anyhow::Result<()> {
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

/// Extract only ordinary Rig messages for resumed model history.
///
/// Compound tool-result metadata is intentionally inaccessible through this
/// path, preserving the structural model/UI boundary.
pub fn model_history(items: &[TranscriptItem]) -> Vec<Message> {
    model_input(items)
        .into_iter()
        .filter_map(ModelRequestItem::message_ref)
        .cloned()
        .collect()
}

/// Exact checkpoint-aware model projection for a raw transcript slice.
pub fn model_input(items: &[TranscriptItem]) -> Vec<ModelRequestItem<'_>> {
    model_input_from_records(items.iter())
}

/// Project a prospective request from borrowed history/checkpoint/suffix parts.
/// No full transcript payload copy is needed for estimation or exact counting.
pub fn model_input_from_records<'a>(
    items: impl Iterator<Item = &'a TranscriptItem> + Clone,
) -> Vec<ModelRequestItem<'a>> {
    let latest = items
        .clone()
        .enumerate()
        .filter_map(|(index, item)| match item {
            TranscriptItem::Compaction(checkpoint) => Some((index, checkpoint)),
            _ => None,
        })
        .last();
    let (mut input, start) = match latest {
        Some((index, checkpoint)) => (
            model_input_with_checkpoint(items.clone().take(index), checkpoint),
            index.saturating_add(1),
        ),
        None => (Vec::new(), 0),
    };
    input.extend(
        items
            .skip(start)
            .filter_map(TranscriptItem::model_request_item),
    );
    input
}

/// Project a checkpoint against its retained prefix without installing or
/// cloning the checkpoint. Prepared prompt records may extend this projection.
pub fn model_input_with_checkpoint<'a>(
    prefix: impl Iterator<Item = &'a TranscriptItem> + Clone,
    checkpoint: &'a CompactionCheckpoint,
) -> Vec<ModelRequestItem<'a>> {
    let mut input = checkpoint
        .replacement_history
        .iter()
        .map(zevria_model::OwnedModelRequestItem::as_borrowed)
        .collect::<Vec<_>>();
    input.extend(
        zevria_instructions::directive::effective_directives(prefix.clone().filter_map(|item| {
            match item {
                TranscriptItem::Directive(directive) => Some(directive),
                _ => None,
            }
        }))
        .into_iter()
        .map(ModelRequestItem::DeveloperInstruction),
    );
    input.extend(
        crate::request_replay::effective_request_directives(prefix)
            .into_iter()
            .map(ModelRequestItem::RequestInstruction),
    );
    input
}

#[derive(Debug)]
struct PendingTaskCall {
    id: String,
    call_id: Option<String>,
    item_index: usize,
    content_index: usize,
    snapshot: TaskList,
}

/// Replay the task-call/result lifecycle and return only the latest snapshot
/// confirmed by a correlated successful result. Invalid, failed, denied, and
/// interrupted calls cannot replace the previously installed checklist.
pub fn latest_successful_task_snapshot(items: &[TranscriptItem]) -> Option<TaskList> {
    let mut pending = Vec::new();
    let mut latest = None;

    for (item_index, item) in items.iter().enumerate() {
        let Some(message) = item.message() else {
            continue;
        };
        match message {
            Message::Assistant { content, .. } => {
                for (content_index, content) in content.iter().enumerate() {
                    let AssistantContent::ToolCall(call) = content else {
                        continue;
                    };
                    if call.function.name != TASK_TOOL_NAME {
                        continue;
                    }
                    let Ok(snapshot) = TaskList::from_tool_arguments(&call.function.arguments)
                    else {
                        continue;
                    };
                    pending.push(PendingTaskCall {
                        id: call.id.to_string(),
                        call_id: provider_call_id(call.provider.as_ref()).cloned(),
                        item_index,
                        content_index,
                        snapshot,
                    });
                }
            }
            Message::User { content } => {
                let metadata = match item {
                    TranscriptItem::ToolResults { metadata, .. } => metadata.as_slice(),
                    _ => &[],
                };
                let mut available_metadata = (0..metadata.len()).collect::<Vec<_>>();
                for content in content {
                    let UserContent::ToolResult(result) = content else {
                        continue;
                    };
                    if result.name != TASK_TOOL_NAME {
                        continue;
                    }
                    let Some(pending_index) = select_pending_task_call(&pending, result) else {
                        continue;
                    };
                    let completed = pending.remove(pending_index);
                    let successful =
                        take_matching_metadata(metadata, &mut available_metadata, result)
                            .is_some_and(|metadata| metadata.outcome.is_success());
                    if successful {
                        latest = Some(completed.snapshot);
                    }
                }
            }
            Message::System { .. } => {}
        }
    }

    latest
}

fn select_pending_task_call(pending: &[PendingTaskCall], result: &ToolResult) -> Option<usize> {
    let result_call_id = provider_call_id(result.provider.as_ref()).map(String::as_str);
    [true, false].into_iter().find_map(|exact_call_id| {
        pending
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                call.id == result.call.as_str()
                    && call_ids_match(call.call_id.as_deref(), result_call_id, exact_call_id)
            })
            .max_by_key(|(_, call)| (call.item_index, std::cmp::Reverse(call.content_index)))
            .map(|(index, _)| index)
    })
}

fn take_matching_metadata<'metadata>(
    metadata: &'metadata [ToolResultMetadata],
    available: &mut Vec<usize>,
    result: &ToolResult,
) -> Option<&'metadata ToolResultMetadata> {
    let result_call_id = provider_call_id(result.provider.as_ref()).map(String::as_str);
    [true, false].into_iter().find_map(|exact_call_id| {
        let position = available.iter().position(|&index| {
            let metadata = &metadata[index];
            metadata.id == result.call.as_str()
                && metadata.tool_name == result.name
                && call_ids_match(metadata.call_id.as_deref(), result_call_id, exact_call_id)
        })?;
        Some(&metadata[available.remove(position)])
    })
}

fn call_ids_match(left: Option<&str>, right: Option<&str>, exact_call_id: bool) -> bool {
    let exact = left == right;
    if exact_call_id {
        exact
    } else {
        exact || left.is_none() || right.is_none()
    }
}

fn provider_call_id(provider: Option<&ProviderCallId>) -> Option<&String> {
    provider.map(|provider| &provider.call_id)
}

/// User/direct-skill prompts and typed Plan handoff instructions eligible for
/// bounded retention in a checkpoint over this transcript slice. Carry forward
/// only the latest checkpoint's retained text, not the full original prompts.
/// Edit preparation uses a retained prefix rather than the discarded tail.
pub fn retained_user_candidates(items: &[TranscriptItem]) -> Vec<String> {
    let (mut candidates, start) = items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| match item {
            TranscriptItem::Compaction(checkpoint) => Some((index, checkpoint)),
            _ => None,
        })
        .map_or((Vec::new(), 0), |(index, checkpoint)| {
            (
                checkpoint.retained_user_messages.clone(),
                index.saturating_add(1),
            )
        });
    candidates.extend(items[start..].iter().filter_map(prompt_text));
    candidates
}

/// Retained instruction text is distinct from both editable prompt rows and
/// compaction anchors; in particular, ensemble evidence is not reinjected here.
fn prompt_text(item: &TranscriptItem) -> Option<String> {
    if !is_prompt_item(item) && !matches!(item, TranscriptItem::Plan(PlanRecord::Handoff { .. })) {
        return None;
    }
    let Message::User { content } = item.message()? else {
        return None;
    };
    let text = content
        .iter()
        .filter_map(|content| match content {
            UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty() && !is_summary_message(&text)).then_some(text)
}

/// The most recently modified `.jsonl` session file in the directory, if any.
/// A missing directory simply means no sessions exist yet.
pub fn latest_session_file(sessions_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    Ok(session_files_by_mtime(sessions_dir)?
        .into_iter()
        .rev()
        .map(|(_, _, path)| path)
        .find(|path| !is_abandoned_root(path)))
}

/// Only a truly empty file or valid models-only root is safe
/// to hide/delete. A mode-bearing root is retained, including canonical Build:
/// it may be the result of an explicit Plan → Build selection. Unreadable,
/// malformed, and substantive histories also stay visible.
pub fn is_abandoned_root(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let mut lines = bounded_record_lines(std::io::BufReader::new(file));
    match lines.next() {
        None => return true,
        Some(Ok(line)) if serde_json::from_slice::<SessionModelsEnvelope>(&line).is_ok() => {}
        _ => return false,
    }
    // Stop as soon as history is substantive/damaged; listing should not load
    // and validate entire conversations just to exclude abandoned roots.
    lines.all(|line| line.is_ok_and(|line| line.iter().all(u8::is_ascii_whitespace)))
}

fn has_session_models_key(line: &[u8]) -> bool {
    top_level_keys(line)
        .iter()
        .any(|key| key.starts_with(SESSION_MODELS_KEY))
}

fn has_unsupported_reserved_tail(line: &[u8]) -> bool {
    fn invalid_field<T: serde::de::DeserializeOwned>(line: &[u8], path: &[&str]) -> bool {
        partial_json_field(line, path)
            .is_some_and(|value| serde_json::from_value::<T>(value).is_err())
    }

    let keys = top_level_keys(line);
    // Probe owned envelope bytes directly: Value would discard duplicate fields
    // in a complete invalid payload whose outer delimiter was interrupted.
    if keys.iter().any(|key| {
        matches!(
            key.as_str(),
            WEB_SEARCH_KEY
                | COMPACTION_RECORD_KEY
                | PROVIDER_REPLAY_KEY
                | SKILL_DIRECTIVE_KEY
                | ENSEMBLE_RECORD_KEY
                | SESSION_MODELS_KEY
        )
    }) {
        let mut closed = line.to_vec();
        closed.push(b'}');
        if serde_json::from_slice::<TranscriptItem>(&closed).is_err_and(|error| error.is_data()) {
            return true;
        }
    }
    // A crash may interrupt the reserved key itself before its closing quote.
    // Probe only newly completed top-level keys, never strings in message text.
    let mut completed_key = line.to_vec();
    completed_key.extend_from_slice(b"\":null}");
    if top_level_keys(&completed_key)
        .iter()
        .skip(keys.len())
        .any(|key| key.starts_with("zevria_"))
    {
        return true;
    }
    if keys.iter().any(|key| {
        matches!(
            key.as_str(),
            INSTRUCTION_PREFIX_KEY
                | DIRECTIVE_KEY
                | COMPACTION_RECORD_KEY
                | SESSION_MODE_KEY
                | SKILL_INVOCATION_KEY
                | SKILL_APPLICATIONS_KEY
                | TOOL_RESULT_METADATA_KEY
                | REQUEST_METADATA_KEY
                | REQUEST_DIRECTIVE_KEY
        )
    }) {
        return true;
    }
    if keys
        .iter()
        .any(|key| key.starts_with("zevria_") && !current_reserved_key(key))
    {
        return true;
    }
    for (envelope, version) in [
        (COMPACTION_RECORD_KEY, crate::compaction::COMPACTION_VERSION),
        (PROVIDER_REPLAY_KEY, crate::PROVIDER_REPLAY_VERSION),
        (
            ENSEMBLE_RECORD_KEY,
            zevria_workflow::ENSEMBLE_REVIEW_VERSION,
        ),
        (
            WEB_SEARCH_KEY,
            zevria_content::web_search::WEB_SEARCH_ATTEMPT_VERSION,
        ),
        (
            SKILL_DIRECTIVE_KEY,
            zevria_instructions::directive::INSTRUCTION_VERSION,
        ),
    ] {
        if partial_json_field(line, &[envelope, "version"])
            .is_some_and(|value| value.as_u64() != Some(u64::from(version)))
        {
            return true;
        }
    }
    // A completed inner envelope must already be current even if its outer
    // record was interrupted. Validate only actual persisted boundaries, not
    // arbitrary JSON in messages, tool arguments, or provider-native items.
    for key in &keys {
        if matches!(
            key.as_str(),
            COMPACTION_RECORD_KEY
                | PROVIDER_REPLAY_KEY
                | PLAN_RECORD_KEY
                | ENSEMBLE_RECORD_KEY
                | SKILL_DIRECTIVE_KEY
                | WEB_SEARCH_KEY
        ) && let Some(value) = partial_json_field(line, &[key])
            && serde_json::from_value::<TranscriptItem>(serde_json::json!({(key): value})).is_err()
        {
            return true;
        }
    }
    invalid_field::<Vec<SkillToolApplication>>(line, &[SKILL_APPLICATIONS_KEY])
        || invalid_field::<SkillInvocation>(line, &[SKILL_INVOCATION_KEY])
}

fn current_reserved_key(key: &str) -> bool {
    matches!(
        key,
        SKILL_APPLICATIONS_KEY
            | SKILL_INVOCATION_KEY
            | SKILL_DIRECTIVE_KEY
            | TOOL_RESULT_METADATA_KEY
            | COMPACTION_RECORD_KEY
            | PROVIDER_REPLAY_KEY
            | PLAN_RECORD_KEY
            | ENSEMBLE_RECORD_KEY
            | SESSION_MODELS_KEY
            | SESSION_MODE_KEY
            | WEB_SEARCH_KEY
            | DISPLAY_ATTEMPT_KEY
            | REQUEST_METADATA_KEY
            | REQUEST_DIRECTIVE_KEY
    )
}

/// Read one structural field even when a later value is interrupted. Paths do
/// not traverse arrays or strings, so quoted marker text cannot become metadata.
pub(crate) fn partial_json_field(line: &[u8], path: &[&str]) -> Option<serde_json::Value> {
    probe_json_field(line, path).1
}

fn probe_json_field(line: &[u8], path: &[&str]) -> (bool, Option<serde_json::Value>) {
    struct Probe<'a, 'b> {
        path: &'a [&'a str],
        found: &'b mut Option<serde_json::Value>,
        seen: &'b mut bool,
    }
    impl<'de> serde::de::DeserializeSeed<'de> for Probe<'_, '_> {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            if self.path.is_empty() {
                *self.seen = true;
                *self.found = Some(serde_json::Value::deserialize(deserializer)?);
                Ok(())
            } else {
                deserializer.deserialize_map(self)
            }
        }
    }
    impl<'de> serde::de::Visitor<'de> for Probe<'_, '_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a record object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == self.path[0] {
                    map.next_value_seed(Probe {
                        path: &self.path[1..],
                        found: self.found,
                        seen: self.seen,
                    })?;
                } else {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(())
        }
    }
    let mut found = None;
    let mut seen = false;
    let _ = serde::de::DeserializeSeed::deserialize(
        Probe {
            path,
            found: &mut found,
            seen: &mut seen,
        },
        &mut serde_json::Deserializer::from_slice(line),
    );
    (seen, found)
}

pub(crate) fn top_level_keys(line: &[u8]) -> Vec<String> {
    struct Probe<'a>(&'a mut Vec<String>);
    impl<'de> serde::de::Visitor<'de> for Probe<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a transcript object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                self.0.push(key);
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut found = Vec::new();
    let _ = serde::Deserializer::deserialize_map(
        &mut serde_json::Deserializer::from_slice(line),
        Probe(&mut found),
    );
    found
}

#[cfg(test)]
#[path = "history_format_tests.rs"]
mod format_tests;

#[cfg(test)]
#[path = "transcript_model_tests.rs"]
mod model_tests;

#[cfg(test)]
#[path = "transcript_mode_tests.rs"]
mod mode_tests;

#[cfg(test)]
#[path = "prepared_change_tests.rs"]
mod prepared_change_tests;

#[cfg(test)]
#[path = "transcript_instruction_tests.rs"]
mod instruction_tests;

#[cfg(test)]
#[path = "tool_result_tests.rs"]
mod tool_result_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::{
        AssistantContent, Reasoning, ToolCall, ToolCallId, ToolFunction, ToolResultContent,
    };
    use serde_json::json;

    use crate::skill::{SkillApplication, SkillName, SkillSnapshot};
    use crate::test_support::TranscriptRewriteBlocker;
    use crate::{
        CompactionBackend, CompactionTrigger, FileChange, FileChangeOperation, FileChangeOutput,
        OwnedModelRequestItem, PlanArtifact, PlanHandoff, PlanId, PlanRecord, PlanVersion,
        SUMMARY_PREFIX, ToolCallOutcome, ToolResultDetail, ToolResultMetadata, TurnId,
    };
    use crate::{OPENAI_RESPONSES_PROVIDER, PROVIDER_REPLAY_VERSION};
    use zevria_foundation::SKILL_TOOL_NAME;

    fn test_profile() -> crate::ModelProfileRef {
        crate::ModelProfileRef::new("test-provider", "test-model")
    }

    fn local_checkpoint(label: &str) -> CompactionCheckpoint {
        CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![
                OwnedModelRequestItem::message(Message::user("retained prompt")),
                OwnedModelRequestItem::message(Message::user(format!("{SUMMARY_PREFIX}\n{label}"))),
            ],
            vec!["retained prompt".to_string()],
        )
        .expect("checkpoint")
    }

    fn sample_metadata() -> Vec<ToolResultMetadata> {
        vec![ToolResultMetadata {
            diagnostic: None,
            id: "provider_call_1".to_string(),
            call_id: Some("provider_call_1".to_string()),
            tool_name: "write".to_string(),
            outcome: ToolCallOutcome::Success,
            detail: Some(ToolResultDetail::FileChanges(vec![FileChangeOutput {
                path: "src/new.rs".into(),
                change: FileChange::Add {
                    content: "fn main() {}\n".to_string(),
                },
            }])),
        }]
    }

    fn compound_tool_results() -> TranscriptItem {
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result_with_call_id(
                    "call_1",
                    "provider_call_1",
                    "write",
                    vec![ToolResultContent::text("wrote 13 bytes to src/new.rs")],
                )],
            },
            metadata: sample_metadata(),
        }
    }

    fn task_call(id: &str, step: &str) -> TranscriptItem {
        TranscriptItem::Message(Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::new_or_mint(id),
                ToolFunction::new(
                    TASK_TOOL_NAME.to_string(),
                    json!({
                        "tasks": [{"step": step, "status": "in_progress"}]
                    }),
                ),
            ))],
        })
    }

    fn task_result(id: &str, outcome: ToolCallOutcome) -> TranscriptItem {
        TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    id,
                    TASK_TOOL_NAME,
                    vec![ToolResultContent::text(if outcome.is_success() {
                        "Task list updated: 0/1 completed, 1 in progress, 0 pending."
                    } else {
                        "status: error\nerror: rejected"
                    })],
                )],
            },
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: id.to_string(),
                call_id: None,
                tool_name: TASK_TOOL_NAME.to_string(),
                outcome,
                detail: None,
            }],
        }
    }

    fn mode_item() -> TranscriptItem {
        TranscriptItem::SessionMode(crate::SessionMode::Build)
    }

    fn mode_json() -> String {
        serde_json::to_string(&mode_item()).expect("mode JSON")
    }

    fn sample_items() -> Vec<TranscriptItem> {
        vec![
            mode_item(),
            TranscriptItem::Message(Message::user("hello")),
            TranscriptItem::Message(Message::Assistant {
                id: Some("msg_1".to_string()),
                content: vec![
                    AssistantContent::Reasoning(Reasoning::summaries(vec![
                        "pondering".to_string(),
                    ])),
                    AssistantContent::ToolCall(ToolCall::new(
                        ToolCallId::new_or_mint("call_1"),
                        ToolFunction::new("lookup".to_string(), json!({"query": "rust"})),
                    )),
                    AssistantContent::text("answer"),
                ],
            }),
            compound_tool_results(),
            TranscriptItem::Message(Message::tool_result(
                "legacy_call",
                "legacy_tool",
                "legacy result",
            )),
            TranscriptItem::Error {
                error: "network down".to_string(),
            },
        ]
    }

    #[test]
    fn latest_task_snapshot_requires_a_correlated_successful_result() {
        let mut items = vec![
            task_call("first", "Keep the installed snapshot"),
            task_result("first", ToolCallOutcome::Success),
            task_call("failed", "Do not install the failed snapshot"),
            task_result("failed", ToolCallOutcome::Error),
            task_call("interrupted", "Do not install the interrupted snapshot"),
        ];

        let snapshot = latest_successful_task_snapshot(&items).expect("first snapshot survives");
        assert_eq!(snapshot.tasks[0].step, "Keep the installed snapshot");

        // A result with no success metadata is not authoritative.
        items.push(TranscriptItem::Message(Message::tool_result(
            "interrupted",
            TASK_TOOL_NAME,
            "Task list updated: 0/1 completed, 1 in progress, 0 pending.",
        )));
        let snapshot = latest_successful_task_snapshot(&items).expect("first snapshot survives");
        assert_eq!(snapshot.tasks[0].step, "Keep the installed snapshot");

        items.extend([
            task_call("second", "Install the newer snapshot"),
            task_result("second", ToolCallOutcome::Success),
        ]);
        let snapshot = latest_successful_task_snapshot(&items).expect("new snapshot installs");
        assert_eq!(snapshot.tasks[0].step, "Install the newer snapshot");
    }

    #[test]
    fn items_round_trip_through_a_session_file() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let sessions = sessions_dir(dir.path());
        let mut writer = TranscriptWriter::create(&sessions).expect("writer should create");

        let items = sample_items();
        for item in &items {
            writer.append(item).expect("append should succeed");
        }

        let loaded = load(writer.path()).expect("session file should load");
        assert_eq!(loaded, items);
    }

    fn sample_plan_handoff() -> PlanHandoff {
        let title = "Durable approval workflow";
        let artifact = PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: title.to_string(),
            markdown: format!("# {title}\n\ncomplete canonical markdown"),
            source_turn_id: TurnId::new(7),
        };
        PlanHandoff::new(artifact, "source-session")
    }

    #[test]
    fn plan_records_round_trip_and_only_handoffs_are_model_visible_and_compactable() {
        let handoff = sample_plan_handoff();
        let artifact = handoff.artifact.clone();
        let items = vec![
            mode_item(),
            TranscriptItem::Plan(PlanRecord::Started {
                id: artifact.version.id,
            }),
            TranscriptItem::Plan(PlanRecord::Ready {
                artifact: artifact.clone(),
            }),
            TranscriptItem::Plan(PlanRecord::RevisionRequested {
                artifact: artifact.clone(),
            }),
            TranscriptItem::Plan(PlanRecord::Resolved {
                id: artifact.version.id,
                artifact: Some(artifact.clone()),
                resolution: crate::PlanResolution::ImplementedFresh,
            }),
            TranscriptItem::Plan(PlanRecord::Handoff {
                handoff: handoff.clone(),
            }),
        ];
        let directory = tempfile::tempdir().expect("directory");
        let mut writer =
            TranscriptWriter::create(&sessions_dir(directory.path())).expect("transcript writer");
        for item in &items {
            writer.append(item).expect("append Plan record");
        }

        let loaded = load(writer.path()).expect("load Plan records");
        assert_eq!(loaded, items);
        assert_eq!(model_history(&loaded), vec![handoff.prompt.clone()]);
        assert!(loaded.iter().all(|item| !is_prompt_item(item)));
        assert!(
            loaded[..5]
                .iter()
                .all(|item| !is_compaction_prompt_item(item))
        );
        assert!(retained_user_candidates(&loaded[..5]).is_empty());
        assert!(is_compaction_prompt_item(&loaded[5]));
        assert_eq!(
            retained_user_candidates(&loaded)
                .into_iter()
                .map(Message::user)
                .collect::<Vec<_>>(),
            vec![handoff.prompt]
        );
        let ready = serde_json::to_value(&loaded[2]).expect("serialize Ready");
        assert_eq!(
            ready[PLAN_RECORD_KEY]["artifact"]["markdown"],
            artifact.markdown
        );
        assert!(ready.get("role").is_none());

        let mut conversation = Conversation::new(writer);
        conversation.adopt_persisted(loaded);
        assert!(conversation.has_compaction_prompt());
        assert!(!conversation.has_real_user_prompt());
        assert_eq!(conversation.prompt_position(0), None);
        conversation
            .push_required(TranscriptItem::Message(Message::user(
                "first editable prompt",
            )))
            .expect("append prompt");
        assert_eq!(conversation.prompt_position(0), Some(6));
        assert_eq!(conversation.prompt_position(1), None);
    }

    #[test]
    fn messages_serialize_bare_and_errors_as_error_objects() {
        let message = serde_json::to_value(TranscriptItem::Message(Message::user("hi")))
            .expect("message should serialize");
        assert_eq!(message["role"], "user");

        let error = serde_json::to_value(TranscriptItem::Error {
            error: "boom".to_string(),
        })
        .expect("error should serialize");
        assert_eq!(error, json!({"error": "boom"}));
    }

    #[test]
    fn provider_messages_serialize_as_replay_only_and_round_trip_canonically() {
        let arguments = r#"{"path": "x", "content": "y\n\"z\"", "n": 1.0}"#;
        let items = vec![
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "summary"}],
                "content": [],
                "encrypted_content": "opaque",
                "status": null,
                "future_reasoning_field": {"kept": true}
            }),
            json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "write",
                "arguments": arguments,
                "status": "completed",
                "future_call_field": [1, null, "x"]
            }),
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {"type": "output_text", "text": "one"},
                    {"type": "output_text", "text": "two", "future_text_field": 3}
                ],
                "future_message_field": {"unknown": "survives"}
            }),
        ];
        let replay = ProviderReplay::openai_responses(test_profile(), items.clone());
        let canonical_message = replay.to_message().expect("replay should convert");
        let item = TranscriptItem::provider_message(replay.clone())
            .expect("provider message should derive from replay");

        let line = serde_json::to_string(&item).expect("provider message should serialize");
        let raw: serde_json::Value = serde_json::from_str(&line).expect("line should be JSON");
        assert_eq!(
            raw.as_object().expect("record should be an object").len(),
            1
        );
        assert_eq!(raw, json!({"zevria_provider_replay": replay}));
        assert!(raw.get("role").is_none());
        assert_eq!(raw[PROVIDER_REPLAY_KEY]["provider"], "openai.responses");
        assert_eq!(raw[PROVIDER_REPLAY_KEY]["version"], 1);
        assert_eq!(
            raw[PROVIDER_REPLAY_KEY]["source_profile"],
            json!({"provider": "test-provider", "model": "test-model"})
        );
        assert_eq!(raw[PROVIDER_REPLAY_KEY]["items"], json!(items));

        let restored: TranscriptItem =
            serde_json::from_str(&line).expect("provider message should deserialize");
        assert_eq!(restored, item);
        let replay = restored.provider_replay().expect("replay should survive");
        assert_eq!(replay.items[1]["arguments"], arguments);
        assert_eq!(
            replay.items[2]["future_message_field"]["unknown"],
            "survives"
        );
        assert_eq!(restored.message(), Some(&canonical_message));
    }

    #[test]
    fn mixed_malformed_unsupported_and_contentless_provider_records_are_rejected() {
        let message = Message::assistant("kept answer");
        let valid_replay = json!({
            "provider": "openai.responses",
            "version": 1,
            "items": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer"}]
            }]
        });

        let mut mixed = serde_json::to_value(&message).expect("message should serialize");
        mixed
            .as_object_mut()
            .expect("message should be an object")
            .insert(PROVIDER_REPLAY_KEY.to_string(), valid_replay.clone());
        assert!(serde_json::from_value::<TranscriptItem>(mixed).is_err());

        for envelope in [
            json!("not an envelope"),
            json!({"provider": "future.provider", "version": 1, "items": [{"type": "x"}]}),
            json!({"provider": "openai.responses", "version": 99, "items": [{"type": "x"}]}),
            json!({"provider": "openai.responses", "version": 1, "items": "not an array"}),
            json!({"provider": "openai.responses", "version": 1, "items": []}),
            json!({"provider": "openai.responses", "version": 1, "items": [{
                "type": "message",
                "id": "msg_empty",
                "role": "assistant",
                "status": "completed",
                "content": []
            }]}),
            json!({"provider": "openai.responses", "version": 1, "items": [{
                "type": "future_output_type",
                "payload": true
            }]}),
        ] {
            let line = json!({"zevria_provider_replay": envelope});
            assert!(
                serde_json::from_value::<TranscriptItem>(line).is_err(),
                "invalid provider record should be rejected"
            );
        }

        let ordinary: TranscriptItem =
            serde_json::from_value(serde_json::to_value(&message).unwrap())
                .expect("an ordinary assistant line should still load");
        assert_eq!(ordinary, TranscriptItem::Message(message));
    }

    #[test]
    fn compound_tool_results_are_atomic_and_old_readers_see_a_bare_message() {
        let item = compound_tool_results();
        let line = serde_json::to_string(&item).expect("compound item should serialize");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains(TOOL_RESULT_METADATA_KEY));
        assert!(
            !line.contains(SUBTASK_RESULTS_KEY),
            "an empty completion list must not add the reserved key"
        );

        let restored: TranscriptItem =
            serde_json::from_str(&line).expect("compound item should deserialize");
        assert_eq!(restored, item);

        let old_reader_message: Message =
            serde_json::from_str(&line).expect("Rig should ignore the reserved top-level key");
        let old_reader_json = serde_json::to_string(&old_reader_message).expect("message");
        assert!(old_reader_json.contains("wrote 13 bytes"));
        assert!(!old_reader_json.contains(TOOL_RESULT_METADATA_KEY));
        assert!(!old_reader_json.contains("metadata-only content"));
    }

    #[test]
    fn large_file_change_metadata_round_trips_exactly_in_one_record() {
        let added = "complete added line\n".repeat(512 * 1024 / 20 + 1);
        let deleted = "complete deleted line\n".repeat(512 * 1024 / 22 + 1);
        let context = (0..30_000)
            .map(|index| format!(" context line {index:05}\n"))
            .collect::<String>();
        let unified_diff = format!(
            "--- original\n+++ modified\n@@ -1,30001 +1,30001 @@\n{context}-old value\n+new value\n"
        );
        assert!(added.len() > 512 * 1024);
        assert!(deleted.len() > 512 * 1024);
        assert!(unified_diff.len() > 512 * 1024);

        let item = TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    "large-files",
                    "write",
                    vec![ToolResultContent::text("plain result remains available")],
                )],
            },
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: "large-files".to_string(),
                call_id: None,
                tool_name: "write".to_string(),
                outcome: ToolCallOutcome::Success,
                detail: Some(ToolResultDetail::FileChanges(vec![
                    FileChangeOutput {
                        path: "large-add.txt".into(),
                        change: FileChange::Add {
                            content: added.clone(),
                        },
                    },
                    FileChangeOutput {
                        path: "large-delete.txt".into(),
                        change: FileChange::Delete {
                            content: deleted.clone(),
                        },
                    },
                    FileChangeOutput {
                        path: "large-update.txt".into(),
                        change: FileChange::Update {
                            unified_diff: unified_diff.clone(),
                            move_path: None,
                        },
                    },
                    FileChangeOutput {
                        path: "legacy-binary.dat".into(),
                        change: FileChange::Omitted {
                            operation: FileChangeOperation::Delete,
                            reason: "deleted file content unavailable: invalid utf-8".to_string(),
                            added: 0,
                            removed: 0,
                            bytes: 2,
                        },
                    },
                ])),
            }],
        };

        let line = serde_json::to_string(&item).expect("large record should serialize");
        assert_eq!(line.lines().count(), 1);
        let restored: TranscriptItem =
            serde_json::from_str(&line).expect("large record should deserialize");
        assert_eq!(restored, item);
    }

    #[test]
    fn both_historical_subtask_sidecars_are_rejected_before_message_fallback() {
        for item in [
            TranscriptItem::Message(Message::user("report")),
            compound_tool_results(),
        ] {
            let mut value = serde_json::to_value(item).unwrap();
            value[SUBTASK_RESULTS_KEY] = json!([]);
            assert!(serde_json::from_value::<TranscriptItem>(value.clone()).is_err());
            assert!(serde_json::from_value::<Message>(value).is_ok());
        }
    }

    #[test]
    fn skill_invocations_round_trip_without_serialized_message_mirrors() {
        let invocation = SkillInvocation::new(
            SkillName::parse("commit").expect("name"),
            "ship it",
            SkillApplication::Activate(
                SkillSnapshot::new("commit".parse().unwrap(), "Commit", "Commit body").unwrap(),
            ),
        );
        let expanded = invocation.model_message().clone();
        let item = TranscriptItem::SkillInvocation(invocation);

        let line = serde_json::to_string(&item).expect("record should serialize");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains(SKILL_INVOCATION_KEY));
        assert!(!line.contains(TOOL_RESULT_METADATA_KEY));

        let restored: TranscriptItem =
            serde_json::from_str(&line).expect("record should deserialize");
        assert_eq!(restored, item);

        assert!(serde_json::from_str::<Message>(&line).is_err());
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert!(value[SKILL_INVOCATION_KEY].get("version").is_none());
        assert!(value[SKILL_INVOCATION_KEY].get("id").is_none());
        // Model history carries the canonical bodyless message, never the sidecar.
        let history = model_history(&[restored]);
        assert_eq!(history, vec![expanded]);
    }

    #[test]
    fn skill_invocations_preview_as_their_compact_form() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
        writer
            .append(&TranscriptItem::SkillInvocation(SkillInvocation::new(
                SkillName::parse("commit").expect("name"),
                "ship it",
                SkillApplication::Activate(
                    SkillSnapshot::new("commit".parse().unwrap(), "Commit", "Commit body").unwrap(),
                ),
            )))
            .expect("append should succeed");

        let summaries = list_sessions(dir.path()).expect("directory should list");
        assert_eq!(summaries[0].preview.as_deref(), Some("$commit ship it"));
    }

    #[test]
    fn marker_like_text_never_creates_lifecycle_state() {
        let checkpoint = CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user(format!(
                "{SUMMARY_PREFIX}\nQuoted <skill name=\"review\"> marker"
            )))],
            Vec::new(),
        )
        .expect("checkpoint");
        let items = vec![
            TranscriptItem::Message(Message::user(
                "literal <skill name=\"review\"> partial marker",
            )),
            TranscriptItem::Message(Message::assistant(
                "assistant quote: <skill name=\"review\">nested</skill>",
            )),
            TranscriptItem::ToolResults {
                skill_applications: Vec::new(),
                message: Message::User {
                    content: vec![UserContent::tool_result(
                        "call-noise",
                        "echo",
                        vec![ToolResultContent::text(
                            "<skill name=\"review\"> tool noise",
                        )],
                    )],
                },
                metadata: Vec::new(),
            },
            TranscriptItem::Compaction(checkpoint),
        ];
        assert!(replay_active_skills(&items).expect("replay").is_empty());
    }

    #[test]
    fn markers_inside_one_active_body_do_not_activate_another_name() {
        let snapshot = SkillSnapshot::new(
            SkillName::parse("commit").expect("name"),
            "Commit changes",
            "Follow commit rules. Literal <skill name=\"review\"> is ordinary text.",
        )
        .expect("snapshot");
        let items = vec![TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("commit").expect("name"),
            "ship",
            SkillApplication::Activate(snapshot),
        ))];
        let active = replay_active_skills(&items).expect("replay");
        assert_eq!(active.len(), 1);
        assert_eq!(active.snapshots().next().unwrap().name().as_str(), "commit");
    }

    #[test]
    fn unversioned_invocations_are_rejected_without_reading_their_bodies() {
        for body in ["first body", "different body"] {
            let mut value = serde_json::to_value(Message::user(format!(
                "<skill name=\"review\">\n{body}\n</skill>\n\napply"
            )))
            .unwrap();
            value[SKILL_INVOCATION_KEY] = json!({"name": "review", "args": "apply"});
            assert!(serde_json::from_value::<TranscriptItem>(value).is_err());
        }
    }

    #[test]
    fn successful_skill_result_without_typed_application_is_rejected() {
        let call = TranscriptItem::Message(Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::new_or_mint("skill-call"),
                ToolFunction::new(
                    SKILL_TOOL_NAME.to_string(),
                    json!({"skill": "review", "args": "inspect"}),
                ),
            ))],
        });
        let result = TranscriptItem::ToolResults {
            skill_applications: Vec::new(),
            message: Message::User {
                content: vec![UserContent::tool_result(
                    "skill-call",
                    SKILL_TOOL_NAME,
                    vec![ToolResultContent::text(
                        "<skill name=\"review\">\nReview instructions\n</skill>\n\ninspect",
                    )],
                )],
            },
            metadata: vec![ToolResultMetadata {
                diagnostic: None,
                id: "skill-call".to_string(),
                call_id: None,
                tool_name: SKILL_TOOL_NAME.to_string(),
                outcome: ToolCallOutcome::Success,
                detail: None,
            }],
        };
        let error = replay_active_skills(&[call, result]).unwrap_err();
        assert!(format!("{error:#}").contains("missing its typed application"));
        assert!(!error.to_string().contains("Review instructions"));
    }

    #[test]
    fn checkpoints_require_v1_and_reject_instruction_state() {
        let value = serde_json::to_value(local_checkpoint("summary")).unwrap();
        assert!(value.get("active_skill_identities").is_none());
        assert_eq!(value["version"], 1);
        for version in [0, 2, 3, 4, 5, 6, 7, 999] {
            let mut unsupported = value.clone();
            unsupported["version"] = json!(version);
            assert!(serde_json::from_value::<CompactionCheckpoint>(unsupported).is_err());
        }
        assert!(value.get("instruction_snapshot").is_none());
        let mut unsupported = value;
        unsupported["instruction_snapshot"] = json!({});
        assert!(serde_json::from_value::<CompactionCheckpoint>(unsupported).is_err());
    }

    #[test]
    fn workspace_directories_live_inside_dot_zevria() {
        let workspace = Path::new("workspace");
        let state = zevria_foundation::runtime_paths::workspace_state_root(workspace);
        assert_eq!(sessions_dir(workspace), state.join("sessions"));
        assert_eq!(plans_dir(workspace), state.join("plans"));
    }

    #[test]
    fn subsession_helpers_isolate_child_transcripts_from_the_root_listing() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = sessions_dir(workspace.path());
        let mut root = TranscriptWriter::create(&sessions).expect("root writer");
        root.append(&mode_item()).expect("root prefix");
        root.append(&TranscriptItem::Message(Message::user("root question")))
            .unwrap();
        let root_id = root.session_id().to_string();

        let children = subsessions_dir(workspace.path(), &root_id);
        assert_eq!(
            subsession_files(&children).expect("missing dir is empty"),
            Vec::new()
        );

        let first_id = pick_session_id();
        let mut first =
            TranscriptWriter::create_with_id(&children, &first_id).expect("first child transcript");
        assert_eq!(first.session_id(), first_id);
        assert!(
            TranscriptWriter::create_with_id(&children, &first_id).is_err(),
            "an existing child id must not be truncated or reopened"
        );
        first.append(&mode_item()).expect("child prefix");
        first
            .append(&TranscriptItem::Message(Message::user("child question")))
            .expect("child append");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second_id = pick_session_id();
        TranscriptWriter::create_with_id(&children, &second_id).expect("second child transcript");

        let discovered = subsession_files(&children).expect("children should list");
        assert_eq!(
            discovered
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            [first_id.as_str(), second_id.as_str()]
        );
        let loaded = load(&discovered[0].1).expect("child transcript should load");
        assert_eq!(
            loaded,
            vec![
                mode_item(),
                TranscriptItem::Message(Message::user("child question"))
            ]
        );

        // Child files never surface as resumable root sessions.
        assert_eq!(
            latest_session_file(&sessions)
                .expect("root listing")
                .as_deref(),
            Some(root.path())
        );
    }

    #[test]
    fn model_history_extracts_messages_without_exposing_metadata() {
        let items = vec![
            TranscriptItem::Message(Message::user("question")),
            compound_tool_results(),
            TranscriptItem::Error {
                error: "display only".to_string(),
            },
        ];

        let history = model_history(&items);
        assert_eq!(history.len(), 2);
        let serialized = serde_json::to_string(&history).expect("history should serialize");
        assert!(serialized.contains("wrote 13 bytes"));
        assert!(!serialized.contains(TOOL_RESULT_METADATA_KEY));
        assert!(!serialized.contains("fn main"));
    }

    #[test]
    fn partial_trailing_compound_record_restores_neither_half() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let path = dir.path().join("session.jsonl");
        let compound = serde_json::to_string(&compound_tool_results()).expect("compound line");
        // Interrupt the ordinary result text, before any reserved sidecar key.
        // A tail that has begun a metadata boundary is intentionally rejected.
        let cut = compound.find("wrote 13 bytes").expect("plain result text") + "wrote".len();
        let partial = &compound[..cut];
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{partial}",
                mode_json(),
                serde_json::to_string(&TranscriptItem::Message(Message::user("kept")))
                    .expect("message line")
            ),
        )
        .expect("session file should write");

        assert_eq!(
            load(&path).expect("session should load"),
            vec![mode_item(), TranscriptItem::Message(Message::user("kept"))]
        );
    }

    #[test]
    fn create_names_the_file_after_the_session_id() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let writer = TranscriptWriter::create(dir.path()).expect("writer should create");

        let stem = writer
            .path()
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("session file should have a name");
        assert_eq!(stem, writer.session_id());
        Uuid::parse_str(stem).expect("the session id should be a uuid");
        assert_eq!(
            writer.path().extension().and_then(|e| e.to_str()),
            Some("jsonl")
        );
    }

    #[test]
    fn append_to_reopens_a_session_and_keeps_its_id() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
        writer.append(&mode_item()).expect("prefix");
        writer
            .append(&TranscriptItem::Message(Message::user("first")))
            .expect("append should succeed");
        let (path, session_id) = (writer.path().to_path_buf(), writer.session_id().to_string());
        drop(writer);

        let mut resumed = TranscriptWriter::append_to(path).expect("session should reopen");
        assert_eq!(resumed.session_id(), session_id);
        resumed
            .append(&TranscriptItem::Message(Message::user("second")))
            .expect("append should succeed");

        let loaded = load(resumed.path()).expect("session file should load");
        assert_eq!(
            loaded,
            vec![
                mode_item(),
                TranscriptItem::Message(Message::user("first")),
                TranscriptItem::Message(Message::user("second")),
            ]
        );
    }

    #[test]
    fn rewrite_replaces_the_file_contents_and_keeps_the_session_id() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
        let session_id = writer.session_id().to_string();
        for item in [
            mode_item(),
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::assistant("answer")),
            TranscriptItem::Message(Message::user("second")),
        ] {
            writer.append(&item).expect("append should succeed");
        }

        writer
            .rewrite(&[
                mode_item(),
                TranscriptItem::Message(Message::user("first")),
                TranscriptItem::Error {
                    error: "tail dropped".to_string(),
                },
            ])
            .expect("rewrite should succeed");

        assert_eq!(writer.session_id(), session_id);
        let loaded = load(writer.path()).expect("session file should load");
        assert_eq!(
            loaded,
            vec![
                mode_item(),
                TranscriptItem::Message(Message::user("first")),
                TranscriptItem::Error {
                    error: "tail dropped".to_string()
                },
            ]
        );
        // Appending after a rewrite still appends to the new contents.
        writer
            .append(&TranscriptItem::Message(Message::user("third")))
            .expect("append should succeed");
        let loaded = load(writer.path()).expect("session file should load");
        assert_eq!(loaded.len(), 4);
        // The staging file never outlives a successful rewrite.
        assert!(!writer.path().with_extension("jsonl.rewrite").exists());
    }

    #[test]
    fn a_failed_rewrite_leaves_the_original_file_whole() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let sessions_dir = dir.path().join("sessions");
        let mut writer = TranscriptWriter::create(&sessions_dir).expect("writer should create");
        let original = vec![
            mode_item(),
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::assistant("answer")),
        ];
        for item in &original {
            writer.append(item).expect("append should succeed");
        }

        // Staging succeeds, but replacing the directory at the transcript
        // filename fails without moving any directory containing open files.
        let original_path = writer.path().to_path_buf();
        let original_bytes = std::fs::read(&original_path).unwrap();
        let mut blocker =
            TranscriptRewriteBlocker::new(&original_path).expect("block transcript replacement");
        let error = writer
            .rewrite(&[mode_item(), TranscriptItem::Message(Message::user("first"))])
            .expect_err("rewrite should fail");
        assert!(
            error
                .to_string()
                .contains("failed to replace the session file"),
            "unexpected error: {error:#}"
        );

        assert_eq!(
            std::fs::read(blocker.backup_path()).unwrap(),
            original_bytes
        );
        assert_eq!(load(blocker.backup_path()).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(&sessions_dir).unwrap().count(),
            2,
            "no leaked stage"
        );
        writer
            .append(&TranscriptItem::Message(Message::user("second")))
            .expect("the untouched append handle should remain usable");
        assert_eq!(load(blocker.backup_path()).unwrap().len(), 4);
        blocker.restore().expect("restore transcript filename");
        writer
            .rewrite(&original)
            .expect("rewrite after restoration");
        writer
            .append(&TranscriptItem::Message(Message::user("after repair")))
            .expect("append after restoration");
        assert_eq!(load(&original_path).unwrap().len(), 4);
        assert_eq!(std::fs::read_dir(&sessions_dir).unwrap().count(), 1);
    }

    #[test]
    fn failed_staging_creation_leaves_the_original_file_and_handle_untouched() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let mut writer = TranscriptWriter::create(dir.path()).expect("writer should create");
        let original = vec![
            mode_item(),
            TranscriptItem::Message(Message::user("first")),
            TranscriptItem::Message(Message::assistant("answer")),
        ];
        for item in &original {
            writer.append(item).expect("append should succeed");
        }
        let original_path = writer.path().to_path_buf();
        let original_bytes = std::fs::read(&original_path).unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "block staging creation").unwrap();
        // Only redirect the destination, not the actual transcript or its live
        // handle. Unit-test access keeps this fault out of the production API.
        writer.path = blocker.join("session.jsonl");
        let error = writer.rewrite(&original[..2]).unwrap_err();
        writer.path = original_path.clone();
        assert!(
            error
                .to_string()
                .contains("failed to create a staged session file"),
            "unexpected error: {error:#}"
        );
        assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
        assert_eq!(load(&original_path).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "no leaked stage"
        );
        writer
            .append(&TranscriptItem::Message(Message::user("second")))
            .expect("the untouched append handle should remain usable");
        assert_eq!(load(&original_path).unwrap().len(), 4);
        writer
            .rewrite(&original)
            .expect("rewrite after restoring destination");
        writer
            .append(&TranscriptItem::Message(Message::user("after repair")))
            .expect("append after rewrite");
        assert_eq!(load(&original_path).unwrap().len(), 4);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn failed_completed_batch_keeps_the_whole_batch_in_memory_and_marks_degradation() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let sessions_dir = dir.path().join("sessions");
        let writer = TranscriptWriter::create(&sessions_dir).expect("writer should create");
        let original_path = writer.path().to_path_buf();
        let mut conversation = Conversation::new(writer);
        conversation.push_required(mode_item()).expect("prefix");
        let original = TranscriptItem::Message(Message::user("durable request"));
        conversation
            .push_required(original.clone())
            .expect("persist original item");

        let original_bytes = std::fs::read(&original_path).unwrap();
        let mut blocker =
            TranscriptRewriteBlocker::new(&original_path).expect("block transcript replacement");
        let batch = vec![
            TranscriptItem::Message(Message::assistant("completed response")),
            TranscriptItem::Error {
                error: "terminal metadata".to_string(),
            },
        ];

        let error = conversation
            .push_completed_batch(batch.clone())
            .expect_err("atomic replacement should fail");

        assert!(error.to_string().contains("session transcript is degraded"));
        assert!(conversation.persistence_error().is_some());
        assert_eq!(
            conversation.items(),
            &[mode_item(), original, batch[0].clone(), batch[1].clone()]
        );
        assert_eq!(
            load(blocker.backup_path()).expect("original durable file remains readable"),
            vec![
                mode_item(),
                TranscriptItem::Message(Message::user("durable request"))
            ]
        );
        assert_eq!(
            std::fs::read(blocker.backup_path()).unwrap(),
            original_bytes
        );
        blocker.restore().expect("restore transcript filename");
        assert!(conversation.ensure_durable().unwrap());
        assert!(conversation.persistence_error().is_none());
        assert_eq!(load(&original_path).unwrap(), conversation.items());
    }

    #[test]
    fn skill_recovery_preserves_malformed_interior_lines_for_inspection() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            "{\"error\":\"first\"}\nnot json\n\n{\"error\":\"second\"}\n{\"role\":\"user\",\"cont",
        )
        .expect("session file should write");

        let original = std::fs::read(&path).expect("original bytes");
        assert!(load(&path).is_err());
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        let error = load_report(&path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<UnsupportedHistory>().unwrap().line,
            Some(2)
        );
        assert_eq!(std::fs::read(&path).expect("preserved bytes"), original);
    }

    #[test]
    fn recognized_stale_provider_replay_is_a_hard_resume_error() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("stale-replay.jsonl");
        let stale = json!({
            (PROVIDER_REPLAY_KEY): {
                "provider": OPENAI_RESPONSES_PROVIDER,
                "version": 2,
                "source_profile": {"provider":"p", "model":"m"},
                "items": [{
                    "type": "message",
                    "id": "msg_old",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "old"}]
                }]
            }
        });
        std::fs::write(&path, format!("{stale}\n")).expect("write stale replay");

        let error = load(&path).expect_err("v2 replay must not be silently dropped");
        let detail = format!("{error:#}");
        assert!(detail.contains("unsupported history"));
        assert!(detail.contains("provider replay v1"));
        assert!(TranscriptWriter::append_to(path).is_err());
    }

    #[test]
    fn current_replay_without_source_profile_is_a_hard_resume_error() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("missing-source.jsonl");
        let missing_source = json!({
            (PROVIDER_REPLAY_KEY): {
                "provider": OPENAI_RESPONSES_PROVIDER,
                "version": PROVIDER_REPLAY_VERSION,
                "items": [{
                    "type": "message",
                    "id": "msg_missing_source",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "old"}]
                }]
            }
        });
        std::fs::write(&path, format!("{missing_source}\n")).expect("write source-less replay");

        let error = load(&path).expect_err("source identity is required");
        let detail = format!("{error:#}");
        assert!(detail.contains("nonblank source_profile"));
        assert!(detail.contains("unsupported history"));
    }

    #[test]
    fn reopening_recovers_only_an_incomplete_final_record() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let path = dir.path().join("recovered.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{{\"error\":\"first\"}}\n{{\"error\":\"second\"}}\n{{\"role\":\"user\",\"cont",
                mode_json()
            ),
        )
        .expect("session file should write");

        let writer = TranscriptWriter::append_to(path.clone()).expect("session should recover");
        assert_eq!(writer.recovered_malformed_lines(), 1);
        assert_eq!(
            load(&path).expect("canonical transcript should load"),
            vec![
                mode_item(),
                TranscriptItem::Error {
                    error: "first".to_string(),
                },
                TranscriptItem::Error {
                    error: "second".to_string(),
                },
            ]
        );
        let canonical = std::fs::read_to_string(path).expect("canonical contents");
        assert_eq!(canonical.lines().count(), 3);
        assert!(!canonical.contains("not json"));
    }

    #[test]
    fn skill_recovery_unknown_reserved_records_and_tampering_are_never_removed() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("blocked.jsonl");
        let snapshot = SkillSnapshot::new(
            SkillName::parse("review").expect("name"),
            "Review",
            "Exact body",
        )
        .expect("snapshot");
        let invocation = TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("review").expect("name"),
            "",
            SkillApplication::Activate(snapshot),
        ));
        let mut tampered = serde_json::to_value(&invocation).expect("value");
        tampered[SKILL_INVOCATION_KEY]["application"]["activate"]["body"] = json!("Different body");
        let mut unknown_invocation = serde_json::to_value(&invocation).expect("value");
        unknown_invocation[SKILL_INVOCATION_KEY]["version"] = json!(999);
        let mut checkpoint =
            serde_json::to_value(TranscriptItem::Compaction(local_checkpoint("summary")))
                .expect("checkpoint");
        checkpoint[COMPACTION_RECORD_KEY]["version"] = json!(999);
        for value in [
            json!({"zevria_skill_activation_v999": {}}),
            json!({"role": "user", "content": [], "zevria_skill_activation_v999": {}}),
            unknown_invocation,
            tampered,
            checkpoint,
        ] {
            let original = format!("{{\"error\":\"visible\"}}\n{value}\n{{\"partial\":");
            std::fs::write(&path, original.as_bytes()).expect("write fixture");
            let error = load_report(&path).unwrap_err();
            assert_eq!(
                error.downcast_ref::<UnsupportedHistory>().unwrap().line,
                Some(2)
            );
            assert!(load(&path).is_err());
            assert!(TranscriptWriter::append_to(path.clone()).is_err());
            assert_eq!(std::fs::read(&path).expect("read"), original.as_bytes());
        }
    }

    #[test]
    fn current_writes_do_not_create_upgrade_backups() {
        use crate::skill::{SkillDefinition, SkillMetadata, SkillSource};
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("mixed.jsonl");
        let original = format!(
            "{}\n{{ \"error\" : \"noncanonical formatting\" }}\n",
            mode_json()
        );
        std::fs::write(&path, original).expect("fixture");
        let snapshot = SkillDefinition::new(
            SkillName::parse("review").expect("name"),
            "Review",
            "Exact body",
            SkillSource::Programmatic("test".into()),
        )
        .expect("definition")
        .with_metadata(SkillMetadata::new("Review"), None)
        .expect("metadata")
        .snapshot();
        let checkpoint = CompactionCheckpoint::new(
            CompactionTrigger::Manual,
            CompactionBackend::LocalSummary,
            vec![OwnedModelRequestItem::message(Message::user("summary"))],
            vec![],
        )
        .expect("v1 checkpoint");
        let mut items = load(&path).expect("current prefix");
        items.extend([
            TranscriptItem::SkillInvocation(SkillInvocation::new(
                SkillName::parse("review").expect("name"),
                "",
                SkillApplication::Activate(snapshot),
            )),
            TranscriptItem::Compaction(checkpoint),
        ]);
        let mut writer = TranscriptWriter::append_to(path.clone()).expect("open");
        assert!(!path.with_extension("jsonl.pre-v3").exists());
        writer.rewrite(&items).expect("upgrade");
        assert!(!path.with_extension("jsonl.pre-v3").exists());
        let upgraded = std::fs::read(&path).expect("upgraded bytes");
        assert_eq!(load(&path).expect("mixed history"), items);
        TranscriptWriter::append_to(path.clone()).expect("resume mixed history");
        assert_eq!(std::fs::read(&path).expect("unchanged on open"), upgraded);
        if let TranscriptItem::Compaction(checkpoint) = items.last_mut().expect("checkpoint") {
            checkpoint.version = 2;
        }
        assert!(
            replay_active_skills(&items).is_err(),
            "a body-only checkpoint cannot assert extended state"
        );
    }

    #[test]
    fn saved_upgrade_backups_are_not_consulted_or_modified() {
        use crate::skill::{SkillDefinition, SkillMetadata, SkillSource};
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("blocked-upgrade.jsonl");
        let original = format!("{}\n{{\"error\":\"unchanged\"}}\n", mode_json());
        std::fs::write(&path, original).expect("fixture");
        let snapshot = SkillDefinition::new(
            SkillName::parse("review").expect("name"),
            "Review",
            "Body",
            SkillSource::Programmatic("test".into()),
        )
        .expect("definition")
        .with_metadata(SkillMetadata::new("Review"), None)
        .expect("metadata")
        .snapshot();
        let item = TranscriptItem::SkillInvocation(SkillInvocation::new(
            snapshot.name().clone(),
            "",
            SkillApplication::Activate(snapshot),
        ));
        std::fs::write(path.with_extension("jsonl.pre-v3"), "conflicting backup")
            .expect("block backup");
        let mut writer = TranscriptWriter::append_to(path.clone()).expect("writer");
        writer.append(&item).unwrap();
        assert_eq!(
            std::fs::read(path.with_extension("jsonl.pre-v3")).unwrap(),
            b"conflicting backup"
        );
    }

    #[test]
    fn unsupported_history_cannot_be_opened_even_read_only() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("blocked.jsonl");
        let original = b"{\"error\":\"inspect\"}\n{\"zevria_skill_activation_v999\":{}}\n";
        std::fs::write(&path, original).expect("fixture");
        assert!(load_report(&path).is_err());
        assert!(TranscriptWriter::read_only(path.clone()).is_err());
        assert!(TranscriptWriter::append_to(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).expect("unchanged"), original);
    }

    #[test]
    fn skill_recovery_plain_marker_text_is_not_a_reserved_record() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("plain.jsonl");
        let item = TranscriptItem::Message(Message::user(
            "zevria_skill_activation_v999 and {\"zevria_skill_invocation\":{\"version\":999}}",
        ));
        let original = format!(
            "{}\n{}",
            mode_json(),
            serde_json::to_string(&item).expect("json")
        );
        std::fs::write(&path, &original).expect("write without final newline");
        let mut writer = TranscriptWriter::append_to(path.clone()).expect("plain message");
        assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), original);
        writer.append(&item).expect("newline-separated append");
        assert_eq!(
            load(&path).expect("load"),
            vec![mode_item(), item.clone(), item]
        );
    }

    #[test]
    fn completed_work_survives_an_append_failure_and_repairs_before_new_work() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        let writer = TranscriptWriter::create(dir.path()).expect("writer should create");
        let path = writer.path().to_path_buf();
        let mut conversation = Conversation::new(writer);
        conversation.push_required(mode_item()).expect("prefix");
        conversation
            .push_required(TranscriptItem::Message(Message::user("question")))
            .expect("initial prompt should persist");

        // Replace only the writer handle with a read-only descriptor. The
        // path remains replaceable, so the subsequent full rewrite can heal
        // the simulated append failure exactly as a later submission would.
        conversation.writer.file = std::fs::File::open(&path).expect("read-only handle");
        let completed = TranscriptItem::Message(Message::assistant("completed answer"));
        conversation
            .push_completed(completed.clone())
            .expect_err("the completed append should fail");
        assert_eq!(
            conversation.items(),
            &[
                mode_item(),
                TranscriptItem::Message(Message::user("question")),
                completed
            ]
        );
        assert!(conversation.persistence_error().is_some());
        assert_eq!(
            load(&path).expect("old disk prefix stays readable").len(),
            2
        );

        assert!(
            conversation
                .ensure_durable()
                .expect("full-history repair should succeed")
        );
        assert!(conversation.persistence_error().is_none());
        assert_eq!(
            load(&path).expect("repaired transcript"),
            conversation.items()
        );
        conversation
            .push_required(TranscriptItem::Message(Message::user("next question")))
            .expect("new work may proceed after repair");
        assert_eq!(load(&path).expect("extended transcript").len(), 4);
    }

    #[test]
    fn latest_session_file_prefers_the_most_recently_modified() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        assert_eq!(
            latest_session_file(&dir.path().join("missing")).expect("missing dir should be fine"),
            None
        );

        let mut first = TranscriptWriter::create(dir.path()).expect("first writer should create");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = TranscriptWriter::create(dir.path()).expect("second writer should create");
        assert_eq!(
            latest_session_file(dir.path())
                .expect("directory should list")
                .as_deref(),
            None
        );
        assert!(is_abandoned_root(second.path()));

        // Appending bumps the first session's mtime, making it the latest
        // again; non-session files are ignored regardless of their mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        first
            .append(&TranscriptItem::Error {
                error: "bump".to_string(),
            })
            .expect("append should succeed");
        std::fs::write(dir.path().join("notes.txt"), "not a session")
            .expect("decoy file should write");
        assert_eq!(
            latest_session_file(dir.path())
                .expect("directory should list")
                .as_deref(),
            Some(first.path())
        );
    }

    #[test]
    fn list_sessions_orders_newest_first_and_skips_empty_files() {
        let dir = tempfile::tempdir().expect("temp dir should create");
        assert_eq!(
            list_sessions(&dir.path().join("missing")).expect("missing dir should be fine"),
            Vec::new()
        );

        let mut first = TranscriptWriter::create(dir.path()).expect("first writer should create");
        first
            .append(&TranscriptItem::Message(Message::user("first prompt")))
            .expect("append should succeed");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut second = TranscriptWriter::create(dir.path()).expect("second writer should create");
        second
            .append(&TranscriptItem::Message(Message::user("second prompt")))
            .expect("append should succeed");
        // A created-then-abandoned session stays empty and must not be listed.
        let abandoned = TranscriptWriter::create(dir.path()).expect("third writer should create");
        std::fs::write(dir.path().join("notes.txt"), "not a session")
            .expect("decoy file should write");

        let summaries = list_sessions(dir.path()).expect("directory should list");
        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.id.as_str())
                .collect::<Vec<_>>(),
            [second.session_id(), first.session_id()]
        );
        assert_eq!(summaries[0].path, second.path());
        assert_eq!(summaries[0].preview.as_deref(), Some("second prompt"));
        assert_eq!(summaries[1].preview.as_deref(), Some("first prompt"));
        assert!(summaries[0].modified >= summaries[1].modified);
        assert!(
            !summaries
                .iter()
                .any(|summary| summary.id == abandoned.session_id())
        );
    }

    #[test]
    fn invocation_and_application_tails_are_never_recoverable_debris() {
        for tail in [
            format!("{{\"{SKILL_INVOCATION_KEY}\":{{\"name\":\"review\""),
            format!("{{\"{SKILL_INVOCATION_KEY}\":{{\"version\":4,\"invocation\":"),
            format!("{{\"{SKILL_APPLICATIONS_KEY}\":["),
        ] {
            assert!(has_unsupported_reserved_tail(tail.as_bytes()));
        }
    }

    #[test]
    fn ensemble_image_previews_use_actual_commands_without_raster_decoding() {
        let directory = tempfile::tempdir().unwrap();
        for workflow in [
            crate::EnsembleWorkflow::Plan,
            crate::EnsembleWorkflow::Review,
        ] {
            let path = directory.path().join("preview.jsonl");
            // Picker projection is intentionally independent of image validation;
            // opening this corrupt history still rejects at the durable boundary.
            std::fs::write(&path, serde_json::json!({(ENSEMBLE_RECORD_KEY): {"start": {"workflow": workflow, "prompt": [{"type":"image", "value":{"mime_type":"image/png", "data":"not a valid bitmap"}}]}}}).to_string()).unwrap();
            assert_eq!(
                session_preview(&path).unwrap(),
                format!("{} [image]", workflow.slash_command())
            );
        }
    }

    #[test]
    fn session_previews_use_the_first_user_prompt_flattened_and_truncated() {
        let dir = tempfile::tempdir().expect("temp dir should create");

        // Leading engine traffic (an error, a compound tool result) is skipped
        // in favor of the first plain user prompt; only its first line shows.
        let mut with_noise = TranscriptWriter::create(dir.path()).expect("writer should create");
        with_noise
            .append(&TranscriptItem::Error {
                error: "transient".to_string(),
            })
            .expect("append should succeed");
        with_noise
            .append(&compound_tool_results())
            .expect("append should succeed");
        with_noise
            .append(&TranscriptItem::Message(Message::user(
                "multi line prompt\nsecond line",
            )))
            .expect("append should succeed");

        let long_prompt = "x".repeat(PREVIEW_MAX_CHARS + 10);
        let mut with_long_prompt =
            TranscriptWriter::create(dir.path()).expect("writer should create");
        with_long_prompt
            .append(&TranscriptItem::Message(Message::user(long_prompt.clone())))
            .expect("append should succeed");

        // A transcript with no user prompt in it stays listed, preview-less.
        let mut without_prompt =
            TranscriptWriter::create(dir.path()).expect("writer should create");
        without_prompt
            .append(&TranscriptItem::Error {
                error: "only an error".to_string(),
            })
            .expect("append should succeed");

        let summaries = list_sessions(dir.path()).expect("directory should list");
        let preview_of = |writer: &TranscriptWriter| {
            summaries
                .iter()
                .find(|summary| summary.id == writer.session_id())
                .expect("session should be listed")
                .preview
                .clone()
        };
        assert_eq!(
            preview_of(&with_noise).as_deref(),
            Some("multi line prompt")
        );
        assert_eq!(
            preview_of(&with_long_prompt),
            Some(format!("{}…", "x".repeat(PREVIEW_MAX_CHARS)))
        );
        assert_eq!(preview_of(&without_prompt), None);
    }

    #[test]
    fn version_one_compaction_round_trips_in_its_reserved_envelope() {
        let item = TranscriptItem::Compaction(local_checkpoint("summary"));
        let value = serde_json::to_value(&item).expect("serialize checkpoint");
        assert_eq!(value["zevria_compaction"]["version"], 1);
        assert!(
            value["zevria_compaction"]
                .get("active_skill_identities")
                .is_none()
        );
        assert!(
            value["zevria_compaction"]
                .get("active_skill_keys")
                .is_none()
        );
        assert_eq!(
            serde_json::from_value::<TranscriptItem>(value).expect("deserialize checkpoint"),
            item
        );
    }

    #[test]
    fn newest_checkpoint_replaces_only_model_history_not_the_transcript() {
        let first = local_checkpoint("first summary");
        let second = local_checkpoint("second summary");
        let items = vec![
            TranscriptItem::Message(Message::user("original prompt")),
            TranscriptItem::Message(Message::assistant("original answer")),
            TranscriptItem::Compaction(first),
            TranscriptItem::Message(Message::user("between checkpoints")),
            TranscriptItem::Compaction(second),
            TranscriptItem::Message(Message::assistant("suffix")),
        ];

        assert_eq!(items.len(), 6, "the readable transcript stays complete");
        assert_eq!(
            model_history(&items),
            vec![
                Message::user("retained prompt"),
                Message::user(format!("{SUMMARY_PREFIX}\nsecond summary")),
                Message::assistant("suffix"),
            ]
        );
    }

    #[test]
    fn plan_handoff_retention_is_bounded_and_carried_forward_once_after_reload() {
        use crate::compaction::{local_replacement_history, select_recent_user_messages};

        for budget in [0, 64, 10_000] {
            let mut artifact = sample_plan_handoff().artifact;
            artifact
                .markdown
                .push_str(&"\nDetailed approved instruction.".repeat(300));
            artifact.markdown.push_str("\napproved ending");
            let handoff = PlanHandoff::new(artifact, "source-session");
            let directory = tempfile::tempdir().expect("directory");
            let writer = TranscriptWriter::create(directory.path()).expect("writer");
            let path = writer.path().to_path_buf();
            let mut conversation = Conversation::new(writer);
            conversation.push_required(mode_item()).expect("prefix");
            let run_id = crate::EnsembleRunId::new();
            for item in [
                TranscriptItem::Message(Message::user("old instruction")),
                TranscriptItem::Plan(PlanRecord::Handoff {
                    handoff: handoff.clone(),
                }),
                TranscriptItem::Message(Message::user("new instruction")),
                TranscriptItem::Message(Message::user(format!(
                    "{SUMMARY_PREFIX}\nnot an instruction"
                ))),
                TranscriptItem::Ensemble(EnsembleRecord::Started {
                    start: crate::EnsembleStart {
                        run_id: run_id.clone(),
                        workflow: crate::EnsembleWorkflow::Review,
                        prompt: "review".into(),
                        agents: vec![crate::AgentRunDescriptor {
                            id: crate::AgentRunId::new(),
                            agent: "worker".into(),
                            label: "Worker".into(),
                            safe_mode: "read-only".into(),
                        }],
                    },
                }),
                TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                    run_id,
                    synthesis_input: Message::user("worker evidence is not a retained instruction"),
                    agents: Vec::new(),
                }),
                compound_tool_results(),
            ] {
                conversation.push_required(item).expect("seed history");
            }
            let candidates = conversation.retained_user_candidates();
            assert_eq!(
                candidates.iter().map(Message::user).collect::<Vec<_>>(),
                vec![
                    Message::user("old instruction"),
                    handoff.prompt.clone(),
                    Message::user("new instruction")
                ]
            );
            let mut retained = select_recent_user_messages(&candidates, budget);
            match budget {
                0 => assert!(retained.is_empty()),
                64 => {
                    assert_eq!(retained.len(), 2);
                    assert!(retained[0].starts_with("Implement the approved plan"));
                    assert!(retained[0].ends_with("approved ending"));
                    assert!(retained[0].contains("tokens truncated"));
                    assert!(retained[0].len() < candidates[1].len());
                    assert_eq!(retained[1], "new instruction");
                }
                _ => assert_eq!(retained, candidates),
            }

            for summary in ["first summary", "second summary"] {
                let replacement = local_replacement_history(&retained, summary);
                let checkpoint = CompactionCheckpoint::new(
                    CompactionTrigger::Manual,
                    CompactionBackend::LocalSummary,
                    replacement.clone(),
                    retained.clone(),
                )
                .expect("checkpoint");
                conversation
                    .push_required(TranscriptItem::Compaction(checkpoint))
                    .expect("persist checkpoint");
                drop(conversation);
                let loaded = load(&path).expect("reload history");
                conversation = Conversation::new(
                    TranscriptWriter::append_to(path.clone()).expect("reopen writer"),
                );
                conversation.adopt_persisted(loaded);
                assert_eq!(conversation.retained_user_candidates(), retained);
                assert_eq!(
                    conversation.model_input(),
                    replacement
                        .iter()
                        .map(OwnedModelRequestItem::as_borrowed)
                        .collect::<Vec<_>>()
                );
                assert!(matches!(
                    &conversation.items()[2],
                    TranscriptItem::Plan(PlanRecord::Handoff { handoff: durable }) if durable == &handoff
                ));
                if summary == "first summary" {
                    conversation
                        .push_required(TranscriptItem::Message(Message::user(
                            "follow-up instruction",
                        )))
                        .expect("append follow-up");
                    retained.push("follow-up instruction".to_string());
                    // A larger next budget must not resurrect the full handoff
                    // or duplicate text from before the latest checkpoint.
                    assert_eq!(
                        select_recent_user_messages(
                            &conversation.retained_user_candidates(),
                            10_000
                        ),
                        retained
                    );
                }
            }
        }
    }

    #[test]
    fn opaque_remote_compaction_output_projects_verbatim_without_a_message() {
        let replay = ProviderReplay::openai_responses(
            test_profile(),
            vec![json!({
                "type": "compaction",
                "encrypted_content": "opaque"
            })],
        );
        let checkpoint = CompactionCheckpoint::new(
            CompactionTrigger::AutomaticPreTurn,
            CompactionBackend::OpenaiResponsesCompact,
            vec![OwnedModelRequestItem::replay_only(replay.clone()).expect("opaque replay")],
            vec!["question".to_string()],
        )
        .expect("checkpoint");
        let items = vec![TranscriptItem::Compaction(checkpoint)];

        assert!(model_history(&items).is_empty());
        assert_eq!(
            model_input(&items),
            vec![ModelRequestItem::replay_only(&replay)]
        );
    }

    #[test]
    fn editing_before_a_checkpoint_drops_it_while_editing_after_preserves_it() {
        fn conversation() -> (tempfile::TempDir, Conversation) {
            let directory = tempfile::tempdir().expect("temp directory");
            let writer = TranscriptWriter::create(directory.path()).expect("writer");
            let mut conversation = Conversation::new(writer);
            conversation
                .push_required(TranscriptItem::Message(Message::user("before")))
                .expect("first prompt");
            conversation
                .push_required(TranscriptItem::Compaction(local_checkpoint("summary")))
                .expect("checkpoint");
            conversation
                .push_required(TranscriptItem::Message(Message::user("after")))
                .expect("second prompt");
            (directory, conversation)
        }

        let (_directory, mut before) = conversation();
        let first = before.prompt_position(0).expect("first prompt");
        before
            .replace_from(
                first,
                TranscriptItem::Message(Message::user("edited first")),
            )
            .expect("edit before checkpoint");
        assert!(before.latest_compaction().is_none());

        let (_directory, mut after) = conversation();
        let second = after.prompt_position(1).expect("second prompt");
        after
            .replace_from(
                second,
                TranscriptItem::Message(Message::user("edited second")),
            )
            .expect("edit after checkpoint");
        assert!(after.latest_compaction().is_some());
    }

    #[test]
    fn ensemble_evidence_is_compactable_but_not_an_editable_prompt_row() {
        let item = TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
            run_id: crate::EnsembleRunId::new(),
            synthesis_input: Message::user("bounded worker evidence"),
            agents: Vec::new(),
        });

        assert!(!is_prompt_item(&item));
        assert!(is_compaction_prompt_item(&item));
        assert!(retained_user_candidates(&[item]).is_empty());
    }

    #[test]
    fn empty_and_tool_result_only_histories_have_no_compaction_anchor() {
        let directory = tempfile::tempdir().expect("directory");
        let writer = TranscriptWriter::create(directory.path()).expect("writer");
        let mut conversation = Conversation::new(writer);
        assert!(!conversation.has_compaction_prompt());
        assert!(conversation.retained_user_candidates().is_empty());

        conversation
            .push_required(compound_tool_results())
            .expect("tool results");
        assert!(!conversation.has_compaction_prompt());
        assert!(!conversation.has_real_user_prompt());
        assert_eq!(conversation.prompt_position(0), None);
        assert!(conversation.retained_user_candidates().is_empty());
    }

    #[test]
    fn first_direct_invocation_prompt_ordinal_anchors_its_owned_pin() {
        let directory = tempfile::tempdir().expect("directory");
        let writer = TranscriptWriter::create(directory.path()).expect("writer");
        let mut conversation = Conversation::new(writer);
        conversation
            .push_required_batch(vec![
                TranscriptItem::Message(Message::user("first prompt")),
                TranscriptItem::SkillInvocation(SkillInvocation::new(
                    SkillName::parse("review").expect("name"),
                    "inspect",
                    SkillApplication::Activate(
                        SkillSnapshot::new(
                            "review".parse().unwrap(),
                            "Review changes",
                            "Review instructions",
                        )
                        .unwrap(),
                    ),
                )),
            ])
            .expect("fixture");

        assert_eq!(conversation.prompt_position(0), Some(0));
        assert_eq!(conversation.prompt_position(1), Some(1));
        assert!(is_prompt_item(&conversation.items()[1]));
    }

    #[test]
    fn retained_prefix_replay_deactivates_only_when_activation_is_removed() {
        let invocation = TranscriptItem::SkillInvocation(SkillInvocation::new(
            SkillName::parse("review").expect("name"),
            "inspect",
            SkillApplication::Activate(
                SkillSnapshot::new(
                    "review".parse().unwrap(),
                    "Review changes",
                    "Review instructions",
                )
                .unwrap(),
            ),
        ));
        let items = vec![
            TranscriptItem::Message(Message::user("before")),
            invocation,
            TranscriptItem::Message(Message::assistant("after activation")),
            TranscriptItem::Message(Message::user("later prompt")),
        ];
        assert!(
            replay_active_skills(&items[..1])
                .expect("before")
                .is_empty()
        );
        assert_eq!(
            replay_active_skills(&items[..3]).expect("retained").len(),
            1
        );
        assert_eq!(replay_active_skills(&items[..4]).expect("after").len(), 1);
        // The visible invocation row anchors index 1, so replacing it retains
        // only `items[..1]` and makes the name eligible again.
        let directory = tempfile::tempdir().expect("directory");
        let writer = TranscriptWriter::create(directory.path()).expect("writer");
        let mut conversation = Conversation::new(writer);
        conversation.adopt_persisted(items);
        assert_eq!(conversation.prompt_position(1), Some(1));
        assert_eq!(conversation.prompt_position(2), Some(3));
    }

    #[test]
    fn ensemble_start_lookup_uses_run_ids_without_changing_prompt_numbering() {
        let directory = tempfile::tempdir().expect("temp directory");
        let writer = TranscriptWriter::create(directory.path()).expect("writer");
        let mut conversation = Conversation::new(writer);
        let first_run = EnsembleRunId::from_string("first-run");
        let second_run = EnsembleRunId::from_string("second-run");
        let items = vec![
            TranscriptItem::Message(Message::user("first prompt")),
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: crate::EnsembleStart {
                    run_id: first_run.clone(),
                    workflow: crate::EnsembleWorkflow::Review,
                    prompt: "first review".into(),
                    agents: Vec::new(),
                },
            }),
            TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                run_id: first_run.clone(),
                synthesis_input: Message::user("evidence"),
                agents: Vec::new(),
            }),
            TranscriptItem::Message(Message::user("second prompt")),
            TranscriptItem::Ensemble(EnsembleRecord::Started {
                start: crate::EnsembleStart {
                    run_id: second_run.clone(),
                    workflow: crate::EnsembleWorkflow::Plan,
                    prompt: "second plan".into(),
                    agents: Vec::new(),
                },
            }),
        ];
        conversation
            .push_required_batch(items)
            .expect("fixture persists");

        assert_eq!(conversation.ensemble_start_position(&first_run), Some(1));
        assert_eq!(conversation.ensemble_start_position(&second_run), Some(4));
        assert_eq!(
            conversation.ensemble_start_position(&EnsembleRunId::from_string("missing")),
            None
        );
        assert_eq!(conversation.prompt_position(0), Some(0));
        assert_eq!(conversation.prompt_position(1), Some(3));
        assert_eq!(conversation.prompt_position(2), None);
        assert!(is_compaction_prompt_item(&conversation.items()[2]));
        assert!(!is_prompt_item(&conversation.items()[1]));
        assert!(!is_prompt_item(&conversation.items()[4]));
    }
}
