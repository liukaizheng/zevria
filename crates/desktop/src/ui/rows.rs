//! Renderer-free row invalidation. Tool completion revisions matter even when
//! row count and text length do not change.
use std::ops::Range;
use zevria_presentation::conversation::ConversationState;

pub fn fingerprint(conversation: &ConversationState, index: usize) -> u64 {
    conversation.entry_revision(index)
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
/// Reuse measured survivors even when several disjoint changes happen at once
/// (e.g. a thought grows above the viewport as its temporary tail row vanishes).
/// Splices use the index space *after* earlier splices; remeasures come last.
pub fn reconcile(before: &[(&str, u64)], after: &[(&str, u64)]) -> Vec<RowChange> {
    if before.iter().map(|r| r.0).eq(after.iter().map(|r| r.0)) {
        return vec![compare(
            &before.iter().map(|r| r.1).collect::<Vec<_>>(),
            &after.iter().map(|r| r.1).collect::<Vec<_>>(),
        )];
    }
    let prior: std::collections::HashMap<_, _> = before
        .iter()
        .enumerate()
        .map(|(index, (key, _))| (*key, index))
        .collect();
    let mut changes = Vec::new();
    let mut remeasure = Vec::new();
    let (mut old, mut new) = (0, 0);
    for (index, &(key, fingerprint)) in after.iter().enumerate() {
        let Some(&survivor) = prior.get(key).filter(|&&prior| prior >= old) else {
            continue;
        };
        if survivor != old || index != new {
            changes.push(RowChange::Splice {
                old: new..new + survivor - old,
                count: index - new,
            });
        }
        if before[survivor].1 != fingerprint {
            remeasure.push(index);
        }
        old = survivor + 1;
        new = index + 1;
    }
    if old != before.len() || new != after.len() {
        changes.push(RowChange::Splice {
            old: new..new + before.len() - old,
            count: after.len() - new,
        });
    }
    changes.push(RowChange::Remeasure(remeasure));
    changes
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
    fn a_thought_merging_does_not_splice_the_steps_in_between() {
        assert_eq!(
            reconcile(
                &[("prompt", 0), ("thoughts", 1), ("step", 2), ("tail", 3)],
                &[("prompt", 0), ("thoughts", 4), ("step", 2)]
            ),
            [
                RowChange::Splice {
                    old: 3..4,
                    count: 0
                },
                RowChange::Remeasure(vec![1])
            ]
        );
    }

    #[test]
    fn structural_notices_and_versions_invalidate_without_debug_serialization() {
        let mut conversation = ConversationState::default();
        conversation.push_error("a".into());
        let before = fingerprint(&conversation, 0);
        conversation.commit_edit(0, true);
        assert_ne!(before, fingerprint(&conversation, 0));
        let artifact = zevria_workflow::PlanArtifact {
            version: zevria_workflow::PlanVersion {
                id: zevria_workflow::PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "large document".repeat(1000),
            source_turn_id: zevria_foundation::TurnId::new(1),
        };
        conversation.push_plan_artifact(artifact.clone());
        let before = fingerprint(&conversation, 1);
        let mut next = artifact;
        next.version.revision += 1;
        conversation.push_plan_artifact(next);
        assert_ne!(before, fingerprint(&conversation, 2));
        assert_eq!(before, fingerprint(&conversation, 1));
    }
}
