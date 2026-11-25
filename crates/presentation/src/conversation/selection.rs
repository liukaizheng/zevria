//! Selection and recall over stable conversation identities.

use super::{
    AgentRunId, ConversationState, HistoryEntry, PresentationBlock, PresentationBlockId,
    PresentationBlockKind, PresentedTool, Selection, SelectionScope, SubtaskId,
    TranscriptEditTarget, TurnStartTarget, selected_agent_run_id, selected_plain_text,
    selected_subtask_id, selected_tool, selected_tool_output,
};

impl ConversationState {
    pub fn turn_starts(&self, diagnostics_visible: bool) -> Vec<TurnStartTarget> {
        let mut targets = Vec::new();
        for (history_index, entry) in self.history.iter().enumerate() {
            let target = TurnStartTarget {
                epoch: self.epoch,
                history_index,
                block: None,
            };
            if entry.has_prompt_header() {
                targets.push(target);
            } else if let HistoryEntry::Conversation(conversation) = entry
                && conversation.header.is_none()
            {
                targets.extend(
                    entry
                        .user_message_blocks(diagnostics_visible)
                        .filter(|(index, start)| index == start)
                        .map(|(index, _)| TurnStartTarget {
                            block: Some(conversation.blocks[index].id),
                            ..target
                        }),
                );
            }
        }
        targets
    }

    pub fn reconcile_turn_start(&self, mut target: TurnStartTarget) -> Option<TurnStartTarget> {
        if target.epoch != self.epoch {
            return None;
        }
        if let Some(id) = target.block {
            let (history_index, id) = self.resolved_identity((target.history_index, id));
            target.history_index = history_index;
            target.block = Some(id);
            let entry = self.entry(history_index)?;
            let HistoryEntry::Conversation(conversation) = entry else {
                return None;
            };
            (conversation.header.is_none()
                && entry
                    .user_message_blocks(true)
                    .any(|(index, start)| index == start && conversation.blocks[index].id == id))
            .then_some(target)
        } else {
            self.entry(target.history_index)?
                .has_prompt_header()
                .then_some(target)
        }
    }

    pub fn selection_identity(&self, selection: Selection) -> Option<PresentationBlockId> {
        let HistoryEntry::Conversation(entry) = self.entry(selection.history_index)? else {
            return None;
        };
        Some(entry.blocks.get(selection.content_index)?.id)
    }

    pub fn resolved_identity(
        &self,
        mut identity: (usize, PresentationBlockId),
    ) -> (usize, PresentationBlockId) {
        for _ in 0..8 {
            if let Some(next) = self.identity_aliases.get(&identity) {
                identity = *next;
            } else {
                break;
            }
        }
        identity
    }

    pub fn selection_for_identity(
        &self,
        history_index: usize,
        id: PresentationBlockId,
    ) -> Option<Selection> {
        let (history_index, id) = self.resolved_identity((history_index, id));
        self.block_index(history_index, id)
            .map(|content_index| Selection {
                history_index,
                content_index,
            })
    }

    pub fn message_recall_selection(&self, history_index: usize) -> Option<Selection> {
        let content_index = match self.history.get(history_index)? {
            HistoryEntry::Conversation(entry) => entry
                .blocks
                .iter()
                .position(PresentationBlock::is_editable)?,
            HistoryEntry::Ensemble(_) => 0,
            _ => return None,
        };
        Some(Selection {
            history_index,
            content_index,
        })
    }

    pub fn recallable_selection(
        &self,
        selection: Selection,
    ) -> Option<(zevria_content::UserPrompt, TranscriptEditTarget)> {
        match self.history.get(selection.history_index)? {
            HistoryEntry::Conversation(entry) => {
                if !entry.blocks.get(selection.content_index)?.is_editable() {
                    return None;
                }
                let blocks = entry
                    .blocks
                    .iter()
                    .map(|block| match &block.kind {
                        PresentationBlockKind::Text {
                            text,
                            editable: true,
                            ..
                        } => Some(zevria_content::PromptBlock::Text(text.clone())),
                        PresentationBlockKind::Image {
                            image,
                            editable: true,
                            ..
                        } => Some(zevria_content::PromptBlock::Image(image.clone())),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                let text = zevria_content::UserPrompt::new(blocks).ok()?;
                Some((
                    text,
                    TranscriptEditTarget::PromptOrdinal(
                        self.prompt_ordinal(selection.history_index),
                    ),
                ))
            }
            HistoryEntry::Ensemble(ensemble) if selection.content_index == 0 => Some((
                ensemble
                    .prompt
                    .with_prefix(format!("{} ", ensemble.workflow.slash_command())),
                TranscriptEditTarget::EnsembleRun(ensemble.run_id.clone()),
            )),
            HistoryEntry::PlanArtifact(_)
            | HistoryEntry::PlanHandoff(..)
            | HistoryEntry::JobNotice(..)
            | HistoryEntry::Ensemble(_)
            | HistoryEntry::CompactionDivider
            | HistoryEntry::Error(_) => None,
        }
    }

    fn prompt_ordinal(&self, history_index: usize) -> usize {
        self.history[..history_index.min(self.history.len())]
            .iter()
            .filter(|entry| match entry {
                HistoryEntry::Conversation(entry) => {
                    entry.blocks.iter().any(PresentationBlock::is_editable)
                }
                _ => false,
            })
            .count()
    }

    pub fn selected_plain_text(&self, selection: Selection) -> Option<String> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_plain_text(entry, selection.content_index))
    }

