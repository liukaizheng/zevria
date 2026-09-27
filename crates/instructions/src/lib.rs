//! Instruction protocol, captured guidance, skills and pinned activation state.
use zevria_content::UserPrompt;
use zevria_foundation::{ToolResultMetadata, TurnPolicy, WEB_SEARCH_TOOL_NAME, config};
pub mod directive;
pub mod guidance;
mod instruction_set;
pub mod prompts;
pub mod request;
pub use request::{RequestDirective, RequestDirectiveKind};
pub mod skill;
pub use directive::{
    DirectiveContent, DirectivePayload, DirectivePolicy, DirectiveSnapshot, DirectiveState,
};
pub use guidance::*;
pub use instruction_set::InstructionSet;
pub use skill::*;

#[cfg(test)]
mod characterization_tests;
