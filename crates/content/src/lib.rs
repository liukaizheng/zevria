//! Validated prompt content, images, citations and provider-neutral display values.
use zevria_foundation::ModelProfileRef;
pub mod message;
pub use message::assistant_plain_text;
pub mod citations;
pub use citations::CitedSource;
pub mod image_diagnostics;
pub mod image_facts;
pub mod image_preparation;
pub mod prompt;
pub mod readable;
pub mod reasoning;
pub mod response;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub use prompt::{PromptBlock, PromptError, PromptImage, UserPrompt};
pub use response::{
    AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
    AssistantSourceAddress, AssistantStreamSnapshot, HostedActivity, HostedActivityKind,
    HostedActivityStatus, ResponseAttempt, ResponseAttemptOutcome,
};
