//! Native in-process desktop frontend. Session semantics live in presentation;
//! this package owns only desktop routing, interaction state and GPUI views.
pub mod cli;
#[path = "ui/disclosure.rs"]
pub mod disclosure;
#[path = "ui/rows.rs"]
pub mod rows;
pub mod session;
#[path = "ui/timeline.rs"]
pub mod timeline;
#[path = "ui/turns.rs"]
pub mod turns;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod actions;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod controller;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod engine;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod launch;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod ui;
