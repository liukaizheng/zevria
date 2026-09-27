//! ACP worker supervision and its independently validated configuration.

pub mod config;
mod supervisor;
pub use supervisor::{EnsembleSupervisor, preflight_existing_logs};
