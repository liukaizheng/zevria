//! Response attempt materialization and stable block identity.

use super::{
    AssistantContent, AttemptBlockKey, BlockVisibility, ConversationEntry, ConversationState,
    HashMap, HashSet, HistoryEntry, HostedAttempt, Message, NativeHeader, PresentationBlock,
    PresentationBlockId, PresentationBlockKind, PresentationRole, PresentedTool, TextFlavor,
    ToolCallLocation, ToolCallState, ToolCallStatus, assistant_answer, native_tool_keys,
    tool_call_round,
};

impl ConversationState {
    pub fn update_response_attempt(&mut self, attempt: zevria_content::ResponseAttempt) {
        self.update_attempt(attempt, false);
    }

    pub fn update_native_attempt(
        &mut self,
        attempt: zevria_content::ResponseAttempt,
        header: Option<NativeHeader>,
    ) {
        let id = attempt.id.clone();
        let existing = self.hosted_attempts.contains_key(&id);
        self.update_response_attempt(attempt);
        if !existing {
            self.bind_attempt_header(&id, header);
        }
    }

    fn bind_attempt_header(&mut self, id: &str, header: Option<NativeHeader>) {
        if let Some(attempt) = self.hosted_attempts.get(id)
            && let HistoryEntry::Conversation(entry) = &mut self.history[attempt.history_index]
        {
            // Later dispatches and activity revisions cannot relabel earlier output.
            if entry.header.is_none()
                && let Some(header) = header
            {
                entry.header = Some(header);
                let index = attempt.history_index;
                let indices = self.represented_headers.entry(header).or_default();
                indices.insert(indices.partition_point(|&old| old < index), index);
                self.touch_entry(index);
            }
        }
    }

    /// The ACP reducer validates the immutable source revision before filtering
    /// its text to verified ordinary projections. Later binding coverage may
    /// enrich that filtered presentation without changing the source revision.
    pub fn update_projected_attempt(&mut self, attempt: zevria_content::ResponseAttempt) {
        self.update_attempt(attempt, true);
    }

    pub(super) fn update_attempt(
        &mut self,
        attempt: zevria_content::ResponseAttempt,
        projected: bool,
    ) {
        if attempt.validate().is_err() || !attempt.has_display() {
            return;
        }
        let id = attempt.id.clone();
        if let Some(existing) = self.hosted_attempts.get_mut(&id) {
            if attempt.revision < existing.attempt.revision
                || (!projected && attempt.revision == existing.attempt.revision)
                || attempt == existing.attempt
            {
                return;
            }
            let previous_answer = existing.projected_answer_item();
            existing.attempt = attempt;
            if previous_answer != existing.projected_answer_item() {
                let index = existing.history_index;
                self.touch_entry(index);
                self.change_structure();
            }
        } else {
            let history_index = self.push_entry(HistoryEntry::Conversation(ConversationEntry {
                references: Vec::new(),
                header: None,
                blocks: Vec::new(),
            }));
            self.hosted_attempts.insert(
                id.clone(),
                HostedAttempt {
                    history_index,
                    attempt,
                    ids: HashMap::new(),
                    owned_ids: HashSet::new(),
                    canonical: None,
                    acp_tools: HashMap::new(),
                    tool_status: ToolCallStatus::Finished,
                    final_answer: false,
                    projected,
                },
            );
        }
        self.reconcile_attempt(&id);
    }

    pub fn commit_response(
        &mut self,
        message: Message,
        id: Option<&str>,
        status: ToolCallStatus,
        header: Option<NativeHeader>,
    ) {
        self.project_response(message, id, status, header, false);
    }

    pub fn complete_response(
        &mut self,
        message: Message,
        id: Option<&str>,
        header: Option<NativeHeader>,
    ) {
        let final_answer = !tool_call_round(&message);
        self.project_response(message, id, ToolCallStatus::Finished, header, final_answer);
    }

