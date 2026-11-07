use serde::{Deserialize, Serialize};
use zevria_model::TokenUsage;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}
impl Usage {
    /// message_delta usage is cumulative but may omit unchanged input fields.
    pub fn update(&mut self, value: &serde_json::Value) {
        for (key, target) in [
            ("input_tokens", &mut self.input_tokens),
            (
                "cache_creation_input_tokens",
                &mut self.cache_creation_input_tokens,
            ),
            ("cache_read_input_tokens", &mut self.cache_read_input_tokens),
            ("output_tokens", &mut self.output_tokens),
        ] {
            if let Some(n) = value[key].as_u64() {
                *target = n;
            }
        }
    }
    pub fn canonical(self) -> TokenUsage {
        let input = self
            .input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens);
        TokenUsage {
            input_tokens: input,
            cached_tokens: self.cache_read_input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: input.saturating_add(self.output_tokens),
        }
    }
}
