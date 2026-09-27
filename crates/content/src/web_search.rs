//! Model-inert provider presentation. Native replay remains the sole authority
//! for successful messages, model input and executable tool calls.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Remove terminal escape sequences and directional controls while preserving
/// readable line/word boundaries. Citation title sanitization remains separate.
pub fn sanitize_readable(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        let escape = match c {
            '\u{1b}' => chars.next(),
            '\u{9b}' => Some('['),
            '\u{9d}' => Some(']'),
            _ => None,
        };
        if let Some(escape) = escape {
            match escape {
                '[' => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                ']' | 'P' | '^' | '_' => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if (c == '\n' || c == '\t' || !c.is_control())
            && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
        {
            result.push(c);
        }
    }
    result
}

/// Readable native reasoning fields only; unknown/opaque object types remain
/// exclusively in replay even if they happen to contain a `text` property.
pub fn readable_reasoning_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| match value.get("type").and_then(Value::as_str) {
            Some("summary_text" | "reasoning_text" | "text") => {
                value.get("text").and_then(Value::as_str)
            }
            _ => None,
        })
}

/// Validated display address. Malformed indices never alias output zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostedSearchAddress<'a> {
    pub attempt: &'a str,
    pub output_index: u64,
}

impl<'a> HostedSearchAddress<'a> {
    pub fn parse(id: &'a str) -> Option<Self> {
        let (attempt, index) = id.strip_prefix("hosted-search:")?.rsplit_once(':')?;
        if attempt.is_empty()
            || index.is_empty()
            || !index.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            attempt,
            output_index: index.parse().ok()?,
        })
    }
}

pub fn is_hosted_search_input(id: &str, raw_input: Option<&Value>) -> bool {
    HostedSearchAddress::parse(id).is_some()
        && raw_input.is_some_and(|raw| raw["origin"] == "provider_hosted_web_search")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchStatus {
    InProgress,
    Searching,
    Completed,
    Failed,
    Interrupted,
}
impl WebSearchStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::InProgress => "in progress",
            Self::Searching => "searching",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchActivity {
    pub item_id: Option<String>,
    pub output_index: u64,
    pub status: WebSearchStatus,
    /// Retained for forward-compatible observation, never rendered as JSON.
    pub action: Option<Value>,
}
impl WebSearchActivity {
    pub fn action_kind(&self) -> Option<&str> {
        self.action.as_ref()?.get("type")?.as_str()
    }
    pub fn action_label(&self) -> &'static str {
        match self.action_kind() {
            Some("search") => "Web search",
            Some("open_page") => "Open page",
            Some("find_in_page") => "Find in page",
            _ => "Web action",
        }
    }
    pub fn details(&self) -> Vec<String> {
        let mut details = Vec::new();
        let mut push = |value: &str| {
            let value = sanitize_readable(value);
            if !value.trim().is_empty() && !details.contains(&value) {
                details.push(value);
            }
        };
        if let Some(action) = &self.action {
            // An explicitly ordered queries array is authoritative for order.
            if let Some(queries) = action.get("queries").and_then(Value::as_array) {
                for query in queries.iter().filter_map(Value::as_str) {
                    push(query);
                }
            }
            for key in ["query", "url", "pattern"] {
                if let Some(value) = action.get(key).and_then(Value::as_str) {
                    push(value);
                }
            }
        }
        details
    }
    pub fn label(&self) -> String {
        let details = self.details();
        if details.is_empty() {
            self.action_label().into()
        } else {
            format!("{}: {}", self.action_label(), details.join(" · "))
        }
    }
    pub fn client_id(&self, attempt_id: &str) -> String {
        format!("hosted-search:{attempt_id}:{}", self.output_index)
    }
}

/// A provider address, before any ordinary message projection flattens parts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantSourceAddress {
    pub output_index: u64,
    pub part: AssistantPartIdentity,
    pub item_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantPartIdentity {
    Summary(u64),
    Content(u64),
    Tool,
}