    pub(super) fn project_response(
        &mut self,
        message: Message,
        id: Option<&str>,
        status: ToolCallStatus,
        header: Option<NativeHeader>,
        final_answer: bool,
    ) -> Option<usize> {
        if let Some(id) = id {
            self.bind_attempt_header(id, header);
        }
        let index = if let Some(id) = id
            && let Some(attempt) = self.hosted_attempts.get_mut(id)
        {
            attempt.canonical = Some(message);
            attempt.tool_status = status;
            attempt.final_answer = final_answer;
            let index = attempt.history_index;
            self.reconcile_attempt(id);
            Some(index)
        } else if let Some(index) = self.push_numbered_message(message, status, header) {
            if let HistoryEntry::Conversation(entry) = &self.history[index] {
                self.response_answers.insert(
                    index,
                    entry
                        .blocks
                        .iter()
                        .filter(|block| final_answer && assistant_answer(block))
                        .map(|block| block.id)
                        .collect(),
                );
            }
            Some(index)
        } else {
            None
        };
        index
    }

    fn reconcile_attempt(&mut self, id: &str) {
        use crate::presentation::WebActivityPresentation;
        use zevria_content::AssistantPresentationContent as Content;
        let state = self.hosted_attempts.get_mut(id).expect("known attempt");
        let attempt = &state.attempt;
        let mut desired = Vec::new();
        let mut ordered = std::collections::BTreeMap::new();
        enum Item<'a> {
            Part(&'a zevria_content::AssistantPresentationPart),
            Action(&'a zevria_content::HostedActivity),
        }
        for part in &attempt.presentation {
            ordered.insert(
                (part.source.output_index, Some(part.source.part)),
                Item::Part(part),
            );
        }
        for action in &attempt.hosted_activity {
            ordered.insert((action.output_index, None), Item::Action(action));
        }
        let native_calls: HashMap<_, _> = match &state.canonical {
            Some(Message::Assistant(assistant)) => assistant
                .content
                .iter()
                .filter_map(|item| {
                    let AssistantContent::ToolCall(call) = item else {
                        return None;
                    };
                    Some((call.id.to_string(), call))
                })
                .collect(),
            _ => HashMap::new(),
        };
        let mut pending = Vec::new();
        let flush = |pending: &mut Vec<&zevria_content::HostedActivity>, desired: &mut Vec<_>| {
            if let Some(first) = pending.first() {
                desired.push((
                    AttemptBlockKey::Action(first.output_index),
                    PresentationBlockKind::WebActivity(WebActivityPresentation::from_actions(
                        attempt, pending,
                    )),
                ));
                pending.clear();
            }
        };
        for ((output, _), item) in ordered {
            let part = match item {
                Item::Part(part) => part,
                Item::Action(action) => {
                    if action.details().is_empty() {
                        pending.push(action);
                    } else {
                        flush(&mut pending, &mut desired);
                        desired.push((
                            AttemptBlockKey::Action(output),
                            PresentationBlockKind::WebActivity(
                                WebActivityPresentation::from_actions(attempt, &[action]),
                            ),
                        ));
                    }
                    continue;
                }
            };
            let kind = match &part.content {
                Content::Reasoning { text } if !text.trim().is_empty() => {
                    PresentationBlockKind::Reasoning {
                        parts: vec![zevria_content::readable::sanitize_readable(text)],
                    }
                }
                Content::Answer { text, sources } if !text.trim().is_empty() => {
                    PresentationBlockKind::Text {
                        text: zevria_content::readable::sanitize_readable(text),
                        flavor: TextFlavor::Markdown,
                        editable: false,
                        sources: sources.clone(),
                    }
                }
                Content::NativeTool { call_id } => {
                    let call = native_calls.get(call_id.as_str()).copied();
                    if let Some(call) = call {
                        PresentationBlockKind::Tool(PresentedTool::Native {
                            call: Box::new(call.clone()),
                            state: Box::new(ToolCallState::new(
                                state.tool_status,
                                &zevria_content::message::tool_arguments(&call.function),
                            )),
                        })
                    } else if let Some(tool) = state.acp_tools.get(call_id) {
                        tool.clone()
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            flush(&mut pending, &mut desired);
            desired.push((AttemptBlockKey::Part(output, part.source.part), kind));
        }
        flush(&mut pending, &mut desired);
        if let Some(marker) = crate::blocks::incomplete_response_marker(attempt.outcome) {
            desired.push((
                AttemptBlockKey::Incomplete,
                PresentationBlockKind::Placeholder(marker.into()),
            ));
        }
        let HistoryEntry::Conversation(entry) = &mut self.history[state.history_index] else {
            return;
        };
        let old_blocks = std::mem::take(&mut entry.blocks);
        let old_ids: Vec<_> = old_blocks.iter().map(|block| block.id).collect();
        let index = state.history_index;
        let native_before = native_tool_keys(&old_blocks, &state.owned_ids);
        let mut changed = false;
        let insertion = old_blocks
            .iter()
            .position(|block| state.owned_ids.contains(&block.id))
            .unwrap_or(old_blocks.len());
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut previous = HashMap::new();
        let mut children: HashMap<_, Vec<_>> = HashMap::new();
        for (index, block) in old_blocks.into_iter().enumerate() {
            if state.owned_ids.contains(&block.id) {
                previous.insert(block.id, block);
            } else if let PresentationBlockKind::Subtask { parent, .. } = &block.kind
                && state.owned_ids.contains(parent)
            {
                children.entry(*parent).or_default().push(block);
            } else if index < insertion {
                before.push(block);
            } else {
                after.push(block);
            }
        }
        entry.blocks = before;
        for (key, kind) in desired {
            let block_id = *state
                .ids
                .entry(key)
                .or_insert_with(|| self.block_ids.allocate());
            state.owned_ids.insert(block_id);
            let mut block = PresentationBlock {
                id: block_id,
                revision: 0,
                role: Some(PresentationRole::Assistant),
                prompt_group: None,
                prompt: None,
                visibility: BlockVisibility::Always,
                kind,
            };
            if let Some(old) = previous.remove(&block_id) {
                if old.native_tool().is_some() && block.native_tool().is_some() {
                    // A result's state belongs to the semantic call, never its old index.
                    block = old;
                } else {
                    block.revision = old.revision;
                    if old.kind != block.kind {
                        block.touch();
                        changed = true;
                    }
                }
            }
            entry.blocks.push(block);
            if let Some(rows) = children.remove(&block_id) {
                entry.blocks.extend(rows);
            }
        }
        entry.blocks.extend(after);
        let removed = !previous.is_empty() || !children.is_empty();
        let order_changed = !entry.blocks.iter().map(|block| block.id).eq(old_ids);
        let native_after = native_tool_keys(&entry.blocks, &state.owned_ids);
        let mut commitment_changed = false;
        if state.canonical.is_some() {
            use zevria_content::ResponseAttemptOutcome::{Failed, Interrupted};
            let answer_item = (state.final_answer
                && !matches!(attempt.outcome, Failed | Interrupted))
            .then(|| state.answer_item())
            .flatten();
            let parts = state
                .ids
                .iter()
                .filter_map(|(key, id)| match key {
                    AttemptBlockKey::Part(output, _) if Some(*output) == answer_item => Some(*id),
                    _ => None,
                })
                .collect();
            if self.response_answers.get(&index) != Some(&parts) {
                self.response_answers.insert(index, parts);
                commitment_changed = true;
            }
        }
        if order_changed {
            self.invalidate_block_indices(index);
        }
        if native_before != native_after {
            let removed_tools: HashSet<_> = previous
                .values()
                .filter_map(|block| block.native_tool().map(|_| block.id))
                .collect();
            if !removed_tools.is_empty() {
                let live = |location: &ToolCallLocation| {
                    location.history_index != index || !removed_tools.contains(&location.block_id)
                };
                self.command_call_locations
                    .retain(|_, location| live(location));
                self.command_job_locations
                    .retain(|_, location| live(location));
                self.subtask_call_locations
                    .retain(|_, location| live(location));
                self.subtask_locations.retain(|_, locations| {
                    locations.retain(|location| live(&location.parent));
                    !locations.is_empty()
                });
            }
            for (call, block_id, status) in native_before {
                if status == ToolCallStatus::Executing {
                    self.remove_executing(
                        &call,
                        ToolCallLocation {
                            history_index: index,
                            block_id,
                        },
                    );
                }
            }
            for (call, block_id, status) in native_after {
                if status == ToolCallStatus::Executing {
                    self.executing_tool_calls
                        .entry(call)
                        .or_default()
                        .push(ToolCallLocation {
                            history_index: index,
                            block_id,
                        });
                }
            }
            self.index_native_locations(index);
        }
        if changed || order_changed || commitment_changed {
            self.touch_entry(index);
        }
        if removed || commitment_changed {
            self.change_structure();
        }
    }

    pub fn bind_attempt_tools(&mut self, id: &str, tools: HashMap<String, PresentationBlockKind>) {
        if let Some(attempt) = self.hosted_attempts.get_mut(id)
            && attempt.acp_tools != tools
        {
            attempt.acp_tools = tools;
            self.reconcile_attempt(id);
        }
    }

    /// Place an ACP attempt at the first explicitly covered projection, keeping
    /// unrelated blocks and all block IDs stable. Subsequent updates reconcile
    /// only this attempt's owned blocks inside the shared entry.
    pub fn place_attempt(&mut self, id: &str, history_index: usize, before: PresentationBlockId) {
        let Some(state) = self.hosted_attempts.get_mut(id) else {
            return;
        };
        if state.history_index == history_index {
            return;
        }
        if !matches!(
            self.history.get(history_index),
            Some(HistoryEntry::Conversation(_))
        ) {
            return;
        }
        let from = state.history_index;
        let HistoryEntry::Conversation(source) = &mut self.history[from] else {
            return;
        };
        let original = std::mem::take(&mut source.blocks);
        let (blocks, remaining): (Vec<_>, Vec<_>) = original
            .into_iter()
            .partition(|block| state.owned_ids.contains(&block.id));
        source.blocks = remaining;
        let HistoryEntry::Conversation(target) = &mut self.history[history_index] else {
            unreachable!()
        };
        let position = target
            .blocks
            .iter()
            .position(|block| block.id == before)
            .unwrap_or(target.blocks.len());
        target.blocks.splice(position..position, blocks);
        state.history_index = history_index;
        self.invalidate_block_indices(from);
        self.invalidate_block_indices(history_index);
        self.touch_entry(from);
        self.touch_entry(history_index);
        self.change_structure();
    }

    pub fn alias_attempt_action(
        &mut self,
        old: (usize, PresentationBlockId),
        attempt_id: &str,
        output: u64,
    ) {
        if let Some(attempt) = self.hosted_attempts.get(attempt_id)
            && let Some(id) = attempt.ids.get(&AttemptBlockKey::Action(output))
        {
            let target = (attempt.history_index, *id);
            if old != target && self.identity_aliases.insert(old, target) != Some(target) {
                self.change_structure();
            }
        }
    }

    pub fn alias_attempt_part(
        &mut self,
        old: (usize, PresentationBlockId),
        attempt_id: &str,
        source: &zevria_content::AssistantSourceAddress,
    ) {
        if let Some(attempt) = self.hosted_attempts.get(attempt_id)
            && let Some(id) = attempt
                .ids
                .get(&AttemptBlockKey::Part(source.output_index, source.part))
        {
            let target = (attempt.history_index, *id);
            if old != target && self.identity_aliases.insert(old, target) != Some(target) {
                self.change_structure();
            }
        }
    }
}
