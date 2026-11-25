//! Durable history replay and accepted tail rewrites.

use super::{
    AgentRunStatus, ConversationChange, ConversationState, EnsembleRecord, HashSet, HistoryEntry,
    Message, NativeHeader, PlanRecord, ToolCallStatus, TranscriptItem, UserContent,
    tool_call_round,
};

impl ConversationState {
    pub fn commit_edit(&mut self, target_index: usize, compacted: bool) -> ConversationChange {
        let anchor = self
            .history
            .get(target_index)
            .and_then(|entry| match entry {
                HistoryEntry::Conversation(entry) => entry.header,
                HistoryEntry::Ensemble(entry) => entry.header,
                HistoryEntry::PlanHandoff(_, header) => *header,
                _ => None,
            })
            .filter(|header| matches!(header, NativeHeader::Prompt(_)));
        let cutoff = anchor
            .and_then(|header| self.headers.iter().position(|entry| entry.header == header))
            .or_else(|| {
                self.headers
                    .iter()
                    .position(|entry| entry.history_index >= target_index)
            });
        if let Some(cutoff) = cutoff {
            self.timing_revision += 1;
            for observation in self.headers.drain(cutoff..) {
                self.header_indices.remove(&observation.header);
                self.running_headers.remove(&observation.header);
                match observation.header {
                    NativeHeader::Prompt(_) => self.prompt_count -= 1,
                    NativeHeader::Assistant { turn, .. } => {
                        let count = self.call_counts.get_mut(&turn).expect("observed turn");
                        *count -= 1;
                        if *count == 0 {
                            self.call_counts.remove(&turn);
                        }
                    }
                }
            }
        }
        let target_index = target_index.min(self.history.len());
        for revision in self.entry_revisions.drain(target_index..) {
            self.entries_by_revision.remove(&revision);
        }
        self.block_indices.truncate(target_index);
        self.represented_headers.retain(|_, indices| {
            indices.truncate(indices.partition_point(|&index| index < target_index));
            !indices.is_empty()
        });
        self.ensemble_indices
            .retain(|_, index| *index < target_index);
        self.revision += 1;
        self.change_structure();
        let removed = self.history.split_off(target_index.min(self.history.len()));
        let mut change = ConversationChange::replaced_from(target_index);
        change.removed_ensemble_runs = removed
            .into_iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Ensemble(ensemble) => Some(ensemble.run_id),
                HistoryEntry::PlanArtifact(artifact) => {
                    self.plan_versions.remove(&artifact.version);
                    None
                }
                HistoryEntry::Conversation(_)
                | HistoryEntry::PlanHandoff(..)
                | HistoryEntry::JobNotice(..)
                | HistoryEntry::CompactionDivider
                | HistoryEntry::Error(_) => None,
            })
            .collect();
        if compacted {
            self.push_entry(HistoryEntry::CompactionDivider);
        }
        self.executing_tool_calls.clear();
        self.hosted_attempts.clear();
        self.response_answers
            .retain(|&index, _| index < target_index);
        self.agent_entry = self.agent_entry.filter(|&index| index < target_index);
        self.agent_previews.retain(|&index, _| index < target_index);
        self.incomplete_agent_entries
            .retain(|&index| index < target_index);
        self.identity_aliases.clear();
        self.pending_subtasks.clear();
        self.subtask_statuses.clear();
        self.command_call_locations.clear();
        self.command_job_locations.clear();
        self.subtask_call_locations.clear();
        self.subtask_locations.clear();
        for index in 0..self.history.len() {
            self.index_native_locations(index);
        }
        change
    }

    fn discard_inferred_turn_answer(&mut self) {
        let start = self
            .history
            .iter()
            .rposition(HistoryEntry::has_prompt_header)
            .unwrap_or(0);
        for (_, parts) in self
            .response_answers
            .iter_mut()
            .filter(|(index, _)| **index >= start)
        {
            parts.clear();
        }
    }

    /// Rebuild the pane from persisted transcript items.
    pub fn restore(&mut self, items: Vec<TranscriptItem>) {
        let review_projection = zevria_transcript::project_worker_reviews(&items);
        // Started seeds projection state but is not review evidence. Only actual
        // review transitions/seals may supersede outcome-only/ReportsReady data.
        let reviewed: std::collections::HashSet<_> = items
            .iter()
            .flat_map(|item| match item.ensemble_record() {
                Some(EnsembleRecord::WorkerReview {
                    run_id, worker_id, ..
                }) => vec![(run_id.clone(), worker_id.clone())],
                Some(EnsembleRecord::WorkersConfirmed {
                    run_id, outcomes, ..
                }) => outcomes
                    .iter()
                    .map(|outcome| (run_id.clone(), outcome.descriptor.id.clone()))
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        *self = Self::default();
        let mut current_turn = None;
        let mut last_response = None;
        for (index, item) in items.into_iter().enumerate() {
            let restored = zevria_transcript::restored_attempt(&item, index);
            match item {
                TranscriptItem::IncompleteResponse(record) => {
                    self.update_attempt(record.to_attempt(), false)
                }
                provider @ (TranscriptItem::Message(_)
                | TranscriptItem::RequestPrompt { .. }
                | TranscriptItem::ProviderMessage(_)) => {
                    let message = provider
                        .display_message()
                        .expect("a provider transcript item must contain a message");
                    let header = match &message {
                        Message::User { content }
                            if matches!(
                                provider,
                                TranscriptItem::Message(_) | TranscriptItem::RequestPrompt { .. }
                            ) && content
                                .iter()
                                .all(|part| !matches!(part, UserContent::ToolResult(_))) =>
                        {
                            let turn = self.allocate_turn();
                            current_turn = Some(turn);
                            Some(NativeHeader::Prompt(turn))
                        }
                        Message::Assistant { .. } => {
                            current_turn.map(|turn| self.allocate_call(turn))
                        }
                        _ => None,
                    };
                    let attempt_id = restored.as_ref().map(|attempt| attempt.id.clone());
                    if let Some(attempt) = restored {
                        self.update_attempt(attempt, false);
                    }
                    let _ = self.finish_tool_results(&message, &[]);
                    let assistant = matches!(message, Message::Assistant(_));
                    let final_answer = assistant
                        && !tool_call_round(&message)
                        && !provider.provider_replay().is_some_and(|replay| {
                            replay.tool_call_handles().any(|(_, external)| external)
                        });
                    let index = self.project_response(
                        message,
                        attempt_id.as_deref(),
                        ToolCallStatus::Executing,
                        header,
                        final_answer,
                    );
                    if assistant {
                        last_response = index;
                    } else if matches!(header, Some(NativeHeader::Prompt(_))) {
                        last_response = None;
                    }
                }
                TranscriptItem::ToolResults {
                    message, metadata, ..
                } => {
                    let _ = self.finish_tool_results(&message, &metadata);
                    self.push_message(message, ToolCallStatus::Executing);
                }
                TranscriptItem::RequestDirective(directive)
                    if directive.kind == zevria_instructions::RequestDirectiveKind::Correction =>
                {
                    // The preceding no-tool response was an orchestration
                    // round, not a successful answer to the request.
                    if let Some(index) = last_response.take() {
                        self.response_answers.insert(index, HashSet::new());
                    }
                }
                TranscriptItem::SessionModels(_)
                | TranscriptItem::SessionMode(_)
                | TranscriptItem::Directive(_)
                | TranscriptItem::RequestDirective(_)
                | TranscriptItem::ToolDeclarations(_) => {}
                TranscriptItem::CommandJobNotice(notice) => {
                    let header =
                        if notice.kind() == zevria_foundation::CommandJobNoticeKind::Continuation {
                            let turn = self.allocate_turn();
                            current_turn = Some(turn);
                            Some(NativeHeader::Prompt(turn))
                        } else {
                            None
                        };
                    self.push_job_notice(*notice, header);
                }
                TranscriptItem::SkillInvocation(invocation) => {
                    let turn = self.allocate_turn();
                    current_turn = Some(turn);
                    self.push_user_turn(invocation.display_message(), turn);
                }
                TranscriptItem::Plan(record) => match *record {
                    PlanRecord::Ready { artifact } | PlanRecord::Published { artifact, .. } => {
                        self.push_plan_artifact(artifact)
                    }
                    PlanRecord::Handoff { handoff } => {
                        let turn = self.allocate_turn();
                        current_turn = Some(turn);
                        self.push_plan_handoff(handoff, turn);
                    }
                    PlanRecord::Started { .. }
                    | PlanRecord::DraftSaved { .. }
                    | PlanRecord::RevisionRequested { .. }
                    | PlanRecord::Resolved { .. } => {}
                },
                TranscriptItem::Ensemble(record) => match *record {
                    EnsembleRecord::Started { start } => {
                        let turn = self
                            .ensemble_turn(&start.run_id)
                            .unwrap_or_else(|| self.allocate_turn());
                        current_turn = Some(turn);
                        if !self.has_ensemble(&start.run_id) {
                            self.add_ensemble(
                                start,
                                AgentRunStatus::Interrupted,
                                Some(NativeHeader::Prompt(turn)),
                            );
                        }
                    }
                    EnsembleRecord::ReportsReady { run_id, agents, .. } => {
                        current_turn = self.ensemble_turn(&run_id).or(current_turn);
                        for summary in agents {
                            self.project_worker_completion(
                                &run_id,
                                crate::projection::WorkerCompletion::summary(&run_id, summary),
                            );
                        }
                    }
                    EnsembleRecord::Cancelled { run_id } => {
                        if self
                            .ensemble_turn(&run_id)
                            .is_some_and(|turn| Some(turn) == current_turn)
                        {
                            self.discard_inferred_turn_answer();
                        }
                        self.update_unfinished_ensemble_workers(&run_id, AgentRunStatus::Cancelled);
                    }
                    EnsembleRecord::Failed { run_id, .. } => {
                        if self
                            .ensemble_turn(&run_id)
                            .is_some_and(|turn| Some(turn) == current_turn)
                        {
                            self.discard_inferred_turn_answer();
                        }
                        self.update_unfinished_ensemble_workers(
                            &run_id,
                            AgentRunStatus::Interrupted,
                        );
                    }
                    EnsembleRecord::WorkerReview {
                        run_id,
                        worker_id,
                        event,
                        ..
                    } if matches!(
                        event.as_ref(),
                        zevria_workflow::WorkerReviewEvent::Abandoned { .. }
                    ) =>
                    {
                        self.update_ensemble_worker(
                            &run_id,
                            &worker_id,
                            AgentRunStatus::Abandoned,
                            Some(zevria_workflow::ensemble::WORKER_ABANDONMENT_REASON.into()),
                        );
                    }
                    EnsembleRecord::WorkersConfirmed {
                        run_id, outcomes, ..
                    } => {
                        for outcome in outcomes {
                            self.project_worker_completion(
                                &run_id,
                                crate::projection::WorkerCompletion::outcome(&run_id, outcome),
                            );
                        }
                    }
                    EnsembleRecord::ReviewStarted { .. }
                    | EnsembleRecord::WorkerReview { .. }
                    | EnsembleRecord::ControlResult { .. }
                    | EnsembleRecord::Completed { .. } => {}
                },
                TranscriptItem::Compaction(_) => {
                    self.push_entry(HistoryEntry::CompactionDivider);
                }
                TranscriptItem::Error { error } => {
                    if error == crate::blocks::TURN_CANCELLED {
                        self.discard_inferred_turn_answer();
                    }
                    self.push_error(error);
                }
            }
        }
        if let Ok(runs) = review_projection {
            for (run_id, states) in runs {
                for state in states {
                    if reviewed.contains(&(run_id.clone(), state.descriptor.id.clone())) {
                        self.project_worker_review(
                            &run_id,
                            &state.descriptor.id,
                            &state,
                            crate::projection::ProjectionMode::Restored,
                        );
                    }
                }
            }
        }
        self.interrupt_executing_tool_calls();
    }
}