/// Readable display text only. Opaque reasoning belongs exclusively to replay.
/// A native tool binding is not a call and cannot be dispatched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssistantPresentationContent {
    Reasoning { text: String },
    Answer { text: String },
    NativeTool { call_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantPresentationPart {
    pub source: AssistantSourceAddress,
    pub content: AssistantPresentationContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchAttemptOutcome {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}
impl WebSearchAttemptOutcome {
    pub fn is_terminal(self) -> bool {
        self != Self::InProgress
    }
}

pub const WEB_SEARCH_ATTEMPT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchAttemptRecord {
    pub version: u32,
    pub id: String,
    pub profile: crate::ModelProfileRef,
    pub response_id: Option<String>,
    pub outcome: WebSearchAttemptOutcome,
    pub activity: Vec<WebSearchActivity>,
    pub revision: u64,
    pub presentation: Vec<AssistantPresentationPart>,
    /// The readable presentation lives in a linked native replay until restoration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub presentation_elided: bool,
    /// Explicit provider terminal evidence, independent of attempt closure.
    #[serde(deserialize_with = "deserialize_terminal_evidence")]
    pub terminal: BTreeMap<u64, WebSearchStatus>,
}
// Internally tagged enums buffer JSON object keys as strings. Decode them
// explicitly rather than depending on serde_json's direct integer-key adapter.
// A visitor (not an intermediate map) also retains duplicate-key evidence.
fn deserialize_terminal_evidence<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<u64, WebSearchStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct TerminalEvidence;

    impl<'de> serde::de::Visitor<'de> for TerminalEvidence {
        type Value = BTreeMap<u64, WebSearchStatus>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object of canonical unsigned decimal terminal indexes")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            use serde::de::Error as _;
            let mut terminal = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                let index = key
                    .parse::<u64>()
                    .ok()
                    .filter(|index| index.to_string() == key)
                    .ok_or_else(|| M::Error::custom(
                        "invalid web action terminal index; expected canonical unsigned decimal u64",
                    ))?;
                if terminal.contains_key(&index) {
                    return Err(M::Error::custom("duplicate web action terminal index"));
                }
                terminal.insert(index, map.next_value()?);
            }
            Ok(terminal)
        }
    }

    deserializer.deserialize_map(TerminalEvidence)
}

