//! Validated prompt content, images, citations and provider-neutral display values.
use zevria_foundation::ModelProfileRef;
pub mod message;
pub use message::assistant_plain_text;
pub mod citations;
pub mod image_diagnostics;
pub mod prompt;
pub mod web_search;
pub use prompt::{PromptBlock, PromptError, PromptImage, UserPrompt};
pub use web_search::{
    AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
    AssistantSourceAddress, AssistantStreamSnapshot, WebSearchActivity, WebSearchAttemptOutcome,
    WebSearchAttemptRecord, WebSearchStatus,
};
