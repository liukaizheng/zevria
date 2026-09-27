//! Discover → resolve a name → prepare an application → commit → project instructions.
//!
//! [`SkillSnapshot`] is persisted content; [`SkillCatalog`] is an immutable installed
//! view; [`ActiveSkills`] retains historical session pins; [`SkillContext`] resolves
//! and projects their effective guidance under captured permissions.
mod active;
mod catalog;
mod context;
mod definition;
mod discovery;
mod management;
mod parser;
mod resource;

pub use active::*;
pub use catalog::*;
pub use context::*;
pub use definition::*;
pub(crate) use definition::{SourceBinding, update_length_delimited};
pub use discovery::*;
pub use management::*;
pub use resource::*;

/// Definitions are rejected rather than truncated.
pub const MAX_SKILL_BYTES: u64 = 64 * 1024;

#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod definition_tests;
#[cfg(test)]
mod management_tests;