impl WebSearchAttemptRecord {
    pub fn new(profile: crate::ModelProfileRef) -> Self {
        Self {
            version: WEB_SEARCH_ATTEMPT_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            profile,
            response_id: None,
            outcome: WebSearchAttemptOutcome::InProgress,
            activity: Vec::new(),
            revision: 0,
            presentation: Vec::new(),
            presentation_elided: false,
            terminal: BTreeMap::new(),
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == WEB_SEARCH_ATTEMPT_VERSION,
            "unsupported web search attempt version; expected {WEB_SEARCH_ATTEMPT_VERSION}"
        );
        validate_display_id(&self.id)?;
        validate_display_id(&self.profile.provider)?;
        validate_display_id(&self.profile.model)?;
        if let Some(id) = &self.response_id {
            validate_display_id(id)?;
        }
        anyhow::ensure!(
            self.activity.len() <= 4096 && self.presentation.len() <= 16384,
            "response display has too many parts"
        );
        anyhow::ensure!(
            !self.presentation_elided || self.presentation.is_empty(),
            "elided web search presentation must be empty"
        );
        anyhow::ensure!(
            !self.presentation_elided || self.outcome == WebSearchAttemptOutcome::Completed,
            "only completed web search attempts can elide presentation"
        );
        let mut actions = HashSet::new();
        for action in &self.activity {
            anyhow::ensure!(
                action.output_index <= 1_000_000,
                "invalid action output index"
            );
            if let Some(id) = &action.item_id {
                validate_display_id(id)?;
            }
            if let Some(action) = &action.action {
                anyhow::ensure!(
                    serde_json::to_vec(action)?.len() <= 4 * 1024 * 1024,
                    "action payload exceeds limit"
                );
            }
            anyhow::ensure!(
                actions.insert(action.output_index),
                "duplicate web action output index"
            );
        }
        for (&index, status) in &self.terminal {
            anyhow::ensure!(
                actions.contains(&index) && status.is_terminal(),
                "invalid web action terminal evidence"
            );
        }
        let mut parts = HashSet::new();
        for part in &self.presentation {
            anyhow::ensure!(
                part.source.output_index <= 1_000_000,
                "invalid presentation output index"
            );
            anyhow::ensure!(
                parts.insert((part.source.output_index, part.source.part)),
                "duplicate presentation source address"
            );
            if let Some(id) = &part.source.item_id {
                validate_display_id(id)?;
            }
            if let AssistantPartIdentity::Summary(index) | AssistantPartIdentity::Content(index) =
                part.source.part
            {
                anyhow::ensure!(index <= 1_000_000, "invalid presentation part index");
            }
            match &part.content {
                AssistantPresentationContent::Reasoning { text }
                | AssistantPresentationContent::Answer { text } => {
                    anyhow::ensure!(
                        text.len() <= 4 * 1024 * 1024,
                        "response display part exceeds limit"
                    );
                    anyhow::ensure!(
                        !sanitize_readable(text).trim().is_empty()
                            && part.source.part != AssistantPartIdentity::Tool,
                        "display text must be readable and have a text-part address"
                    );
                    if matches!(part.content, AssistantPresentationContent::Answer { .. }) {
                        anyhow::ensure!(
                            matches!(part.source.part, AssistantPartIdentity::Content(_)),
                            "answer requires a content-part address"
                        );
                    }
                }
                AssistantPresentationContent::NativeTool { call_id } => {
                    validate_display_id(call_id)?;
                    anyhow::ensure!(
                        part.source.part == AssistantPartIdentity::Tool,
                        "invalid native tool address"
                    );
                }
            }
        }
        Ok(())
    }
    /// Reconstruct presentation from an already validated native output ledger.
    /// Never converts this display trail back into canonical messages.
    pub fn reconcile_native_presentation(&mut self, items: &[Value]) {
        use rig_core::message::{AdditionalParams, Text};
        self.presentation.clear();
        for (output_index, item) in items.iter().enumerate() {
            let output_index = output_index as u64;
            let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
            let mut push = |part, content| {
                self.presentation.push(AssistantPresentationPart {
                    source: AssistantSourceAddress {
                        output_index,
                        part,
                        item_id: item_id.clone(),
                    },
                    content,
                })
            };
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => {
                    for (field, summary) in [("summary", true), ("content", false)] {
                        for (index, part) in item
                            .get(field)
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .enumerate()
                        {
                            if let Some(text) = readable_reasoning_value(part) {
                                let text = sanitize_readable(text);
                                if !text.trim().is_empty() {
                                    push(
                                        if summary {
                                            AssistantPartIdentity::Summary(index as u64)
                                        } else {
                                            AssistantPartIdentity::Content(index as u64)
                                        },
                                        AssistantPresentationContent::Reasoning { text },
                                    );
                                }
                            }
                        }
                    }
                }
                Some("message") => {
                    for (index, part) in item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        let text = match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => part.get("text").and_then(Value::as_str),
                            Some("refusal") => part.get("refusal").and_then(Value::as_str),
                            _ => None,
                        };
                        if let Some(text) = text {
                            let mut text = Text::new(text);
                            if let Some(annotations) =
                                part.get("annotations").and_then(Value::as_array)
                            {
                                text.additional_params = AdditionalParams::from_entries([(
                                    "openai_responses",
                                    serde_json::json!({"annotations": annotations}),
                                )]);
                            }
                            let text = sanitize_readable(&crate::citations::render_text(&text));
                            if !text.trim().is_empty() {
                                push(
                                    AssistantPartIdentity::Content(index as u64),
                                    AssistantPresentationContent::Answer { text },
                                );
                            }
                        }
                    }
                }
                Some("function_call") => {
                    if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                        push(
                            AssistantPartIdentity::Tool,
                            AssistantPresentationContent::NativeTool {
                                call_id: call_id.into(),
                            },
                        );
                    }
                }
                Some("web_search_call") => {
                    let status = match item.get("status").and_then(Value::as_str) {
                        Some("completed") => Some(WebSearchStatus::Completed),
                        Some("failed") => Some(WebSearchStatus::Failed),
                        Some("incomplete" | "cancelled") => Some(WebSearchStatus::Interrupted),
                        _ => None,
                    };
                    if let Some(status) = status {
                        self.terminal.insert(output_index, status);
                    }
                    let action = WebSearchActivity {
                        output_index,
                        item_id,
                        status: status.unwrap_or(WebSearchStatus::InProgress),
                        action: item.get("action").cloned(),
                    };
                    if let Some(existing) = self
                        .activity
                        .iter_mut()
                        .find(|action| action.output_index == output_index)
                    {
                        *existing = action;
                    } else {
                        self.activity.push(action);
                    }
                }
                _ => {}
            }
        }
        self.presentation_elided = false;
    }

    /// Elide a completed presentation when its native replay commits atomically.
    /// Storage-only compaction must not advance the live display revision.
    pub fn compact_presentation(&mut self) {
        self.presentation.clear();
        self.presentation_elided = true;
    }

    pub fn has_display(&self) -> bool {
        self.presentation_elided || !self.activity.is_empty() || !self.presentation.is_empty()
    }
    pub fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    pub fn finish(&mut self, outcome: WebSearchAttemptOutcome) {
        if self.outcome.is_terminal() || !outcome.is_terminal() {
            return;
        }
        self.outcome = outcome;
        self.touch();
    }
    pub fn confirmed_status(&self, action: &WebSearchActivity) -> Option<WebSearchStatus> {
        self.terminal.get(&action.output_index).copied()
    }
    pub fn status_label(&self, action: &WebSearchActivity) -> &'static str {
        if let Some(status) = self.confirmed_status(action) {
            status.label()
        } else if self.outcome.is_terminal() {
            "completion unconfirmed"
        } else {
            action.status.label()
        }
    }
}

