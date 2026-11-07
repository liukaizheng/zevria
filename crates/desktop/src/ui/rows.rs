//! Renderer-free row invalidation. Tool completion revisions matter even when
//! row count and text length do not change.
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
};
use zevria_presentation::conversation::HistoryEntry;
pub fn fingerprint(entry: &HistoryEntry) -> u64 {
    let mut hash = DefaultHasher::new();
    match entry {
        HistoryEntry::Conversation(entry) => {
            entry.header.hash(&mut hash);
            for block in &entry.blocks {
                block.id.hash(&mut hash);
                block.revision.hash(&mut hash);
            }
        }
        _ => format!("{entry:?}").hash(&mut hash),
    }
    hash.finish()
}
#[derive(Debug, PartialEq, Eq)]
pub enum RowChange {
    None,
    Remeasure(Vec<usize>),
    Splice { old: Range<usize>, count: usize },
}
pub fn compare(before: &[u64], after: &[u64]) -> RowChange {
    if before == after {
        return RowChange::None;
    }
    if before.len() == after.len() {
        return RowChange::Remeasure(
            before
                .iter()
                .zip(after)
                .enumerate()
                .filter_map(|(i, (a, b))| (a != b).then_some(i))
                .collect(),
        );
    }
    let prefix = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    RowChange::Splice {
        old: prefix..before.len() - suffix,
        count: after.len() - prefix - suffix,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn append_growth_and_replacements() {
        assert_eq!(
            compare(&[1], &[1, 2]),
            RowChange::Splice {
                old: 1..1,
                count: 1
            }
        );
        assert_eq!(compare(&[1, 2], &[1, 3]), RowChange::Remeasure(vec![1]));
        assert_eq!(
            compare(&[1, 2, 3], &[1, 3]),
            RowChange::Splice {
                old: 1..2,
                count: 0
            }
        );
        assert_eq!(compare(&[1], &[1]), RowChange::None);
    }
    #[test]
    fn ordinary_transcript_text_is_not_subject_to_tool_preview_bounds() {
        let text = "x".repeat(32_000);
        let entry = zevria_presentation::conversation::HistoryEntry::from_message(
            rig_core::message::Message::assistant(text.clone()),
            zevria_presentation::conversation::ToolCallStatus::Finished,
        )
        .unwrap();
        let blocks = crate::ui::blocks::entry("epoch", 0, &entry, 0);
        assert_eq!(blocks[0].body, text);
        assert_eq!(blocks[0].primary, blocks[0].body);
    }
}
