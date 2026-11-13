//! JSONL persistence for conversation transcripts.
//!
//! Every conversation owns a session id; its items are appended live to
//! `{workspace}/.zevria/sessions/{id}.jsonl`, one JSON object per line.
//! Ordinary messages keep their bare wire shape (`{"role": ...}`), compound
//! records add reserved metadata to that same object, provider responses store
//! only their replay envelope, and turn failures are stored as
//! `{"error": ...}`. Each line remains both a resumable history record and a
//! readable log.

mod conversation;
mod header;
mod item;
mod load;
mod projection;
mod sessions;
mod writer;
pub use conversation::{AttemptCheckpointWriter, Conversation};
pub(crate) use header::SessionHeader;
pub use header::{install_session_mode, is_leading_metadata, session_mode};
pub use item::{
    MAX_ROOT_RECORD_BYTES, ReplayBackedMessage, ToolDeclarationPlacement, ToolDeclarationRecord,
    ToolWindow, TranscriptItem, replay_active_skills, tool_window,
};
pub use load::{
    HistoryFailure, TranscriptDamage, TranscriptDiagnostic, TranscriptLoadOutcome,
    UnsupportedHistory, load, load_report,
};
pub(crate) use load::{partial_json_field, top_level_keys};
pub use projection::{
    is_compaction_prompt_item, is_conversation_item, is_prompt_item,
    latest_successful_task_snapshot, model_history, model_input, model_input_from_records,
    model_input_with_checkpoint, retained_user_candidates,
};
pub use sessions::{
    SessionSummary, is_abandoned_root, latest_session_file, list_sessions, pick_session_id,
    plans_dir, sessions_dir, subsession_files, subsessions_dir,
};
pub use writer::TranscriptWriter;

#[cfg(test)]
mod test_prelude;
#[cfg(test)]
mod test_support;
