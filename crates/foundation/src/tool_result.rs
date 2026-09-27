//! Structured tool-result metadata kept outside Rig model messages.

use std::path::PathBuf;

use rig_core::tool::ToolResult;
use serde::{Deserialize, Serialize};

use crate::{question::QuestionTerminalDisposition, subtask::SubtaskEntryMetadata};

/// ACP extension key for display-only outcome, diagnostic and mutation evidence.
pub const TOOL_RESULT_META_KEY: &str = "zevria.toolResult";

/// The filesystem operation represented by an omitted file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeOperation {
    Add,
    Delete,
    Update,
}

/// A captured file mutation for frontend rendering and transcript restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum FileChange {
    Add {
        content: String,
    },
    Delete {
        content: String,
    },
    Update {
        unified_diff: String,
        move_path: Option<PathBuf>,
    },
    Omitted {
        operation: FileChangeOperation,
        reason: String,
        added: usize,
        removed: usize,
        bytes: usize,
    },
}

/// One changed path and the structured representation of its mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeOutput {
    pub path: PathBuf,
    pub change: FileChange,
}

/// Cloneable Rig *call* extension carrying the assistant tool-call id into a
/// dispatched tool. Tools that announce work to the frontend (the subtask
/// tool) read it to correlate their events with the pending call row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallId(pub String);

/// Rig result extension set by a running tool that observed turn
/// cancellation and stopped before producing its ordinary result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCancelled;

/// Model-independent lifecycle outcome for one correlated tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Success,
    Error,
    Skipped,
    Denied,
    Cancelled,
    /// The call failed after changing external state; the file-change detail
    /// records the state that actually survived.
    Partial,
}

impl From<&ToolResult> for ToolCallOutcome {
    fn from(result: &ToolResult) -> Self {
        if result.is_success() {
            Self::Success
        } else if result.is_skipped() {
            Self::Skipped
        } else if result.is_refused() {
            Self::Denied
        } else {
            Self::Error
        }
    }
}

impl ToolCallOutcome {
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }
}

/// Exclusive tool-specific payload, also used as the cloneable Rig result
/// extension. Its presence is independent of the call's lifecycle outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolResultDetail {
    /// Captured mutations, including any that survive a failed edit.
    FileChanges(Vec<FileChangeOutput>),
    /// Input-ordered terminal slots, retaining every accepted child identity.
    Subtasks(Vec<SubtaskEntryMetadata>),
    /// Terminal root-question semantics, including frontend errors. Malformed
    /// model calls and cancellation do not produce a question disposition.
    QuestionDisposition(QuestionTerminalDisposition),
}

impl ToolResultDetail {
    pub fn file_changes(&self) -> &[FileChangeOutput] {
        match self {
            Self::FileChanges(changes) => changes,
            _ => &[],
        }
    }

    pub fn subtasks(&self) -> &[SubtaskEntryMetadata] {
        match self {
            Self::Subtasks(entries) => entries,
            _ => &[],
        }
    }

    pub fn question_disposition(&self) -> Option<QuestionTerminalDisposition> {
        match self {
            Self::QuestionDisposition(disposition) => Some(*disposition),
            _ => None,
        }
    }
}

/// Display/persistence metadata correlated with one ordinary Rig tool result.
///
/// This type deliberately has no conversion into `Message`, `Text`, or
/// `ToolResultContent`; model history stores only the separate plain result
/// message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolResultMetadata {
    /// Rig correlation handle, matching `ToolCall::id` and `ToolResult::call`.
    pub id: String,
    /// Provider-issued call identifier, when the provider supplied one.
    pub call_id: Option<String>,
    pub tool_name: String,
    pub outcome: ToolCallOutcome,
    /// Human-readable diagnostic captured before constructing model-facing
    /// envelopes. Display-only; never copied into a model message implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    /// Optional per-call detail for frontend rendering and transcript recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ToolResultDetail>,
}

impl ToolResultMetadata {
    pub fn with_diagnostic(mut self, diagnostic: impl Into<String>) -> Self {
        self.diagnostic = Some(diagnostic.into());
        self
    }

    pub fn file_changes(&self) -> &[FileChangeOutput] {
        self.detail
            .as_ref()
            .map_or(&[], ToolResultDetail::file_changes)
    }

    pub fn subtasks(&self) -> &[SubtaskEntryMetadata] {
        self.detail.as_ref().map_or(&[], ToolResultDetail::subtasks)
    }

    pub fn question_disposition(&self) -> Option<QuestionTerminalDisposition> {
        self.detail
            .as_ref()
            .and_then(ToolResultDetail::question_disposition)
    }
}

#[cfg(test)]
#[path = "tool_result_tests.rs"]
mod tests;

pub fn validate_tool_result_message(message: &rig_core::message::Message) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(message, rig_core::message::Message::User { content } if content.iter().any(|block| matches!(block, rig_core::message::UserContent::ToolResult(_)))),
        "tool-result sidecars require a user message containing tool results"
    );
    Ok(())
}
