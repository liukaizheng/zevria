//! Ratatui frontend for Zevria's provider-neutral session engine.

mod agent_transcript;
mod app;
mod clipboard;
mod frame_layout;
mod hints;
mod input;
mod layout;
mod models;
mod picker;
mod presentation;
mod projection;
mod render;
mod runtime;
mod skills;
mod status;
mod status_icon;
mod workspace;
mod workspace_files;
use zevria_theme as theme;
use zevria_tui_widgets::{chrome, diff_render, markdown, syntax, text, viewport, workspace_header};

pub use app::{App, RestorationInput, UiAction};
#[cfg(test)]
use app::{AppEffect, HistoryEntry, OperationKind, PlanChoice, Selection, ToolCallStatus};
#[cfg(test)]
use ratatui::crossterm::event::{Event, KeyCode};
#[cfg(test)]
use rig_core::message::{AssistantContent, Message, UserContent};
pub use runtime::{UiContext, UiOutcome, run_ui};
#[cfg(test)]
use status::format_token_count;
#[cfg(test)]
use theme::ZEVRIA_DARK;
pub use workspace::SessionViews;
#[cfg(test)]
use zevria_tui_input::SlashCommand;
use zevria_tui_input::{command, completion, composer, question};
#[cfg(test)]
const SELECTION_BG: ratatui::style::Color = ZEVRIA_DARK.surfaces.selection_background;
#[cfg(test)]
use viewport::RowRange;
#[cfg(test)]
use zevria_foundation::SessionMode;
#[cfg(test)]
use zevria_session_api::SessionEvent;
#[cfg(test)]
use zevria_transcript::transcript::TranscriptItem;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod theme_tests;
