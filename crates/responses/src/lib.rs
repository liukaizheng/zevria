//! Deterministic Responses conversion, event parsing and accumulation. No execution channels or I/O.
pub mod accumulator;
pub mod protocol;
pub mod replay;
pub mod search;
pub mod web_search_config;
pub use web_search_config::{
    SearchContextSize, SearchReturnTokenBudget, WebSearchConfig, WebSearchFilters,
    WebSearchLocation,
};

#[cfg(test)]
mod tests;
