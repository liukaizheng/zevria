//! Pure Anthropic Messages wire codec. No transport or engine dependencies.
pub mod binding;
pub mod body;
pub mod error;
pub mod project;
pub mod stream;
pub mod usage;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    #[default]
    Summarized,
    Omitted,
    Updates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalFallback {
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Compatibility {
    pub prompt_caching: bool,
    pub system_messages: bool,
    pub eager_tool_input_streaming: bool,
    pub strict_thinking_binding: bool,
}
impl Default for Compatibility {
    fn default() -> Self {
        Self {
            prompt_caching: true,
            system_messages: true,
            eager_tool_input_streaming: true,
            strict_thinking_binding: false,
        }
    }
}

#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod tests;
