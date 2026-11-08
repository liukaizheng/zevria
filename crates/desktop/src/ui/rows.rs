//! Renderer-free row invalidation. Tool completion revisions matter even when
//! row count and text length do not change.
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
};
use zevria_presentation::{blocks::NativeHeader, conversation::HistoryEntry};
/// A following assistant entry in the same native turn does not repeat its nick.
pub fn continues_turn(previous: Option<&HistoryEntry>, entry: &HistoryEntry) -> bool {
    matches!((previous, entry),
        (Some(HistoryEntry::Conversation(previous)), HistoryEntry::Conversation(current))
        if matches!((previous.header, current.header),
            (Some(NativeHeader::Assistant { turn: a, .. }), Some(NativeHeader::Assistant { turn: b, .. })) if a == b))
}
pub fn is_assistant(entry: &HistoryEntry) -> bool {
    matches!(entry, HistoryEntry::Conversation(e) if matches!(e.header, Some(NativeHeader::Assistant { .. })))
}
pub fn fingerprint(entry: &HistoryEntry, continues_turn: bool) -> u64 {
    let mut hash = DefaultHasher::new();
    continues_turn.hash(&mut hash);
    std::mem::discriminant(entry).hash(&mut hash);
    match entry {
        HistoryEntry::Conversation(entry) => {
            entry.header.hash(&mut hash);
            for block in &entry.blocks {
                block.id.hash(&mut hash);
                block.revision.hash(&mut hash);
            }
        }
        HistoryEntry::PlanArtifact(a) => {
            a.version.hash(&mut hash);
            a.title.hash(&mut hash);
        }
        HistoryEntry::PlanHandoff(h, header) => {
            h.artifact.version.hash(&mut hash);
            h.artifact.title.hash(&mut hash);
            header.hash(&mut hash);
        }
        HistoryEntry::JobNotice(notice, header) => {
            notice.text().hash(&mut hash);
            header.hash(&mut hash);
        }
        HistoryEntry::Ensemble(e) => {
            e.header.hash(&mut hash);
            e.run_id.hash(&mut hash);
            std::mem::discriminant(&e.workflow).hash(&mut hash);
            for block in e.prompt.blocks() {
                std::mem::discriminant(block).hash(&mut hash);
                match block {
                    zevria_content::PromptBlock::Text(text) => text.hash(&mut hash),
                    zevria_content::PromptBlock::Image(image) => {
                        image.mime_type().hash(&mut hash);
                        image.dimensions().hash(&mut hash);
                    }
                }
            }
            for worker in &e.workers {
                worker.descriptor.id.hash(&mut hash);
                std::mem::discriminant(&worker.status).hash(&mut hash);
                worker.failure.hash(&mut hash);
            }
        }
        HistoryEntry::Error(error) => error.hash(&mut hash),
        HistoryEntry::CompactionDivider => {}
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
    fn continuing_turn_changes_the_fingerprint_and_remeasures() {
        let entry = HistoryEntry::from_message(
            rig_core::message::Message::assistant("Text"),
            zevria_presentation::conversation::ToolCallStatus::Finished,
        )
        .unwrap();
        let a = fingerprint(&entry, false);
        let b = fingerprint(&entry, true);
        assert_ne!(a, b);
        assert_eq!(compare(&[a], &[b]), RowChange::Remeasure(vec![0]));
        #[cfg(target_os = "macos")]
        {
            let first = crate::ui::blocks::entry("epoch", 0, &entry, a, false, false);
            let next = crate::ui::blocks::entry("epoch", 0, &entry, b, true, false);
            assert_eq!(
                first[0].gutter,
                crate::ui::blocks::Gutter::Label(crate::ui::blocks::Speaker::Zevria)
            );
            assert_eq!(next[0].gutter, crate::ui::blocks::Gutter::None);
        }
    }
    #[test]
    fn structural_notices_and_versions_invalidate_without_debug_serialization() {
        assert_ne!(
            fingerprint(&HistoryEntry::Error("a".into()), false),
            fingerprint(&HistoryEntry::Error("b".into()), false)
        );
        assert_ne!(
            fingerprint(&HistoryEntry::CompactionDivider, false),
            fingerprint(&HistoryEntry::Error(String::new()), false)
        );
        let artifact = zevria_workflow::PlanArtifact {
            version: zevria_workflow::PlanVersion {
                id: zevria_workflow::PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "large document".repeat(1000),
            source_turn_id: zevria_foundation::TurnId::new(1),
        };
        let before = fingerprint(&HistoryEntry::PlanArtifact(artifact.clone()), false);
        let mut next = artifact;
        next.version.revision += 1;
        assert_ne!(
            before,
            fingerprint(&HistoryEntry::PlanArtifact(next), false)
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn ordinary_transcript_text_is_not_subject_to_tool_preview_bounds() {
        let text = "x".repeat(32_000);
        let entry = zevria_presentation::conversation::HistoryEntry::from_message(
            rig_core::message::Message::assistant(text.clone()),
            zevria_presentation::conversation::ToolCallStatus::Finished,
        )
        .unwrap();
        let blocks = crate::ui::blocks::entry("epoch", 0, &entry, 0, false, false);
        assert_eq!(blocks[0].body, text);
        assert_eq!(blocks[0].primary, blocks[0].body);
    }
}
