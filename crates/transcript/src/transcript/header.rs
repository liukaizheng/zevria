//! Model-inert session selection records.
use super::item::TranscriptItem;
use crate::{SessionMode, models::SessionModels};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionMetadataState {
    pub(crate) models: Option<SessionModels>,
    pub(crate) mode: Option<SessionMode>,
}

impl SessionMetadataState {
    /// The replay layer has already checked the metadata boundary.
    pub(crate) fn apply(&mut self, item: &TranscriptItem) {
        match item {
            TranscriptItem::SessionModels(models) => self.models = Some((**models).clone()),
            TranscriptItem::SessionMode(mode) => self.mode = Some(*mode),
            TranscriptItem::SideChatOrigin(_) => {}
            _ => unreachable!("only session metadata enters this reducer"),
        }
    }
}

/// Last explicitly selected mode in an already validated transcript.
pub fn session_mode(items: &[TranscriptItem]) -> anyhow::Result<Option<SessionMode>> {
    #[cfg(feature = "test-support")]
    crate::replay_probe::record(|counts| counts.header_scans += 1);
    Ok(items.iter().rev().find_map(|item| match item {
        TranscriptItem::SessionMode(mode) => Some(*mode),
        _ => None,
    }))
}

pub fn install_session_mode(
    items: &mut Vec<TranscriptItem>,
    mode: SessionMode,
) -> anyhow::Result<()> {
    if session_mode(items)? != Some(mode) {
        items.push(TranscriptItem::SessionMode(mode));
    }
    Ok(())
}

pub fn is_leading_metadata(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::SessionModels(_)
            | TranscriptItem::SessionMode(_)
            | TranscriptItem::SideChatOrigin(_)
    )
}
