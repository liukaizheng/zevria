//! Native in-process desktop frontend. Session semantics live in presentation;
//! this package owns only desktop routing, interaction state and GPUI views.
pub mod cli;
pub mod session;

#[cfg(target_os = "macos")]
pub mod actions;
#[cfg(target_os = "macos")]
pub mod controller;
#[cfg(target_os = "macos")]
pub mod engine;
#[cfg(target_os = "macos")]
pub mod launch;
#[cfg(target_os = "macos")]
pub mod ui;
