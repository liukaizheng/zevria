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
                if matches!(action, Action::PreviousTurn | Action::NextTurn) {
                    self.view.jump_turn(if action == Action::PreviousTurn {
                        view::TurnDirection::Previous
                    } else {
                        view::TurnDirection::Next
                    });
                    return None;
                }
                if (action == Action::Confirm
                    || (action == Action::RecoverDraft && !self.drafts.has_recovery()))
                    && !self.model.session.is_busy()
                    && self.composer.is_blank()
                    && let Some(expected) = self.model.workflow.fresh_retry_version()
                {
                    return self.begin_plan_decision(expected, PlanDecision::ImplementFresh);
                }
                if action == Action::Confirm
                    && self.model.workflow.fresh_retry_version().is_none()
                    && self.composer.is_blank()
                    && self.can_send_queued()
                {
                    return self.send_queued_now();
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
                let mode = match self.model.session.next_mode() {
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
            Action::EditQueued => self.edit_newest_queued(),
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
            Action::PlanReview if self.pane.is_root() && self.model.workflow.open_review() => {
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
            .model
            .workflow
            .handle_dialog_action(action, self.model.session.is_busy())
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
                self.model.conversation.reconcile_selection(
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
                        .model
                        .conversation
                        .previous_user_message(selection, diagnostics),
                    (SelectionScope::Block, _) => self
                        .model
                        .conversation
                        .next_user_message(selection, diagnostics),
                };
                if let Some(target) =
                    target.map(|target| self.span_representative(target, SelectionScope::Message).0)
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
                let (target, target_scope) =
                    self.adjacent_visible_selection(selection, scope, next);
                // Boundary Up preserves reveal intent; Down can reveal a clipped last item.
                if !next && target == selection {
                    return None;
                }
                if let Some(state) = self.interaction.selection_state_mut() {
                    state.set_selection(target);
                    if target_scope == SelectionScope::Message && scope == SelectionScope::Block {
                        state.enter_message_scope();
                    }
                    state.request_reveal();
                }
                None
            }
            Action::Terminate | Action::MoveToBackground => {
                use zevria_presentation::jobs::JobsInspector;
                let terminate = action == Action::Terminate;
                self.selected_command_job()
                    .filter(|job| {
                        if terminate {
                            JobsInspector::can_terminate(job)
                        } else {
                            JobsInspector::can_background(job)
                        }
                    })
                    .map(|job| UiAction::JobControl {
                        id: job.id.clone(),
                        terminate,
                    })
            }
            Action::Copy | Action::CopyOutput => self.handle_yank(selection, now),
            Action::CopyList if scope == SelectionScope::Block => self
                .model
                .conversation
                .selected_readable_list(selection)
                .map(|text| UiAction::Copy { text }),
            Action::Edit => {
                self.recall_selected();
                None
            }
            Action::Confirm if scope == SelectionScope::Message => {
                if let Some(key) = self.selection_fold_key(selection, scope)
                    && self.folds.contains(key)
                {
                    self.apply_fold(|folds, model, diagnostics| {
                        folds.open(model.conversation.history(), key, diagnostics)
                    });
                    if matches!(key, FoldKey::Span { .. }) {
                        return None;
                    }
                }
                // The raw first block of an answer entry might be on the
                // preceding span's row. Start block selection in this entry.
                let mut target = selection;
                while let Some(FoldKey::Span { .. }) =
                    self.selection_fold_key(target, SelectionScope::Block)
                {
                    let next = self
                        .model
                        .conversation
                        .next_block_selection(target, diagnostics);
                    if next == target {
                        return None;
                    }
                    target = next;
                }
                let (target, _) = self.span_representative(target, SelectionScope::Block);
                if let Some(state) = self.interaction.selection_state_mut() {
                    state.enter_block_scope();
                    state.set_selection(target);
                }
                None
            }
            Action::Confirm
                if self
                    .selection_fold_key(selection, scope)
                    .is_some_and(|key| self.folds.mode(key) == crate::fold::FoldMode::Summary) =>
            {
                let key = self.selection_fold_key(selection, scope).unwrap();
                self.apply_fold(|folds, model, diagnostics| {
                    folds.open(model.conversation.history(), key, diagnostics)
                });
                None
            }
            Action::Confirm => self
                .model
                .conversation
                .selected_subtask_id(selection)
                .map(|id| UiAction::OpenSubtask { id })
                .or_else(|| {
                    self.model
                        .conversation
                        .selected_agent_run_id(selection)
                        .map(|id| UiAction::OpenAgentRun { id })
                }),
            _ => None,
        }
    }

    pub(super) fn span_boundary(&self, selection: Selection, forward: bool) -> Selection {
        if let Some(FoldKey::Span { start, end }) =
            self.selection_fold_key(selection, SelectionScope::Message)
        {
            Selection {
                history_index: if forward { end } else { start },
                ..selection
            }
        } else {
            selection
        }
    }

    fn selection_fold_key(&self, selection: Selection, scope: SelectionScope) -> Option<FoldKey> {
        let conversation = &self.model.conversation;
        let diagnostics = self.pane.diagnostics_visible();
        let key = FoldKey::for_selection(conversation, selection, scope, diagnostics)?;
        Some(
            self.folds
                .representative(conversation.history(), key, diagnostics),
        )
    }

    fn fold_key_selection(&self, key: FoldKey) -> Option<Selection> {
        let conversation = &self.model.conversation;
        match key {
            FoldKey::Block { history_index, id } => {
                conversation.selection_for_identity(history_index, id)
            }
            FoldKey::Item {
                history_index,
                content_index,
            } => Some(Selection {
                history_index,
                content_index,
            }),
            FoldKey::Span { .. } | FoldKey::Message { .. } => conversation.reconcile_selection(
                Selection {
                    history_index: key.history_index(),
                    content_index: 0,
                },
                SelectionScope::Message,
                self.pane.diagnostics_visible(),
            ),
        }
    }

    fn span_representative(
        &self,
        selection: Selection,
        scope: SelectionScope,
    ) -> (Selection, SelectionScope) {
        let Some(key) = self.selection_fold_key(selection, scope) else {
            return (selection, scope);
        };
        match key {
            FoldKey::Span { .. } => (
                self.fold_key_selection(key).unwrap_or(selection),
                SelectionScope::Message,
            ),
            FoldKey::Message { .. } => (selection, SelectionScope::Message),
            _ => (self.fold_key_selection(key).unwrap_or(selection), scope),
        }
    }

    fn adjacent_visible_selection(
        &self,
        selection: Selection,
        scope: SelectionScope,
        forward: bool,
    ) -> (Selection, SelectionScope) {
        let conversation = &self.model.conversation;
        let diagnostics = self.pane.diagnostics_visible();
        let current = self.selection_fold_key(selection, scope);
        let mut from = if scope == SelectionScope::Message {
            self.span_boundary(selection, forward)
        } else if forward
            && let Some(key @ FoldKey::Block { .. }) = current
            && self.folds.mode(key) == crate::fold::FoldMode::Summary
        {
            self.folds
                .group(conversation.history(), key, diagnostics)
                .last()
                .and_then(|&member| self.fold_key_selection(member))
                .unwrap_or(selection)
        } else {
            selection
        };
        loop {
            let next = match (scope, forward) {
                (SelectionScope::Message, true) => {
                    conversation.next_entry_selection(from, diagnostics)
                }
                (SelectionScope::Message, false) => {
                    conversation.previous_entry_selection(from, diagnostics)
                }
                (SelectionScope::Block, true) => {
                    conversation.next_block_selection(from, diagnostics)
                }
                (SelectionScope::Block, false) => {
                    conversation.previous_block_selection(from, diagnostics)
                }
            };
            if next == from {
                return (selection, scope);
            }
            if self.selection_fold_key(next, scope) != current {
                return self.span_representative(next, scope);
            }
            // Advance past hidden members of the current disclosure, not to
            // its representative again. Block navigation stays entry-local.
            from = next;
        }
    }

    pub(super) fn user_entry_selection(
        &self,
        selection: Selection,
        forward: bool,
        diagnostics: bool,
    ) -> Option<Selection> {
        let current = self
            .span_representative(selection, SelectionScope::Message)
            .0;
        let mut from = self.span_boundary(selection, forward);
        loop {
            let target = if forward {
                self.model.conversation.next_user_entry(from, diagnostics)?
            } else {
                self.model
                    .conversation
                    .previous_user_entry(from, diagnostics)?
            };
            let representative = self.span_representative(target, SelectionScope::Message).0;
            if representative != current {
                return Some(representative);
            }
            from = self.span_boundary(target, forward);
        }
    }

    pub(super) fn handle_fold_action(&mut self, action: Action) -> bool {
        match action {
            Action::UnfoldAll => self.apply_fold(|folds, _, _| folds.unfold_all()),
            Action::FoldTurns => self.fold_transcript(TurnFold::AllFinals),
            Action::FoldOlder => self.fold_transcript(TurnFold::LastFinal),
            Action::FoldToggle | Action::FoldClose | Action::FoldOpen
                if self.interaction.is_selecting() =>
            {
                if let Some(key) = self
                    .interaction
                    .active_selection()
                    .and_then(|active| self.selection_fold_key(active.selection, active.scope))
                {
                    self.apply_fold(|folds, model, diagnostics| match action {
                        Action::FoldToggle if !folds.contains(key) => folds.fold(key),
                        Action::FoldClose => folds.fold(key),
                        Action::FoldToggle | Action::FoldOpen => {
                            folds.open(model.conversation.history(), key, diagnostics)
                        }
                        _ => unreachable!(),
                    });
                }
            }
            _ => return false,
        }
        true
    }

    fn fold_transcript(&mut self, action: TurnFold) {
        self.apply_fold(|folds, model, diagnostics| {
            folds.fold_turns(&model.conversation, diagnostics, action);
            folds.fold_stream(model.session.stream_snapshot());
        });
    }

    pub(super) fn apply_fold(&mut self, f: impl FnOnce(&mut FoldState, &SessionModel, bool)) {
        self.view.capture_conversation_anchor();
        f(
            &mut self.folds,
            &self.model,
            self.pane.diagnostics_visible(),
        );
        self.view.invalidate_rendered_geometry();
        let changed_scope = self.reconcile_fold_selection();
        if let Some(selection) = self.interaction.selection_state_mut()
            // Keep the viewport on an in-place message preview; a span can
            // move the selection to an entirely different history entry.
            && (!changed_scope || self.folds.span_containing(selection.selection().history_index).is_some())
        {
            selection.request_reveal();
        }
    }

    /// Snap to the rendered disclosure. Returns whether Block scope was dropped.
    pub(super) fn reconcile_fold_selection(&mut self) -> bool {
        let Some(active) = self.interaction.active_selection() else {
            return false;
        };
        let (target, scope) = self.span_representative(active.selection, active.scope);
        let changed_scope = scope != active.scope;
        if let Some(state) = self.interaction.selection_state_mut() {
            if changed_scope {
                state.enter_message_scope();
            }
            if target != active.selection {
                state.set_selection(target);
            }
        }
        changed_scope
    }

    pub(super) fn handle_yank(&mut self, selection: Selection, now: Instant) -> Option<UiAction> {
        let scope = self.interaction.selection_scope()?;
        let conversation = &self.model.conversation;
        if let Some(key) = self.selection_fold_key(selection, scope) {
            let diagnostics = self.pane.diagnostics_visible();
            let group = self.folds.group(conversation.history(), key, diagnostics);
            if scope == SelectionScope::Message || group.len() > 1 {
                self.interaction.clear_yank();
                let mut texts = Vec::new();
                for member in group {
                    match member {
                        FoldKey::Span { start, end } => {
                            texts.extend((start..=end).filter_map(|row| {
                                conversation.selected_message_text(row, diagnostics)
                            }))
                        }
                        FoldKey::Message { history_index } => texts
                            .extend(conversation.selected_message_text(history_index, diagnostics)),
                        _ => texts.extend(
                            self.fold_key_selection(member)
                                .and_then(|target| conversation.selected_plain_text(target)),
                        ),
                    }
                }
                return (!texts.is_empty()).then(|| UiAction::Copy {
                    text: texts.join("\n\n"),
                });
            }
        }
        if !self.model.conversation.selection_is_tool(selection) {
            self.interaction.clear_yank();
            return self
                .model
                .conversation
                .selected_plain_text(selection)
                .map(|text| UiAction::Copy { text });
        }
        let double = self
            .interaction
            .selection_state_mut()
            .is_some_and(|state| state.press_yank(now));
        if double {
            self.model
                .conversation
                .selected_tool_output(selection)
                .map(|text| UiAction::Copy { text })
        } else {
            self.model
                .conversation
                .selected_plain_text(selection)
                .map(|text| UiAction::Copy { text })
        }
    }

    pub(super) fn recall_target(&self) -> Option<Selection> {
        let active = self.interaction.active_selection()?;
        match active.scope {
            SelectionScope::Message => self
                .model
                .conversation
                .message_recall_selection(active.selection.history_index),
            SelectionScope::Block => Some(active.selection),
        }
    }

    pub(crate) fn can_recall_selected(&self) -> bool {
        self.capabilities().edit_transcript.is_ok()
            && !self.model.conversation.has_executing_tool_calls()
            && self.model.workflow.dialog().is_none()
            && self.recall_target().is_some_and(|selection| {
                self.model
                    .conversation
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
        let Some((text, target)) = self.model.conversation.recallable_selection(selection) else {
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
