//! Desktop row-granular turn navigation, separate from fold grouping.
use zevria_presentation::{conversation::TurnStartTarget, fold::FoldState};

pub fn turn_rows(starts: &[TurnStartTarget], folds: &FoldState) -> Vec<usize> {
    let mut rows = starts
        .iter()
        .map(|start| {
            folds
                .span_containing(start.history_index)
                .map_or(start.history_index, |(row, _)| row)
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnDirection {
    Previous,
    Next,
}

pub fn destination(
    turn_rows: &[usize],
    top_row: usize,
    within_row: bool,
    direction: TurnDirection,
) -> Option<usize> {
    match direction {
        TurnDirection::Previous => turn_rows
            .iter()
            .copied()
            .filter(|row| *row < top_row || (within_row && *row == top_row))
            .max(),
        TurnDirection::Next => turn_rows.iter().copied().filter(|row| *row > top_row).min(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::Message;
    use zevria_presentation::{
        blocks::{ConversationEntry, DisplayTurn, NativeHeader},
        conversation::{ConversationState, HistoryEntry, ToolCallStatus},
        fold::FoldKey,
    };
    use zevria_transcript::transcript::TranscriptItem;
    use zevria_workflow::{PlanArtifact, PlanHandoff, PlanId, PlanVersion};

    fn entry(message: Message) -> HistoryEntry {
        HistoryEntry::from_message(message, ToolCallStatus::Finished).unwrap()
    }
    fn assistant() -> HistoryEntry {
        entry(Message::assistant("Answer or progress"))
    }
    fn plan() -> PlanArtifact {
        PlanArtifact {
            version: PlanVersion {
                id: PlanId::new(),
                revision: 1,
            },
            title: "Plan".into(),
            markdown: "Keep this document".into(),
            source_turn_id: zevria_foundation::TurnId::new(1),
        }
    }
    #[test]
    fn shared_turn_rows_sort_and_deduplicate_headerless_groups() {
        let mut conversation = ConversationState::default();
        conversation.restore(vec![
            TranscriptItem::Message(Message::user("Prompt")),
            TranscriptItem::Message(Message::assistant("Answer")),
            TranscriptItem::Message(Message::user("Next")),
        ]);
        let starts = conversation.turn_starts(false);
        let mut repeated = vec![starts[1], starts[0], starts[0]];
        assert_eq!(turn_rows(&repeated, &FoldState::default()), [0, 2]);
        repeated.clear();
        assert!(turn_rows(&repeated, &FoldState::default()).is_empty());
    }
    fn job_notice(header: Option<NativeHeader>) -> HistoryEntry {
        use zevria_foundation::*;
        HistoryEntry::JobNotice(
            CommandJobNotice::new(
                CommandJobNoticeKind::Continuation,
                vec![CommandJobSnapshot {
                    id: CommandJobId::new("job"),
                    call_id: None,
                    command: "echo done".into(),
                    mode: CommandJobMode::Background,
                    status: CommandJobStatus::Exited {
                        code: Some(0),
                        success: true,
                        signal: None,
                    },
                    started_at: std::time::SystemTime::UNIX_EPOCH,
                    finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
                    stdout_bytes: 5,
                    stderr_bytes: 0,
                }],
                vec![],
                "Background job finished".into(),
            ),
            header,
        )
    }
    #[test]
    fn special_native_headers_share_the_real_turn_boundaries() {
        let mut conversation = ConversationState::default();
        conversation.push_entry(entry(Message::system("Leading group")));
        conversation.push_entry(assistant());
        conversation.push_user_turn(Message::user("Prompt or skill prompt"), DisplayTurn(1));
        conversation.push_entry(assistant());
        conversation.push_entry(assistant());
        conversation.push_plan_handoff(PlanHandoff::new(plan(), "session"), DisplayTurn(2));
        conversation.push_entry(assistant());
        conversation.push_entry(assistant());
        conversation.push_entry(job_notice(Some(NativeHeader::Prompt(DisplayTurn(3)))));
        conversation.push_entry(assistant());
        conversation.push_entry(assistant());
        conversation.push_entry(HistoryEntry::Ensemble(
            zevria_presentation::conversation::EnsembleHistory {
                header: Some(NativeHeader::Prompt(DisplayTurn(4))),
                run_id: zevria_workflow::EnsembleRunId::from_string("ensemble"),
                workflow: zevria_workflow::EnsembleWorkflow::Review,
                prompt: zevria_content::UserPrompt::from_text("Review"),
                workers: vec![],
                baseline: None,
            },
        ));
        conversation.push_entry(assistant());
        conversation.push_entry(assistant());
        assert_eq!(
            turn_rows(&conversation.turn_starts(false), &FoldState::default()),
            [2, 5, 8, 11]
        );
    }
    #[test]
    fn acp_user_groups_in_one_row_share_a_stop() {
        let mut blocks = Vec::new();
        for message in [
            Message::user("First ACP group"),
            Message::assistant("Mixed answer"),
            Message::user("Second ACP group"),
        ] {
            let HistoryEntry::Conversation(entry) = entry(message) else {
                unreachable!()
            };
            blocks.extend(entry.blocks);
        }
        let mut conversation = ConversationState::default();
        conversation.push_entry(HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks,
        }));
        conversation.push_entry(assistant());
        conversation.push_entry(assistant());
        let starts = conversation.turn_starts(false);
        assert_eq!(starts.len(), 2);
        assert!(starts.iter().all(|start| start.block.is_some()));
        assert_eq!(turn_rows(&starts, &FoldState::default()), [0]);
    }
    #[test]
    fn covered_turn_stops_land_on_the_summary_and_deduplicate() {
        let mut conversation = ConversationState::default();
        conversation.restore(vec![
            TranscriptItem::Message(Message::user("First")),
            TranscriptItem::Message(Message::assistant("Progress")),
            TranscriptItem::Message(Message::user("Second")),
            TranscriptItem::Message(Message::user("Third")),
            TranscriptItem::Message(Message::user("Fourth")),
        ]);
        let starts = conversation.turn_starts(false);
        assert_eq!(turn_rows(&starts, &FoldState::default()), [0, 2, 3, 4]);
        let mut folds = FoldState::default();
        folds.fold(FoldKey::Span { start: 1, end: 3 });
        let rows = turn_rows(&starts, &folds);
        assert_eq!(rows, [0, 1, 4]);
        assert_eq!(destination(&rows, 0, false, TurnDirection::Next), Some(1));
        assert_eq!(destination(&rows, 1, false, TurnDirection::Next), Some(4));
        assert_eq!(
            destination(&rows, 4, false, TurnDirection::Previous),
            Some(1)
        );
    }
    #[test]
    fn destinations_are_strict_row_granular_and_never_wrap() {
        use TurnDirection::*;
        let rows = [0, 5, 10];
        for (row, within, previous, next) in [
            (0, false, None, Some(5)),
            (0, true, Some(0), Some(5)),
            (3, false, Some(0), Some(5)),
            (5, false, Some(0), Some(10)),
            (5, true, Some(5), Some(10)),
            (9, true, Some(5), Some(10)),
            (10, false, Some(5), None),
            (99, false, Some(10), None),
        ] {
            assert_eq!(destination(&rows, row, within, Previous), previous);
            assert_eq!(destination(&rows, row, within, Next), next);
        }
        assert_eq!(destination(&[], 0, false, Previous), None);
        assert_eq!(destination(&[], 0, true, Next), None);
    }
}
