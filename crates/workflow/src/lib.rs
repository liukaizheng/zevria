//! Pure Plan and ensemble records, validation and deterministic reducers.
use zevria_content::{PromptBlock, PromptError, PromptImage, UserPrompt, response};
use zevria_foundation::{QuestionRequestId, TurnId};

// Keep the known-tag list used by crash-tail readers tied to an exhaustive
// match, so adding a variant cannot silently omit it from recovery.
macro_rules! impl_wire_types {
    ($event:ident, $known:ident; $($variant:pat => $tag:literal),+ $(,)?) => {
        impl $event {
            /// Wire tags recognized for this event enum.
            pub const $known: &'static [&'static str] = &[$($tag),+];

            /// The serialized tag for this variant.
            pub fn wire_type(&self) -> &'static str {
                match self {
                    $($variant => $tag),+
                }
            }
        }
    };
}

pub mod config;
pub mod ensemble;
pub mod ensemble_review;
pub mod plan;
pub use ensemble::*;
pub use ensemble_review::*;
pub use plan::*;
