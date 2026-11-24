//! JSONL persistence for conversation transcripts.
//!
//! Every conversation owns a session id; its items are appended live to
//! `{workspace}/.zevria/sessions/{id}.jsonl`, one JSON object per line.
//! Every line starts with `type`, followed by typed fields. A v2 batch
//! stores a non-recursive array of items under one durable commit boundary.

mod conversation;
mod header;
mod item;
mod load;
mod projection;
mod sessions;
mod writer;
pub use conversation::{AttemptCheckpointWriter, Conversation};
pub(crate) use header::SessionMetadataState;
pub use header::{install_session_mode, is_leading_metadata, session_mode};
pub use item::{
    MAX_ROOT_RECORD_BYTES, ReplayBackedMessage, ToolDeclarationPlacement, ToolDeclarationRecord,
    ToolWindow, TranscriptItem, replay_active_skills, tool_window,
};
pub use load::{
    HistoryFailure, TranscriptDamage, TranscriptDiagnostic, TranscriptLoadOutcome,
    UnsupportedHistory, load, load_report,
};
pub use projection::{
    is_compaction_prompt_item, is_conversation_item, is_prompt_item,
    latest_successful_task_snapshot, model_history, model_input, model_input_from_records,
    model_input_with_checkpoint, retained_user_candidates,
};
pub use sessions::{
    SessionSummary, find_session_file, is_abandoned_root, latest_session_file, list_sessions,
    pick_session_id, plans_dir, sessions_dir, subsession_files, subsessions_dir,
};
pub use writer::TranscriptWriter;

#[cfg(test)]
mod metadata_tests;
#[cfg(test)]
mod test_prelude;
#[cfg(test)]
mod test_support;
