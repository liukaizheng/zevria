use zevria_foundation::{ModelProfileRef, ModelRole};
/// Token accounting for one completed model response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    /// Share of the input that was served from the provider's prompt cache,
    /// in percent.
    pub fn cached_percent(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 * 100.0 / self.input_tokens as f64
        }
    }
}

/// Authority used for the projected next-request input measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTokenSource {
    Exact,
    UsagePlusDelta,
    ConservativeEstimate,
}

impl ContextTokenSource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::UsagePlusDelta => "usage+delta",
            Self::ConservativeEstimate => "conservative estimate",
        }
    }

    pub const fn is_estimated(self) -> bool {
        matches!(self, Self::ConservativeEstimate)
    }
}

/// One authoritative assessment of the next fully prepared provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTokenSnapshot {
    pub profile: ModelProfileRef,
    pub model_role: ModelRole,
    pub projected_input_tokens: u64,
    pub source: ContextTokenSource,
    pub automatic_trigger: u64,
    pub input_token_limit: u64,
    pub context_window_tokens: u64,
}