pub fn validate_display_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control),
        "invalid response display identity"
    );
    Ok(())
}

pub const RESPONSE_DISPLAY_META_KEY: &str = "zevria.response_display";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayProjectionKind {
    Message,
    Thought,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DisplayProjectionBinding {
    Text {
        kind: DisplayProjectionKind,
        message_id: String,
        start: usize,
        end: usize,
        sources: Vec<AssistantSourceAddress>,
    },
    Tool {
        tool_call_id: String,
        output_index: u64,
        native: bool,
    },
}

/// Notification-level ACP extension. Byte ranges address the accumulated UTF-8
/// standard projection, not one chunk. Standard events are always retained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseDisplay {
    pub version: u32,
    pub attempt: WebSearchAttemptRecord,
    pub bindings: Vec<DisplayProjectionBinding>,
}
impl ResponseDisplay {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 1 && self.attempt.version == WEB_SEARCH_ATTEMPT_VERSION,
            "unsupported response display version"
        );
        self.attempt.validate()?;
        anyhow::ensure!(self.bindings.len() <= 16384, "too many display bindings");
        let mut ranges = BTreeMap::<(u8, &str), Vec<(usize, usize)>>::new();
        for binding in &self.bindings {
            match binding {
                DisplayProjectionBinding::Text {
                    kind,
                    message_id,
                    start,
                    end,
                    sources,
                } => {
                    validate_display_id(message_id)?;
                    anyhow::ensure!(
                        !sources.is_empty()
                            && sources.len() <= 16384
                            && start < end
                            && *end <= 16 * 1024 * 1024,
                        "invalid projection range"
                    );
                    anyhow::ensure!(
                        sources.iter().collect::<HashSet<_>>().len() == sources.len(),
                        "duplicate projection source"
                    );
                    let expected = self
                        .binding_text(binding)
                        .ok_or_else(|| anyhow::anyhow!("invalid projection sources"))?;
                    anyhow::ensure!(
                        expected.len() == end - start,
                        "projection range does not match covered content"
                    );
                    let prior = ranges
                        .entry((
                            if *kind == DisplayProjectionKind::Message {
                                0
                            } else {
                                1
                            },
                            message_id,
                        ))
                        .or_default();
                    anyhow::ensure!(
                        !prior.iter().any(|(a, b)| *a < *end && *start < *b),
                        "overlapping projection bindings"
                    );
                    prior.push((*start, *end));
                }
                DisplayProjectionBinding::Tool {
                    tool_call_id,
                    output_index,
                    native,
                } => {
                    validate_display_id(tool_call_id)?;
                    let valid = if *native {
                        self.attempt.presentation.iter().any(|part| part.source.output_index == *output_index && matches!(&part.content, AssistantPresentationContent::NativeTool { call_id } if call_id == tool_call_id))
                    } else {
                        self.attempt.activity.iter().any(|action| {
                            action.output_index == *output_index
                                && action.client_id(&self.attempt.id) == *tool_call_id
                        })
                    };
                    anyhow::ensure!(valid, "invalid tool projection binding");
                }
            }
        }
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= 16 * 1024 * 1024,
            "response display payload exceeds limit"
        );
        Ok(())
    }
    pub fn binding_text(&self, binding: &DisplayProjectionBinding) -> Option<String> {
        let DisplayProjectionBinding::Text { kind, sources, .. } = binding else {
            return None;
        };
        sources
            .iter()
            .map(|source| {
                self.attempt
                    .presentation
                    .iter()
                    .find(|part| &part.source == source)
                    .and_then(|part| match (&part.content, kind) {
                        (
                            AssistantPresentationContent::Answer { text },
                            DisplayProjectionKind::Message,
                        )
                        | (
                            AssistantPresentationContent::Reasoning { text },
                            DisplayProjectionKind::Thought,
                        ) => Some(text.as_str()),
                        _ => None,
                    })
            })
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join("\n"))
    }
}

