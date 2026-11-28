//! Materialize the append-only ACP event journal into semantic TUI blocks.

use std::collections::{BTreeSet, HashMap, HashSet};

use zevria_content::response::{
    DisplayProjectionBinding as Binding, DisplayProjectionKind as DisplayKind,
};
use zevria_workflow::AgentProtocolDirection;
use zevria_workflow::AgentRunEvent;
use zevria_workflow::AgentRunLocation;
use zevria_workflow::AgentStructuredPlan;
use zevria_workflow::AgentUsage;

use crate::{
    conversation::{ConversationChange, ConversationState, HistoryEntry},
    presentation::{
        AcpToolPresentation, BlockVisibility, ChecklistItem, ChecklistStatus, ConversationEntry,
        DiagnosticTone, PresentationBlock, PresentationBlockId, PresentationBlockKind,
        PresentationRole, PresentedChecklist, PresentedDiagnostic, PresentedPlan,
        PresentedPlanContent, PresentedTool, PromptAnnotation, PromptOrigin, PromptPhase,
        TextFlavor,
    },
};

#[path = "agent_transcript_review.rs"]
mod review;
use review::ReviewProjection;

#[cfg(test)]
mod hosted_tests {
    use super::*;
    use crate::{blocks::WebActionKind, status_icon::StatusIcon};
    use serde_json::json;

