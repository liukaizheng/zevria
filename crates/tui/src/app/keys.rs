//! Semantic action dispatch. Key spelling and chords live in input/keymap.
use super::*;
use crate::input::Action;

impl App {
    pub(super) fn dispatch_input_at(
        &mut self,
        owner: Surface,
        input: UserInput,
        now: Instant,
    ) -> Option<UiAction> {
        let key = match input {
            UserInput::Paste(text) => {
                self.interaction.clear_chords();
                if matches!(
                    owner.id.kind,
                    SurfaceKind::Composer | SurfaceKind::Completion
                ) && self.can_edit_draft()
                {
                    self.composer.insert_text(&text);
                }
                return None;
            }
            UserInput::Key(key) => key,
        };
        if let Some(help) = &mut self.help {
            if help.handle_input(key) {
                self.help = None;
            }
            return None;
        }
        self.view.clear_pending_repin();
        let action = self.interaction.resolve(owner.context(), key);
        if action != Some(Action::Close) {
            self.interaction.interrupt_escape();
        }
        if !matches!(action, Some(Action::Copy | Action::CopyOutput)) {
            self.interaction.clear_yank();
        }
        let action = action?;
        if action == Action::Help {
            self.help = Some(crate::hints::HelpOverlay::new(
                owner.context(),
                &self.hint_eligibility(),
            ));
            self.interaction.clear_chords();
            return None;
        }
        if action == Action::Cancel {
            return self.cancel_surface(owner.id.kind);
        }
        if self.composer.is_paste_pending() && owner.id.kind == SurfaceKind::Composer {
            return None;
        }
        match owner.id.kind {
            SurfaceKind::PlanReview => return self.handle_plan_dialog_action(action),
            SurfaceKind::Selection => {
                return self.handle_select_action(
                    action,
                    self.interaction.selection().expect("selection owner"),
                    now,
                );
            }
            SurfaceKind::Composer | SurfaceKind::Completion => {
                if !self.can_edit_draft() {
                    if action == Action::Close {
                        self.handle_escape(now);
                    }
                    return None;
                }
                match action {
                    Action::Paste => {
                        let action = UiAction::ReadClipboard {
                            generation: self.composer.generation(),
                            cursor: self.composer.cursor(),
                        };
                        self.composer.begin_paste();
                        return Some(action);
                    }
                    Action::Undo => {
                        self.composer.undo();
                        return None;
                    }
                    Action::Redo => {
                        self.composer.redo();
                        return None;
                    }
                    Action::Submit => return self.submit(),
                    _ => {}
                }
                if owner.id.kind == SurfaceKind::Completion
                    && matches!(
                        action,
                        Action::AcceptCompletion
                            | Action::Complete
                            | Action::Up
                            | Action::Down
                            | Action::PageUp
                            | Action::PageDown
                            | Action::Home
                            | Action::End
                            | Action::Close
                    )
                {
                    return self.handle_command_menu_action(action, now);
                }
            }
            SurfaceKind::Transcript => {
                if (action == Action::Confirm
                    || (action == Action::RecoverDraft && !self.drafts.has_recovery()))
                    && !self.session.is_busy()
                    && self.composer.is_blank()
                    && let Some(expected) = self.workflow.fresh_retry_version()
                {
                    return self.begin_plan_decision(expected, PlanDecision::ImplementFresh);
                }
                if self.pane.is_worker() && action == Action::ConfirmWorker {
                    return self.confirm_worker(false);
                }
            }
            _ => return None,
        }
        self.handle_unselected_action(action, now)
    }

