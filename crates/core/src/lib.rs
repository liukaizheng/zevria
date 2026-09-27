//! Provider-neutral session engine: turn execution, orchestration, and private runtime state.
//!
//! Shared contracts, instructions, model values, workflows, and persistence are
//! owned by their domain crates. Adapters import those owners directly.

mod session;
pub use session::SessionEngine;

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub use session::benchmark;
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod transcript_bench;
