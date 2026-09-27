//! Materialize the append-only ACP event journal into semantic TUI blocks.

use std::collections::HashMap;

use zevria_workflow::AgentProtocolDirection;
use zevria_workflow::AgentRunEvent;
use zevria_workflow::AgentRunLocation;
use zevria_workflow::AgentStructuredPlan;
use zevria_workflow::AgentUsage;

use crate::{
    app::{ConversationChange, ConversationState, HistoryEntry},
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
pub(crate) struct AgentTranscriptReducer {
    current_entry: Option<usize>,
    segment: Option<SegmentTarget>,
    tools: HashMap<String, BlockLocation>,
    current_plan: Option<BlockLocation>,
    current_plan_markdown: Option<String>,
    plan_scope_start: usize,
    prompt_echo: Option<PromptEcho>,
    review_prompts: HashMap<u64, zevria_content::UserPrompt>,
    image_echoes: std::collections::VecDeque<zevria_content::PromptImage>,
    user_group: Option<(Option<String>, PresentationBlockId, usize)>,
    review: ReviewProjection,
    latest_usage: Option<AgentUsage>,
    replay: Option<Box<ReplayStage>>,
    review_inputs: std::collections::HashSet<zevria_workflow::WorkerControlId>,
    review_markers: std::collections::HashSet<String>,
    replaying_review: bool,
    review_sealed: bool,
    review_abandoned: bool,
    projections: HashMap<(SegmentKind, String), (String, Vec<ProjectionPiece>)>,
    displays: HashMap<String, zevria_content::web_search::ResponseDisplay>,
    // Latest coverage per emitted segment/range, not a history of token
    // snapshots. A corrected segment may arrive before its predecessor's
    // coalesced preview; keep enough evidence to cover that predecessor later.
    display_text_coverage: HashMap<(SegmentKind, String, usize), DisplayTextCoverage>,
    fallback_groups: HashMap<String, PresentationBlockId>,
    applied_answers: HashMap<String, usize>,
}

struct DisplayTextCoverage {
    attempt_id: String,
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
    pub(crate) fn abandon(&mut self, conversation: &mut ConversationState) -> ConversationChange {
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
                },
            );
        }
        ConversationChange::default()
    }

    /// Apply one lossless durable/lifecycle event. Message and thought values
    /// are deltas on this path.
    pub(crate) fn apply_event(
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
            AgentRunEvent::ResponseDisplay { display } => {
                if display.validate().is_ok()
                    && !self.displays.get(&display.attempt.id).is_some_and(|old| {
                        old.attempt.revision > display.attempt.revision
                            || (old.attempt.revision == display.attempt.revision
                                && old.attempt != display.attempt)
                    })
                {
                    for binding in &display.bindings {
                        if let zevria_content::web_search::DisplayProjectionBinding::Text {
                            kind,
                            message_id,
                            start,
                            end,
                            ..
                        } = binding
                            && let Some(text) = display.binding_text(binding)
                        {
                            let kind = match kind {
                                zevria_content::web_search::DisplayProjectionKind::Message => {
                                    SegmentKind::Agent
                                }
                                zevria_content::web_search::DisplayProjectionKind::Thought => {
                                    SegmentKind::Thought
                                }
                            };
                            self.display_text_coverage.insert(
                                (kind, message_id.clone(), *start),
                                DisplayTextCoverage {
                                    attempt_id: display.attempt.id.clone(),
                                    end: *end,
                                    text,
                                },
                            );
                        }
                    }
                    self.displays.insert(display.attempt.id.clone(), *display);
                }
            }
            AgentRunEvent::ReplayBoundary => unreachable!("replay boundary handled above"),
            AgentRunEvent::Review { event } => self.apply_review_event(conversation, *event),
            AgentRunEvent::Prompt { mut text, .. } if !self.review_inputs.is_empty() => {
                self.fence();
                for (index, image) in self.image_echoes.iter().enumerate() {
                    text = text.replacen(&image.label(index + 1), "", 1);
                }
                self.enable_review_publication();
                self.prompt_echo = Some(PromptEcho {
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
                if self.image_echoes.front() == Some(&image) {
                    self.image_echoes.pop_front();
                    return change;
                }
                let ordinal = self
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
                    && let Some(block) = block_mut(conversation, location)
                    && let PresentationBlockKind::Tool(PresentedTool::Acp(tool)) = &mut block.kind
                    && tool.metadata.as_ref() != Some(metadata.as_ref())
                {
                    tool.metadata = Some(*metadata);
                    block.touch();
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
            AgentRunEvent::Failure { error } => {
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
        self.refresh_displays(conversation);
        self.regroup_hosted_actions(conversation);
        change
    }

    /// Apply one lossy latest-value message/thought preview. The text already
    /// includes every delta in the current segment and therefore replaces the
    /// matching block rather than appending to it.
    pub(crate) fn apply_preview(
        &mut self,
        conversation: &mut ConversationState,
        event: AgentRunEvent,
    ) -> ConversationChange {
        if self.review_abandoned {
            return ConversationChange::default();
        }
        if let Some(replay) = &mut self.replay {
            return replay
                .reducer
                .apply_preview(&mut replay.conversation, event);
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
        ConversationChange::default()
    }

    /// Repair a lossy live preview from the terminal durable report while
    /// retaining thoughts, tools, plans, and diagnostics in their positions.
    pub(crate) fn reconcile_report(
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
        if self.applied_answers.iter().any(|(id, history_index)| {
            Some(*history_index) == self.current_entry && self.displays.contains_key(id)
        }) {
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
        self.rebuild_locations(conversation);
        self.fence();
        self.user_group = None;
        ConversationChange::replaced_from(history_index)
    }

    pub(crate) fn reconcile_plan(
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

    pub(crate) fn report_duplicates_current_plan(&self, report: &str) -> bool {
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
        self.user_group = None;
        self.fence();
        let history_index =
            conversation.push_entry(HistoryEntry::Conversation(ConversationEntry {
                header: None,
                blocks: Vec::new(),
            }));
        self.current_entry = Some(history_index);
        if !continuation {
            self.current_plan = None;
            self.current_plan_markdown = None;
            self.plan_scope_start = history_index;
        }
        // Review acceptance can queue ahead of the active input. Only dispatch
        // installs its echo queue; ordinary prompts have no separate dispatch.
        if annotation.generation.is_none() {
            self.prompt_echo = Some(PromptEcho {
                prompt: prompt.text_projection(),
                accumulated: String::new(),
            });
            self.image_echoes = prompt.images().cloned().collect();
        }
        let mut header: Option<BlockLocation> = None;
        let mut ordinal = 0;
        for block in prompt.into_blocks() {
            let kind = match block {
                zevria_content::PromptBlock::Text(text) => PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Plain,
                    editable: false,
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
            }
        }
        // Accepted review content is complete. Unmatched provider user chunks
        // must not inherit its provenance or lifecycle. Ordinary Prompt events
        // can still acquire following image/text segments without a message ID.
        if annotation.generation.is_none() {
            self.user_group = header.map(|location| (None, location.block_id, ordinal));
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
            .user_group
            .as_ref()
            .filter(|(id, _, _)| id == &message_id)
            .map_or(location.block_id, |(_, group, _)| *group);
        self.user_group = Some((message_id, group, ordinal));
        if let Some(block) = block_mut(conversation, location) {
            block.prompt_group = Some(group);
            if group == block.id {
                block.prompt = Some(PromptAnnotation::default());
            }
        }
    }

    fn apply_user_delta(
        &mut self,
        conversation: &mut ConversationState,
        text: String,
        message_id: Option<String>,
    ) {
        if let Some(echo) = &mut self.prompt_echo {
            echo.accumulated.push_str(&text);
            if echo.prompt.starts_with(&echo.accumulated) {
                if echo.prompt == echo.accumulated {
                    self.prompt_echo = None;
                }
                return;
            }
            let divergent = echo
                .accumulated
                .strip_prefix(&echo.prompt)
                .unwrap_or(&echo.accumulated)
                .to_string();
            self.prompt_echo = None;
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
        self.apply_segment_raw(conversation, kind, message_id.clone(), text.clone(), update);
        if kind != SegmentKind::User
            && let Some(message_id) = message_id
            && let Some(target) = &self.segment
        {
            let (buffer, pieces) = self.projections.entry((kind, message_id)).or_default();
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
        self.refresh_displays(conversation);
        self.regroup_hosted_actions(conversation);
    }

    fn regroup_hosted_actions(&mut self, conversation: &mut ConversationState) {
        for history_index in 0..conversation.history().len() {
            let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(history_index)
            else {
                continue;
            };
            let original = std::mem::take(&mut entry.blocks);
            let mut blocks = Vec::new();
            let mut old_groups = HashMap::new();
            let mut ordinary = Vec::new();
            for block in original {
                if self.fallback_groups.values().any(|id| *id == block.id) {
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
                    self.flush_hosted_group(
                        conversation,
                        &mut pending,
                        &mut blocks,
                        &mut old_groups,
                    );
                    pending_attempt = None;
                    blocks.push(block);
                }
            }
            self.flush_hosted_group(conversation, &mut pending, &mut blocks, &mut old_groups);
            if let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(history_index) {
                entry.blocks = blocks;
            }
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
            .fallback_groups
            .entry(key)
            .or_insert_with(|| conversation.allocate_block_id());
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
        use zevria_content::web_search::DisplayProjectionBinding as Binding;
        use zevria_content::web_search::DisplayProjectionKind as Kind;
        let mut displays = self.displays.values().cloned().collect::<Vec<_>>();
        displays.sort_by(|a, b| a.attempt.id.cmp(&b.attempt.id));
        for display in displays {
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
                        let kind = if *kind == Kind::Message {
                            SegmentKind::Agent
                        } else {
                            SegmentKind::Thought
                        };
                        let Some((buffer, pieces)) =
                            self.projections.get(&(kind, message_id.clone()))
                        else {
                            valid = false;
                            break;
                        };
                        if buffer.get(*start..*end) != display.binding_text(binding).as_deref() {
                            valid = false;
                            break;
                        }
                        sources.extend(addresses.iter().cloned());
                        for piece in pieces {
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
                            && let Some(block) = block_mut(conversation, location)
                        {
                            if !*native
                                && !matches!(&block.kind, PresentationBlockKind::Tool(PresentedTool::Acp(tool)) if zevria_content::web_search::is_hosted_search_input(&tool.id, tool.raw_input.as_ref()))
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
            for ((kind, message_id, start), evidence) in &self.display_text_coverage {
                if evidence.attempt_id != display.attempt.id {
                    continue;
                }
                let Some((buffer, pieces)) = self.projections.get(&(*kind, message_id.clone()))
                else {
                    continue;
                };
                if buffer.get(*start..evidence.end) != Some(evidence.text.as_str()) {
                    continue;
                }
                for piece in pieces {
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
                    let HistoryEntry::Conversation(entry) =
                        conversation.entry(location.history_index)?
                    else {
                        return None;
                    };
                    let index = entry
                        .blocks
                        .iter()
                        .position(|block| block.id == location.block_id)?;
                    Some((location.history_index, index, location.block_id))
                })
                .min_by_key(|(history, index, _)| (*history, *index));
            for (location, remaining) in covered {
                if let Some(block) = block_mut(conversation, location) {
                    if let Some(remaining) = remaining.filter(|text| !text.is_empty()) {
                        block.visibility = BlockVisibility::Always;
                        match &mut block.kind {
                            PresentationBlockKind::Text { text, .. } => *text = remaining,
                            PresentationBlockKind::Reasoning { parts } => *parts = vec![remaining],
                            _ => {}
                        }
                        block.touch();
                    } else {
                        block.visibility = BlockVisibility::Covered;
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
                self.applied_answers
                    .insert(display.attempt.id.clone(), history_index);
            }
            let mut attempt = display.attempt.clone();
            // No blanket suppression: text without explicit verified coverage
            // stays in the ordinary projection, rather than being shown twice.
            attempt.presentation.retain(|part| sources.contains(&part.source) || matches!(&part.content, zevria_content::AssistantPresentationContent::NativeTool { call_id } if tools.contains_key(call_id)));
            conversation.update_projected_web_search(attempt);
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
                            if let Some(group) = self.fallback_groups.get(tool_call_id) {
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
                            && let Some(part) = display.attempt.presentation.iter().find(|part| {
                                part.source.output_index == *output_index
                                    && part.source.part
                                        == zevria_content::AssistantPartIdentity::Tool
                            })
                        {
                            conversation.alias_attempt_part(
                                (location.history_index, location.block_id),
                                &display.attempt.id,
                                &part.source,
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
                        let kind = if *kind == Kind::Message {
                            SegmentKind::Agent
                        } else {
                            SegmentKind::Thought
                        };
                        if let Some((_, pieces)) = self.projections.get(&(kind, message_id.clone()))
                        {
                            for piece in pieces
                                .iter()
                                .filter(|piece| piece.start >= *start && piece.end <= *end)
                            {
                                let mut offset = *start;
                                for source in sources {
                                    let length = display.attempt.presentation.iter().find(|part| &part.source == source).map_or(0, |part| match &part.content { zevria_content::AssistantPresentationContent::Reasoning { text } | zevria_content::AssistantPresentationContent::Answer { text } => text.len(), _ => 0 });
                                    if piece.start < offset + length {
                                        conversation.alias_attempt_part(
                                            (piece.location.history_index, piece.location.block_id),
                                            &display.attempt.id,
                                            source,
                                        );
                                        break;
                                    }
                                    offset += length + 1;
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
    ) {
        if kind != SegmentKind::User {
            self.prompt_echo = None;
            self.user_group = None;
        }
        if let Some(target) = &self.segment
            && target.kind == kind
            && target.message_id == message_id
            && let Some(block) = block_mut(conversation, target.location)
        {
            match (&mut block.kind, update) {
                (PresentationBlockKind::Text { text: existing, .. }, SegmentUpdate::Append) => {
                    existing.push_str(&text);
                    block.touch();
                    return;
                }
                (PresentationBlockKind::Text { text: existing, .. }, SegmentUpdate::Replace) => {
                    existing.clone_from(&text);
                    block.touch();
                    return;
                }
                (PresentationBlockKind::Reasoning { parts }, SegmentUpdate::Append) => {
                    match parts.last_mut() {
                        Some(existing) => existing.push_str(&text),
                        None => parts.push(text),
                    }
                    block.touch();
                    return;
                }
                (PresentationBlockKind::Reasoning { parts }, SegmentUpdate::Replace) => {
                    match parts.last_mut() {
                        Some(existing) => existing.clone_from(&text),
                        None => parts.push(text),
                    }
                    block.touch();
                    return;
                }
                _ => {}
            }
        }

        let (role, block_kind) = match kind {
            SegmentKind::User => (
                Some(PresentationRole::User),
                PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Plain,
                    editable: false,
                },
            ),
            SegmentKind::Agent => (
                Some(PresentationRole::Assistant),
                PresentationBlockKind::Text {
                    text,
                    flavor: TextFlavor::Markdown,
                    editable: false,
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
    }

    fn apply_tool_call(
        &mut self,
        conversation: &mut ConversationState,
        incoming: AcpToolPresentation,
    ) {
        self.prompt_echo = None;
        if !self.tools.contains_key(&incoming.id) {
            self.fence();
        }
        if let Some(location) = self.tools.get(&incoming.id).copied()
            && let Some(block) = block_mut(conversation, location)
            && let PresentationBlockKind::Tool(PresentedTool::Acp(existing)) = &mut block.kind
        {
            **existing = incoming;
            block.touch();
            return;
        }
        let id = incoming.id.clone();
        let location = self.append_block(
            conversation,
            Some(PresentationRole::Assistant),
            BlockVisibility::Always,
            PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(incoming))),
        );
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
        self.prompt_echo = None;
        if !self.tools.contains_key(&id) {
            self.fence();
        }
        let location = self.tools.get(&id).copied().unwrap_or_else(|| {
            let location = self.append_block(
                conversation,
                Some(PresentationRole::Assistant),
                BlockVisibility::Always,
                PresentationBlockKind::Tool(PresentedTool::Acp(Box::new(AcpToolPresentation {
                    metadata: None,
                    id: id.clone(),
                    title: title.clone().unwrap_or_else(|| id.clone()),
                    kind: kind.clone().unwrap_or_else(|| "tool".to_string()),
                    status: status.clone().unwrap_or_else(|| "updated".to_string()),
                    content: Vec::new(),
                    locations: Vec::new(),
                    raw_input: None,
                    raw_output: None,
                }))),
            );
            self.tools.insert(id.clone(), location);
            location
        });
        let Some(block) = block_mut(conversation, location) else {
            return;
        };
        let PresentationBlockKind::Tool(PresentedTool::Acp(tool)) = &mut block.kind else {
            return;
        };
        let previous = tool.clone();
        if let Some(title) = title {
            tool.title = title;
        }
        if let Some(kind) = kind {
            tool.kind = kind;
        }
        if let Some(status) = status {
            tool.status = status;
        }
        if let Some(content) = content {
            tool.content = content;
        }
        if let Some(locations) = locations {
            tool.locations = locations;
        }
        if let Some(raw_input) = raw_input {
            tool.raw_input = Some(raw_input);
        }
        if let Some(raw_output) = raw_output {
            tool.raw_output = Some(raw_output);
        }
        if *tool != previous {
            block.touch();
        }
    }

    fn apply_plan(&mut self, conversation: &mut ConversationState, plan: AgentStructuredPlan) {
        self.prompt_echo = None;
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
            *existing = plan;
            block.touch();
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
        self.prompt_echo = None;
        self.fence();
        let Some(location) = self.current_plan else {
            return;
        };
        let matches = block_mut(conversation, location).is_some_and(|block| {
            matches!(
                &block.kind,
                PresentationBlockKind::Plan(plan)
                    if plan.plan_id.as_deref() == Some(plan_id)
            )
        });
        if !matches {
            return;
        }
        let Some(HistoryEntry::Conversation(entry)) =
            conversation.entry_mut(location.history_index)
        else {
            return;
        };
        entry.blocks.retain(|block| block.id != location.block_id);
        self.current_plan = None;
        self.current_plan_markdown = None;
        self.rebuild_locations(conversation);
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
        self.current_entry = None;
        self.segment = None;
        self.tools.clear();
        self.current_plan = None;
        self.current_plan_markdown = None;
        self.plan_scope_start = 0;
        self.prompt_echo = None;
        self.image_echoes.clear();
        self.user_group = None;
        self.review = ReviewProjection::default();
        self.review_prompts.clear();
        self.review_inputs.clear();
        self.review_markers.clear();
        self.replaying_review = false;
        self.review_sealed = false;
        self.latest_usage = None;
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
            header: None,
            blocks: Vec::new(),
        }));
        self.current_entry = Some(index);
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
            self.user_group = None;
        }
        let history_index = self.ensure_entry(conversation);
        let block = Self::new_block(conversation, role, visibility, kind);
        let Some(HistoryEntry::Conversation(entry)) = conversation.entry_mut(history_index) else {
            unreachable!("ensure_entry creates a conversation entry")
        };
        let block_id = block.id;
        entry.blocks.push(block);
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
    let HistoryEntry::Conversation(entry) = conversation.entry_mut(location.history_index)? else {
        return None;
    };
    entry
        .blocks
        .iter_mut()
        .find(|block| block.id == location.block_id)
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