/// A present activity-only value is different from a cleared stream (`None`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssistantStreamSnapshot {
    pub message: Option<rig_core::message::Message>,
    pub attempt: Option<WebSearchAttemptRecord>,
}
impl From<rig_core::message::Message> for AssistantStreamSnapshot {
    fn from(message: rig_core::message::Message) -> Self {
        Self {
            message: Some(message),
            attempt: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum TaggedAttempt {
        Display { attempt: WebSearchAttemptRecord },
    }

    fn attempt_with_terminal_evidence() -> WebSearchAttemptRecord {
        let mut attempt = WebSearchAttemptRecord::new(crate::ModelProfileRef::new("p", "m"));
        attempt.activity.push(WebSearchActivity {
            item_id: Some("search".into()),
            output_index: 1,
            status: WebSearchStatus::Completed,
            action: None,
        });
        attempt.terminal.insert(1, WebSearchStatus::Completed);
        attempt
    }

    #[test]
    fn terminal_evidence_round_trips_directly_and_under_internal_tag() {
        let attempt = attempt_with_terminal_evidence();
        attempt.validate().unwrap();
        assert_eq!(attempt.outcome, WebSearchAttemptOutcome::InProgress);
        let bytes = serde_json::to_vec(&attempt).unwrap();
        assert_eq!(
            attempt,
            serde_json::from_slice::<WebSearchAttemptRecord>(&bytes).unwrap()
        );
        let wrapped = TaggedAttempt::Display { attempt };
        let bytes = serde_json::to_vec(&wrapped).unwrap();
        assert_eq!(
            wrapped,
            serde_json::from_slice::<TaggedAttempt>(&bytes).unwrap()
        );
    }

    #[test]
    fn terminal_evidence_keeps_decimal_wire_keys_and_numeric_order() {
        let mut attempt = attempt_with_terminal_evidence();
        for (output_index, status) in [
            (11, WebSearchStatus::Failed),
            (2, WebSearchStatus::Interrupted),
        ] {
            attempt.activity.push(WebSearchActivity {
                item_id: None,
                output_index,
                status,
                action: None,
            });
            attempt.terminal.insert(output_index, status);
        }
        attempt.validate().unwrap();
        let json = serde_json::to_string(&attempt).unwrap();
        assert!(json.contains(r#""terminal":{"1":"completed","2":"interrupted","11":"failed"}"#));
        assert_eq!(
            attempt,
            serde_json::from_str::<WebSearchAttemptRecord>(&json).unwrap()
        );
        let wrapped = TaggedAttempt::Display { attempt };
        assert_eq!(
            wrapped,
            serde_json::from_slice::<TaggedAttempt>(&serde_json::to_vec(&wrapped).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn terminal_evidence_rejects_invalid_and_duplicate_keys_at_both_boundaries() {
        let json = serde_json::to_string(&attempt_with_terminal_evidence()).unwrap();
        for terminal in [
            r#"{"":"completed"}"#,
            r#"{"-1":"completed"}"#,
            r#"{"+1":"completed"}"#,
            r#"{"01":"completed"}"#,
            r#"{"00":"completed"}"#,
            r#"{"1.0":"completed"}"#,
            r#"{"1e0":"completed"}"#,
            r#"{" 1":"completed"}"#,
            r#"{"1 ":"completed"}"#,
            r#"{"one":"completed"}"#,
            r#"{"١":"completed"}"#,
            r#"{"18446744073709551616":"completed"}"#,
            r#"{"1":"completed","1":"failed"}"#,
            r#"{"1":"completed","\u0031":"completed"}"#,
        ] {
            // Use bytes, not Value: intermediate maps would erase duplicates.
            let raw = json.replace(
                r#""terminal":{"1":"completed"}"#,
                &format!(r#""terminal":{terminal}"#),
            );
            let wrapped = format!(r#"{{"type":"display","attempt":{raw}}}"#);
            for error in [
                serde_json::from_str::<WebSearchAttemptRecord>(&raw).unwrap_err(),
                serde_json::from_str::<TaggedAttempt>(&wrapped).unwrap_err(),
            ] {
                assert!(
                    error.to_string().contains("web action terminal index"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn terminal_evidence_is_required_and_semantically_validated() {
        let mut value = serde_json::to_value(attempt_with_terminal_evidence()).unwrap();
        value["terminal"] = serde_json::json!({});
        let attempt: WebSearchAttemptRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(attempt.terminal.is_empty());
        attempt.validate().unwrap();
        let wrapped: TaggedAttempt =
            serde_json::from_value(serde_json::json!({"type":"display", "attempt":value})).unwrap();
        assert_eq!(wrapped, TaggedAttempt::Display { attempt });
        for field in ["revision", "presentation", "terminal"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<WebSearchAttemptRecord>(missing.clone()).is_err());
            assert!(
                serde_json::from_value::<TaggedAttempt>(
                    serde_json::json!({"type":"display", "attempt":missing})
                )
                .is_err()
            );
        }
        for terminal in [
            serde_json::json!({"1":"searching"}),
            serde_json::json!({"1":"in_progress"}),
            serde_json::json!({"11":"completed"}),
            serde_json::json!({"18446744073709551615":"completed"}),
        ] {
            value["terminal"] = terminal;
            let attempt: WebSearchAttemptRecord = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                attempt.validate().unwrap_err().to_string(),
                "invalid web action terminal evidence"
            );
        }
        value["terminal"] = serde_json::json!({"1":"unknown_status"});
        assert!(serde_json::from_value::<WebSearchAttemptRecord>(value).is_err());
    }

    #[test]
    fn current_attempt_and_display_versions_are_one_and_reject_other_versions() {
        let attempt = attempt_with_terminal_evidence();
        assert_eq!(serde_json::to_value(&attempt).unwrap()["version"], 1);
        let display = ResponseDisplay {
            version: 1,
            attempt: attempt.clone(),
            bindings: vec![],
        };
        display.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&display).unwrap()["attempt"]["version"],
            1
        );
        for version in [0, 2, 99] {
            let mut invalid = display.clone();
            invalid.version = version;
            assert!(invalid.validate().is_err());
            let mut invalid = display.clone();
            invalid.attempt.version = version;
            assert!(invalid.attempt.validate().is_err());
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn action_status_alone_never_supplies_terminal_evidence() {
        let mut attempt = attempt_with_terminal_evidence();
        attempt.terminal.clear();
        assert_eq!(attempt.confirmed_status(&attempt.activity[0]), None);
        attempt.finish(WebSearchAttemptOutcome::Completed);
        assert_eq!(
            attempt.status_label(&attempt.activity[0]),
            "completion unconfirmed"
        );
        attempt.terminal.insert(1, WebSearchStatus::Failed);
        assert_eq!(attempt.status_label(&attempt.activity[0]), "failed");
    }

    #[test]
    fn presentation_elision_defaults_to_false_and_is_omitted_when_full() {
        let attempt = attempt_with_terminal_evidence();
        assert!(!attempt.presentation_elided);
        let value = serde_json::to_value(&attempt).unwrap();
        assert!(value.get("presentation_elided").is_none());
        assert_eq!(
            serde_json::from_value::<WebSearchAttemptRecord>(value).unwrap(),
            attempt
        );
    }

    #[test]
    fn presentation_elision_requires_empty_presentation_and_completed_outcome() {
        let mut attempt = attempt_with_terminal_evidence();
        attempt.presentation_elided = true;
        for outcome in [
            WebSearchAttemptOutcome::InProgress,
            WebSearchAttemptOutcome::Failed,
            WebSearchAttemptOutcome::Interrupted,
        ] {
            attempt.outcome = outcome;
            assert_eq!(
                attempt.validate().unwrap_err().to_string(),
                "only completed web search attempts can elide presentation"
            );
        }
        attempt.outcome = WebSearchAttemptOutcome::Completed;
        attempt.validate().unwrap();
        attempt.presentation.push(AssistantPresentationPart {
            source: AssistantSourceAddress {
                output_index: 0,
                part: AssistantPartIdentity::Summary(0),
                item_id: Some("reasoning".into()),
            },
            content: AssistantPresentationContent::Reasoning {
                text: "readable evidence".into(),
            },
        });
        assert_eq!(
            attempt.validate().unwrap_err().to_string(),
            "elided web search presentation must be empty"
        );
    }

    #[test]
    fn compact_presentation_preserves_evidence_and_reconciliation_materializes_it() {
        let mut attempt = attempt_with_terminal_evidence();
        let items = vec![serde_json::json!({
            "type":"message", "id":"answer", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"readable answer", "annotations":[]}]
        })];
        attempt.reconcile_native_presentation(&items);
        attempt.finish(WebSearchAttemptOutcome::Completed);
        let full = attempt.clone();
        attempt.compact_presentation();
        attempt.validate().unwrap();
        assert!(attempt.presentation.is_empty());
        assert!(attempt.presentation_elided);
        assert_eq!(attempt.revision, full.revision);
        let mut expected = full.clone();
        expected.presentation.clear();
        expected.presentation_elided = true;
        assert_eq!(attempt, expected);
        attempt.compact_presentation();
        assert_eq!(attempt, expected, "compaction is idempotent");
        attempt.reconcile_native_presentation(&items);
        assert_eq!(attempt, full);
        attempt.validate().unwrap();
    }

    #[test]
    fn elided_presentation_still_has_display_without_activity() {
        let mut attempt = WebSearchAttemptRecord::new(crate::ModelProfileRef::new("p", "m"));
        assert!(!attempt.has_display());
        attempt.finish(WebSearchAttemptOutcome::Completed);
        attempt.compact_presentation();
        assert!(attempt.has_display());
        attempt.validate().unwrap();
        attempt.reconcile_native_presentation(&[]);
        assert!(!attempt.presentation_elided);
        assert!(!attempt.has_display());
    }

    #[test]
    fn closure_does_not_manufacture_provider_evidence() {
        let mut attempt = WebSearchAttemptRecord::new(crate::ModelProfileRef::new("p", "m"));
        attempt.activity.push(WebSearchActivity {
            item_id: None,
            output_index: 0,
            status: WebSearchStatus::Searching,
            action: None,
        });
        attempt.finish(WebSearchAttemptOutcome::Failed);
        assert_eq!(attempt.activity[0].status, WebSearchStatus::Searching);
        assert_eq!(
            attempt.status_label(&attempt.activity[0]),
            "completion unconfirmed"
        );
        attempt.terminal.insert(0, WebSearchStatus::Completed);
        assert_eq!(attempt.status_label(&attempt.activity[0]), "completed");
    }
    #[test]
    fn queries_keep_supplied_order_and_full_sanitized_details() {
        let activity = WebSearchActivity {
            item_id: None,
            output_index: 0,
            status: WebSearchStatus::Searching,
            action: Some(
                serde_json::json!({"type":"search", "query":"second", "queries":["first", "second", "first"], "pattern":"日本語\u{1b}[31m"}),
            ),
        };
        assert_eq!(activity.details(), vec!["first", "second", "日本語"]);
    }
}
