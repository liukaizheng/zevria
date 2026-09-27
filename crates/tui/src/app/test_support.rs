//! Test fixture accessors; production state stays private.
use super::*;

impl App {
    pub(crate) fn has_executing_tool_calls(&self) -> bool {
        self.conversation.has_executing_tool_calls()
    }

    pub(crate) fn next_mode(&self) -> SessionMode {
        self.session.next_mode()
    }

    pub(crate) fn in_flight_mode(&self) -> Option<SessionMode> {
        self.session.in_flight_mode()
    }

    pub(crate) fn in_flight_role(&self) -> Option<ModelRole> {
        self.session.in_flight_role()
    }

    pub(crate) fn status_for_test(&mut self) -> StatusBarView {
        self.render_parts().status
    }

    pub(crate) fn is_compacting(&self) -> bool {
        self.session.is_compacting()
    }

    pub(crate) fn input(&self) -> &str {
        self.composer.text()
    }

    pub(crate) fn input_cursor(&self) -> usize {
        self.composer.cursor()
    }

    pub(crate) fn selection(&self) -> Option<Selection> {
        self.interaction.selection()
    }

    pub(crate) fn selection_scope(&self) -> Option<SelectionScope> {
        self.interaction.selection_scope()
    }

    pub(crate) fn view_scroll(&self) -> usize {
        self.view.scroll()
    }

    pub(crate) fn view_follow(&self) -> bool {
        self.view.follow()
    }

    pub(crate) fn history(&self) -> &[HistoryEntry] {
        self.conversation.history()
    }

    pub(crate) fn is_recalling(&self) -> bool {
        self.edit.is_recalling()
    }

    #[cfg(test)]
    pub(crate) fn recalled_edit_target(&self) -> Option<&zevria_session_api::TranscriptEditTarget> {
        self.edit.recalling().map(|edit| &edit.target)
    }

    pub(crate) fn is_awaiting_edit_acceptance(&self) -> bool {
        self.edit.is_awaiting_acceptance()
    }
}

impl App {
    #[cfg(test)]
    pub(crate) fn plan_state(&self) -> zevria_workflow::PlanWorkflowState {
        self.workflow.snapshot().clone()
    }

    #[cfg(test)]
    pub(crate) fn pending_g(&self) -> bool {
        self.interaction.pending_g()
    }

    #[cfg(test)]
    pub(crate) fn plan_choice(&self) -> PlanChoice {
        self.workflow
            .dialog()
            .map(PlanDialogState::choice)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn plan_recovery_open(&self) -> bool {
        self.workflow
            .dialog()
            .is_some_and(PlanDialogState::recovering)
    }

    #[cfg(test)]
    pub(crate) fn command_menu_selection(&self) -> usize {
        self.composer.menu().selected()
    }

    #[cfg(test)]
    pub(crate) fn command_match_position(&self, name: &str) -> Option<usize> {
        self.composer
            .registry()
            .matches(self.composer.text(), self.composer.cursor())
            .iter()
            .position(|entry| entry.name() == name)
    }

    #[cfg(test)]
    pub(crate) fn streaming(&self) -> Option<Message> {
        match self.session.activity() {
            SessionActivity::Active(session::ActiveOperation {
                phase: ActivePhase::Running(TurnTail::Streaming(message)),
                ..
            }) => Some(message.clone()),
            SessionActivity::Idle | SessionActivity::Pending(_) | SessionActivity::Active(_) => {
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn retry_notice(&self) -> Option<RetryNotice> {
        match self.session.activity() {
            SessionActivity::Active(session::ActiveOperation {
                phase: ActivePhase::Running(TurnTail::Retrying(notice)),
                ..
            }) => Some(notice.clone()),
            SessionActivity::Idle | SessionActivity::Pending(_) | SessionActivity::Active(_) => {
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn inspect_only(&self) -> bool {
        !self.pane.can_compose()
    }

    #[cfg(test)]
    pub(crate) fn subsession_title(&self) -> Option<&str> {
        self.pane.title()
    }

    #[cfg(test)]
    pub(crate) fn seed_history_entry(&mut self, entry: HistoryEntry) {
        self.view.invalidate_rendered_geometry();
        self.conversation.seed_entry(entry);
    }

    #[cfg(test)]
    pub(crate) fn set_input_for_test(&mut self, input: impl Into<String>, cursor: usize) {
        self.view.invalidate_rendered_geometry();
        self.composer.set_for_test(input, cursor);
    }

    #[cfg(test)]
    pub(crate) fn set_menu_selection_for_test(&mut self, selected: usize) {
        self.composer.set_menu_selection_for_test(selected);
    }

    #[cfg(test)]
    pub(crate) fn set_composer_raw_for_test(
        &mut self,
        input: impl Into<String>,
        cursor: usize,
        preferred_column: Option<u16>,
        menu_selection: usize,
    ) {
        self.composer
            .set_raw_for_test(input, cursor, preferred_column, menu_selection);
    }

    #[cfg(test)]
    pub(crate) fn set_focus_for_test(&mut self, focus: FocusState) {
        self.interaction.set_focus(focus);
    }

    #[cfg(test)]
    pub(crate) fn folds(&self) -> &FoldState {
        &self.folds
    }

    #[cfg(test)]
    pub(crate) fn interaction(&self) -> &InteractionState {
        &self.interaction
    }

    #[cfg(test)]
    pub(crate) fn begin_operation_for_test(
        &mut self,
        kind: OperationKind,
        mode: SessionMode,
    ) -> bool {
        self.session
            .begin_operation(kind, mode, role_for_mode(mode))
    }

    #[cfg(test)]
    pub(crate) fn open_plan_recovery_for_test(&mut self) -> bool {
        self.workflow.open_review()
    }

    #[cfg(test)]
    pub(crate) fn select_for_test(&mut self, selection: Option<Selection>) {
        self.select_message_for_test(selection);
        if let Some(state) = self.interaction.selection_state_mut() {
            state.enter_block_scope();
            state.request_reveal();
        }
    }

    #[cfg(test)]
    pub(crate) fn select_message_for_test(&mut self, selection: Option<Selection>) {
        match selection {
            Some(selection) => self.interaction.enter_selection(selection),
            None => self.interaction.clear_selection(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_view_for_test(&mut self, scroll: usize, follow: bool) {
        self.view.set_scroll_for_test(scroll, follow);
    }

    #[cfg(test)]
    pub(crate) fn detach_follow_for_test(&mut self) {
        let scroll = self.view.scroll();
        self.view.set_scroll_for_test(scroll, false);
    }

    #[cfg(test)]
    pub(crate) fn view_cache(&self) -> &crate::layout::ConversationCache {
        self.view.conversation_cache()
    }

    #[cfg(test)]
    pub(crate) fn rendered_selection_window(&self) -> Option<crate::viewport::RowRange> {
        self.view.rendered_selection_window()
    }

    #[cfg(test)]
    pub(crate) fn composer_preferred_column(&self) -> Option<u16> {
        self.composer.preferred_column()
    }

    #[cfg(test)]
    pub(crate) fn composer_scroll(&self) -> usize {
        self.view.composer_scroll()
    }

    #[cfg(test)]
    pub(crate) fn set_composer_scroll_for_test(&mut self, scroll: usize) {
        self.view.set_composer_scroll(scroll);
    }

    #[cfg(test)]
    pub(crate) fn composer_width(&self) -> u16 {
        self.view.composer_width()
    }

    #[cfg(test)]
    pub(crate) fn set_mode_for_test(&mut self, mode: SessionMode) {
        self.session.set_next_mode(mode);
    }
}
