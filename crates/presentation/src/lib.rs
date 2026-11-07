//! Frontend-neutral transcript projection, interaction grammar and session reduction.
//! No renderer, filesystem reads, provider, tools or engine ownership lives here.
#[path = "agent_transcript.rs"]
pub mod agents;
pub mod blocks;
pub mod command;
pub mod completion;
pub mod conversation;
pub mod projection;
pub mod session;
pub mod session_model;
#[cfg(test)]
mod session_model_tests;
pub mod status_icon;
pub mod workflow;
pub use session_model::{
    ConversationTail, PaneScope, Reduction, RestorationInput, SessionModel, Settlement,
};
// Stable internal aliases retained while moving the existing projection code.
#[cfg(test)]
use agents as agent_transcript;
use blocks as presentation;