    pub(super) fn handle_command_menu_action(
        &mut self,
        action: Action,
        now: Instant,
    ) -> Option<UiAction> {
        match action {
            Action::AcceptCompletion => {
                if self.composer.accept_highlighted_completion()
                    == Some(CompletionAcceptance::Builtin)
                    && matches!(self.composer.classify(), Ok(ClassifiedInput::Builtin(_)))
                {
                    return self.submit();
                }
            }
            Action::Complete => {
                self.composer.accept_highlighted_completion();
            }
            Action::Up => self.composer.move_menu_up(),
            Action::Down => self.composer.move_menu_down(),
            Action::PageUp => self
                .composer
                .move_menu_by(-(self.view.completion_page_rows() as isize)),
            Action::PageDown => self
                .composer
                .move_menu_by(self.view.completion_page_rows() as isize),
            Action::Home => self.composer.move_menu_by(isize::MIN),
            Action::End => self.composer.move_menu_by(isize::MAX),
            Action::Close => {
                if self.edit.is_recalling()
                    && self.composer.completion_kind() != Some(CompletionKind::File)
                {
                    self.handle_escape(now);
                } else {
                    self.composer.cancel_completion();
                    self.view.set_composer_scroll(0);
                    self.interaction.clear_chords();
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn handle_unselected_action(
        &mut self,
        action: Action,
        now: Instant,
    ) -> Option<UiAction> {
        match action {
            Action::Close => self.handle_escape(now),
            Action::PageUp | Action::PageDown => {
                self.composer.close_edit_group();
                if self.composer_editable() {
                    self.composer.page_vertical(
                        if action == Action::PageUp { -1 } else { 1 },
                        self.view.composer_page_rows(),
                        self.view.composer_width(),
                    );
                } else if action == Action::PageUp {
                    self.view.page_up();
                } else {
                    self.view.page_down();
                }
            }
            Action::HalfPageUp => self.view.half_page_up(),
            Action::HalfPageDown => self.view.half_page_down(),
            Action::Up | Action::Down if self.composer_editable() => {
                return self.handle_insert_action(action);
            }
            Action::Up => self.view.scroll_up(SCROLL_STEP),
            Action::Down => self.view.scroll_down(SCROLL_STEP),
            Action::Home => {
                if self.composer_editable() {
                    self.composer.move_home();
                } else {
                    self.view.jump_top();
                }
            }
            Action::End => {
                if self.composer_editable() {
                    self.composer.move_end();
                } else {
                    self.view.jump_bottom();
                }
            }
            Action::ToggleMode => {
                let mode = match self.session.next_mode() {
                    SessionMode::Build => SessionMode::Plan,
                    SessionMode::Plan => SessionMode::Build,
                };
                return self.begin_mode_selection(mode, false);
            }
            _ if self.composer_editable() => return self.handle_insert_action(action),
            _ => self.handle_normal_action(action),
        }
        None
    }

    pub(super) fn handle_insert_action(&mut self, action: Action) -> Option<UiAction> {
        match action {
            Action::Submit => return self.submit(),
            Action::Newline => self.composer.insert_character('\n'),
            Action::Backspace => self.composer.backspace(),
            Action::WordLeft => self.composer.move_word_left(),
            Action::WordRight => self.composer.move_word_right(),
            Action::DeleteLine => self.composer.delete_current_line(),
            Action::DeleteToEnd => self.composer.delete_to_line_end(),
            Action::Left => self.composer.move_left(),
            Action::Right => self.composer.move_right(),
            Action::Up => self.composer.move_vertical(-1, self.view.composer_width()),
            Action::Down => self.composer.move_vertical(1, self.view.composer_width()),
            Action::Type(character) => self.composer.insert_character(character),
            _ => self.composer.close_edit_group(),
        }
        None
    }

    pub(super) fn handle_normal_action(&mut self, action: Action) {
        if action == Action::RecoverDraft && self.drafts.has_recovery() && self.can_edit_draft() {
            if self.drafts.recover(&mut self.composer) {
                self.interaction.enter_insert();
            }
            return;
        }
        if self.handle_fold_action(action) {
            return;
        }
        match action {
            Action::Select if self.interaction.is_normal() => {
                if let Some(entry) = self.rendered_selection_entry() {
                    self.interaction.enter_selection_entry(entry);
                    self.view.set_follow(false);
                }
            }
            Action::Insert if self.can_edit_draft() => {
                self.composer.close_edit_group();
                self.interaction.enter_insert();
            }
            Action::PlanReview if self.pane.is_root() && self.workflow.open_review() => {
                self.interaction.clear_chords()
            }
            Action::Diagnostics if self.pane.toggle_diagnostics() => {
                self.view.invalidate_rendered_geometry();
                self.interaction.clear_selection();
                self.interaction.clear_yank();
            }
            _ => {}
        }
    }

    pub(super) fn handle_plan_dialog_action(&mut self, action: Action) -> Option<UiAction> {
        match action {
            Action::PageUp => {
                self.view.page_up();
                return None;
            }
            Action::PageDown => {
                self.view.page_down();
                return None;
            }
            Action::HalfPageUp => {
                self.view.half_page_up();
                return None;
            }
            Action::HalfPageDown => {
                self.view.half_page_down();
                return None;
            }
            Action::Home => {
                self.composer.close_edit_group();
                self.view.jump_top();
                return None;
            }
            Action::End => {
                self.composer.close_edit_group();
                self.view.jump_bottom();
                return None;
            }
            _ => {}
        }
        match self
            .workflow
            .handle_dialog_action(action, self.session.is_busy())
        {
            PlanIntent::None => None,
            PlanIntent::Close => {
                self.interaction.enter_normal();
                None
            }
            PlanIntent::Copy(text) => Some(UiAction::Copy { text }),
            PlanIntent::Decide { expected, decision } => {
                self.begin_plan_decision(expected, decision)
            }
        }
    }

    pub(super) fn handle_escape(&mut self, now: Instant) {
        self.composer.close_edit_group();
        if let Some(recall) = self.edit.cancel_recall() {
            self.composer.restore(recall.saved_input);
            self.interaction.enter_normal();
            return;
        }
        let target = self.rendered_selection_entry();
        self.interaction.escape(now, target);
        if self.interaction.is_selecting() {
            self.view.set_follow(false);
        }
    }

    pub(super) fn rendered_selection_entry(&self) -> Option<SelectionEntry> {
        self.view
            .rendered_selection_window()
            .and_then(|window| self.view.conversation_cache().selection_at_bottom(window))
            .filter(|&selection| {
                self.conversation.reconcile_selection(
                    selection,
                    SelectionScope::Block,
                    self.pane.diagnostics_visible(),
                ) == Some(selection)
            })
            .map(|selection| SelectionEntry {
                selection,
                reveal: false,
            })
    }

    pub(super) fn handle_select_action(
        &mut self,
        action: Action,
        selection: Selection,
        now: Instant,
    ) -> Option<UiAction> {
        match action {
            Action::PageUp => {
                self.view.page_up();
                return None;
            }
            Action::PageDown => {
                self.view.page_down();
                return None;
            }
            Action::Home => {
                self.view.jump_top();
                return None;
            }
            Action::End => {
                self.view.jump_bottom();
                return None;
            }
            _ => {}
        }
        if !matches!(action, Action::Copy | Action::CopyOutput) {
            self.interaction.clear_yank();
        }
        if self.handle_fold_action(action) {
            return None;
        }
        let scope = self.interaction.selection_scope().expect("selection scope");
        let diagnostics = self.pane.diagnostics_visible();
        match action {
            Action::Close => {
                if scope == SelectionScope::Block {
                    if let Some(state) = self.interaction.selection_state_mut() {
                        state.enter_message_scope();
                    }
                } else {
                    self.interaction.escape(now, None);
                }
                None
            }
            Action::HalfPageUp | Action::HalfPageDown => {
                let target = match (scope, action) {
                    (SelectionScope::Message, _) => self.user_entry_selection(
                        selection,
                        action == Action::HalfPageDown,
                        diagnostics,
                    ),
                    (SelectionScope::Block, Action::HalfPageUp) => self
                        .conversation
                        .previous_user_message(selection, diagnostics),
                    (SelectionScope::Block, _) => {
                        self.conversation.next_user_message(selection, diagnostics)
                    }
                };
                if let Some(target) = target.map(|target| self.span_representative(target))
                    && let Some(state) = self.interaction.selection_state_mut()
                {
                    state.set_selection(target);
                    state.enter_message_scope();
                    state.request_reveal();
                } else if scope == SelectionScope::Block
                    && let Some(state) = self.interaction.selection_state_mut()
                {
                    state.enter_message_scope();
                }
                None
            }
            Action::Down | Action::Up => {
                let next = action == Action::Down;
                let target = match (scope, next) {
                    (SelectionScope::Message, _) => {
                        let from = self.span_boundary(selection, next);
                        let target = if next {
                            self.conversation.next_entry_selection(from, diagnostics)
                        } else {
                            self.conversation
                                .previous_entry_selection(from, diagnostics)
                        };
                        self.span_representative(target)
                    }
                    (SelectionScope::Block, true) => self
                        .conversation
                        .next_block_selection(selection, diagnostics),
                    (SelectionScope::Block, false) => self
                        .conversation
                        .previous_block_selection(selection, diagnostics),
                };
                if let Some(state) = self.interaction.selection_state_mut() {
                    state.set_selection(target);
                    state.request_reveal();
                }
                None
            }
            Action::Copy | Action::CopyOutput => self.handle_yank(selection, now),
            Action::CopyList if scope == SelectionScope::Block => self
                .conversation
                .selected_readable_list(selection)
                .map(|text| UiAction::Copy { text }),
            Action::Edit => {
                self.recall_selected();
                None
            }
            Action::Confirm if scope == SelectionScope::Message => {
                let history_index = selection.history_index;
                if let Some((start, end)) = self.folds.span_containing(history_index) {
                    self.apply_fold(|folds, _, _| folds.unfold(FoldKey::Span { start, end }));
                    return None;
                }
                if self.folds.entry(history_index).is_message_folded() {
                    self.apply_fold(|folds, _, _| folds.unfold(FoldKey::Message { history_index }));
                }
                if let Some(state) = self.interaction.selection_state_mut() {
                    state.enter_block_scope();
                }
                None
            }
            Action::Confirm => self
                .conversation
                .selected_subtask_id(selection)
                .map(|id| UiAction::OpenSubtask { id })
                .or_else(|| {
                    self.conversation
                        .selected_agent_run_id(selection)
                        .map(|id| UiAction::OpenAgentRun { id })
                }),
            _ => None,
        }
    }

    pub(super) fn span_boundary(&self, selection: Selection, forward: bool) -> Selection {
        self.folds
            .span_containing(selection.history_index)
            .map_or(selection, |(start, end)| Selection {
                history_index: if forward { end } else { start },
                ..selection
            })
    }

    pub(super) fn span_representative(&self, selection: Selection) -> Selection {
        let Some((start, _)) = self.folds.span_containing(selection.history_index) else {
            return selection;
        };
        let representative = Selection {
            history_index: start,
            content_index: 0,
        };
        self.conversation
            .reconcile_selection(
                representative,
                SelectionScope::Message,
                self.pane.diagnostics_visible(),
            )
            .unwrap_or(representative)
    }

    pub(super) fn user_entry_selection(
        &self,
        selection: Selection,
        forward: bool,
        diagnostics: bool,
    ) -> Option<Selection> {
        let current = self.span_representative(selection);
        let mut from = self.span_boundary(selection, forward);
        loop {
            let target = if forward {
                self.conversation.next_user_entry(from, diagnostics)?
            } else {
                self.conversation.previous_user_entry(from, diagnostics)?
            };
            let representative = self.span_representative(target);
            if representative != current {
                return Some(representative);
            }
            from = self.span_boundary(target, forward);
        }
    }

    pub(super) fn handle_fold_action(&mut self, action: Action) -> bool {
        match action {
            Action::UnfoldAll => self.apply_fold(|folds, _, _| folds.unfold_all()),
            Action::FoldTurns => self.apply_fold(|folds, conversation, diagnostics| {
                folds.fold_turns(conversation.history(), diagnostics, TurnFold::FinalExpanded);
            }),
            Action::FoldOlder => self.apply_fold(|folds, conversation, diagnostics| {
                folds.fold_turns(
                    conversation.history(),
                    diagnostics,
                    TurnFold::OlderFinalFolded,
                );
            }),
            Action::FoldToggle | Action::FoldClose | Action::FoldOpen
                if self.interaction.is_selecting() =>
            {
                if let Some(key) = self.interaction.active_selection().and_then(|active| {
                    if active.scope == SelectionScope::Message
                        && let Some((start, end)) =
                            self.folds.span_containing(active.selection.history_index)
                    {
                        return Some(FoldKey::Span { start, end });
                    }
                    FoldKey::for_selection(
                        &self.conversation,
                        active.selection,
                        active.scope,
                        self.pane.diagnostics_visible(),
                    )
                }) {
                    self.apply_fold(|folds, _, _| match action {
                        Action::FoldToggle => folds.toggle(key),
                        Action::FoldClose => folds.fold(key),
                        Action::FoldOpen => folds.unfold(key),
                        _ => unreachable!(),
                    });
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn apply_fold(&mut self, f: impl FnOnce(&mut FoldState, &ConversationState, bool)) {
        self.view.capture_conversation_anchor();
        f(
            &mut self.folds,
            &self.conversation,
            self.pane.diagnostics_visible(),
        );
        self.view.invalidate_rendered_geometry();
        let representative = self.interaction.active_selection().and_then(|active| {
            self.folds
                .span_containing(active.selection.history_index)
                .map(|_| self.span_representative(active.selection))
        });
        if let Some(selection) = self.interaction.selection_state_mut() {
            if let Some(representative) = representative {
                selection.set_selection(representative);
                selection.enter_message_scope();
                selection.request_reveal();
            } else if selection.scope() == SelectionScope::Block
                && self
                    .folds
                    .entry(selection.selection().history_index)
                    .is_message_folded()
            {
                selection.enter_message_scope();
            } else {
                selection.request_reveal();
            }
        }
    }

    pub(super) fn handle_yank(&mut self, selection: Selection, now: Instant) -> Option<UiAction> {
        if self.interaction.selection_scope() == Some(SelectionScope::Message) {
            self.interaction.clear_yank();
            let (start, end) = self
                .folds
                .span_containing(selection.history_index)
                .unwrap_or((selection.history_index, selection.history_index));
            let texts = (start..=end)
                .filter_map(|index| {
                    self.conversation
                        .selected_message_text(index, self.pane.diagnostics_visible())
                })
                .collect::<Vec<_>>();
            return (!texts.is_empty()).then(|| UiAction::Copy {
                text: texts.join("\n\n"),
            });
        }
        if !self.conversation.selection_is_tool(selection) {
            self.interaction.clear_yank();
            return self
                .conversation
                .selected_plain_text(selection)
                .map(|text| UiAction::Copy { text });
        }
        let double = self
            .interaction
            .selection_state_mut()
            .is_some_and(|state| state.press_yank(now));
        if double {
            self.conversation
                .selected_tool_output(selection)
                .map(|text| UiAction::Copy { text })
        } else {
            self.conversation
                .selected_plain_text(selection)
                .map(|text| UiAction::Copy { text })
        }
    }

    pub(super) fn recall_target(&self) -> Option<Selection> {
        let active = self.interaction.active_selection()?;
        match active.scope {
            SelectionScope::Message => self
                .conversation
                .message_recall_selection(active.selection.history_index),
            SelectionScope::Block => Some(active.selection),
        }
    }

    pub(crate) fn can_recall_selected(&self) -> bool {
        self.capabilities().edit_transcript.is_ok()
            && !self.conversation.has_executing_tool_calls()
            && self.workflow.dialog().is_none()
            && self.recall_target().is_some_and(|selection| {
                self.conversation
                    .recallable_selection(selection)
                    .is_some_and(|(text, _)| !text.is_blank())
            })
    }

    pub(super) fn recall_selected(&mut self) -> bool {
        if !self.can_recall_selected() {
            return false;
        }
        let Some(selection) = self.recall_target() else {
            return false;
        };
        let Some((text, target)) = self.conversation.recallable_selection(selection) else {
            return false;
        };
        let saved_input = self.composer.snapshot();
        if !self.edit.recall(RecallEdit {
            saved_input,
            target,
            target_index: selection.history_index,
        }) {
            return false;
        }
        self.composer.replace_prompt(&text);
        self.interaction.enter_insert();
        self.view.set_composer_scroll(0);
        true
    }
}
