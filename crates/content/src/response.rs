//! Model-inert provider presentation. Native replay remains the sole authority
//! for successful messages, model input and executable tool calls.

use crate::citations::{
    self, CitedSource, MAX_SOURCE_TITLE_CHARS, MAX_SOURCE_URL_BYTES, MAX_SOURCES,
};
use crate::readable::{readable_reasoning_value, sanitize_readable};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;

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
pub enum HostedActivityStatus {
    InProgress,
    Searching,
    Completed,
    Failed,
    Interrupted,
}
impl HostedActivityStatus {
    pub fn from_native_item_status(status: Option<&str>) -> Self {
        match status {
            Some("completed") => Self::Completed,
            Some("failed") => Self::Failed,
            Some("incomplete" | "cancelled") => Self::Interrupted,
            Some("searching") => Self::Searching,
            _ => Self::InProgress,
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedActivityKind {
    WebSearch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedActivity {
    pub kind: HostedActivityKind,
    pub item_id: Option<String>,
    pub output_index: u64,
    pub status: HostedActivityStatus,
    /// Retained for forward-compatible observation, never rendered as JSON.
    pub action: Option<Value>,
}
impl HostedActivity {
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
    pub fn queries(&self) -> Vec<String> {
        let Some(action) = &self.action else {
            return Vec::new();
        };
        let mut queries = Vec::new();
        for query in action
            .get("queries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(action.get("query"))
            .filter_map(Value::as_str)
            .map(sanitize_readable)
        {
            if !query.trim().is_empty() && !queries.contains(&query) {
                queries.push(query);
            }
        }
        queries
    }
    fn raw_detail(&self, key: &str) -> Option<&str> {
        self.action.as_ref()?.get(key)?.as_str()
    }
    fn readable_detail(&self, key: &str) -> Option<String> {
        self.raw_detail(key)
            .map(sanitize_readable)
            .filter(|value| !value.trim().is_empty())
    }
    pub fn url(&self) -> Option<String> {
        self.readable_detail("url")
    }
    /// Check the original URL, not the sanitized display string: stripping a
    /// control character must not turn a rejected URL into an openable one.
    pub fn safe_url(&self) -> Option<String> {
        citations::safe_http_url(self.raw_detail("url")?)
    }
    pub fn pattern(&self) -> Option<String> {
        self.readable_detail("pattern")
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
    Reasoning {
        text: String,
    },
    Answer {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<CitedSource>,
    },
    NativeTool {
        call_id: String,
    },
}

impl AssistantPresentationContent {
    pub fn answer(text: impl Into<String>) -> Self {
        Self::Answer {
            text: text.into(),
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantPresentationPart {
    pub source: AssistantSourceAddress,
    pub content: AssistantPresentationContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseAttemptOutcome {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}
impl ResponseAttemptOutcome {
    pub fn is_terminal(self) -> bool {
        self != Self::InProgress
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseAttempt {
    pub id: String,
    pub profile: crate::ModelProfileRef,
    pub response_id: Option<String>,
    pub outcome: ResponseAttemptOutcome,
    pub revision: u64,
    pub presentation: Vec<AssistantPresentationPart>,
    pub hosted_activity: Vec<HostedActivity>,
}

impl ResponseAttempt {
    pub fn new(profile: crate::ModelProfileRef) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            profile,
            response_id: None,
            outcome: ResponseAttemptOutcome::InProgress,
            hosted_activity: Vec::new(),
            revision: 0,
            presentation: Vec::new(),
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        Self::validate_parts(
            &self.id,
            &self.profile,
            self.response_id.as_deref(),
            &self.hosted_activity,
            &self.presentation,
        )
    }

    pub fn validate_parts(
        id: &str,
        profile: &crate::ModelProfileRef,
        response_id: Option<&str>,
        hosted_activity: &[HostedActivity],
        presentation: &[AssistantPresentationPart],
    ) -> anyhow::Result<()> {
        validate_display_id(id)?;
        validate_display_id(&profile.provider)?;
        validate_display_id(&profile.model)?;
        if let Some(id) = response_id {
            validate_display_id(id)?;
        }
        anyhow::ensure!(
            hosted_activity.len() <= 4096 && presentation.len() <= 16384,
            "response display has too many parts"
        );
        let mut actions = HashSet::new();
        for action in hosted_activity {
            anyhow::ensure!(
                action.output_index <= 1_000_000,
                "invalid action output index"
            );
            if let Some(id) = &action.item_id {
                validate_display_id(id)?;
            }
            if let Some(action) = &action.action {
                anyhow::ensure!(
                    zevria_foundation::BoundedCountingWriter::json_fits(action, 4 * 1024 * 1024)?,
                    "action payload exceeds limit"
                );
            }
            anyhow::ensure!(
                actions.insert(action.output_index),
                "duplicate web action output index"
            );
        }
        let mut parts = HashSet::new();
        for part in presentation {
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
                | AssistantPresentationContent::Answer { text, .. } => {
                    anyhow::ensure!(
                        text.len() <= 4 * 1024 * 1024,
                        "response display part exceeds limit"
                    );
                    anyhow::ensure!(
                        !sanitize_readable(text).trim().is_empty()
                            && part.source.part != AssistantPartIdentity::Tool,
                        "display text must be readable and have a text-part address"
                    );
                    if let AssistantPresentationContent::Answer { sources, .. } = &part.content {
                        anyhow::ensure!(
                            matches!(part.source.part, AssistantPartIdentity::Content(_)),
                            "answer requires a content-part address"
                        );
                        anyhow::ensure!(
                            sources.len() <= MAX_SOURCES,
                            "answer has too many sources"
                        );
                        let mut urls = HashSet::new();
                        for source in sources {
                            anyhow::ensure!(
                                source.url.len() <= MAX_SOURCE_URL_BYTES
                                    && citations::safe_http_url(&source.url).as_deref()
                                        == Some(source.url.as_str()),
                                "invalid source URL"
                            );
                            anyhow::ensure!(urls.insert(&source.url), "duplicate source URL");
                            if let Some(title) = &source.title {
                                anyhow::ensure!(
                                    !title.trim().is_empty()
                                        && title.chars().count() <= MAX_SOURCE_TITLE_CHARS
                                        && citations::sanitize_display(title) == *title,
                                    "invalid source title"
                                );
                            }
                        }
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
    pub fn has_display(&self) -> bool {
        !self.hosted_activity.is_empty() || !self.presentation.is_empty()
    }
    pub fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    /// Closure is sticky. It never manufactures hosted-action completion.
    pub fn close(&mut self, outcome: ResponseAttemptOutcome) {
        if self.outcome.is_terminal() || !outcome.is_terminal() {
            return;
        }
        self.outcome = outcome;
        self.touch();
    }
    /// A successful provider response discarded by core is still uncommitted.
    pub fn close_uncommitted(&mut self, fallback: ResponseAttemptOutcome) {
        debug_assert!(matches!(
            fallback,
            ResponseAttemptOutcome::Failed | ResponseAttemptOutcome::Interrupted
        ));
        if matches!(
            self.outcome,
            ResponseAttemptOutcome::InProgress | ResponseAttemptOutcome::Completed
        ) {
            self.outcome = fallback;
            self.touch();
        }
    }
    pub fn complete_with(
        &mut self,
        presentation: Vec<AssistantPresentationPart>,
        hosted: Vec<HostedActivity>,
    ) {
        self.presentation = presentation;
        self.hosted_activity = hosted;
        self.outcome = ResponseAttemptOutcome::Completed;
        self.touch();
    }
    pub fn action_display_status(&self, action: &HostedActivity) -> &'static str {
        if action.status.is_terminal() || !self.outcome.is_terminal() {
            action.status.label()
        } else {
            "completion unconfirmed"
        }
    }
}

/// Pure display projection of the authoritative Responses native output ledger.
/// Native replay, model input and executable calls are never mutated.
pub fn responses_native_display(
    items: &[Value],
) -> (Vec<AssistantPresentationPart>, Vec<HostedActivity>) {
    let mut presentation = Vec::new();
    let mut hosted_activity = Vec::new();
    for (output_index, item) in items.iter().enumerate() {
        let output_index = output_index as u64;
        let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
        let mut push = |part, content| {
            presentation.push(AssistantPresentationPart {
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
                    let kind = part.get("type").and_then(Value::as_str);
                    let text = match kind {
                        Some("output_text") => part.get("text").and_then(Value::as_str),
                        Some("refusal") => part.get("refusal").and_then(Value::as_str),
                        _ => None,
                    };
                    if let Some(text) = text {
                        let text = sanitize_readable(&crate::citations::render(
                            text,
                            part.get("annotations"),
                        ));
                        if !text.trim().is_empty() {
                            let content = if kind == Some("output_text") {
                                AssistantPresentationContent::Answer {
                                    text,
                                    sources: citations::sources(part.get("annotations")),
                                }
                            } else {
                                AssistantPresentationContent::answer(text)
                            };
                            push(AssistantPartIdentity::Content(index as u64), content);
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
                let status = HostedActivityStatus::from_native_item_status(
                    item.get("status").and_then(Value::as_str),
                );
                let action = HostedActivity {
                    kind: HostedActivityKind::WebSearch,
                    output_index,
                    item_id,
                    status,
                    action: item.get("action").cloned(),
                };
                hosted_activity.push(action);
            }
            _ => {}
        }
    }
    (presentation, hosted_activity)
}

pub fn validate_display_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control),
        "invalid response display identity"
    );
    Ok(())
}

pub const RESPONSE_DISPLAY_VERSION: u32 = 2;
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
    pub attempt: ResponseAttempt,
    pub bindings: Vec<DisplayProjectionBinding>,
}
/// A borrowed source-address index shared by validation, projection, and replay.
pub struct DisplayParts<'a>(
    std::collections::HashMap<&'a AssistantSourceAddress, &'a AssistantPresentationContent>,
);

impl<'a> DisplayParts<'a> {
    pub fn text(
        &self,
        kind: DisplayProjectionKind,
        source: &AssistantSourceAddress,
    ) -> Option<&'a str> {
        match (self.0.get(source)?, kind) {
            (AssistantPresentationContent::Answer { text, .. }, DisplayProjectionKind::Message)
            | (AssistantPresentationContent::Reasoning { text }, DisplayProjectionKind::Thought) => {
                Some(text)
            }
            _ => None,
        }
    }

    pub fn binding_text(&self, binding: &DisplayProjectionBinding) -> Option<String> {
        let DisplayProjectionBinding::Text { kind, sources, .. } = binding else {
            return None;
        };
        let mut text = String::new();
        for (index, source) in sources.iter().enumerate() {
            if index != 0 {
                text.push('\n');
            }
            text.push_str(self.text(*kind, source)?);
        }
        Some(text)
    }
}

impl ResponseDisplay {
    pub fn parts(&self) -> DisplayParts<'_> {
        DisplayParts(
            self.attempt
                .presentation
                .iter()
                .map(|part| (&part.source, &part.content))
                .collect(),
        )
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_and_parts().map(|_| ())
    }

    pub fn validate_and_parts(&self) -> anyhow::Result<DisplayParts<'_>> {
        anyhow::ensure!(
            self.version == RESPONSE_DISPLAY_VERSION,
            "unsupported response display version"
        );
        self.attempt.validate()?;
        anyhow::ensure!(self.bindings.len() <= 16384, "too many display bindings");
        let parts = self.parts();
        let mut ranges =
            std::collections::HashMap::<(DisplayProjectionKind, &str), Vec<(usize, usize)>>::new();
        let native_tools: HashSet<_> = self
            .attempt
            .presentation
            .iter()
            .filter_map(|part| {
                let AssistantPresentationContent::NativeTool { call_id } = &part.content else {
                    return None;
                };
                Some((part.source.output_index, call_id.as_str()))
            })
            .collect();
        let hosted_actions: std::collections::HashMap<_, _> = self
            .attempt
            .hosted_activity
            .iter()
            .map(|action| (action.output_index, action))
            .collect();
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
                    let mut seen = HashSet::new();
                    let mut length = sources.len() - 1;
                    for source in sources {
                        anyhow::ensure!(seen.insert(source), "duplicate projection source");
                        let text = parts
                            .text(*kind, source)
                            .ok_or_else(|| anyhow::anyhow!("invalid projection sources"))?;
                        length = length
                            .checked_add(text.len())
                            .ok_or_else(|| anyhow::anyhow!("projection range length overflow"))?;
                    }
                    anyhow::ensure!(
                        length == end - start,
                        "projection range does not match covered content"
                    );
                    ranges
                        .entry((*kind, message_id))
                        .or_default()
                        .push((*start, *end));
                }
                DisplayProjectionBinding::Tool {
                    tool_call_id,
                    output_index,
                    native,
                } => {
                    validate_display_id(tool_call_id)?;
                    let valid = if *native {
                        native_tools.contains(&(*output_index, tool_call_id.as_str()))
                    } else {
                        hosted_actions.get(output_index).is_some_and(|action| {
                            action.client_id(&self.attempt.id) == *tool_call_id
                        })
                    };
                    anyhow::ensure!(valid, "invalid tool projection binding");
                }
            }
        }
        for ranges in ranges.values_mut() {
            ranges.sort_unstable();
            anyhow::ensure!(
                ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0),
                "overlapping projection bindings"
            );
        }
        anyhow::ensure!(
            zevria_foundation::BoundedCountingWriter::json_fits(self, 16 * 1024 * 1024)?,
            "response display payload exceeds limit"
        );
        Ok(parts)
    }

    pub fn binding_text(&self, binding: &DisplayProjectionBinding) -> Option<String> {
        self.parts().binding_text(binding)
    }
}

/// A present activity-only value is different from a cleared stream (`None`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssistantStreamSnapshot {
    pub message: Option<rig_core::message::Message>,
    pub attempt: Option<ResponseAttempt>,
}
impl From<rig_core::message::Message> for AssistantStreamSnapshot {
    fn from(message: rig_core::message::Message) -> Self {
        Self {
            message: Some(message),
            attempt: None,
        }
    }
}
