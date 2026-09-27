//! A fixed allowlist observed before SDK defaults/projection. Never raw bodies.
use super::{
    fingerprint::{self, Fingerprint},
    identifier,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zevria_model::TokenUsage;

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Counter {
    #[default]
    Missing,
    Null,
    Malformed,
    Present(u64),
}
impl Counter {
    fn at(response: &Value, path: &[&str]) -> Self {
        let mut node = response;
        for name in path {
            if node.is_null() {
                return Self::Null;
            }
            let Some(object) = node.as_object() else {
                return Self::Malformed;
            };
            let Some(next) = object.get(*name) else {
                return Self::Missing;
            };
            node = next;
        }
        if node.is_null() {
            Self::Null
        } else {
            node.as_u64().map_or(Self::Malformed, Self::Present)
        }
    }
    pub fn value(self) -> Option<u64> {
        if let Self::Present(value) = self {
            Some(value)
        } else {
            None
        }
    }
}
// Arithmetic categories of reported input, NOT provider-confirmed hits/misses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReportedInput {
    Unavailable,
    ZeroInput,
    CachedExceedsInput,
    ZeroCached,
    Partial,
    FullyCached,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Accounting {
    pub category: ReportedInput,
    // Derived only when both raw counters are present and consistent. The raw
    // counters retain their individual missing/null/malformed states.
    pub uncached: Option<u64>,
    pub fraction: Option<f64>,
}
impl Accounting {
    pub fn new(input: Counter, cached: Counter) -> Self {
        let mut result = Self {
            category: ReportedInput::Unavailable,
            uncached: None,
            fraction: None,
        };
        if let (Counter::Present(input), Counter::Present(cached)) = (input, cached) {
            if cached > input {
                result.category = ReportedInput::CachedExceedsInput;
            } else {
                result.uncached = Some(input - cached);
                result.category = if input == 0 {
                    ReportedInput::ZeroInput
                } else if cached == 0 {
                    ReportedInput::ZeroCached
                } else if cached == input {
                    ReportedInput::FullyCached
                } else {
                    ReportedInput::Partial
                };
                if input > 0 {
                    result.fraction = Some(cached as f64 / input as f64);
                }
            }
        }
        result
    }
}

// Fixed allowlists: even identifier-shaped unknown strings may contain secrets.
// Future variants remain Unknown; never persist arbitrary provider text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ComparisonOutcome {
    Absent,
    Null,
    Malformed,
    CacheHit,
    CacheMiss,
    ComparisonResponseNotFound,
    Unavailable,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ComparisonReason {
    Missing,
    Null,
    Malformed,
    ModelChanged,
    PromptCacheKeyChanged,
    ToolsChanged,
    TextFormatChanged,
    ReasoningEffortChanged,
    VerbosityChanged,
    ContextCompacted,
    InputChanged,
    ServiceTierChanged,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderComparison {
    pub outcome: ComparisonOutcome,
    pub reason: ComparisonReason,
    pub comparison_reusable_tokens: Counter,
    pub cache_missed_tokens: Counter,
}
impl ProviderComparison {
    fn at(response: &Value) -> Self {
        let value = response.get("prompt_cache_diagnostics");
        let outcome = match value {
            None => ComparisonOutcome::Absent,
            Some(Value::Null) => ComparisonOutcome::Null,
            Some(Value::Object(object)) => match object.get("type").and_then(Value::as_str) {
                Some("cache_hit") => ComparisonOutcome::CacheHit,
                Some("cache_miss") => ComparisonOutcome::CacheMiss,
                Some("comparison_response_not_found") => {
                    ComparisonOutcome::ComparisonResponseNotFound
                }
                Some("unavailable") => ComparisonOutcome::Unavailable,
                Some(_) => ComparisonOutcome::Unknown,
                None => ComparisonOutcome::Malformed,
            },
            Some(_) => ComparisonOutcome::Malformed,
        };
        let reason = match (value, value.and_then(|v| v.get("reason"))) {
            (None, _) | (Some(Value::Object(_)), None) => ComparisonReason::Missing,
            (Some(Value::Null), _) | (Some(Value::Object(_)), Some(Value::Null)) => {
                ComparisonReason::Null
            }
            (Some(Value::Object(_)), Some(Value::String(text))) => match text.as_str() {
                "model_changed" => ComparisonReason::ModelChanged,
                "prompt_cache_key_changed" => ComparisonReason::PromptCacheKeyChanged,
                "tools_changed" => ComparisonReason::ToolsChanged,
                "text_format_changed" => ComparisonReason::TextFormatChanged,
                "reasoning_effort_changed" => ComparisonReason::ReasoningEffortChanged,
                "verbosity_changed" => ComparisonReason::VerbosityChanged,
                "context_compacted" => ComparisonReason::ContextCompacted,
                "input_changed" => ComparisonReason::InputChanged,
                "service_tier_changed" => ComparisonReason::ServiceTierChanged,
                _ => ComparisonReason::Unknown,
            },
            _ => ComparisonReason::Malformed,
        };
        Self {
            outcome,
            reason,
            comparison_reusable_tokens: Counter::at(
                response,
                &["prompt_cache_diagnostics", "comparison_reusable_tokens"],
            ),
            cache_missed_tokens: Counter::at(
                response,
                &["prompt_cache_diagnostics", "cache_missed_tokens"],
            ),
        }
    }
    pub fn conclusive(self) -> bool {
        match self.outcome {
            // The documented hit has no reason/estimate fields. Keep a
            // reported hit distinct, but do not use contradictory or malformed
            // known fields as evidence that the investigation is resolved.
            ComparisonOutcome::CacheHit => {
                self.reason == ComparisonReason::Missing
                    && self.comparison_reusable_tokens == Counter::Missing
                    && self.cache_missed_tokens == Counter::Missing
            }
            ComparisonOutcome::CacheMiss => {
                !matches!(
                    self.reason,
                    ComparisonReason::Missing
                        | ComparisonReason::Null
                        | ComparisonReason::Malformed
                        | ComparisonReason::Unknown
                ) && self.cache_missed_tokens.value().is_some()
                    && matches!(
                        self.comparison_reusable_tokens,
                        Counter::Missing | Counter::Present(_)
                    )
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeCounts {
    pub search: u64,
    pub open_page: u64,
    pub find_in_page: u64,
    pub other_search: u64,
    pub reasoning: u64,
}
impl NativeCounts {
    pub fn valid(self, item_count: usize) -> bool {
        [
            self.search,
            self.open_page,
            self.find_in_page,
            self.other_search,
            self.reasoning,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .is_some_and(|count| count <= item_count as u64)
    }
    pub fn from_items(items: &[Value]) -> Self {
        let mut counts = Self::default();
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("reasoning") => counts.reasoning += 1,
                Some("web_search_call") => match item
                    .get("action")
                    .and_then(|a| a.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("search") => counts.search += 1,
                    Some("open_page") => counts.open_page += 1,
                    Some("find_in_page") => counts.find_in_page += 1,
                    _ => counts.other_search += 1,
                },
                _ => {}
            }
        }
        counts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum TerminalEvent {
    Completed,
    Done,
    Failed,
    Incomplete,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Echo {
    Missing,
    Null,
    Present(Fingerprint),
}
impl Echo {
    pub fn at(value: Option<&Value>) -> Self {
        match value {
            None => Self::Missing,
            Some(Value::Null) => Self::Null,
            Some(value) => Self::Present(fingerprint::optional(Some(value))),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Observation {
    pub event: TerminalEvent,
    pub response_id: Option<String>,
    pub response_identity: Option<Fingerprint>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub service_tier: Option<String>,
    pub input: Counter,
    pub cached: Counter,
    pub cache_write: Counter,
    pub output: Counter,
    pub total: Counter,
    pub instructions: Echo,
    pub tools: Echo,
    pub cache_key: Echo,
    pub comparison: ProviderComparison,
}
impl Observation {
    pub fn parse(message: &str) -> Option<Self> {
        if message.len() > 16 * 1024 * 1024 {
            return None;
        }
        let event: Value = serde_json::from_str(message).ok()?;
        let kind = match event.get("type")?.as_str()? {
            "response.completed" => TerminalEvent::Completed,
            "response.done" => TerminalEvent::Done,
            "response.failed" => TerminalEvent::Failed,
            "response.incomplete" => TerminalEvent::Incomplete,
            "response.cancelled" => TerminalEvent::Cancelled,
            _ => return None,
        };
        let response = event.get("response")?;
        let text = |name| {
            response
                .get(name)
                .and_then(Value::as_str)
                .map(|s| identifier(s).to_owned())
        };
        let cache_write = Counter::at(
            response,
            &["usage", "input_tokens_details", "cache_write_tokens"],
        );
        Some(Self {
            event: kind,
            response_id: text("id"),
            response_identity: response
                .get("id")
                .and_then(Value::as_str)
                .map(|s| fingerprint::bytes(s.as_bytes())),
            model: text("model"),
            status: text("status"),
            service_tier: text("service_tier"),
            input: Counter::at(response, &["usage", "input_tokens"]),
            cached: Counter::at(
                response,
                &["usage", "input_tokens_details", "cached_tokens"],
            ),
            cache_write,
            output: Counter::at(response, &["usage", "output_tokens"]),
            total: Counter::at(response, &["usage", "total_tokens"]),
            instructions: Echo::at(response.get("instructions")),
            tools: Echo::at(response.get("tools")),
            cache_key: Echo::at(response.get("prompt_cache_key")),
            comparison: ProviderComparison::at(response),
        })
    }
    pub fn accounting(&self) -> Accounting {
        Accounting::new(self.input, self.cached)
    }
    pub fn valid(&self) -> bool {
        [
            &self.response_id,
            &self.model,
            &self.status,
            &self.service_tier,
        ]
        .into_iter()
        .flatten()
        .all(|value| safe(value))
    }
    pub fn counters_differ(&self, other: &Self) -> bool {
        (
            self.input,
            self.cached,
            self.cache_write,
            self.output,
            self.total,
        ) != (
            other.input,
            other.cached,
            other.cache_write,
            other.output,
            other.total,
        )
    }
    pub fn projection_differs(&self, usage: Option<Usage>, cache_write: Option<u64>) -> bool {
        // Missing/null/malformed projected as zero is itself an accounting
        // difference, not evidence of a raw provider-reported cache miss.
        let values = usage.map(|u| [u.input, u.cached, u.output, u.total]);
        let raw = [self.input, self.cached, self.output, self.total];
        raw.into_iter()
            .enumerate()
            .any(|(i, c)| c.value() != values.map(|v| v[i]))
            || self.cache_write.value() != cache_write
    }
}
pub(super) fn safe(value: &str) -> bool {
    value == "<redacted>" || identifier(value) == value
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub total: u64,
}
impl From<TokenUsage> for Usage {
    fn from(u: TokenUsage) -> Self {
        Self {
            input: u.input_tokens,
            cached: u.cached_tokens,
            output: u.output_tokens,
            total: u.total_tokens,
        }
    }
}
