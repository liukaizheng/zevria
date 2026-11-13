//! Transcript header responsibilities.
use super::item::TranscriptItem;

#[cfg(test)]
#[allow(unused_imports)]
use super::test_prelude::*;

/// Validate the authoritative selection's uniqueness and exact header position.
/// Missing metadata is supported, but never malformed or misplaced metadata.
pub fn session_mode(items: &[TranscriptItem]) -> anyhow::Result<Option<crate::SessionMode>> {
    #[cfg(feature = "test-support")]
    crate::replay_probe::record(|counts| counts.header_scans += 1);
    validate_header(items.iter())
}

pub(super) fn validate_header<'a>(
    items: impl Iterator<Item = &'a TranscriptItem>,
) -> anyhow::Result<Option<crate::SessionMode>> {
    let mut header = SessionHeader::default();
    for (index, item) in items.enumerate() {
        header.apply(index, item)?;
    }
    Ok(header.selected)
}

/// Incremental form of the same header rules used by persistence and replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionHeader {
    selected: Option<crate::SessionMode>,
    mode_index: usize,
}

impl SessionHeader {
    pub(crate) fn selected(&self) -> Option<crate::SessionMode> {
        self.selected
    }

    pub(crate) fn apply(&mut self, index: usize, item: &TranscriptItem) -> anyhow::Result<()> {
        match item {
            TranscriptItem::SessionModels(_) => {
                anyhow::ensure!(index == 0, "session models must be a unique first record");
                self.mode_index = 1;
            }
            TranscriptItem::SessionMode(mode) => {
                anyhow::ensure!(self.selected.is_none(), "session mode must be unique");
                anyhow::ensure!(
                    index == self.mode_index,
                    "session mode must be first or immediately follow session models"
                );
                self.selected = Some(*mode);
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn install_session_mode(
    items: &mut Vec<TranscriptItem>,
    mode: crate::SessionMode,
) -> anyhow::Result<()> {
    session_mode(items)?;
    let index = usize::from(matches!(
        items.first(),
        Some(TranscriptItem::SessionModels(_))
    ));
    if matches!(items.get(index), Some(TranscriptItem::SessionMode(_))) {
        items[index] = TranscriptItem::SessionMode(mode);
    } else {
        items.insert(index, TranscriptItem::SessionMode(mode));
    }
    Ok(())
}

pub fn is_leading_metadata(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::SessionModels(_) | TranscriptItem::SessionMode(_)
    )
}

#[cfg(test)]
#[path = "header_model_tests.rs"]
mod model_tests;

#[cfg(test)]
#[path = "header_mode_tests.rs"]
mod mode_tests;