    pub fn selected_message_text(
        &self,
        history_index: usize,
        diagnostics_visible: bool,
    ) -> Option<String> {
        let entry = self.history.get(history_index)?;
        let items = (0..entry.selectable_upper_bound())
            .filter(|&index| entry.index_is_selectable(index, diagnostics_visible))
            .filter_map(|index| selected_plain_text(entry, index))
            .collect::<Vec<_>>();
        (!items.is_empty()).then(|| items.join("\n\n"))
    }

    pub fn selected_readable_list(&self, selection: Selection) -> Option<String> {
        let HistoryEntry::Conversation(entry) = self.history.get(selection.history_index)? else {
            return None;
        };
        entry
            .blocks
            .get(selection.content_index)?
            .readable_list_copy()
    }

    pub fn selected_tool_output(&self, selection: Selection) -> Option<String> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_tool_output(entry, selection.content_index))
    }

    pub fn selection_is_tool(&self, selection: Selection) -> bool {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_tool(entry, selection.content_index))
            .is_some()
    }

    pub fn selected_command_job(
        &self,
        selection: Selection,
    ) -> Option<&zevria_foundation::CommandJobSnapshot> {
        let tool = selected_tool(
            self.history.get(selection.history_index)?,
            selection.content_index,
        )?;
        let PresentedTool::Native { call, state } = tool else {
            return None;
        };
        if call.function.name != zevria_foundation::COMMAND_TOOL_NAME {
            return None;
        }
        state.command_job.as_ref()
    }

    pub fn selected_subtask_id(&self, selection: Selection) -> Option<SubtaskId> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_subtask_id(entry, selection.content_index))
    }

    pub fn selected_agent_run_id(&self, selection: Selection) -> Option<AgentRunId> {
        self.history
            .get(selection.history_index)
            .and_then(|entry| selected_agent_run_id(entry, selection.content_index))
    }

    pub fn next_entry_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        if let Some((history_index, content_index)) = self
            .history
            .iter()
            .enumerate()
            .skip(selection.history_index.saturating_add(1))
            .find_map(|(history_index, entry)| {
                entry
                    .first_selectable_index(diagnostics_visible)
                    .map(|content_index| (history_index, content_index))
            })
        {
            return Selection {
                history_index,
                content_index,
            };
        }
        selection
    }

    pub fn previous_entry_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        let Some((history_index, content_index)) = self
            .history
            .iter()
            .enumerate()
            .take(selection.history_index)
            .rev()
            .find_map(|(history_index, entry)| {
                entry
                    .first_selectable_index(diagnostics_visible)
                    .map(|content_index| (history_index, content_index))
            })
        else {
            return selection;
        };
        Selection {
            history_index,
            content_index,
        }
    }

    pub fn next_block_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        self.history
            .get(selection.history_index)
            .and_then(|entry| {
                entry.next_selectable_index(selection.content_index, diagnostics_visible)
            })
            .map_or(selection, |content_index| Selection {
                content_index,
                ..selection
            })
    }

    pub fn previous_block_selection(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Selection {
        self.history
            .get(selection.history_index)
            .and_then(|entry| {
                entry.previous_selectable_index(selection.content_index, diagnostics_visible)
            })
            .map_or(selection, |content_index| Selection {
                content_index,
                ..selection
            })
    }

    pub fn next_user_entry(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        let mut target = selection;
        loop {
            target = self.next_user_message(target, diagnostics_visible)?;
            if target.history_index != selection.history_index {
                return Some(target);
            }
        }
    }

    pub fn previous_user_entry(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        let mut target = selection;
        loop {
            target = self.previous_user_message(target, diagnostics_visible)?;
            if target.history_index != selection.history_index {
                return Some(target);
            }
        }
    }

    pub fn next_user_message(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        self.history
            .iter()
            .enumerate()
            .skip(selection.history_index)
            .find_map(|(history_index, entry)| {
                entry
                    .user_message_blocks(diagnostics_visible)
                    .find(|&(_, start)| {
                        history_index > selection.history_index || start > selection.content_index
                    })
                    .map(|(_, content_index)| Selection {
                        history_index,
                        content_index,
                    })
            })
    }

    pub fn previous_user_message(
        &self,
        selection: Selection,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        // A selection on any user block excludes that whole message. Other
        // items (including diagnostics) search from their own coordinates.
        let before = self
            .history
            .get(selection.history_index)
            .and_then(|entry| {
                entry
                    .user_message_blocks(diagnostics_visible)
                    .find(|&(index, _)| index == selection.content_index)
            })
            .map_or(selection.content_index, |(_, start)| start);
        self.history
            .iter()
            .enumerate()
            .take(selection.history_index.saturating_add(1))
            .rev()
            .find_map(|(history_index, entry)| {
                entry
                    .user_message_blocks(diagnostics_visible)
                    .filter(|&(_, start)| history_index < selection.history_index || start < before)
                    .last()
                    .map(|(_, content_index)| Selection {
                        history_index,
                        content_index,
                    })
            })
    }

    pub fn reconcile_selection(
        &self,
        selection: Selection,
        scope: SelectionScope,
        diagnostics_visible: bool,
    ) -> Option<Selection> {
        let entry = self.history.get(selection.history_index)?;
        if entry.index_is_selectable(selection.content_index, diagnostics_visible) {
            Some(selection)
        } else if scope == SelectionScope::Message {
            entry
                .first_selectable_index(diagnostics_visible)
                .map(|content_index| Selection {
                    content_index,
                    ..selection
                })
        } else {
            None
        }
    }

    // A terminal failure record can rule out an inferred answer. Ordinary
    // notices (and other incomplete attempts) do not supply this evidence.
}