    #[test]
    fn fallback_hosted_group_keeps_order_and_structured_outcomes() {
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        for (index, kind, status) in [(2, "search", "completed"), (3, "open_page", "in_progress")] {
            reducer.apply_event(
                &mut conversation,
                AgentRunEvent::ToolCall {
                    id: format!("hosted-search:attempt:{index}"),
                    title: "Web search".into(),
                    kind: "search".into(),
                    status: status.into(),
                    content: vec![],
                    locations: vec![],
                    raw_output: None,
                    raw_input: Some(
                        json!({"origin":"provider_hosted_web_search","action":{"type":kind}}),
                    ),
                },
            );
        }
        let [HistoryEntry::Conversation(entry)] = conversation.history() else {
            panic!("one entry")
        };
        let groups = entry
            .blocks
            .iter()
            .filter_map(|block| match &block.kind {
                PresentationBlockKind::WebActivity(activity) if block.visible(false) => {
                    Some(activity)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [group] = groups.as_slice() else {
            panic!("one hosted group")
        };
        assert_eq!(group.members, [2, 3]);
        assert_eq!(group.status(), StatusIcon::Running);
        assert_eq!(group.copy_text(), "Web actions · 2 · 1 ✓ · 1 ◐");
        assert_eq!(
            group
                .actions
                .iter()
                .map(|action| (action.output_index, action.kind, action.status))
                .collect::<Vec<_>>(),
            [
                (2, WebActionKind::Search, StatusIcon::Done),
                (3, WebActionKind::OpenPage, StatusIcon::Running),
            ]
        );
    }

    #[test]
    fn indexed_acp_answer_projects_sources_on_its_covered_text() {
        use zevria_content::response::{
            DisplayProjectionBinding, DisplayProjectionKind, RESPONSE_DISPLAY_VERSION,
            ResponseDisplay, responses_native_display,
        };
        let (parts, web) = responses_native_display(&[
            json!({"type":"web_search_call","action":{"type":"search","query":"Rust"},"status":"completed"}),
            json!({"type":"message","content":[{"type":"output_text","text":"Claim.","annotations":[
                {"type":"url_citation","url":"https://example.org/","title":"Source","start_index":0,"end_index":6}
            ]}]}),
        ]);
        let mut attempt =
            zevria_content::ResponseAttempt::new(zevria_foundation::ModelProfileRef::new("p", "m"));
        attempt.complete_with(parts, web);
        let text = "Claim. [Source](https://example.org/)";
        let display = ResponseDisplay {
            version: RESPONSE_DISPLAY_VERSION,
            bindings: vec![DisplayProjectionBinding::Text {
                kind: DisplayProjectionKind::Message,
                message_id: "answer".into(),
                start: 0,
                end: text.len(),
                sources: vec![attempt.presentation[0].source.clone()],
            }],
            attempt,
        };
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::AgentMessage {
                text: text.into(),
                message_id: Some("answer".into()),
            },
        );
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ResponseDisplay {
                display: Box::new(display),
            },
        );
        let answers = conversation
            .history()
            .iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Conversation(entry) => Some(&entry.blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match &block.kind {
                PresentationBlockKind::Text { text, sources, .. } if block.visible(false) => {
                    Some((text, sources))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].0, text);
        assert_eq!(answers[0].1[0].url, "https://example.org/");
    }

    #[test]
    fn new_hosted_actions_separate_text_but_sparse_updates_do_not() {
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        reducer.apply_preview(
            &mut conversation,
            AgentRunEvent::AgentMessage {
                text: "before".into(),
                message_id: Some("message".into()),
            },
        );
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ToolCall {
                id: "hosted-search:attempt:0".into(),
                title: "Web search".into(),
                kind: "search".into(),
                status: "in_progress".into(),
                content: vec![],
                locations: vec![],
                raw_input: Some(serde_json::json!({"origin":"provider_hosted_web_search"})),
                raw_output: None,
            },
        );
        reducer.apply_preview(
            &mut conversation,
            AgentRunEvent::AgentMessage {
                text: "after".into(),
                message_id: Some("message".into()),
            },
        );
        reducer.apply_event(
            &mut conversation,
            AgentRunEvent::ToolCallUpdate {
                id: "hosted-search:attempt:0".into(),
                title: None,
                kind: None,
                status: Some("completed".into()),
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
        );
        reducer.apply_preview(
            &mut conversation,
            AgentRunEvent::AgentMessage {
                text: "after continued".into(),
                message_id: Some("message".into()),
            },
        );
        let blocks = conversation
            .history()
            .iter()
            .filter_map(|entry| {
                if let HistoryEntry::Conversation(entry) = entry {
                    Some(&entry.blocks)
                } else {
                    None
                }
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(&block.kind, PresentationBlockKind::Text { .. }))
                .count(),
            2
        );
        assert!(
            blocks
                .iter()
                .any(|block| block.primary_copy() == "after continued")
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum SegmentKind {
    User,
    Agent,
    Thought,
}

#[derive(Clone, Debug)]
struct SegmentTarget {
    kind: SegmentKind,
    message_id: Option<String>,
    location: BlockLocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct BlockLocation {
    history_index: usize,
    block_id: PresentationBlockId,
}

#[derive(Clone, Debug)]
struct PromptEcho {
    prompt: String,
    accumulated: String,
}

struct ReplayStage {
    reducer: AgentTranscriptReducer,
    conversation: ConversationState,
}

/// Stateful projection used by one live or historical ACP worker pane.
#[derive(Default)]
pub struct AgentTranscriptReducer {
    current_entry: Option<usize>,
    segment: Option<SegmentTarget>,
    tools: HashMap<String, BlockLocation>,
    current_plan: Option<BlockLocation>,
    current_plan_markdown: Option<String>,
    plan_scope_start: usize,
    review: ReviewProjection,
    latest_usage: Option<AgentUsage>,
    replay: Option<Box<ReplayStage>>,
    review_inputs: std::collections::HashSet<zevria_workflow::WorkerControlId>,
    review_markers: std::collections::HashSet<String>,
    replaying_review: bool,
    review_sealed: bool,
    review_abandoned: bool,
    echo: EchoFilter,
    coverage: DisplayCoverageState,
}

#[derive(Default)]
struct EchoFilter {
    prompt_echo: Option<PromptEcho>,
    review_prompts: HashMap<u64, zevria_content::UserPrompt>,
    image_echoes: std::collections::VecDeque<zevria_content::PromptImage>,
    user_group: Option<(Option<String>, PresentationBlockId, usize)>,
}

#[derive(Default)]
struct DisplayCoverageState {
    projections: HashMap<(SegmentKind, String), (String, Vec<ProjectionPiece>)>,
    displays: HashMap<String, zevria_content::response::ResponseDisplay>,
    // Latest coverage per emitted segment/range, not a history of token
    // snapshots. A corrected segment may arrive before its predecessor's
    // coalesced preview; keep enough evidence to cover that predecessor later.
    display_text_coverage:
        HashMap<String, HashMap<(SegmentKind, String, usize), DisplayTextCoverage>>,
    display_segments: HashMap<(SegmentKind, String), HashSet<String>>,
    display_tools: HashMap<String, HashSet<String>>,
    displays_dirty: BTreeSet<String>,
    hosted_actions_dirty: BTreeSet<usize>,
    fallback_groups: HashMap<String, PresentationBlockId>,
    fallback_group_ids: HashSet<PresentationBlockId>,
    applied_answers: HashMap<String, usize>,
}

struct DisplayTextCoverage {
    end: usize,
    text: String,
}

#[derive(Clone)]
struct ProjectionPiece {
    start: usize,
    end: usize,
    location: BlockLocation,
}

impl AgentTranscriptReducer {
    /// Presentation-only boundary, also restored from a root snapshot without
    /// inventing a worker journal receipt or discarding preceding history.
    pub fn abandon(&mut self, conversation: &mut ConversationState) -> ConversationChange {
        if !self.review_abandoned {
            self.review_abandoned = true;
            self.replay = None;
            self.fence();
            self.append_block(
                conversation,
                None,
                BlockVisibility::Always,
                PresentationBlockKind::Text {
                    text: zevria_workflow::ensemble::WORKER_ABANDONMENT_REASON.into(),
                    flavor: TextFlavor::Plain,
                    editable: false,
                    sources: Vec::new(),
                },
            );
        }
        ConversationChange::default()
    }

    /// Apply one lossless durable/lifecycle event. Message and thought values
    /// are deltas on this path.
    pub fn apply_event(
        &mut self,
        conversation: &mut ConversationState,
        event: AgentRunEvent,
    ) -> ConversationChange {
        if self.review_abandoned {
            return ConversationChange::default();
        }
        if self.review_sealed
            && matches!(
                &event,
                AgentRunEvent::Plan { .. }
                    | AgentRunEvent::NativePlanCaptured { .. }
                    | AgentRunEvent::PlanRemoved { .. }
            )
        {
            // Late provider telemetry remains in JSONL but cannot replace the
            // host's frozen final proposal. Outcome reconciliation is explicit.
            return ConversationChange::default();
        }
        if !self.review_inputs.is_empty() && matches!(&event, AgentRunEvent::ReplayBoundary) {
            self.replaying_review = true;
            self.review_replay_boundary();
            return ConversationChange::default();
        }
        if self.replaying_review {
            if matches!(
                &event,
                AgentRunEvent::SessionEstablished { .. }
                    | AgentRunEvent::Failure { .. }
                    | AgentRunEvent::Review { .. }
            ) {
                self.replaying_review = false;
            } else {
                return ConversationChange::default();
            }
        }
        if matches!(&event, AgentRunEvent::ReplayBoundary) {
            self.begin_replay();
            return ConversationChange::default();
        }
        let mut change = ConversationChange::default();
        if self.replay.is_some() {
            let commit = matches!(
                &event,
                AgentRunEvent::SessionEstablished {
                    recovered: true,
                    ..
                }
            );
            let rollback = match &event {
                AgentRunEvent::SessionEstablished {
                    recovered: false, ..
                } => true,
                AgentRunEvent::Status { status, .. } => status.is_terminal(),
                _ => false,
            };
            if commit {
                change.merge(self.commit_replay(conversation));
            } else if rollback {
                self.replay = None;
            } else {
                let replay = self.replay.as_mut().expect("replay stage exists");
                let _ = replay.reducer.apply_event(&mut replay.conversation, event);
                return change;
            }
        }
        if matches!(&event, AgentRunEvent::SessionEstablished { .. }) {
            self.review_session_established();
        }
        match event {
            AgentRunEvent::ResponseDisplay { display } => self.install_display(*display),
            AgentRunEvent::ReplayBoundary => unreachable!("replay boundary handled above"),
            AgentRunEvent::Review { event } => self.apply_review_event(conversation, *event),
            AgentRunEvent::Prompt { mut text, .. } if !self.review_inputs.is_empty() => {
                self.fence();
                for (index, image) in self.echo.image_echoes.iter().enumerate() {
                    text = text.replacen(&image.label(index + 1), "", 1);
                }
                self.enable_review_publication();
                self.echo.prompt_echo = Some(PromptEcho {
                    prompt: text,
                    accumulated: String::new(),
                });
            }
            AgentRunEvent::Prompt {
                text, continuation, ..
            } => {
                self.begin_prompt(
                    conversation,
                    text,
                    continuation,
                    PromptAnnotation {
                        origin: if continuation {
                            PromptOrigin::Continuation
                        } else {
                            PromptOrigin::Initial
                        },
                        ..PromptAnnotation::default()
                    },
                );
            }
            AgentRunEvent::UserImage { image, message_id } => {
                if self.echo.image_echoes.front() == Some(&image) {
                    self.echo.image_echoes.pop_front();
                    return change;
                }
                let ordinal = self
                    .echo
                    .user_group
                    .as_ref()
                    .filter(|(id, _, _)| id == &message_id)
                    .map_or(1, |(_, _, ordinal)| ordinal + 1);
                self.fence();
                let location = self.append_block(
                    conversation,
                    Some(PresentationRole::User),
                    BlockVisibility::Always,
                    PresentationBlockKind::Image {
                        image,
                        ordinal,
                        editable: false,
                    },
                );
                self.group_user_block(conversation, location, message_id, ordinal);
            }
            AgentRunEvent::UserMessage { text, message_id } => {
                self.apply_user_delta(conversation, text, message_id)
            }
            AgentRunEvent::AgentMessage { text, message_id } => self.apply_segment(
                conversation,
                SegmentKind::Agent,
                message_id,
                text,
                SegmentUpdate::Append,
            ),
            AgentRunEvent::Thought { text, message_id } => self.apply_segment(
                conversation,
                SegmentKind::Thought,
                message_id,
                text,
                SegmentUpdate::Append,
            ),
            AgentRunEvent::ToolCall {
                id,
                title,
                kind,
                status,
                content,
                locations,
                raw_input,
                raw_output,
            } => self.apply_tool_call(
                conversation,
                AcpToolPresentation {
                    metadata: None,
                    id,
                    title,
                    kind,
                    status,
                    content,
                    locations,
                    raw_input,
                    raw_output,
                },
            ),
            AgentRunEvent::ToolCallUpdate {
                id,
                title,
                kind,
                status,
                content,
                locations,
                raw_input,
                raw_output,
            } => self.apply_tool_update(
                conversation,
                id,
                title,
                kind,
                status,
                content,
                locations,
                raw_input,
                raw_output,
            ),
            AgentRunEvent::ToolResultMetadata { metadata } => {
                if let Some(location) = self.tools.get(&metadata.id).copied()
                    && let Some(block) =
                        conversation.hinted_block_mut(location.history_index, location.block_id)
                    && let PresentationBlockKind::Tool(PresentedTool::Acp(tool)) = &mut block.kind
                    && tool.metadata.as_ref() != Some(metadata.as_ref())
                {
                    let id = metadata.id.clone();
                    tool.metadata = Some(*metadata);
                    let hosted = tool.is_hosted_search();
                    block.touch();
                    conversation.touch_entry(location.history_index);
                    self.tool_changed(&id, location, hosted);
                }
            }
            AgentRunEvent::Plan { plan } | AgentRunEvent::NativePlanCaptured { plan, .. } => {
                self.observe_review_plan(&plan);
                self.apply_plan(conversation, plan);
            }
            AgentRunEvent::PlanRemoved { plan_id } => {
                self.observe_review_removal(&plan_id);
                self.remove_plan(conversation, &plan_id);
            }
            AgentRunEvent::Status { status, .. } if status.is_terminal() => {
                if let Some(index) = self.current_entry {
                    if status == zevria_workflow::AgentRunStatus::Completed {
                        conversation.complete_agent_entry(index);
                    } else {
                        conversation.mark_agent_entry_incomplete(index);
                    }
                }
                self.append_diagnostic(conversation, diagnostic_for_event(&event));
            }
            AgentRunEvent::Failure { error } => {
                if let Some(index) = self.current_entry {
                    conversation.mark_agent_entry_incomplete(index);
                }
                self.observe_provider_failure(conversation, &error);
                self.fence();
                self.append_block(
                    conversation,
                    None,
                    BlockVisibility::Always,
                    PresentationBlockKind::Error(error),
                );
            }
            AgentRunEvent::Unsupported {
                context,
                placeholder,
            } if context == "agent message" || context == "agent thought" => {
                self.fence();
                self.append_block(
                    conversation,
                    Some(PresentationRole::Assistant),
                    BlockVisibility::Always,
                    PresentationBlockKind::Placeholder(format!(
                        "[unsupported ACP {context}: {placeholder}]"
                    )),
                );
            }
            AgentRunEvent::Usage { usage } => {
                self.latest_usage = Some(usage.clone());
                self.append_diagnostic(
                    conversation,
                    diagnostic_for_event(&AgentRunEvent::Usage { usage }),
                );
            }
            AgentRunEvent::Protocol { direction, json } => {
                // ACP adapters commonly publish the raw packet immediately
                // before each normalized token. It remains lossless and in
                // chronological position, but is transparent to the active
                // semantic segment so token-sized chunks still materialize
                // as one message/reasoning block.
                self.append_transparent_diagnostic(
                    conversation,
                    diagnostic_for_event(&AgentRunEvent::Protocol { direction, json }),
                );
            }
            event => self.append_diagnostic(conversation, diagnostic_for_event(&event)),
        }
        self.flush_projection_changes(conversation);
        change
    }

    /// Apply one lossy latest-value message/thought preview. The text already
    /// includes every delta in the current segment and therefore replaces the
    /// matching block rather than appending to it.
    pub fn apply_preview(
        &mut self,
        conversation: &mut ConversationState,
        event: AgentRunEvent,
    ) -> ConversationChange {
        if self.review_abandoned {
            return ConversationChange::default();
        }
        if let Some(replay) = &mut self.replay {
            replay
                .reducer
                .apply_preview(&mut replay.conversation, event);
            return ConversationChange::default();
        }
        match event {
            AgentRunEvent::AgentMessage { text, message_id } => self.apply_segment(
                conversation,
                SegmentKind::Agent,
                message_id,
                text,
                SegmentUpdate::Replace,
            ),
            AgentRunEvent::Thought { text, message_id } => self.apply_segment(
                conversation,
                SegmentKind::Thought,
                message_id,
                text,
                SegmentUpdate::Replace,
            ),
            event => return self.apply_event(conversation, event),
        }
        self.flush_projection_changes(conversation);
        ConversationChange::default()
    }

    /// Repair a lossy live preview from the terminal durable report while
    /// retaining thoughts, tools, plans, and diagnostics in their positions.
    pub fn reconcile_report(
        &mut self,
        conversation: &mut ConversationState,
        report: &str,
    ) -> ConversationChange {
        if self.review_abandoned {
            return ConversationChange::default();
        }
        if let Some(replay) = &mut self.replay {
            return replay
                .reducer
                .reconcile_report(&mut replay.conversation, report);
        }
        if report.is_empty() {
            return ConversationChange::default();
        }
        // This fallback repairs a flattened lossy preview, not indexed source
        // content. Replacing a verified indexed display with the flattened
        // report would duplicate covered text and erase its source boundaries.
        // This also applies before terminal metadata arrives and to retained
        // failed/interrupted answers; a report cannot promote that evidence.
        if self
            .coverage
            .applied_answers
            .iter()
            .any(|(id, history_index)| {
                Some(*history_index) == self.current_entry
                    && self.coverage.displays.contains_key(id)
            })
        {
            return ConversationChange::default();
        }
        let history_index = self
            .current_entry
            .unwrap_or_else(|| self.ensure_entry(conversation));
        let prior_report = conversation.history()[..history_index]
            .iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Conversation(entry) => Some(&entry.blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match &block.kind {
                PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Markdown,
                    editable: false,
                    ..
                } if block.role == Some(PresentationRole::Assistant) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let report = if prior_report.is_empty() {
            report
        } else {
            report
                .strip_prefix(&prior_report)
                .and_then(|suffix| suffix.strip_prefix('\n'))
                .unwrap_or(report)
        };
        let Some(HistoryEntry::Conversation(entry)) = conversation.entry(history_index) else {
            return ConversationChange::default();
        };
        let message_indexes = entry
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| {
                (matches!(
                    block.kind,
                    PresentationBlockKind::Text {
                        flavor: TextFlavor::Markdown,
                        editable: false,
                        ..
                    }
                ) && block.role == Some(PresentationRole::Assistant))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let projected = message_indexes
            .iter()
            .filter_map(|index| match &entry.blocks[*index].kind {
                PresentationBlockKind::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if projected == report {
            return ConversationChange::default();
        }

        let replacement = message_indexes.is_empty().then(|| {
            Self::new_block(
                conversation,
                Some(PresentationRole::Assistant),
                BlockVisibility::Always,
                PresentationBlockKind::Text {
                    text: report.to_string(),
                    flavor: TextFlavor::Markdown,
                    editable: false,
                    sources: Vec::new(),
                },
            )
        });
        let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(history_index) else {
            return ConversationChange::default();
        };
        if let Some(first) = message_indexes.first().copied() {
            if let PresentationBlockKind::Text { text, .. } = &mut entry.blocks[first].kind {
                text.clear();
                text.push_str(report);
                entry.blocks[first].touch();
            }
            for index in message_indexes.into_iter().skip(1).rev() {
                entry.blocks.remove(index);
            }
        } else if let Some(block) = replacement {
            entry.blocks.push(block);
        }
        conversation.change_structure();
        self.rebuild_locations(conversation);
        self.fence();
        self.echo.user_group = None;
        ConversationChange::replaced_from(history_index)
    }

    pub fn reconcile_plan(
        &mut self,
        conversation: &mut ConversationState,
        plan: &AgentStructuredPlan,
    ) -> ConversationChange {
        if self.review_abandoned {
            return ConversationChange::default();
        }
        if let Some(replay) = &mut self.replay {
            return replay
                .reducer
                .reconcile_plan(&mut replay.conversation, plan);
        }
        self.apply_plan(conversation, plan.clone());
        ConversationChange::default()
    }

    pub fn report_duplicates_current_plan(&self, report: &str) -> bool {
        self.current_plan_markdown.as_deref() == Some(report)
    }

    fn begin_prompt(
        &mut self,
        conversation: &mut ConversationState,
        text: impl Into<zevria_content::UserPrompt>,
        continuation: bool,
        annotation: PromptAnnotation,
    ) -> Option<BlockLocation> {
        let prompt = text.into();
        self.echo.user_group = None;
        self.fence();
        let history_index =
            conversation.push_entry(HistoryEntry::Conversation(ConversationEntry {
                references: Vec::new(),
                header: None,
                blocks: Vec::new(),
            }));
        self.current_entry = Some(history_index);
        conversation.set_agent_entry(history_index);
        if !continuation {
            self.current_plan = None;
            self.current_plan_markdown = None;
            self.plan_scope_start = history_index;
        }
        // Review acceptance can queue ahead of the active input. Only dispatch
        // installs its echo queue; ordinary prompts have no separate dispatch.
        if annotation.generation.is_none() {
            self.echo.prompt_echo = Some(PromptEcho {
                prompt: prompt.text_projection(),
                accumulated: String::new(),
            });
            self.echo.image_echoes = prompt.images().cloned().collect();
        }
        let mut header: Option<BlockLocation> = None;
        let mut ordinal = 0;
        for block in prompt.into_blocks() {
            let kind = match block {
                zevria_content::PromptBlock::Text(text) => PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Plain,
                    editable: false,
                    sources: Vec::new(),
                },
                zevria_content::PromptBlock::Image(image) => {
                    ordinal += 1;
                    PresentationBlockKind::Image {
                        image,
                        ordinal,
                        editable: false,
                    }
                }
            };
            let location = self.append_block(
                conversation,
                Some(PresentationRole::User),
                BlockVisibility::Always,
                kind,
            );
            let group = header.get_or_insert(location).block_id;
            if let Some(block) = block_mut(conversation, location) {
                block.prompt_group = Some(group);
                if group == block.id {
                    block.prompt = Some(annotation.clone());
                }
                conversation.touch_entry(location.history_index);
            }
        }
        // Accepted review content is complete. Unmatched provider user chunks
        // must not inherit its provenance or lifecycle. Ordinary Prompt events
        // can still acquire following image/text segments without a message ID.
        if annotation.generation.is_none() {
            self.echo.user_group = header.map(|location| (None, location.block_id, ordinal));
        }
        header
    }

    fn group_user_block(
        &mut self,
        conversation: &mut ConversationState,
        location: BlockLocation,
        message_id: Option<String>,
        ordinal: usize,
    ) {
        let group = self
            .echo
            .user_group
            .as_ref()
            .filter(|(id, _, _)| id == &message_id)
            .map_or(location.block_id, |(_, group, _)| *group);
        self.echo.user_group = Some((message_id, group, ordinal));
        if let Some(block) = block_mut(conversation, location) {
            block.prompt_group = Some(group);
            if group == block.id {
                block.prompt = Some(PromptAnnotation::default());
            }
            conversation.touch_entry(location.history_index);
        }
    }

    fn apply_user_delta(
        &mut self,
        conversation: &mut ConversationState,
        text: String,
        message_id: Option<String>,
    ) {
        if let Some(echo) = &mut self.echo.prompt_echo {
            echo.accumulated.push_str(&text);
            if echo.prompt.starts_with(&echo.accumulated) {
                if echo.prompt == echo.accumulated {
                    self.echo.prompt_echo = None;
                }
                return;
            }
            let divergent = echo
                .accumulated
                .strip_prefix(&echo.prompt)
                .unwrap_or(&echo.accumulated)
                .to_string();
            self.echo.prompt_echo = None;
            if divergent.is_empty() {
                return;
            }
            self.apply_segment(
                conversation,
                SegmentKind::User,
                message_id,
                divergent,
                SegmentUpdate::Append,
            );
            return;
        }
        self.apply_segment(
            conversation,
            SegmentKind::User,
            message_id,
            text,
            SegmentUpdate::Append,
        );
    }

    fn apply_segment(
        &mut self,
        conversation: &mut ConversationState,
        kind: SegmentKind,
        message_id: Option<String>,
        text: String,
        update: SegmentUpdate,
    ) {
        if message_id.is_none() {
            self.apply_segment_raw(conversation, kind, None, text, update);
            if kind == SegmentKind::Agent
                && matches!(update, SegmentUpdate::Replace)
                && let Some(target) = &self.segment
            {
                conversation
                    .mark_agent_preview(target.location.history_index, target.location.block_id);
            }
            return;
        }
        let replacing_projection = if kind != SegmentKind::User
            && matches!(update, SegmentUpdate::Replace)
            && let Some(id) = &message_id
            && let Some(target) = &self.segment
            && target.kind == kind
            && target.message_id.as_ref() == Some(id)
            && let Some((buffer, pieces)) = self.coverage.projections.get(&(kind, id.clone()))
            && let Some(piece) = pieces
                .last()
                .filter(|piece| piece.location == target.location)
        {
            // Coverage may have left only a suffix in the ordinary block.
            // The complete source slice is authoritative for Replace.
            if buffer[piece.start..piece.end] == text {
                self.echo.prompt_echo = None;
                self.echo.user_group = None;
                return;
            }
            true
        } else {
            false
        };
        let changed =
            self.apply_segment_raw(conversation, kind, message_id.clone(), text.clone(), update)
                || replacing_projection;
        if kind == SegmentKind::Agent
            && matches!(update, SegmentUpdate::Replace)
            && let Some(target) = &self.segment
        {
            conversation
                .mark_agent_preview(target.location.history_index, target.location.block_id);
        }
        if changed
            && kind != SegmentKind::User
            && let Some(message_id) = message_id
            && let Some(target) = &self.segment
        {
            let key = (kind, message_id);
            if replacing_projection
                && let Some(block) = block_mut(conversation, target.location)
                && block.visibility == BlockVisibility::Covered
            {
                block.visibility = BlockVisibility::Always;
                block.touch();
                conversation.touch_entry(target.location.history_index);
            }
            if let Some(ids) = self.coverage.display_segments.get(&key) {
                self.coverage.displays_dirty.extend(ids.iter().cloned());
            }
            let (buffer, pieces) = self.coverage.projections.entry(key).or_default();
            if let Some(piece) = pieces
                .last_mut()
                .filter(|piece| piece.location == target.location)
            {
                if matches!(update, SegmentUpdate::Replace) {
                    buffer.truncate(piece.start);
                }
                buffer.push_str(&text);
                piece.end = buffer.len();
            } else {
                let start = buffer.len();
                buffer.push_str(&text);
                pieces.push(ProjectionPiece {
                    start,
                    end: buffer.len(),
                    location: target.location,
                });
            }
        }
    }

    fn install_display(&mut self, display: zevria_content::response::ResponseDisplay) {
        let id = &display.attempt.id;
        if self.coverage.displays.get(id).is_some_and(|old| {
            old.attempt.revision > display.attempt.revision
                || (old.attempt.revision == display.attempt.revision
                    && (old.attempt != display.attempt || old.bindings == display.bindings))
        }) {
            return;
        }
        let Ok(parts) = display.validate_and_parts() else {
            return;
        };
        let coverage = self
            .coverage
            .display_text_coverage
            .entry(id.clone())
            .or_default();
        for binding in &display.bindings {
            match binding {
                Binding::Text {
                    kind,
                    message_id,
                    start,
                    end,
                    ..
                } => {
                    let kind = segment_kind(*kind);
                    self.coverage
                        .display_segments
                        .entry((kind, message_id.clone()))
                        .or_default()
                        .insert(id.clone());
                    let evidence = coverage.entry((kind, message_id.clone(), *start));
                    if let Some(text) = parts.binding_text(binding) {
                        evidence
                            .and_modify(|prior| {
                                prior.end = *end;
                                prior.text.clone_from(&text);
                            })
                            .or_insert(DisplayTextCoverage { end: *end, text });
                    }
                }
                Binding::Tool { tool_call_id, .. } => {
                    self.coverage
                        .display_tools
                        .entry(tool_call_id.clone())
                        .or_default()
                        .insert(id.clone());
                }
            }
        }
        self.coverage.displays_dirty.insert(id.clone());
        self.coverage.displays.insert(id.clone(), display);
    }

    fn tool_changed(&mut self, id: &str, location: BlockLocation, hosted: bool) {
        if let Some(attempts) = self.coverage.display_tools.get(id) {
            self.coverage
                .displays_dirty
                .extend(attempts.iter().cloned());
        }
        if hosted {
            self.coverage
                .hosted_actions_dirty
                .insert(location.history_index);
        }
    }

    fn flush_projection_changes(&mut self, conversation: &mut ConversationState) {
        if !self.coverage.displays_dirty.is_empty() {
            self.refresh_displays(conversation);
        }
        for index in std::mem::take(&mut self.coverage.hosted_actions_dirty) {
            self.regroup_hosted_actions(conversation, index);
        }
    }

    fn regroup_hosted_actions(
        &mut self,
        conversation: &mut ConversationState,
        history_index: usize,
    ) {
        let Some(HistoryEntry::Conversation(entry)) =
            conversation.entry_mut_untracked(history_index)
        else {
            return;
        };
        let original = std::mem::take(&mut entry.blocks);
        let old: Vec<_> = original
            .iter()
            .map(|block| (block.id, block.revision, block.visibility))
            .collect();
        let mut blocks = Vec::with_capacity(original.len());
        let mut old_groups = HashMap::new();
        let mut ordinary = Vec::new();
        for block in original {
            if self.coverage.fallback_group_ids.contains(&block.id) {
                old_groups.insert(block.id, block);
            } else {
                ordinary.push(block);
            }
        }
        let mut pending = Vec::new();
        let mut pending_attempt = None;
        for block in ordinary {
            let candidate = if block.visibility != BlockVisibility::Covered {
                if let PresentationBlockKind::Tool(PresentedTool::Acp(tool)) = &block.kind {
                    tool.hosted_activity().map(|activity| {
                        (
                            tool.id
                                .rsplit_once(':')
                                .map(|(prefix, _)| prefix.to_string())
                                .unwrap_or_else(|| tool.id.clone()),
                            activity.detail.is_none(),
                        )
                    })
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((attempt, unknown)) = candidate {
                if pending_attempt
                    .as_ref()
                    .is_some_and(|prior| prior != &attempt)
                    || !unknown
                {
                    self.flush_hosted_group(
                        conversation,
                        &mut pending,
                        &mut blocks,
                        &mut old_groups,
                    );
                }
                pending_attempt = Some(attempt);
                pending.push(block);
                if !unknown {
                    self.flush_hosted_group(
                        conversation,
                        &mut pending,
                        &mut blocks,
                        &mut old_groups,
                    );
                }
            } else {
                let mut block = block;
                if block.visibility == BlockVisibility::Grouped {
                    block.visibility = BlockVisibility::Always;
                }
                self.flush_hosted_group(conversation, &mut pending, &mut blocks, &mut old_groups);
                pending_attempt = None;
                blocks.push(block);
            }
        }
        self.flush_hosted_group(conversation, &mut pending, &mut blocks, &mut old_groups);
        let changed = !blocks
            .iter()
            .map(|block| (block.id, block.revision, block.visibility))
            .eq(old);
        let removed = !old_groups.is_empty();
        let Some(HistoryEntry::Conversation(entry)) =
            conversation.entry_mut_untracked(history_index)
        else {
            unreachable!()
        };
        entry.blocks = blocks;
        if changed {
            conversation.invalidate_block_indices(history_index);
            conversation.touch_entry(history_index);
        }
        if removed {
            conversation.change_structure();
        }
    }

    fn flush_hosted_group(
        &mut self,
        conversation: &mut ConversationState,
        pending: &mut Vec<PresentationBlock>,
        blocks: &mut Vec<PresentationBlock>,
        old: &mut HashMap<PresentationBlockId, PresentationBlock>,
    ) {
        let Some(first) = pending.first() else {
            return;
        };
        let PresentationBlockKind::Tool(PresentedTool::Acp(first_tool)) = &first.kind else {
            return;
        };
        let key = first_tool.id.clone();
        let mut activity = first_tool.hosted_activity().expect("hosted action");
        for block in pending.iter().skip(1) {
            let PresentationBlockKind::Tool(PresentedTool::Acp(tool)) = &block.kind else {
                continue;
            };
            let next = tool.hosted_activity().expect("hosted action");
            activity.members.extend(next.members);
            activity.actions.extend(next.actions);
            for (status, count) in next.outcomes {
                if let Some((_, prior)) = activity
                    .outcomes
                    .iter_mut()
                    .find(|(prior, _)| prior == &status)
                {
                    *prior += count;
                } else {
                    activity.outcomes.push((status, count));
                }
            }
        }
        let id = *self
            .coverage
            .fallback_groups
            .entry(key)
            .or_insert_with(|| conversation.allocate_block_id());
        self.coverage.fallback_group_ids.insert(id);
        let kind = PresentationBlockKind::WebActivity(activity);
        let mut block = PresentationBlock {
            id,
            revision: 0,
            role: Some(PresentationRole::Assistant),
            prompt_group: None,
            prompt: None,
            visibility: BlockVisibility::Always,
            kind,
        };
        if let Some(previous) = old.remove(&id) {
            block.revision = previous.revision;
            if previous.kind != block.kind {
                block.touch();
            }
        }
        blocks.push(block);
        blocks.extend(pending.drain(..).map(|mut block| {
            block.visibility = BlockVisibility::Grouped;
            block
        }));
    }

    fn refresh_displays(&mut self, conversation: &mut ConversationState) {
        for id in std::mem::take(&mut self.coverage.displays_dirty) {
            let Some(display) = self.coverage.displays.get(&id) else {
                continue;
            };
            let parts = display.parts();
            let tool_sources: HashMap<_, _> = display
                .attempt
                .presentation
                .iter()
                .filter_map(|part| {
                    matches!(
                        part.content,
                        zevria_content::AssistantPresentationContent::NativeTool { .. }
                    )
                    .then_some((part.source.output_index, &part.source))
                })
                .collect();
            let mut covered = Vec::new();
            let mut text_coverage: HashMap<BlockLocation, (String, Vec<(usize, usize)>)> =
                HashMap::new();
            let mut sources = std::collections::HashSet::new();
            let mut tools = HashMap::new();
            let mut valid = true;
            for binding in &display.bindings {
                match binding {
                    Binding::Text {
                        kind,
                        message_id,
                        start,
                        end,
                        sources: addresses,
                    } => {
                        let kind = segment_kind(*kind);
                        let Some((buffer, pieces)) =
                            self.coverage.projections.get(&(kind, message_id.clone()))
                        else {
                            valid = false;
                            break;
                        };
                        let expected = self
                            .coverage
                            .display_text_coverage
                            .get(&id)
                            .and_then(|coverage| coverage.get(&(kind, message_id.clone(), *start)));
                        if buffer.get(*start..*end).is_none_or(|actual| {
                            expected.is_none_or(|evidence| actual != evidence.text)
                        }) {
                            valid = false;
                            break;
                        }
                        sources.extend(addresses.iter().cloned());
                        for piece in intersecting_pieces(pieces, *start, *end) {
                            let a = piece.start.max(*start);
                            let b = piece.end.min(*end);
                            if a < b {
                                let (_, ranges) =
                                    text_coverage.entry(piece.location).or_insert_with(|| {
                                        (buffer[piece.start..piece.end].to_string(), Vec::new())
                                    });
                                ranges.push((a - piece.start, b - piece.start));
                            }
                        }
                    }
                    Binding::Tool {
                        tool_call_id,
                        native,
                        ..
                    } => {
                        if let Some(location) = self.tools.get(tool_call_id).copied()
                            && let Some(index) =
                                conversation.block_index(location.history_index, location.block_id)
                            && let Some(HistoryEntry::Conversation(entry)) =
                                conversation.entry(location.history_index)
                        {
                            let block = &entry.blocks[index];
                            if !*native
                                && !matches!(&block.kind, PresentationBlockKind::Tool(PresentedTool::Acp(tool)) if zevria_content::response::is_hosted_search_input(&tool.id, tool.raw_input.as_ref()))
                            {
                                valid = false;
                                break;
                            }
                            if *native {
                                tools.insert(tool_call_id.clone(), block.kind.clone());
                            }
                            covered.push((location, None));
                        }
                    }
                }
            }
            if !valid {
                continue;
            }
            // Only exact, previously advertised coverage can retire an older
            // append-only segment. Never blanket-hide a message ID or guess a
            // binding by proximity, and preserve any unrelated suffix.
            for ((kind, message_id, start), evidence) in self
                .coverage
                .display_text_coverage
                .get(&id)
                .into_iter()
                .flatten()
            {
                let Some((buffer, pieces)) =
                    self.coverage.projections.get(&(*kind, message_id.clone()))
                else {
                    continue;
                };
                if buffer.get(*start..evidence.end) != Some(evidence.text.as_str()) {
                    continue;
                }
                for piece in intersecting_pieces(pieces, *start, evidence.end) {
                    let a = piece.start.max(*start);
                    let b = piece.end.min(evidence.end);
                    if a < b {
                        let (_, ranges) =
                            text_coverage.entry(piece.location).or_insert_with(|| {
                                (buffer[piece.start..piece.end].to_string(), Vec::new())
                            });
                        ranges.push((a - piece.start, b - piece.start));
                    }
                }
            }
            for (location, (text, mut ranges)) in text_coverage {
                ranges.sort_unstable();
                let mut remaining = String::new();
                let mut cursor = 0;
                for (start, end) in ranges {
                    if start > cursor {
                        remaining.push_str(&text[cursor..start]);
                    }
                    cursor = cursor.max(end);
                }
                remaining.push_str(&text[cursor..]);
                covered.push((location, Some(remaining)));
            }
            let first = covered
                .iter()
                .filter_map(|(location, _)| {
                    let index =
                        conversation.block_index(location.history_index, location.block_id)?;
                    Some((location.history_index, index, location.block_id))
                })
                .min_by_key(|(history, index, _)| (*history, *index));
            for (location, remaining) in covered {
                if let Some(block) =
                    conversation.hinted_block_mut(location.history_index, location.block_id)
                {
                    let mut changed = false;
                    if let Some(remaining) = remaining.filter(|text| !text.is_empty()) {
                        changed = block.visibility != BlockVisibility::Always;
                        block.visibility = BlockVisibility::Always;
                        match &mut block.kind {
                            PresentationBlockKind::Text { text, .. } => {
                                changed |= *text != remaining;
                                *text = remaining;
                            }
                            PresentationBlockKind::Reasoning { parts } => {
                                changed |= parts.len() != 1 || parts[0] != remaining;
                                *parts = vec![remaining];
                            }
                            _ => {}
                        }
                    } else if block.visibility != BlockVisibility::Covered {
                        changed = true;
                        block.visibility = BlockVisibility::Covered;
                        if matches!(block.kind, PresentationBlockKind::Tool(_)) {
                            self.coverage
                                .hosted_actions_dirty
                                .insert(location.history_index);
                        }
                    }
                    if changed {
                        block.touch();
                        conversation.touch_entry(location.history_index);
                    }
                }
            }
            if let Some((history_index, _, _)) = first
                && display.attempt.presentation.iter().any(|part| {
                    sources.contains(&part.source)
                        && matches!(
                            part.content,
                            zevria_content::AssistantPresentationContent::Answer { .. }
                        )
                })
            {
                self.coverage
                    .applied_answers
                    .insert(display.attempt.id.clone(), history_index);
            }
            let mut attempt = display.attempt.clone();
            // No blanket suppression: text without explicit verified coverage
            // stays in the ordinary projection, rather than being shown twice.
            attempt.presentation.retain(|part| sources.contains(&part.source) || matches!(&part.content, zevria_content::AssistantPresentationContent::NativeTool { call_id } if tools.contains_key(call_id)));
            conversation.update_projected_attempt(attempt);
            conversation.bind_attempt_tools(&display.attempt.id, tools);
            if let Some((history, _, block)) = first {
                conversation.place_attempt(&display.attempt.id, history, block);
            }
            for binding in &display.bindings {
                match binding {
                    Binding::Tool {
                        tool_call_id,
                        output_index,
                        native: false,
                    } => {
                        if let Some(location) = self.tools.get(tool_call_id) {
                            conversation.alias_attempt_action(
                                (location.history_index, location.block_id),
                                &display.attempt.id,
                                *output_index,
                            );
                            if let Some(group) = self.coverage.fallback_groups.get(tool_call_id) {
                                conversation.alias_attempt_action(
                                    (location.history_index, *group),
                                    &display.attempt.id,
                                    *output_index,
                                );
                            }
                        }
                    }
                    Binding::Tool {
                        tool_call_id,
                        output_index,
                        native: true,
                    } => {
                        if let Some(location) = self.tools.get(tool_call_id)
                            && let Some(source) = tool_sources.get(output_index)
                        {
                            conversation.alias_attempt_part(
                                (location.history_index, location.block_id),
                                &display.attempt.id,
                                source,
                            );
                        }
                    }
                    Binding::Text {
                        kind,
                        message_id,
                        sources,
                        start,
                        end,
                    } => {
                        if let Some((_, pieces)) = self
                            .coverage
                            .projections
                            .get(&(segment_kind(*kind), message_id.clone()))
                        {
                            let mut offset = *start;
                            let mut sources = sources.iter().peekable();
                            for piece in intersecting_pieces(pieces, *start, *end)
                                .filter(|piece| piece.start >= *start && piece.end <= *end)
                            {
                                while let Some(source) = sources.peek() {
                                    let length = parts.text(*kind, source).map_or(0, str::len);
                                    if piece.start < offset + length {
                                        conversation.alias_attempt_part(
                                            (piece.location.history_index, piece.location.block_id),
                                            &display.attempt.id,
                                            source,
                                        );
                                        break;
                                    }
                                    offset += length + 1;
                                    sources.next();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn apply_segment_raw(
        &mut self,
        conversation: &mut ConversationState,
        kind: SegmentKind,
        message_id: Option<String>,
        text: String,
        update: SegmentUpdate,
    ) -> bool {
        if kind != SegmentKind::User {
            self.echo.prompt_echo = None;
            self.echo.user_group = None;
        }
        if let Some(target) = &self.segment
            && target.kind == kind
            && target.message_id == message_id
            && let Some(block) = conversation
                .hinted_block_mut(target.location.history_index, target.location.block_id)
        {
            let changed = match (&mut block.kind, update) {
                (PresentationBlockKind::Text { text: existing, .. }, update) => {
                    Some(update_text(existing, text.as_str(), update))
                }
                (PresentationBlockKind::Reasoning { parts }, update) => Some(update_text(
                    parts
                        .last_mut()
                        .unwrap_or_else(|| unreachable!("reasoning starts with a part")),
                    text.as_str(),
                    update,
                )),
                _ => None,
            };
            if let Some(changed) = changed {
                if changed {
                    block.touch();
                    conversation.touch_entry(target.location.history_index);
                }
                return changed;
            }
        }

        let (role, block_kind) = match kind {
            SegmentKind::User => (
                Some(PresentationRole::User),
                PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Plain,
                    editable: false,
                    sources: Vec::new(),
                },
            ),
            SegmentKind::Agent => (
                Some(PresentationRole::Assistant),
                PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Markdown,
                    editable: false,
                    sources: Vec::new(),
                },
            ),
            SegmentKind::Thought => (
                Some(PresentationRole::Assistant),
                PresentationBlockKind::Reasoning { parts: vec![text] },
            ),
        };
        let location = self.append_block(conversation, role, BlockVisibility::Always, block_kind);
        if kind == SegmentKind::User {
            let ordinal = self
                .echo
                .user_group
                .as_ref()
                .filter(|(id, _, _)| id == &message_id)
                .map_or(0, |(_, _, ordinal)| *ordinal);
            self.group_user_block(conversation, location, message_id.clone(), ordinal);
        }
        self.segment = Some(SegmentTarget {
            kind,
            message_id,
            location,
        });
        true
    }

    fn apply_tool_call(
        &mut self,
        conversation: &mut ConversationState,
        incoming: AcpToolPresentation,
    ) {
        self.echo.prompt_echo = None;
        if !self.tools.contains_key(&incoming.id) {
            self.fence();
        }
        if let Some(location) = self.tools.get(&incoming.id).copied()
            && let Some(block) =
                conversation.hinted_block_mut(location.history_index, location.block_id)
            && let PresentationBlockKind::Tool(PresentedTool::Acp(existing)) = &mut block.kind
        {
            if **existing != incoming {
                let hosted = existing.is_hosted_search() || incoming.is_hosted_search();
                let id = incoming.id.clone();
                **existing = incoming;
                block.touch();
                conversation.touch_entry(location.history_index);
                self.tool_changed(&id, location, hosted);
            }
            return;
        }
        let id = incoming.id.clone();
        let hosted = incoming.is_hosted_search();
        let location = self.append_block(
            conversation,
            Some(PresentationRole::Assistant),
            BlockVisibility::Always,
            PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(incoming))),
        );
        self.tool_changed(&id, location, hosted);
        self.tools.insert(id, location);
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_tool_update(
        &mut self,
        conversation: &mut ConversationState,
        id: String,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
        content: Option<Vec<String>>,
        locations: Option<Vec<AgentRunLocation>>,
        raw_input: Option<serde_json::Value>,
        raw_output: Option<serde_json::Value>,
    ) {
        self.echo.prompt_echo = None;
        if !self.tools.contains_key(&id) {
            self.fence();
        }
        let Some(location) = self.tools.get(&id).copied() else {
            self.apply_tool_call(
                conversation,
                AcpToolPresentation {
                    metadata: None,
                    title: title.unwrap_or_else(|| id.clone()),
                    id,
                    kind: kind.unwrap_or_else(|| "tool".into()),
                    status: status.unwrap_or_else(|| "updated".into()),
                    content: content.unwrap_or_default(),
                    locations: locations.unwrap_or_default(),
                    raw_input,
                    raw_output,
                },
            );
            return;
        };
        let Some(block) = conversation.hinted_block_mut(location.history_index, location.block_id)
        else {
            return;
        };
        let PresentationBlockKind::Tool(PresentedTool::Acp(tool)) = &mut block.kind else {
            return;
        };
        let hosted_before = tool.is_hosted_search();
        let changed = update_field(&mut tool.title, title)
            | update_field(&mut tool.kind, kind)
            | update_field(&mut tool.status, status)
            | update_field(&mut tool.content, content)
            | update_field(&mut tool.locations, locations)
            | update_field(&mut tool.raw_input, raw_input.map(Some))
            | update_field(&mut tool.raw_output, raw_output.map(Some));
        if changed {
            let hosted = hosted_before || tool.is_hosted_search();
            block.touch();
            conversation.touch_entry(location.history_index);
            self.tool_changed(&id, location, hosted);
        }
    }

    fn apply_plan(&mut self, conversation: &mut ConversationState, plan: AgentStructuredPlan) {
        self.echo.prompt_echo = None;
        self.fence();
        let plan = presented_plan(plan);
        let next_markdown = match &plan.content {
            PresentedPlanContent::Markdown(markdown) => Some(markdown.as_str()),
            PresentedPlanContent::Checklist(_) => None,
        };
        if !self.review_inputs.is_empty() && next_markdown != self.current_plan_markdown.as_deref()
        {
            // Keep earlier proposals in interactive history; the latest block
            // is the current presentation, not a destructive rewrite of audit evidence.
            self.current_plan = None;
        }
        self.current_plan_markdown = match &plan.content {
            PresentedPlanContent::Markdown(markdown) => Some(markdown.clone()),
            PresentedPlanContent::Checklist(_) => None,
        };
        if let Some(location) = self.current_plan
            && let Some(block) = block_mut(conversation, location)
            && let PresentationBlockKind::Plan(existing) = &mut block.kind
        {
            if *existing != plan {
                *existing = plan;
                block.touch();
                conversation.touch_entry(location.history_index);
            }
            return;
        }
        let location = self.append_block(
            conversation,
            Some(PresentationRole::Assistant),
            BlockVisibility::Always,
            PresentationBlockKind::Plan(plan),
        );
        self.current_plan = Some(location);
    }

    fn remove_plan(&mut self, conversation: &mut ConversationState, plan_id: &str) {
        self.echo.prompt_echo = None;
        self.fence();
        let Some(location) = self.current_plan else {
            return;
        };
        if block_mut(conversation, location).is_some_and(|block| {
            matches!(&block.kind, PresentationBlockKind::Plan(plan) if plan.plan_id.as_deref() == Some(plan_id))
        }) && conversation.remove_block(location.history_index, location.block_id)
        {
            self.current_plan = None;
            self.current_plan_markdown = None;
            self.coverage.hosted_actions_dirty.insert(location.history_index);
            self.rebuild_locations(conversation);
        }
    }

    fn append_diagnostic(
        &mut self,
        conversation: &mut ConversationState,
        diagnostic: PresentedDiagnostic,
    ) {
        self.fence();
        self.append_transparent_diagnostic(conversation, diagnostic);
    }

    fn append_transparent_diagnostic(
        &mut self,
        conversation: &mut ConversationState,
        diagnostic: PresentedDiagnostic,
    ) {
        self.append_block(
            conversation,
            None,
            BlockVisibility::Diagnostics,
            PresentationBlockKind::Diagnostic(diagnostic),
        );
    }

    fn begin_replay(&mut self) {
        let mut replay = Box::new(ReplayStage {
            reducer: Self::default(),
            conversation: ConversationState::default(),
        });
        replay.reducer.reset_for_replay(&mut replay.conversation);
        self.replay = Some(replay);
    }

    fn commit_replay(&mut self, conversation: &mut ConversationState) -> ConversationChange {
        let Some(mut replay) = self.replay.take() else {
            return ConversationChange::default();
        };
        if replay.reducer.latest_usage.is_none() {
            replay.reducer.latest_usage.clone_from(&self.latest_usage);
        }
        let change = conversation.replace_projection(replay.conversation);
        *self = replay.reducer;
        change
    }

    fn reset_for_replay(&mut self, conversation: &mut ConversationState) {
        let _ = conversation.clear_projection();
        *self = Self::default();
        self.append_diagnostic(
            conversation,
            PresentedDiagnostic {
                label: "ACP replay".to_string(),
                text: "reloaded session history".to_string(),
                tone: DiagnosticTone::Muted,
            },
        );
    }

    fn ensure_entry(&mut self, conversation: &mut ConversationState) -> usize {
        if let Some(index) = self.current_entry
            && matches!(
                conversation.entry(index),
                Some(HistoryEntry::Conversation(_))
            )
        {
            return index;
        }
        let index = conversation.push_entry(HistoryEntry::Conversation(ConversationEntry {
            references: Vec::new(),
            header: None,
            blocks: Vec::new(),
        }));
        self.current_entry = Some(index);
        conversation.set_agent_entry(index);
        index
    }

    fn append_block(
        &mut self,
        conversation: &mut ConversationState,
        role: Option<PresentationRole>,
        visibility: BlockVisibility,
        kind: PresentationBlockKind,
    ) -> BlockLocation {
        if role != Some(PresentationRole::User) && visibility == BlockVisibility::Always {
            self.echo.user_group = None;
        }
        let history_index = self.ensure_entry(conversation);
        let block = Self::new_block(conversation, role, visibility, kind);
        let block_id = block.id;
        conversation.append_entry_block(history_index, block);
        BlockLocation {
            history_index,
            block_id,
        }
    }

    fn new_block(
        conversation: &mut ConversationState,
        role: Option<PresentationRole>,
        visibility: BlockVisibility,
        kind: PresentationBlockKind,
    ) -> PresentationBlock {
        let id = conversation.allocate_block_id();
        PresentationBlock {
            id,
            revision: 0,
            role,
            prompt_group: None,
            prompt: None,
            visibility,
            kind,
        }
    }

    fn rebuild_locations(&mut self, conversation: &ConversationState) {
        self.rebuild_review_headers(conversation);
        self.tools.clear();
        self.current_plan = None;
        self.current_plan_markdown = None;
        for (history_index, entry) in conversation.history().iter().enumerate() {
            let HistoryEntry::Conversation(entry) = entry else {
                continue;
            };
            for block in &entry.blocks {
                let location = BlockLocation {
                    history_index,
                    block_id: block.id,
                };
                match &block.kind {
                    PresentationBlockKind::Tool(PresentedTool::Acp(tool)) => {
                        self.tools.insert(tool.id.clone(), location);
                    }
                    PresentationBlockKind::Plan(plan) if history_index >= self.plan_scope_start => {
                        self.current_plan = Some(location);
                        self.current_plan_markdown = match &plan.content {
                            PresentedPlanContent::Markdown(markdown) => Some(markdown.clone()),
                            PresentedPlanContent::Checklist(_) => None,
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    fn fence(&mut self) {
        self.segment = None;
    }
}

#[derive(Clone, Copy)]
enum SegmentUpdate {
    Append,
    Replace,
}

fn block_mut(
    conversation: &mut ConversationState,
    location: BlockLocation,
) -> Option<&mut PresentationBlock> {
    conversation.hinted_block_mut(location.history_index, location.block_id)
}

fn segment_kind(kind: DisplayKind) -> SegmentKind {
    match kind {
        DisplayKind::Message => SegmentKind::Agent,
        DisplayKind::Thought => SegmentKind::Thought,
    }
}

fn intersecting_pieces(
    pieces: &[ProjectionPiece],
    start: usize,
    end: usize,
) -> impl Iterator<Item = &ProjectionPiece> {
    pieces[pieces.partition_point(|piece| piece.end <= start)..]
        .iter()
        .take_while(move |piece| piece.start < end)
}

fn update_field<T: PartialEq>(target: &mut T, value: Option<T>) -> bool {
    if let Some(value) = value
        && *target != value
    {
        *target = value;
        true
    } else {
        false
    }
}

fn update_text(target: &mut String, text: &str, update: SegmentUpdate) -> bool {
    match update {
        SegmentUpdate::Append if !text.is_empty() => target.push_str(text),
        SegmentUpdate::Replace if target != text => {
            target.clear();
            target.push_str(text);
        }
        _ => return false,
    }
    true
}

fn presented_plan(plan: AgentStructuredPlan) -> PresentedPlan {
    let AgentStructuredPlan {
        plan_id,
        markdown,
        entries,
    } = plan;
    let content = markdown.map_or_else(
        || {
            PresentedPlanContent::Checklist(PresentedChecklist {
                label: "plan".to_string(),
                items: entries
                    .into_iter()
                    .map(|entry| ChecklistItem {
                        text: entry.content,
                        priority: Some(entry.priority),
                        status: match entry.status.as_str() {
                            "pending" => ChecklistStatus::Pending,
                            "in_progress" => ChecklistStatus::InProgress,
                            "completed" => ChecklistStatus::Completed,
                            _ => ChecklistStatus::Unknown,
                        },
                    })
                    .collect(),
            })
        },
        PresentedPlanContent::Markdown,
    );
    PresentedPlan { plan_id, content }
}

fn diagnostic_for_event(event: &AgentRunEvent) -> PresentedDiagnostic {
    let (label, text, tone) = match event {
        AgentRunEvent::Review { event } => {
            ("Host review", format!("{event:?}"), DiagnosticTone::Muted)
        }
        AgentRunEvent::SessionAllocated { session_id } => (
            "ACP session",
            format!("Allocated {session_id}; safety verification pending"),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Status { status, detail } => (
            "ACP status",
            detail.as_ref().map_or_else(
                || status.to_string(),
                |detail| format!("{status}: {detail}"),
            ),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::SessionEstablished {
            session_id,
            safe_mode,
            recovered,
            ..
        } => (
            "ACP session",
            format!(
                "session {session_id} · mode {safe_mode}{}",
                if *recovered { " · recovered" } else { "" }
            ),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::ModeChanged { mode } => (
            "ACP mode",
            format!("mode changed to {mode}"),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::ConfigOptionsChanged { options } => (
            "ACP config",
            format!("configuration changed: {options}"),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::SessionInfo {
            title,
            updated_at,
            metadata,
        } => (
            "ACP session info",
            format!(
                "{} · {}{}",
                title.as_deref().unwrap_or("untitled"),
                updated_at.as_deref().unwrap_or("unknown time"),
                metadata
                    .as_ref()
                    .map(|metadata| format!("\n{metadata}"))
                    .unwrap_or_default()
            ),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Usage { usage } => (
            "ACP usage",
            format!(
                "context usage {} / {}{}",
                usage.used,
                usage.size,
                usage
                    .cost
                    .as_ref()
                    .map(|cost| format!(" · cost {cost}"))
                    .unwrap_or_default()
            ),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Permission {
            tool_kind,
            decision,
            option_id,
        } => (
            "ACP permission",
            format!(
                "{} · {decision}{}",
                tool_kind.as_deref().unwrap_or("unknown tool kind"),
                option_id
                    .as_ref()
                    .map(|option_id| format!(" · option {option_id}"))
                    .unwrap_or_default()
            ),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Elicitation {
            field_count,
            outcome,
            ..
        } => (
            "ACP question",
            format!("{field_count} field(s) · {outcome}"),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Stderr { text } => ("ACP stderr", text.clone(), DiagnosticTone::Error),
        AgentRunEvent::Protocol { direction, json } => (
            match direction {
                AgentProtocolDirection::ClientToAgent => "ACP client → agent",
                AgentProtocolDirection::AgentToClient => "ACP agent → client",
            },
            json.clone(),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Unsupported {
            context,
            placeholder,
        } => (
            "ACP unsupported",
            format!("{context}: {placeholder}"),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::ResponseDisplay { .. } => {
            ("ACP display", String::new(), DiagnosticTone::Muted)
        }
        AgentRunEvent::ReplayBoundary => (
            "ACP replay",
            "reloaded session history".to_string(),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Prompt { text, .. }
        | AgentRunEvent::UserMessage { text, .. }
        | AgentRunEvent::AgentMessage { text, .. }
        | AgentRunEvent::Thought { text, .. }
        | AgentRunEvent::Failure { error: text } => ("ACP", text.clone(), DiagnosticTone::Muted),
        AgentRunEvent::UserImage { image, .. } => {
            ("ACP image", image.label(1), DiagnosticTone::Muted)
        }
        AgentRunEvent::ToolCall { title, .. } => ("ACP tool", title.clone(), DiagnosticTone::Muted),
        AgentRunEvent::ToolCallUpdate { id, .. } => ("ACP tool", id.clone(), DiagnosticTone::Muted),
        AgentRunEvent::ToolResultMetadata { metadata } => (
            "ACP tool metadata",
            metadata.id.clone(),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::Plan { .. } => (
            "ACP plan",
            "plan updated".to_string(),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::NativePlanCaptured { capture, .. } => (
            "Native proposal",
            format!(
                "host captured generation {} (unconfirmed)",
                capture.generation
            ),
            DiagnosticTone::Muted,
        ),
        AgentRunEvent::PlanRemoved { plan_id } => (
            "ACP plan",
            format!("plan {plan_id} removed"),
            DiagnosticTone::Muted,
        ),
    };
    PresentedDiagnostic {
        label: label.to_string(),
        text,
        tone,
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use zevria_content::response::{RESPONSE_DISPLAY_VERSION, ResponseDisplay};
    use zevria_content::{
        AssistantPartIdentity, AssistantPresentationContent, AssistantPresentationPart,
        AssistantSourceAddress, ResponseAttempt,
    };

    #[test]
    fn replacing_a_partially_covered_preview_compares_the_full_projection() {
        let mut reducer = AgentTranscriptReducer::default();
        let mut conversation = ConversationState::default();
        let key = (SegmentKind::Agent, "msg".to_owned());
        let replace = |reducer: &mut AgentTranscriptReducer,
                       conversation: &mut ConversationState,
                       text: &str| {
            reducer.apply_segment(
                conversation,
                key.0,
                Some(key.1.clone()),
                text.into(),
                SegmentUpdate::Replace,
            );
            reducer.flush_projection_changes(conversation);
        };
        replace(&mut reducer, &mut conversation, "lead tail");
        let location = reducer.segment.as_ref().unwrap().location;
        let source = AssistantSourceAddress {
            output_index: 0,
            part: AssistantPartIdentity::Content(0),
            item_id: None,
        };
        let mut attempt = ResponseAttempt::new(zevria_foundation::ModelProfileRef::new("p", "m"));
        attempt.presentation.push(AssistantPresentationPart {
            source: source.clone(),
            content: AssistantPresentationContent::answer("lead"),
        });
        reducer.install_display(ResponseDisplay {
            version: RESPONSE_DISPLAY_VERSION,
            attempt,
            bindings: vec![Binding::Text {
                kind: DisplayKind::Message,
                message_id: key.1.clone(),
                start: 0,
                end: 4,
                sources: vec![source],
            }],
        });
        reducer.flush_projection_changes(&mut conversation);
        let block = block_mut(&mut conversation, location).unwrap();
        assert!(matches!(&block.kind, PresentationBlockKind::Text { text, .. } if text == " tail"));

        let revision = conversation.entry_revision(location.history_index);
        replace(&mut reducer, &mut conversation, "lead tail");
        assert_eq!(
            conversation.entry_revision(location.history_index),
            revision
        );

        replace(&mut reducer, &mut conversation, " tail");
        let (buffer, pieces) = reducer.coverage.projections.get(&key).unwrap();
        assert_eq!(buffer, " tail");
        assert_eq!(pieces[0].end, buffer.len());
    }
}
