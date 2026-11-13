//! Renderer-free Desktop turn grouping and row-granular navigation.
//! All identities and folds here are view state, never provider/persisted state.
use zevria_presentation::{
    blocks::{DisplayTurn, NativeHeader, PresentationRole},
    conversation::{HistoryEntry, TurnStartTarget},
};

pub fn turn_rows(starts: &[TurnStartTarget]) -> Vec<usize> {
    let mut rows = starts
        .iter()
        .map(|start| start.history_index)
        .collect::<Vec<_>>();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepRun {
    pub start: usize,
    /// Inclusive; empty interior entries belong to the surrounding run.
    pub end: usize,
    pub live: bool,
}

fn header(entry: &HistoryEntry) -> Option<NativeHeader> {
    match entry {
        HistoryEntry::Conversation(entry) => entry.header,
        HistoryEntry::PlanHandoff(_, header) | HistoryEntry::JobNotice(_, header) => *header,
        HistoryEntry::Ensemble(entry) => entry.header,
        _ => None,
    }
}

pub fn step_runs(
    history: &[HistoryEntry],
    turn_rows: &[usize],
    live_turn: Option<DisplayTurn>,
) -> Vec<StepRun> {
    let mut boundaries = turn_rows
        .iter()
        .copied()
        .filter(|row| *row < history.len())
        .collect::<Vec<_>>();
    boundaries.push(0);
    boundaries.push(history.len());
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut runs = Vec::new();
    for group in boundaries.windows(2) {
        let (start, end) = (group[0], group[1]);
        // Trailing errors/notices must not displace the last assistant response.
        let Some(final_response) = (start..end).rev().find(|&index| {
            matches!(&history[index], HistoryEntry::Conversation(entry)
                if entry.blocks.iter().any(|block| block.visible(false)
                    && block.role == Some(PresentationRole::Assistant)))
        }) else {
            continue;
        };
        let live = live_turn.is_some_and(|turn| {
            history[start..end]
                .iter()
                .any(|entry| header(entry).is_some_and(|h| h.turn() == turn))
        });
        let first = start + usize::from(turn_rows.contains(&start));
        let mut run: Option<StepRun> = None;
        for (index, entry) in history.iter().enumerate().take(final_response).skip(first) {
            if matches!(
                entry,
                HistoryEntry::PlanArtifact(_)
                    | HistoryEntry::Error(_)
                    | HistoryEntry::CompactionDivider
            ) {
                if let Some(run) = run.take() {
                    runs.push(run);
                }
            } else if entry.message_fold_eligible(false) {
                run.get_or_insert(StepRun {
                    start: index,
                    end: index,
                    live,
                })
                .end = index;
            }
        }
        if let Some(run) = run {
            runs.push(run);
        }
    }
    runs
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct StepCounts {
    pub tool_calls: usize,
    pub thoughts: usize,
    pub messages: usize,
    pub added: usize,
    pub removed: usize,
}
impl StepCounts {
    pub fn steps(&self) -> usize {
        self.tool_calls + self.thoughts + self.messages
    }
    pub fn title(&self) -> String {
        counted(self.steps(), "step", "steps")
    }
    pub fn description(&self) -> String {
        [
            (self.tool_calls, "tool call", "tool calls"),
            (self.thoughts, "thought", "thoughts"),
            (self.messages, "message", "messages"),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, singular, plural)| counted(count, singular, plural))
        .collect::<Vec<_>>()
        .join(", ")
    }
}
fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::Message;
    use zevria_presentation::{
        blocks::ConversationEntry,
        conversation::{ConversationState, ToolCallStatus},
    };
    use zevria_transcript::transcript::TranscriptItem;
    use zevria_workflow::{PlanArtifact, PlanHandoff, PlanId, PlanVersion};

    fn entry(message: Message) -> HistoryEntry {
        HistoryEntry::from_message(message, ToolCallStatus::Finished).unwrap()
    }
    fn assistant() -> HistoryEntry {
        entry(Message::assistant("Answer or progress"))
    }
    fn empty() -> HistoryEntry {
        HistoryEntry::Conversation(ConversationEntry {
            header: None,
            blocks: Vec::new(),
        })
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
    fn run(start: usize, end: usize) -> StepRun {
        StepRun {
            start,
            end,
            live: false,
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
        assert_eq!(turn_rows(&repeated), [0, 2]);
        repeated.clear();
        assert!(turn_rows(&repeated).is_empty());
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
        let rows = turn_rows(&conversation.turn_starts(false));
        assert_eq!(rows, [2, 5, 8, 11]);
        let runs = step_runs(conversation.history(), &rows, Some(DisplayTurn(4)));
        assert_eq!(runs[..4], [run(0, 0), run(3, 3), run(6, 6), run(9, 9)]);
        assert_eq!(
            runs[4],
            StepRun {
                start: 12,
                end: 12,
                live: true
            }
        );
    }
    #[test]
    fn acp_user_groups_in_one_row_share_a_stop_and_never_fold_the_prompt_row() {
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
        assert_eq!(turn_rows(&starts), [0]);
        assert_eq!(
            step_runs(conversation.history(), &turn_rows(&starts), None),
            [run(1, 1)]
        );
    }
    #[test]
    fn groups_keep_starts_and_include_a_leading_headerless_group() {
        let history = vec![
            entry(Message::system("Startup")),
            assistant(),
            entry(Message::user("Prompt")),
            assistant(),
            assistant(),
            HistoryEntry::PlanHandoff(
                PlanHandoff::new(plan(), "session"),
                Some(NativeHeader::Prompt(DisplayTurn(2))),
            ),
            assistant(),
            assistant(),
            entry(Message::system("Continuation heading")),
            assistant(),
            assistant(),
        ];
        assert_eq!(
            step_runs(&history, &[2, 5, 8], None),
            [run(0, 0), run(3, 3), run(6, 6), run(9, 9)]
        );
    }
    #[test]
    fn trailing_error_and_notice_never_hide_the_answer() {
        let history = vec![
            entry(Message::user("Prompt")),
            assistant(),
            assistant(),
            HistoryEntry::Error("Stopped".into()),
            entry(Message::system("Startup notice")),
            job_notice(None),
        ];
        assert_eq!(step_runs(&history, &[0], None), [run(1, 1)]);
    }
    #[test]
    fn kept_entries_split_runs_and_empty_interior_entries_are_absorbed() {
        let history = vec![
            entry(Message::user("Prompt")),
            assistant(),
            empty(),
            assistant(),
            HistoryEntry::PlanArtifact(plan()),
            assistant(),
            HistoryEntry::Error("Error".into()),
            assistant(),
            HistoryEntry::CompactionDivider,
            assistant(),
            assistant(),
        ];
        assert_eq!(
            step_runs(&history, &[0], None),
            [run(1, 3), run(5, 5), run(7, 7), run(9, 9)]
        );
    }
    #[test]
    fn prompt_only_response_less_and_direct_answers_have_no_runs() {
        for history in [
            vec![],
            vec![entry(Message::user("Prompt"))],
            vec![
                entry(Message::user("Prompt")),
                entry(Message::system("Notice")),
                HistoryEntry::Error("Error".into()),
            ],
            vec![entry(Message::user("Prompt")), assistant()],
        ] {
            assert!(step_runs(&history, &[0], None).is_empty());
        }
    }
    #[test]
    fn live_group_uses_any_native_header_not_only_the_run() {
        let mut prompt = entry(Message::user("Prompt"));
        let HistoryEntry::Conversation(ref mut entry) = prompt else {
            unreachable!()
        };
        entry.header = Some(NativeHeader::Prompt(DisplayTurn(3)));
        let history = vec![prompt, assistant(), assistant()];
        assert_eq!(
            step_runs(&history, &[0], Some(DisplayTurn(3))),
            [StepRun {
                start: 1,
                end: 1,
                live: true
            }]
        );
        assert_eq!(step_runs(&history, &[0], Some(DisplayTurn(4))), [run(1, 1)]);
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
    #[test]
    fn counts_omit_zero_parts_and_pluralize() {
        let mut counts = StepCounts::default();
        assert_eq!(counts.title(), "0 steps");
        assert_eq!(counts.description(), "");
        counts.tool_calls = 1;
        assert_eq!(counts.title(), "1 step");
        assert_eq!(counts.description(), "1 tool call");
        counts.thoughts = 1;
        counts.messages = 1;
        assert_eq!(counts.description(), "1 tool call, 1 thought, 1 message");
        counts.tool_calls = 8;
        counts.thoughts = 4;
        counts.messages = 2;
        counts.added = 42;
        counts.removed = 7;
        assert_eq!(counts.title(), "14 steps");
        assert_eq!(counts.description(), "8 tool calls, 4 thoughts, 2 messages");
    }
}
