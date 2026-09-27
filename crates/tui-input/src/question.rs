//! Global modal for a blocking `question` tool call.
//!
//! Each prompt owns a wrapped-row viewport. Rendering measures the same styled
//! segments and width it draws, then chooses pinned prompt/validation regions
//! or a whole-body fallback when the terminal cannot leave one answer row.

use std::cell::Cell;

use ratatui::{
    Frame,
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyModifiers},
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use zevria_foundation::QuestionAnswer;
use zevria_foundation::QuestionAnswerValue;
use zevria_foundation::QuestionPrompt;
use zevria_foundation::QuestionPromptKind;
use zevria_foundation::QuestionRequest;
use zevria_foundation::QuestionRequestId;
use zevria_foundation::QuestionResponse;
use zevria_foundation::TurnId;

use crate::chrome::{ScreenRows, paint_selection, selection_style, style_selected_line};
use crate::composer::{
    clamp_cursor, next_boundary, next_word_start, previous_boundary, previous_word_start,
};
#[cfg(test)]
use crate::text::display_width;
use crate::theme::theme;
use crate::viewport::{RowRange, Viewport, render_scrollbar, rows_to_u16};
use crate::{
    action::Action,
    hints::{Eligibility, hint_line},
    keymap::{ChordState, KeyContext},
};
use zevria_tui_widgets::overlay::{modal, modal_block};

#[derive(Debug, PartialEq, Eq)]
pub enum QuestionDialogAction {
    Handled,
    Respond {
        request_id: QuestionRequestId,
        response: QuestionResponse,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuestionLayoutMode {
    Pinned,
    WholeBody,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QuestionLayoutKey {
    inner_width: u16,
    popup_height: u16,
    mode: QuestionLayoutMode,
    prompt_rows: usize,
    content_rows: usize,
    visible_rows: usize,
    validation_rows: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorMeasurementKey {
    revision: u64,
    caret: usize,
    width: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorMeasurement {
    key: EditorMeasurementKey,
    rows: usize,
    caret_row: usize,
}

#[derive(Default)]
struct PromptState {
    cursor: usize,
    answer: Option<Option<QuestionAnswerValue>>,
    input: String,
    /// Byte offset at an extended-grapheme boundary, independent of the selected row.
    caret: usize,
    input_revision: u64,
    // Rendering may update measurements, never the draft or its caret.
    editor_measurement: Cell<Option<EditorMeasurement>>,
    editing_other: bool,
    other_selected: bool,
    multi_selected: Vec<bool>,
    viewport: Viewport,
    reveal_requested: bool,
    last_layout: Option<QuestionLayoutKey>,
}

impl PromptState {
    fn new(prompt: &QuestionPrompt) -> Self {
        let mut state = Self {
            multi_selected: vec![false; prompt.options.len()],
            ..Self::default()
        };
        match (&prompt.kind, &prompt.default) {
            (QuestionPromptKind::Text { .. }, Some(QuestionAnswerValue::String(value))) => {
                state.input.clone_from(value);
            }
            (
                QuestionPromptKind::SingleSelect { allow_other },
                Some(QuestionAnswerValue::String(value)),
            ) => {
                if let Some(index) = prompt
                    .options
                    .iter()
                    .position(|option| option.label == *value)
                {
                    state.cursor = index;
                } else if *allow_other && !value.trim().is_empty() {
                    state.cursor = prompt.options.len();
                    state.input.clone_from(value);
                }
            }
            (
                QuestionPromptKind::MultiSelect { allow_other, .. },
                Some(QuestionAnswerValue::Strings(values)),
            ) => {
                for value in values {
                    if let Some(index) = prompt
                        .options
                        .iter()
                        .position(|option| option.label == *value)
                    {
                        state.multi_selected[index] = true;
                    } else if *allow_other && !state.other_selected && !value.trim().is_empty() {
                        state.input.clone_from(value);
                        state.other_selected = true;
                    }
                }
            }
            _ => {}
        }
        state.caret = state.input.len();
        state.reveal_requested = true;
        state
    }

    /// `None` leaves the key to the dialog; `Some(changed)` consumes even boundary no-ops.
    fn handle_editor_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<bool> {
        let caret = clamp_cursor(&self.input, self.caret);
        let mut text_changed = false;
        let next = match code {
            KeyCode::Left if modifiers.contains(KeyModifiers::CONTROL) => {
                previous_word_start(&self.input, caret)
            }
            KeyCode::Right if modifiers.contains(KeyModifiers::CONTROL) => {
                next_word_start(&self.input, caret)
            }
            KeyCode::Left => previous_boundary(&self.input, caret),
            KeyCode::Right => next_boundary(&self.input, caret),
            KeyCode::Home => 0,
            KeyCode::End => self.input.len(),
            KeyCode::Backspace => {
                let previous = previous_boundary(&self.input, caret);
                self.input.replace_range(previous..caret, "");
                text_changed = previous != caret;
                previous
            }
            KeyCode::Delete => {
                let next = next_boundary(&self.input, caret);
                self.input.replace_range(caret..next, "");
                text_changed = next != caret;
                caret
            }
            KeyCode::Char(character)
                if !character.is_control()
                    && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert(caret, character);
                text_changed = true;
                caret + character.len_utf8()
            }
            _ => return None,
        };
        // Edits can join neighboring graphemes (e.g. inserting a ZWJ). Normalize
        // after mutation too, using the composer's preceding-boundary rule.
        let next = clamp_cursor(&self.input, next);
        let changed = text_changed || self.caret != next;
        self.caret = next;
        if text_changed {
            self.input_revision = self.input_revision.wrapping_add(1);
        }
        if changed {
            self.editor_measurement.set(None);
            self.reveal_requested = true;
        }
        Some(changed)
    }
}

/// Step-by-step state for a native or ACP question batch.
pub struct QuestionDialog {
    turn_id: TurnId,
    request: QuestionRequest,
    question_index: usize,
    states: Vec<PromptState>,
    validation_error: Option<String>,
}

impl QuestionDialog {
    pub fn new(turn_id: TurnId, request: QuestionRequest) -> Self {
        let states = request.questions.iter().map(PromptState::new).collect();
        Self {
            turn_id,
            request,
            question_index: 0,
            states,
            validation_error: None,
        }
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn request_id(&self) -> &QuestionRequestId {
        &self.request.id
    }

    pub fn invalidate_geometry(&mut self) {
        for state in &mut self.states {
            state.viewport.invalidate_geometry();
            state.reveal_requested = true;
        }
    }

    /// Paste is text in this form, never a sequence of activation keys.
    pub fn paste(&mut self, text: &str) {
        if self.state().editing_other
            || (matches!(self.current().kind, QuestionPromptKind::Text { .. })
                && self.state().cursor == 0)
        {
            for character in text.chars().filter(|ch| !ch.is_control()) {
                self.state_mut()
                    .handle_editor_key(KeyCode::Char(character), KeyModifiers::NONE);
            }
        }
    }

    pub fn context(&self) -> KeyContext {
        if self.state().editing_other
            || matches!(self.current().kind, QuestionPromptKind::Text { .. })
        {
            KeyContext::TextEntry
        } else {
            KeyContext::Question
        }
    }

    pub fn hint_eligibility(&self) -> Eligibility {
        let mut hints = Eligibility::default();
        if !self.request.dismissible && !self.state().editing_other {
            hints.disabled.push(Action::Close);
        }
        if !matches!(self.current().kind, QuestionPromptKind::MultiSelect { .. }) {
            hints.disabled.push(Action::Toggle);
        }
        if self.question_index == 0 {
            hints.disabled.push(Action::Back);
        }
        hints.labels.push((
            Action::Confirm,
            if self.state().editing_other {
                "accept"
            } else if matches!(self.current().kind, QuestionPromptKind::Text { .. })
                && self.state().cursor > 0
            {
                "skip"
            } else {
                "answer"
            },
        ));
        hints.labels.push((
            Action::Close,
            if self.state().editing_other {
                "choices"
            } else {
                "dismiss"
            },
        ));
        hints
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> QuestionDialogAction {
        let action = ChordState::default().resolve(
            self.context(),
            ratatui::crossterm::event::KeyEvent::new(code, modifiers),
        );
        if let Some(
            action
            @ (Action::PageUp | Action::PageDown | Action::HalfPageUp | Action::HalfPageDown),
        ) = action
        {
            let page = self.state().viewport.visible_rows().max(1);
            let rows = if matches!(action, Action::HalfPageUp | Action::HalfPageDown) {
                (page / 2).max(1)
            } else {
                page
            };
            let state = self.state_mut();
            if matches!(action, Action::PageUp | Action::HalfPageUp) {
                state.viewport.page_up(rows);
            } else {
                state.viewport.page_down(rows);
            }
            state.reveal_requested = false;
            return QuestionDialogAction::Handled;
        }
        let code = match action {
            Some(Action::Confirm) => KeyCode::Enter,
            Some(Action::Close) => KeyCode::Esc,
            Some(Action::Up) => KeyCode::Up,
            Some(Action::Down) => KeyCode::Down,
            Some(Action::Home) => KeyCode::Home,
            Some(Action::End) => KeyCode::End,
            _ => code,
        };
        // SHIFT contributes to a reported character, not to activation keys.
        // Standalone users of this reusable form get the same policy as the TUI.
        let modifiers = if modifiers == KeyModifiers::SHIFT && matches!(code, KeyCode::Char(_)) {
            KeyModifiers::NONE
        } else {
            modifiers
        };
        if (code, modifiers) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
            return self.respond(QuestionResponse::Dismissed);
        }
        if !modifiers.is_empty() {
            if modifiers == KeyModifiers::CONTROL
                && matches!(code, KeyCode::Left | KeyCode::Right)
                && (self.state().editing_other
                    || matches!(self.current().kind, QuestionPromptKind::Text { .. }))
            {
                self.validation_error = None;
                self.state_mut().handle_editor_key(code, modifiers);
            }
            return QuestionDialogAction::Handled;
        }
        if matches!(code, KeyCode::PageUp | KeyCode::PageDown) && modifiers.is_empty() {
            let visible_rows = self.state().viewport.visible_rows();
            let page_rows = visible_rows.max(1);
            let state = self.state_mut();
            if code == KeyCode::PageUp {
                state.viewport.page_up(page_rows);
            } else {
                state.viewport.page_down(page_rows);
            }
            state.reveal_requested = false;
            return QuestionDialogAction::Handled;
        }

        self.validation_error = None;
        if self.state().editing_other {
            return self.handle_other_key(code, modifiers);
        }
        let text_back = code == KeyCode::Left
            && modifiers.is_empty()
            && clamp_cursor(&self.state().input, self.state().caret) == 0;
        if matches!(&self.current().kind, QuestionPromptKind::Text { .. })
            && self.state().cursor == 0
            && !text_back
            && self
                .state_mut()
                .handle_editor_key(code, modifiers)
                .is_some()
        {
            return QuestionDialogAction::Handled;
        }
        if matches!(code, KeyCode::Home | KeyCode::End) {
            self.select_boundary(code == KeyCode::End);
            return QuestionDialogAction::Handled;
        }
        if code == KeyCode::Esc {
            return if self.request.dismissible {
                self.respond(QuestionResponse::Dismissed)
            } else {
                QuestionDialogAction::Handled
            };
        }
        if code == KeyCode::Left && self.question_index > 0 {
            self.question_index -= 1;
            self.request_reveal();
            return QuestionDialogAction::Handled;
        }

        match self.current().kind.clone() {
            QuestionPromptKind::Text {
                min_length,
                max_length,
            } => self.handle_text_key(code, modifiers, min_length, max_length),
            QuestionPromptKind::SingleSelect { allow_other } => {
                self.handle_single_key(code, allow_other)
            }
            QuestionPromptKind::MultiSelect {
                min_selections,
                max_selections,
                allow_other,
            } => self.handle_multi_key(code, min_selections, max_selections, allow_other),
        }
    }

    fn handle_text_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        min_length: Option<usize>,
        max_length: Option<usize>,
    ) -> QuestionDialogAction {
        let skip_index = usize::from(!self.current().required);
        match code {
            KeyCode::Down if skip_index > 0 => {
                let next = self.state().cursor.saturating_add(1).min(skip_index);
                self.set_cursor(next);
                QuestionDialogAction::Handled
            }
            KeyCode::Up if skip_index > 0 => {
                let previous = self.state().cursor.saturating_sub(1);
                self.set_cursor(previous);
                QuestionDialogAction::Handled
            }
            KeyCode::Enter if !self.current().required && self.state().cursor == 1 => {
                self.accept_answer(None)
            }
            KeyCode::Enter => {
                let length = self.state().input.chars().count();
                if let Some(minimum) = min_length
                    && length < minimum
                {
                    return self.invalid(format!("Enter at least {minimum} characters."));
                }
                if let Some(maximum) = max_length
                    && length > maximum
                {
                    return self.invalid(format!("Enter no more than {maximum} characters."));
                }
                self.accept_answer(Some(QuestionAnswerValue::String(
                    self.state().input.clone(),
                )))
            }
            KeyCode::Backspace => {
                self.set_cursor(0);
                self.state_mut().handle_editor_key(code, modifiers);
                QuestionDialogAction::Handled
            }
            KeyCode::Char(character)
                if !character.is_control()
                    && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.set_cursor(0);
                self.state_mut().handle_editor_key(code, modifiers);
                QuestionDialogAction::Handled
            }
            _ => QuestionDialogAction::Handled,
        }
    }

    fn handle_single_key(&mut self, code: KeyCode, allow_other: bool) -> QuestionDialogAction {
        let option_count = self.current().options.len();
        let required = self.current().required;
        let row_count = option_count + usize::from(allow_other) + usize::from(!required);
        if row_count == 0 {
            return self.invalid("No choices are available for this question.".to_string());
        }
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                let next = self
                    .state()
                    .cursor
                    .saturating_add(1)
                    .min(row_count.saturating_sub(1));
                self.set_cursor(next);
                QuestionDialogAction::Handled
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let previous = self.state().cursor.saturating_sub(1);
                self.set_cursor(previous);
                QuestionDialogAction::Handled
            }
            KeyCode::Enter if self.state().cursor < option_count => {
                let answer = self.current().options[self.state().cursor].label.clone();
                self.accept_answer(Some(QuestionAnswerValue::String(answer)))
            }
            KeyCode::Enter if allow_other && self.state().cursor == option_count => {
                self.state_mut().editing_other = true;
                self.request_reveal();
                QuestionDialogAction::Handled
            }
            KeyCode::Enter if !required => self.accept_answer(None),
            KeyCode::Enter => self.invalid("Select a choice before continuing.".to_string()),
            _ => QuestionDialogAction::Handled,
        }
    }

    fn handle_multi_key(
        &mut self,
        code: KeyCode,
        min_selections: Option<usize>,
        max_selections: Option<usize>,
        allow_other: bool,
    ) -> QuestionDialogAction {
        let option_count = self.current().options.len();
        let other_index = allow_other.then_some(option_count);
        let skip_index =
            (!self.current().required).then_some(option_count + usize::from(allow_other));
        let row_count = option_count + usize::from(allow_other) + usize::from(skip_index.is_some());
        if row_count == 0 {
            return self.invalid("No choices are available for this question.".to_string());
        }
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                let next = self
                    .state()
                    .cursor
                    .saturating_add(1)
                    .min(row_count.saturating_sub(1));
                self.set_cursor(next);
                QuestionDialogAction::Handled
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let previous = self.state().cursor.saturating_sub(1);
                self.set_cursor(previous);
                QuestionDialogAction::Handled
            }
            KeyCode::Char(' ') if self.state().cursor < option_count => {
                let cursor = self.state().cursor;
                self.state_mut().multi_selected[cursor] = !self.state().multi_selected[cursor];
                self.request_reveal();
                QuestionDialogAction::Handled
            }
            KeyCode::Char(' ') if other_index == Some(self.state().cursor) => {
                if self.state().other_selected {
                    self.state_mut().other_selected = false;
                } else {
                    self.state_mut().editing_other = true;
                }
                self.request_reveal();
                QuestionDialogAction::Handled
            }
            KeyCode::Enter if skip_index == Some(self.state().cursor) => self.accept_answer(None),
            KeyCode::Enter
                if other_index == Some(self.state().cursor)
                    && (!self.state().other_selected || self.state().input.trim().is_empty()) =>
            {
                self.state_mut().editing_other = true;
                self.request_reveal();
                QuestionDialogAction::Handled
            }
            KeyCode::Enter => self.accept_multi(min_selections, max_selections),
            _ => QuestionDialogAction::Handled,
        }
    }

    fn handle_other_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> QuestionDialogAction {
        if self
            .state_mut()
            .handle_editor_key(code, modifiers)
            .is_some()
        {
            return QuestionDialogAction::Handled;
        }
        match code {
            KeyCode::Esc => {
                self.state_mut().editing_other = false;
                self.request_reveal();
                QuestionDialogAction::Handled
            }
            KeyCode::Enter if !self.state().input.trim().is_empty() => {
                self.state_mut().editing_other = false;
                self.state_mut().other_selected = true;
                self.request_reveal();
                match self.current().kind.clone() {
                    QuestionPromptKind::SingleSelect { .. } => self.accept_answer(Some(
                        QuestionAnswerValue::String(self.state().input.trim().to_string()),
                    )),
                    QuestionPromptKind::MultiSelect { .. } => QuestionDialogAction::Handled,
                    QuestionPromptKind::Text { .. } => QuestionDialogAction::Handled,
                }
            }
            _ => QuestionDialogAction::Handled,
        }
    }

    fn accept_multi(
        &mut self,
        min_selections: Option<usize>,
        max_selections: Option<usize>,
    ) -> QuestionDialogAction {
        let mut answers = self
            .current()
            .options
            .iter()
            .zip(&self.state().multi_selected)
            .filter_map(|(option, selected)| selected.then_some(option.label.clone()))
            .collect::<Vec<_>>();
        if self.state().other_selected {
            answers.push(self.state().input.trim().to_string());
        }
        if let Some(minimum) = min_selections
            && answers.len() < minimum
        {
            return self.invalid(format!("Select at least {minimum} choices."));
        }
        if let Some(maximum) = max_selections
            && answers.len() > maximum
        {
            return self.invalid(format!("Select no more than {maximum} choices."));
        }
        self.accept_answer(Some(QuestionAnswerValue::Strings(answers)))
    }

    fn invalid(&mut self, message: String) -> QuestionDialogAction {
        self.validation_error = Some(message);
        self.request_reveal();
        QuestionDialogAction::Handled
    }

    fn accept_answer(&mut self, answer: Option<QuestionAnswerValue>) -> QuestionDialogAction {
        self.state_mut().answer = Some(answer);
        if self.question_index + 1 == self.request.questions.len() {
            let answers = self
                .request
                .questions
                .iter()
                .zip(&self.states)
                .map(|(question, state)| QuestionAnswer {
                    id: question.id.clone(),
                    answer: state
                        .answer
                        .clone()
                        .expect("every preceding question was answered before advancing"),
                })
                .collect();
            return self.respond(QuestionResponse::Answered { answers });
        }

        self.question_index += 1;
        self.request_reveal();
        QuestionDialogAction::Handled
    }

    fn respond(&self, response: QuestionResponse) -> QuestionDialogAction {
        QuestionDialogAction::Respond {
            request_id: self.request.id.clone(),
            response,
        }
    }

    fn selectable_row_count(&self) -> usize {
        let current = self.current();
        match &current.kind {
            QuestionPromptKind::Text { .. } => 1 + usize::from(!current.required),
            QuestionPromptKind::SingleSelect { allow_other } => {
                current.options.len() + usize::from(*allow_other) + usize::from(!current.required)
            }
            QuestionPromptKind::MultiSelect { allow_other, .. } => {
                current.options.len() + usize::from(*allow_other) + usize::from(!current.required)
            }
        }
    }

    fn select_boundary(&mut self, end: bool) {
        let row_count = self.selectable_row_count();
        if row_count == 0 {
            return;
        }
        let cursor = if end { row_count - 1 } else { 0 };
        let state = self.state_mut();
        state.editing_other = false;
        state.cursor = cursor;
        state.reveal_requested = true;
    }

    fn set_cursor(&mut self, cursor: usize) {
        if self.state().cursor != cursor {
            self.state_mut().cursor = cursor;
            self.request_reveal();
        }
    }

    fn request_reveal(&mut self) {
        self.state_mut().reveal_requested = true;
    }

    fn current(&self) -> &zevria_foundation::QuestionPrompt {
        &self.request.questions[self.question_index]
    }

    fn state(&self) -> &PromptState {
        &self.states[self.question_index]
    }

    fn state_mut(&mut self) -> &mut PromptState {
        &mut self.states[self.question_index]
    }

    pub fn render(&mut self, frame: &mut Frame, modal_body: Rect) {
        let popup_width = question_width(modal_body);
        let sizing_block = modal_block("", Line::default());
        let inner_width = sizing_block.inner(Rect::new(0, 0, popup_width, 2)).width;
        let content = QuestionContent::new(
            self.current(),
            self.state(),
            self.validation_error.as_deref(),
            inner_width,
        );
        let natural_height = content
            .prompt_rows
            .saturating_add(content.answer_rows)
            .saturating_add(content.validation_rows)
            .saturating_add(2);
        let popup_area = centered_area(modal_body, popup_width, natural_height);
        let progress = self.title(popup_area.width);
        let footer = hint_line(
            self.context(),
            &self.hint_eligibility(),
            usize::from(popup_area.width.saturating_sub(2)),
        );
        let inner_area = modal(frame, popup_area, progress, footer);
        let pinned_answer_rows = usize::from(inner_area.height)
            .saturating_sub(content.prompt_rows)
            .saturating_sub(content.validation_rows);
        let mode = if pinned_answer_rows > 0 {
            QuestionLayoutMode::Pinned
        } else {
            QuestionLayoutMode::WholeBody
        };
        let selected = self.state().cursor;
        let question_index = self.question_index;

        match mode {
            QuestionLayoutMode::Pinned => {
                let prompt_height = rows_to_u16(content.prompt_rows);
                let validation_height = rows_to_u16(content.validation_rows);
                let prompt_area = Rect {
                    x: inner_area.x,
                    y: inner_area.y,
                    width: inner_area.width,
                    height: prompt_height,
                };
                let answer_area = Rect {
                    x: inner_area.x,
                    y: inner_area.y.saturating_add(prompt_height),
                    width: inner_area.width,
                    height: rows_to_u16(pinned_answer_rows),
                };
                let validation_area = Rect {
                    x: inner_area.x,
                    y: answer_area.y.saturating_add(answer_area.height),
                    width: inner_area.width,
                    height: validation_height,
                };
                let target = content.focus_target(selected);
                let key = QuestionLayoutKey {
                    inner_width,
                    popup_height: popup_area.height,
                    mode,
                    prompt_rows: content.prompt_rows,
                    content_rows: content.answer_rows,
                    visible_rows: pinned_answer_rows,
                    validation_rows: content.validation_rows,
                };
                let state = &mut self.states[question_index];
                reconcile_prompt_viewport(
                    state,
                    key,
                    content.answer_rows,
                    pinned_answer_rows,
                    target,
                );

                frame.render_widget(
                    Paragraph::new(content.prompt_lines).wrap(Wrap { trim: false }),
                    prompt_area,
                );
                frame.render_widget(
                    Paragraph::new(content.answer_lines)
                        .wrap(Wrap { trim: false })
                        .scroll((state.viewport.paragraph_offset(), 0)),
                    answer_area,
                );
                if validation_height > 0 {
                    frame.render_widget(
                        Paragraph::new(content.validation_lines).wrap(Wrap { trim: false }),
                        validation_area,
                    );
                }
                paint_focus_selection(frame, answer_area, &state.viewport, target);
                render_scrollbar(frame, popup_area, &state.viewport);
            }
            QuestionLayoutMode::WholeBody => {
                let body = content.whole_body(selected);
                let visible_rows = usize::from(inner_area.height);
                let key = QuestionLayoutKey {
                    inner_width,
                    popup_height: popup_area.height,
                    mode,
                    prompt_rows: content.prompt_rows,
                    content_rows: body.rows,
                    visible_rows,
                    validation_rows: content.validation_rows,
                };
                let state = &mut self.states[question_index];
                reconcile_prompt_viewport(state, key, body.rows, visible_rows, body.focus_target);
                let focus_target = body.focus_target;
                frame.render_widget(
                    Paragraph::new(body.lines)
                        .wrap(Wrap { trim: false })
                        .scroll((state.viewport.paragraph_offset(), 0)),
                    inner_area,
                );
                paint_focus_selection(frame, inner_area, &state.viewport, focus_target);
                render_scrollbar(frame, popup_area, &state.viewport);
            }
        }
    }

    fn title(&self, popup_width: u16) -> String {
        let current = self.current();
        let progress = self.request.source_label.as_deref().map_or_else(
            || {
                format!(
                    "{} · {}/{}",
                    current.header,
                    self.question_index + 1,
                    self.request.questions.len()
                )
            },
            |label| {
                format!(
                    "{label} · {} · {}/{}",
                    current.header,
                    self.question_index + 1,
                    self.request.questions.len()
                )
            },
        );
        crate::text::truncate_display_width(
            &format!(" {progress} "),
            usize::from(popup_width.saturating_sub(2)),
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum FocusAnchor {
    Full,
    End,
    /// A wrapped row relative to the full selected segment, not a shortened selection.
    Row(usize),
}

#[derive(Clone, Copy, Debug)]
struct FocusTarget {
    range: RowRange,
    anchor: FocusAnchor,
}

impl FocusTarget {
    const fn full(range: RowRange) -> Self {
        Self {
            range,
            anchor: FocusAnchor::Full,
        }
    }

    const fn end(range: RowRange) -> Self {
        Self {
            range,
            anchor: FocusAnchor::End,
        }
    }

    const fn row(range: RowRange, offset: usize) -> Self {
        Self {
            range,
            anchor: FocusAnchor::Row(offset),
        }
    }

    const fn shifted(self, rows: usize) -> Self {
        Self {
            range: self.range.shifted(rows),
            anchor: self.anchor,
        }
    }
}

struct QuestionContent {
    prompt_lines: Vec<Line<'static>>,
    prompt_rows: usize,
    answer_lines: Vec<Line<'static>>,
    answer_rows: usize,
    focus_targets: Vec<FocusTarget>,
    validation_lines: Vec<Line<'static>>,
    validation_rows: usize,
}

impl QuestionContent {
    fn new(
        prompt: &QuestionPrompt,
        state: &PromptState,
        validation_error: Option<&str>,
        width: u16,
    ) -> Self {
        let prompt_lines = vec![
            Line::styled(
                prompt.question.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::default(),
        ];
        let prompt_rows = measured_rows(&prompt_lines, width);
        let mut answer_lines = Vec::new();
        let mut answer_rows = 0;
        let mut focus_targets = Vec::new();

        match &prompt.kind {
            QuestionPromptKind::Text { .. } => {
                let selected = state.cursor == 0;
                let style = selected_style(selected);
                let (line, measurement) = editor_layout(state, style, width);
                let range = push_measured_segment(
                    &mut answer_lines,
                    &mut answer_rows,
                    vec![line],
                    measurement.rows,
                    selected,
                );
                focus_targets.push(FocusTarget::row(range, measurement.caret_row));
            }
            QuestionPromptKind::SingleSelect { .. } | QuestionPromptKind::MultiSelect { .. } => {
                let multi = matches!(&prompt.kind, QuestionPromptKind::MultiSelect { .. });
                for (index, option) in prompt.options.iter().enumerate() {
                    let selected = state.cursor == index;
                    let style = selected_style(selected);
                    let prefix = if multi {
                        if state.multi_selected.get(index).copied().unwrap_or(false) {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    } else {
                        ""
                    };
                    let range = push_segment(
                        &mut answer_lines,
                        &mut answer_rows,
                        vec![
                            Line::from(Span::styled(format!("{prefix}{}", option.label), style)),
                            Line::from(Span::styled(
                                format!("  {}", option.description),
                                style.fg(theme().text.muted),
                            )),
                        ],
                        width,
                        selected,
                    );
                    focus_targets.push(FocusTarget::full(range));
                }
            }
        }

        let allow_other = match &prompt.kind {
            QuestionPromptKind::SingleSelect { allow_other }
            | QuestionPromptKind::MultiSelect { allow_other, .. } => *allow_other,
            QuestionPromptKind::Text { .. } => false,
        };
        if allow_other {
            let index = focus_targets.len();
            let selected = state.cursor == index;
            let style = selected_style(selected);
            let multi = matches!(&prompt.kind, QuestionPromptKind::MultiSelect { .. });
            let prefix = if multi {
                if state.other_selected { "[x] " } else { "[ ] " }
            } else {
                ""
            };
            let mut lines = vec![Line::from(Span::styled(format!("{prefix}Other"), style))];
            let mut rows = measured_rows(&lines, width);
            let mut caret_row = 0;
            if state.editing_other {
                let (line, measurement) = editor_layout(state, style, width);
                caret_row = rows.saturating_add(measurement.caret_row);
                rows = rows.saturating_add(measurement.rows);
                lines.push(line);
            }
            let range =
                push_measured_segment(&mut answer_lines, &mut answer_rows, lines, rows, selected);
            focus_targets.push(if state.editing_other {
                FocusTarget::row(range, caret_row)
            } else {
                FocusTarget::full(range)
            });
        }
        if !prompt.required {
            let index = focus_targets.len();
            let selected = state.cursor == index;
            let style = selected_style(selected);
            let range = push_segment(
                &mut answer_lines,
                &mut answer_rows,
                vec![Line::from(Span::styled("Skip", style))],
                width,
                selected,
            );
            focus_targets.push(FocusTarget::full(range));
        }

        let validation_lines = validation_error.map_or_else(Vec::new, |error| {
            vec![Line::from(Span::styled(
                error.to_string(),
                crate::chrome::error_style(),
            ))]
        });
        let validation_rows = measured_rows(&validation_lines, width);
        Self {
            prompt_lines,
            prompt_rows,
            answer_lines,
            answer_rows,
            focus_targets,
            validation_lines,
            validation_rows,
        }
    }

    fn focus_target(&self, selected: usize) -> Option<FocusTarget> {
        self.focus_targets
            .get(selected)
            .copied()
            .or_else(|| self.focus_targets.last().copied())
    }

    fn whole_body(&self, selected: usize) -> WholeBody {
        let mut lines = Vec::with_capacity(
            self.prompt_lines
                .len()
                .saturating_add(self.answer_lines.len())
                .saturating_add(self.validation_lines.len()),
        );
        lines.extend(self.prompt_lines.iter().cloned());
        lines.extend(self.answer_lines.iter().cloned());
        lines.extend(self.validation_lines.iter().cloned());
        let answer_shift = self.prompt_rows;
        let validation_start = answer_shift.saturating_add(self.answer_rows);
        let focus_target = if self.validation_rows > 0 {
            Some(FocusTarget::end(RowRange::from_start_len(
                validation_start,
                self.validation_rows,
            )))
        } else {
            self.focus_target(selected)
                .map(|target| target.shifted(answer_shift))
        };
        WholeBody {
            lines,
            rows: validation_start.saturating_add(self.validation_rows),
            focus_target,
        }
    }
}

struct WholeBody {
    lines: Vec<Line<'static>>,
    rows: usize,
    focus_target: Option<FocusTarget>,
}

fn selected_style(selected: bool) -> Style {
    if selected {
        selection_style()
    } else {
        Style::default()
    }
}

fn push_segment(
    destination: &mut Vec<Line<'static>>,
    current_rows: &mut usize,
    segment: Vec<Line<'static>>,
    width: u16,
    selected: bool,
) -> RowRange {
    let rows = measured_rows(&segment, width);
    push_measured_segment(destination, current_rows, segment, rows, selected)
}

fn push_measured_segment(
    destination: &mut Vec<Line<'static>>,
    current_rows: &mut usize,
    mut segment: Vec<Line<'static>>,
    rows: usize,
    selected: bool,
) -> RowRange {
    if selected {
        for line in &mut segment {
            style_selected_line(line);
        }
    }
    let range = RowRange::from_start_len(*current_rows, rows);
    *current_rows = (*current_rows).saturating_add(rows);
    destination.extend(segment);
    range
}

fn paint_focus_selection(
    frame: &mut Frame,
    area: Rect,
    viewport: &Viewport,
    target: Option<FocusTarget>,
) {
    let Some(target) = target else {
        return;
    };
    let visible = viewport.visible_range();
    let start = target.range.start().max(visible.start());
    let end = target.range.end().min(visible.end());
    if start >= end {
        return;
    }
    let screen_start = area
        .y
        .saturating_add(rows_to_u16(start.saturating_sub(visible.start())));
    let screen_end = area
        .y
        .saturating_add(rows_to_u16(end.saturating_sub(visible.start())));
    paint_selection(
        frame.buffer_mut(),
        area,
        ScreenRows::new(screen_start, screen_end),
        selection_style(),
    );
}

fn editor_line(input: &str, caret: usize, style: Style) -> Line<'static> {
    let caret = clamp_cursor(input, caret);
    Line::from(vec![
        Span::styled("  > ", style.fg(theme().roles.tools)),
        Span::styled(input[..caret].to_string(), style),
        Span::styled("▌", style.fg(theme().roles.tools)),
        Span::styled(input[caret..].to_string(), style),
    ])
}

fn editor_layout(
    state: &PromptState,
    style: Style,
    width: u16,
) -> (Line<'static>, EditorMeasurement) {
    let key = EditorMeasurementKey {
        revision: state.input_revision,
        caret: clamp_cursor(&state.input, state.caret),
        width,
    };
    let line = editor_line(&state.input, key.caret, style);
    let measurement = state
        .editor_measurement
        .get()
        .filter(|cached| cached.key == key)
        .unwrap_or_else(|| {
            let rows = measured_rows(std::slice::from_ref(&line), width);
            let measurement = EditorMeasurement {
                key,
                rows,
                caret_row: measured_caret_row(&line, width, rows),
            };
            state.editor_measurement.set(Some(measurement));
            measurement
        });
    (line, measurement)
}

/// Locate the synthetic caret using the same renderer and complete suffix as
/// the visible paragraph. A prefix only seeds the search: later text can move
/// an entire word, including the caret, onto a different wrapped row.
fn measured_caret_row(line: &Line<'static>, width: u16, rows: usize) -> usize {
    if width == 0 || rows == 0 {
        return 0;
    }
    let mut tagged = line.clone();
    tagged.style = Style::default();
    for span in &mut tagged.spans {
        span.style = Style::default();
    }
    // The tag exists only in scratch rendering, so literal ▌ in user text and
    // theme changes cannot be mistaken for the caret or alter visible styling.
    const TAG: Modifier = Modifier::RAPID_BLINK;
    tagged.spans[2].style = Style::default().add_modifier(TAG);
    let mut prefix = tagged.clone();
    prefix.spans.truncate(3);
    let estimate = Paragraph::new(prefix)
        .wrap(Wrap { trim: false })
        .line_count(width)
        .saturating_sub(1);
    let paragraph = Paragraph::new(tagged).wrap(Wrap { trim: false });

    // Bound both scratch height and cell count, regardless of draft length.
    // Paragraph scroll offsets are u16; rows beyond that retain the existing
    // saturating viewport behavior rather than requiring a document-sized buffer.
    let row_limit = rows.min(usize::from(u16::MAX) + 1);
    // Ratatui can overdraw a wide grapheme at a narrow wrap boundary. Match
    // the popup's two right-hand padding/border cells without widening the
    // paragraph itself; a buffer exactly as wide as the wrap area can panic.
    let scratch_width = width.saturating_add(2);
    let probe_rows = (16_384 / usize::from(scratch_width))
        .clamp(1, 32)
        .min(row_limit);
    let mut scratch = Buffer::empty(Rect::new(0, 0, scratch_width, rows_to_u16(probe_rows)));
    let mut probe = |start: usize, end: usize| {
        scratch.reset();
        paragraph.clone().scroll((rows_to_u16(start), 0)).render(
            Rect::new(0, 0, width, rows_to_u16(end - start)),
            &mut scratch,
        );
        scratch
            .content()
            .iter()
            .position(|cell| cell.modifier.contains(TAG))
            .map(|index| start + index / usize::from(scratch_width))
    };
    let mut low = estimate
        .saturating_sub(probe_rows / 2)
        .min(row_limit - probe_rows);
    let mut high = low + probe_rows;
    if let Some(row) = probe(low, high) {
        return row;
    }
    // Expand in bounded bands until all addressable rows have been checked.
    while low > 0 || high < row_limit {
        if low > 0 {
            let start = low.saturating_sub(probe_rows);
            if let Some(row) = probe(start, low) {
                return row;
            }
            low = start;
        }
        if high < row_limit {
            let end = high.saturating_add(probe_rows).min(row_limit);
            if let Some(row) = probe(high, end) {
                return row;
            }
            high = end;
        }
    }
    row_limit.saturating_sub(1)
}

fn measured_rows(lines: &[Line<'static>], width: u16) -> usize {
    if lines.is_empty() {
        0
    } else if width == 0 {
        lines.len()
    } else {
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width)
    }
}

fn reconcile_prompt_viewport(
    state: &mut PromptState,
    key: QuestionLayoutKey,
    content_rows: usize,
    visible_rows: usize,
    target: Option<FocusTarget>,
) {
    if state.last_layout != Some(key) {
        state.reveal_requested = true;
        state.last_layout = Some(key);
    }
    state.viewport.reconcile(content_rows, visible_rows);
    if state.reveal_requested {
        if let Some(target) = target {
            match target.anchor {
                FocusAnchor::Full => state.viewport.reveal(target.range),
                FocusAnchor::End => state.viewport.reveal_end(target.range),
                FocusAnchor::Row(offset) => state.viewport.reveal(RowRange::from_start_len(
                    target.range.start().saturating_add(offset),
                    1,
                )),
            }
        }
        state.reveal_requested = false;
    }
}

fn question_width(modal_body: Rect) -> u16 {
    (modal_body.width.saturating_mul(7) / 10)
        .max(30)
        .min(modal_body.width)
}

fn centered_area(modal_body: Rect, width: u16, natural_height: usize) -> Rect {
    let height = rows_to_u16(natural_height.min(usize::from(modal_body.height)));
    Rect {
        x: modal_body.x + modal_body.width.saturating_sub(width) / 2,
        y: modal_body.y + modal_body.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use zevria_foundation::QuestionOption;
    use zevria_foundation::QuestionPrompt;
    use zevria_foundation::QuestionPromptKind;

    fn request() -> QuestionRequest {
        QuestionRequest {
            id: QuestionRequestId::new("request-1"),
            questions: vec![
                QuestionPrompt {
                    id: "scope".to_string(),
                    header: "Scope".to_string(),
                    question: "Which scope?".to_string(),
                    options: vec![
                        QuestionOption {
                            label: "Focused".to_string(),
                            description: "Keep it narrow.".to_string(),
                        },
                        QuestionOption {
                            label: "Broad".to_string(),
                            description: "Include adjacent work.".to_string(),
                        },
                    ],
                    kind: QuestionPromptKind::SingleSelect { allow_other: true },
                    required: true,
                    default: None,
                },
                QuestionPrompt {
                    id: "tests".to_string(),
                    header: "Tests".to_string(),
                    question: "Which tests?".to_string(),
                    options: vec![
                        QuestionOption {
                            label: "Focused".to_string(),
                            description: "Only feature tests.".to_string(),
                        },
                        QuestionOption {
                            label: "Full".to_string(),
                            description: "Run the workspace.".to_string(),
                        },
                    ],
                    kind: QuestionPromptKind::SingleSelect { allow_other: true },
                    required: true,
                    default: None,
                },
            ],
            source_label: None,
            dismissible: true,
        }
    }

    fn render_buffer(dialog: &mut QuestionDialog, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area);
            })
            .expect("render question dialog");
        terminal.backend().buffer().clone()
    }

    fn render_text(dialog: &mut QuestionDialog, width: u16, height: u16) -> String {
        render_buffer(dialog, width, height)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn text_kind() -> QuestionPromptKind {
        QuestionPromptKind::Text {
            min_length: None,
            max_length: None,
        }
    }

    fn editor_kinds() -> [QuestionPromptKind; 3] {
        [
            text_kind(),
            QuestionPromptKind::SingleSelect { allow_other: true },
            QuestionPromptKind::MultiSelect {
                min_selections: None,
                max_selections: None,
                allow_other: true,
            },
        ]
    }

    fn editor_request(kind: QuestionPromptKind, input: &str, required: bool) -> QuestionRequest {
        let mut request = request();
        request.questions.truncate(1);
        let prompt = &mut request.questions[0];
        prompt.question = "Q".into();
        prompt.options.clear();
        prompt.default = Some(if matches!(kind, QuestionPromptKind::MultiSelect { .. }) {
            QuestionAnswerValue::Strings(vec![input.into()])
        } else {
            QuestionAnswerValue::String(input.into())
        });
        prompt.kind = kind;
        prompt.required = required;
        request
    }

    fn editor_dialog(kind: QuestionPromptKind, input: &str, required: bool) -> QuestionDialog {
        let mut dialog =
            QuestionDialog::new(TurnId::new(21), editor_request(kind, input, required));
        if !matches!(dialog.current().kind, QuestionPromptKind::Text { .. }) {
            if dialog.state().other_selected {
                dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
            }
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            assert!(dialog.state().editing_other);
        }
        dialog
    }

    fn type_text(dialog: &mut QuestionDialog, text: &str) {
        for character in text.chars() {
            assert_eq!(
                dialog.handle_key(KeyCode::Char(character), KeyModifiers::NONE),
                QuestionDialogAction::Handled
            );
        }
    }

    fn assert_submitted(dialog: &mut QuestionDialog, answer: Option<QuestionAnswerValue>) {
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: dialog.request.id.clone(),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope".into(),
                        answer,
                    }],
                },
            }
        );
    }

    #[test]
    fn all_editors_insert_and_delete_at_the_caret_and_submit_ordinary_values() {
        for kind in editor_kinds() {
            for required in [true, false] {
                let multi = matches!(kind, QuestionPromptKind::MultiSelect { .. });
                let mut dialog = editor_dialog(kind.clone(), "", required);
                type_text(&mut dialog, "ac");
                dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
                type_text(&mut dialog, "b");
                assert_eq!((&*dialog.state().input, dialog.state().caret), ("abc", 2));
                dialog.handle_key(KeyCode::Right, KeyModifiers::NONE);
                for code in [KeyCode::Right, KeyCode::Delete, KeyCode::End] {
                    dialog.state_mut().reveal_requested = false;
                    dialog.handle_key(code, KeyModifiers::NONE);
                    assert!(!dialog.state().reveal_requested, "boundary {code:?}");
                    assert_eq!((&*dialog.state().input, dialog.state().caret), ("abc", 3));
                }
                dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
                dialog.handle_key(KeyCode::Delete, KeyModifiers::NONE);
                assert_eq!((&*dialog.state().input, dialog.state().caret), ("bc", 0));
                type_text(&mut dialog, "a");
                dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
                dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
                assert_eq!((&*dialog.state().input, dialog.state().caret), ("ab", 2));
                type_text(&mut dialog, "c");
                if multi {
                    assert_eq!(
                        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
                        QuestionDialogAction::Handled
                    );
                    assert!(dialog.state().other_selected);
                    assert!(!dialog.state().editing_other);
                    assert_submitted(
                        &mut dialog,
                        Some(QuestionAnswerValue::Strings(vec!["abc".into()])),
                    );
                } else {
                    assert_submitted(&mut dialog, Some(QuestionAnswerValue::String("abc".into())));
                }
            }
        }
    }

    #[test]
    fn shared_editor_consumes_boundary_noops_separately_from_changes() {
        let mut state = PromptState::default();
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            for code in [
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Backspace,
                KeyCode::Delete,
            ] {
                assert_eq!(state.handle_editor_key(code, modifiers), Some(false));
                assert_eq!(state.caret, 0);
                assert!(!state.reveal_requested);
            }
        }
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::Char('\n'),
        ] {
            assert_eq!(state.handle_editor_key(code, KeyModifiers::NONE), None);
        }
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(state.handle_editor_key(KeyCode::Char('x'), modifiers), None);
        }
        assert_eq!(
            state.handle_editor_key(KeyCode::Char('J'), KeyModifiers::SHIFT),
            Some(true)
        );
        assert_eq!(state.input, "J");
        assert_eq!(state.caret, 1);
        assert!(state.reveal_requested);
    }

    #[test]
    fn editor_word_movement_matches_composer_tokens_and_unicode() {
        let input = "  alpha...beta_gamma  e\u{301}lan\n世界!";
        let alpha = input.find("alpha").unwrap();
        let beta = input.find("beta_gamma").unwrap();
        let elan = input.find("e\u{301}lan").unwrap();
        let world = input.find("世界").unwrap();
        let left = [
            (0, 0),
            (alpha + 3, alpha),
            (alpha + 5, alpha),
            (beta, alpha),
            (beta + 5, beta),
            (elan, beta),
            (world, elan),
            (input.len(), world),
        ];
        let right = [
            (0, alpha),
            (alpha, beta),
            (alpha + 2, beta),
            (alpha + 5, beta),
            (beta, elan),
            (beta + 5, elan),
            (elan, world),
            (world, input.len()),
            (input.len(), input.len()),
        ];
        for kind in editor_kinds() {
            let mut dialog = editor_dialog(kind, input, true);
            for (code, cases) in [
                (KeyCode::Left, left.as_slice()),
                (KeyCode::Right, right.as_slice()),
            ] {
                for &(start, expected) in cases {
                    dialog.state_mut().caret = start;
                    dialog.handle_key(code, KeyModifiers::CONTROL);
                    assert_eq!(dialog.state().caret, expected, "{code:?} at {start}");
                    assert_eq!(dialog.state().input, input);
                    assert_eq!(dialog.question_index, 0);
                }
            }
        }
    }

    #[test]
    fn combining_wide_and_zwj_graphemes_are_single_editing_units() {
        for kind in editor_kinds() {
            let mut dialog = editor_dialog(kind, "e\u{301}界👩‍💻", true);
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
            assert_eq!(dialog.state().caret, "e\u{301}界".len());
            dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
            assert_eq!(dialog.state().input, "e\u{301}👩‍💻");
            dialog.handle_key(KeyCode::Delete, KeyModifiers::NONE);
            assert_eq!(dialog.state().input, "e\u{301}");
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
            assert_eq!(dialog.state().caret, 0);
            dialog.handle_key(KeyCode::Right, KeyModifiers::NONE);
            assert_eq!(dialog.state().caret, "e\u{301}".len());
            dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
            assert!(dialog.state().input.is_empty());
            assert_eq!(dialog.state().caret, 0);
        }
    }

    #[test]
    fn mutations_that_join_graphemes_clamp_to_the_preceding_boundary() {
        use unicode_segmentation::UnicodeSegmentation;
        for (input, caret, key, expected) in [
            ("👩💻", "👩".len(), KeyCode::Char('\u{200d}'), "👩‍💻"),
            ("\u{301}b", 0, KeyCode::Char('a'), "a\u{301}b"),
            ("👩‍x💻", "👩‍".len(), KeyCode::Delete, "👩‍💻"),
            ("👩‍x💻", "👩‍x".len(), KeyCode::Backspace, "👩‍💻"),
            ("🇦X🇧", "🇦".len(), KeyCode::Delete, "🇦🇧"),
            ("\rX\n", 1, KeyCode::Delete, "\r\n"),
        ] {
            let mut state = PromptState {
                input: input.into(),
                caret,
                ..PromptState::default()
            };
            assert_eq!(state.handle_editor_key(key, KeyModifiers::NONE), Some(true));
            assert_eq!(state.input, expected);
            assert_eq!(state.caret, 0, "{key:?} in {input:?}");
            assert!(
                state
                    .input
                    .grapheme_indices(true)
                    .any(|(byte, _)| byte == state.caret)
            );
        }
        // Defensive normalization also precedes slicing, even for invalid UTF-8 offsets.
        let mut state = PromptState {
            input: "界x".into(),
            caret: 1,
            ..PromptState::default()
        };
        state.handle_editor_key(KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!((&*state.input, state.caret), ("x", 0));
    }

    #[test]
    fn defaults_initialize_the_caret_once_for_each_editor_kind() {
        for kind in editor_kinds() {
            let request = editor_request(kind, " custom 界 ", true);
            let state = PromptState::new(&request.questions[0]);
            assert_eq!(state.caret, state.input.len());
            assert_eq!(state.input, " custom 界 ");
            assert!(!state.editing_other);
            assert!(state.answer.is_none());
        }
    }

    #[test]
    fn other_retains_draft_and_caret_on_escape_reopen_and_revisit() {
        for kind in editor_kinds().into_iter().skip(1) {
            let multi = matches!(kind, QuestionPromptKind::MultiSelect { .. });
            let mut dialog = editor_dialog(kind, "abc", true);
            dialog.request.questions.push(request().questions.remove(1));
            dialog
                .states
                .push(PromptState::new(&dialog.request.questions[1]));
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
            dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE);
            assert!(!dialog.state().editing_other);
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            assert!(dialog.state().editing_other);
            assert_eq!((&*dialog.state().input, dialog.state().caret), ("abc", 2));
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            if multi {
                dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            }
            assert_eq!(dialog.question_index, 1);
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
            assert_eq!(dialog.question_index, 0);
            if multi {
                dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
            }
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
            assert!(dialog.state().editing_other);
            assert_eq!((&*dialog.state().input, dialog.state().caret), ("abc", 2));
        }
    }

    #[test]
    fn text_left_only_goes_back_when_already_at_start_and_revisits_keep_carets() {
        let mut request = editor_request(text_kind(), "first", true);
        let mut second = editor_request(text_kind(), "xy", false).questions.remove(0);
        second.id = "second".into();
        request.questions.push(second);
        let mut dialog = QuestionDialog::new(TurnId::new(22), request);
        dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for expected in [1, 0] {
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
            assert_eq!(dialog.question_index, 1);
            assert_eq!(dialog.state().caret, expected);
        }
        dialog.handle_key(KeyCode::Left, KeyModifiers::CONTROL);
        dialog.handle_key(KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(dialog.question_index, 1);
        dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(dialog.question_index, 0);
        assert_eq!((&*dialog.state().input, dialog.state().caret), ("first", 4));
        dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
        dialog.state_mut().reveal_requested = false;
        dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(dialog.question_index, 0);
        assert!(!dialog.state().reveal_requested);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!((&*dialog.state().input, dialog.state().caret), ("xy", 0));
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(dialog.question_index, 0, "Skip's Left still goes back");
        assert_eq!(dialog.states[1].caret, 2);
    }

    #[test]
    fn other_editor_keys_never_leave_editing_or_navigate_questions() {
        for kind in editor_kinds().into_iter().skip(1) {
            let mut dialog = editor_dialog(kind, "", true);
            dialog
                .request
                .questions
                .insert(0, request().questions.remove(0));
            dialog
                .states
                .insert(0, PromptState::new(&dialog.request.questions[0]));
            dialog.question_index = 1;
            for code in [
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Up,
                KeyCode::Down,
            ] {
                for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
                    dialog.handle_key(code, modifiers);
                    assert!(dialog.state().editing_other);
                    assert_eq!(dialog.question_index, 1);
                }
            }
            type_text(&mut dialog, "jk ");
            assert_eq!(dialog.state().input, "jk ");
            assert_eq!(dialog.state().cursor, 0);
            assert!(!dialog.state().other_selected);
        }
    }

    #[test]
    fn skip_row_typing_and_backspace_refocus_even_an_empty_draft() {
        for input in ["", "abc"] {
            let mut dialog = editor_dialog(text_kind(), input, false);
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
            dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
            assert_eq!(dialog.state().cursor, 1);
            assert_eq!(dialog.state().caret, input.len());
            dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
            assert_eq!(dialog.state().cursor, 0);
            assert_eq!(
                dialog.state().caret,
                input.len(),
                "row Home must not move the caret"
            );
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
            dialog.state_mut().reveal_requested = false;
            dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
            assert_eq!(dialog.state().cursor, 0);
            assert!(dialog.state().reveal_requested);
            assert_eq!(
                dialog.state().input,
                if input.is_empty() { "" } else { "ab" }
            );
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
            dialog.handle_key(KeyCode::Delete, KeyModifiers::NONE);
            assert_eq!(dialog.state().cursor, 1, "Delete does not refocus Skip");
            dialog.handle_key(KeyCode::Char('J'), KeyModifiers::SHIFT);
            assert_eq!(dialog.state().cursor, 0);
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
            type_text(&mut dialog, "k ");
            assert_eq!(dialog.state().cursor, 0);
            assert!(dialog.state().input.ends_with("Jk "));
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
            assert_submitted(&mut dialog, None);
        }
    }

    #[test]
    fn text_keeps_scalar_length_validation_whitespace_and_dismissal_rules() {
        let kind = QuestionPromptKind::Text {
            min_length: Some(2),
            max_length: Some(3),
        };
        let mut dialog = editor_dialog(kind.clone(), "e\u{301}", true);
        dialog.request.source_label = Some("ACP worker".into());
        assert_submitted(
            &mut dialog,
            Some(QuestionAnswerValue::String("e\u{301}".into())),
        );

        let mut dialog = editor_dialog(kind, " ab ", true);
        dialog.request.dismissible = false;
        assert_eq!(
            dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            dialog.validation_error.as_deref(),
            Some("Enter no more than 3 characters.")
        );
        dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(dialog.validation_error.is_none());
        assert_submitted(&mut dialog, Some(QuestionAnswerValue::String(" ab".into())));
        dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
        for _ in 0..3 {
            dialog.handle_key(KeyCode::Delete, KeyModifiers::NONE);
        }
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            dialog.validation_error.as_deref(),
            Some("Enter at least 2 characters.")
        );
    }

    #[test]
    fn normal_width_title_keeps_primary_actions_visible_for_every_question() {
        let mut dialog = QuestionDialog::new(TurnId::new(18), request());

        let first = render_text(&mut dialog, 80, 20);
        assert!(first.contains("Enter answer"));
        assert!(first.contains("Esc dismiss"));

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        let second = render_text(&mut dialog, 80, 20);
        assert!(second.contains("Enter answer"));
        assert!(second.contains("Esc dismiss"));
    }

    /// A full-buffer oracle for bounded test fixtures, independent of the
    /// implementation's probe, cache, and focus target. Tag only our synthetic
    /// span so literal caret glyphs in the draft cannot satisfy the assertion.
    fn reference_editor_caret(input: &str, caret: usize, width: u16) -> (usize, usize, usize) {
        let paragraph = Paragraph::new(Line::from(vec![
            Span::raw("  > "),
            Span::raw(&input[..caret]),
            Span::styled("▌", Style::default().add_modifier(Modifier::UNDERLINED)),
            Span::raw(&input[caret..]),
        ]))
        .wrap(Wrap { trim: false });
        let rows = paragraph.line_count(width);
        assert!(rows <= 4096, "only bounded test fixtures use a full buffer");
        // Leave the same guard cells as a real popup for Ratatui's narrow
        // wide-grapheme overdraw, while keeping the paragraph wrap width exact.
        let mut buffer = Buffer::empty(Rect::new(0, 0, width + 2, rows_to_u16(rows)));
        paragraph.render(Rect::new(0, 0, width, rows_to_u16(rows)), &mut buffer);
        let index = buffer
            .content()
            .iter()
            .position(|cell| cell.modifier.contains(Modifier::UNDERLINED))
            .expect("the reference renderer must draw the synthetic caret");
        (
            index / usize::from(buffer.area.width),
            index % usize::from(buffer.area.width),
            rows,
        )
    }

    fn assert_caret_visible(dialog: &QuestionDialog, buffer: &Buffer) -> usize {
        let state = dialog.state();
        let key = state.last_layout.unwrap();
        assert!(
            dialog.current().options.is_empty(),
            "editor-only test fixture"
        );
        let (row, column, _) = reference_editor_caret(
            &state.input,
            clamp_cursor(&state.input, state.caret),
            key.inner_width,
        );
        let label_rows = if state.editing_other {
            let label = if matches!(
                dialog.current().kind,
                QuestionPromptKind::MultiSelect { .. }
            ) {
                if state.other_selected {
                    "[x] Other"
                } else {
                    "[ ] Other"
                }
            } else {
                "Other"
            };
            Paragraph::new(label)
                .wrap(Wrap { trim: false })
                .line_count(key.inner_width)
        } else {
            0
        };
        let body_shift = if key.mode == QuestionLayoutMode::WholeBody {
            key.prompt_rows
        } else {
            0
        };
        let row = row + label_rows + body_shift;
        let visible = state.viewport.visible_range();
        assert!(
            visible.start() <= row && row < visible.end(),
            "actual caret row {row} is outside {visible:?}, layout {key:?}"
        );
        let popup_width = question_width(buffer.area);
        let inner_x = usize::from((buffer.area.width - popup_width) / 2) + 2;
        let inner_y = usize::from((buffer.area.height - key.popup_height) / 2) + 1;
        let pinned_shift = if key.mode == QuestionLayoutMode::Pinned {
            key.prompt_rows
        } else {
            0
        };
        let y = inner_y + pinned_shift + row - visible.start();
        assert_eq!(
            buffer[(rows_to_u16(inner_x + column), rows_to_u16(y))].symbol(),
            "▌",
            "the independently measured synthetic caret must actually be on screen"
        );
        y
    }

    #[test]
    fn editor_rendering_splits_at_the_caret_without_mutating_the_draft() {
        for kind in editor_kinds() {
            let mut dialog = editor_dialog(kind, "abc", true);
            dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
            dialog.handle_key(KeyCode::Right, KeyModifiers::NONE);
            assert!(render_text(&mut dialog, 80, 20).contains("a▌bc"));
            dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
            assert!(render_text(&mut dialog, 80, 20).contains("> ▌abc"));
            dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
            assert!(render_text(&mut dialog, 80, 20).contains("abc▌"));
        }
        let mut dialog = editor_dialog(text_kind(), "e\u{301}bc", true);
        dialog.state_mut().caret = 1;
        assert!(render_text(&mut dialog, 80, 20).contains("> ▌e\u{301}bc"));
        assert_eq!(
            dialog.state().caret,
            1,
            "defensive rendering cannot normalize draft state"
        );
        assert_eq!(dialog.state().input, "e\u{301}bc");
    }

    #[test]
    fn caret_measurement_matches_full_rendering_with_the_complete_suffix() {
        use unicode_segmentation::UnicodeSegmentation;
        let unicode = "e\u{301} 世界 👩‍💻 words";
        let fixtures = [
            ("ab 123456789 tail".to_string(), vec![0, 3, 5, 16]),
            ("x".repeat(512), vec![0, 1, 255, 512]),
            (format!("ab{}tail", " ".repeat(64)), vec![2, 21, 66, 70]),
            (
                unicode.into(),
                unicode
                    .grapheme_indices(true)
                    .map(|(byte, _)| byte)
                    .collect(),
            ),
            (
                "▌ prefix ▌ target ▌ suffix".into(),
                vec![0, "▌ prefix ▌ t".len(), "▌ prefix ▌ target ▌ suffix".len()],
            ),
        ];
        for (input, carets) in fixtures {
            let dialog = editor_dialog(text_kind(), &input, true);
            for width in [1, 2, 5, 8, 12, 26, 64] {
                for &caret in &carets {
                    let state = PromptState {
                        input: input.clone(),
                        caret,
                        ..PromptState::default()
                    };
                    let (_, measurement) = editor_layout(&state, selection_style(), width);
                    let (row, _, rows) = reference_editor_caret(&input, caret, width);
                    assert_eq!(
                        (measurement.caret_row, measurement.rows),
                        (row, rows),
                        "width {width}, caret {caret}, input {:?}",
                        dialog.state().input
                    );
                }
            }
        }
        // A fitting word moves as a unit: measuring only through the caret would give row zero.
        assert_eq!(reference_editor_caret("ab 123456789 tail", 3, 12).0, 1);
    }

    #[test]
    fn caret_probes_expand_beyond_the_prefix_estimate_and_cache_long_drafts() {
        // A very wide layout reduces the cell-bounded probe to one row. The
        // suffix moves the caret to row one, outside the initial prefix probe.
        let input = format!("abc {}", "x".repeat(19_994));
        let mut state = PromptState {
            input,
            caret: 4,
            ..PromptState::default()
        };
        let (_, measured) = editor_layout(&state, Style::default(), 20_000);
        assert_eq!(measured.caret_row, 1);
        assert_eq!(
            measured.caret_row,
            reference_editor_caret(&state.input, state.caret, 20_000).0
        );
        // Seed a recognizable cached value to prove an unchanged layout is not rescanned.
        let sentinel = EditorMeasurement {
            caret_row: 123,
            ..measured
        };
        state.editor_measurement.set(Some(sentinel));
        assert_eq!(editor_layout(&state, selection_style(), 20_000).1, sentinel);
        state.handle_editor_key(KeyCode::Right, KeyModifiers::NONE);
        assert!(state.editor_measurement.get().is_none());
        assert_eq!(
            state.input_revision, 0,
            "caret movement is not a text revision"
        );
        editor_layout(&state, Style::default(), 20_000);
        state.handle_editor_key(KeyCode::Char('!'), KeyModifiers::NONE);
        assert!(state.editor_measurement.get().is_none());
        assert_eq!(state.input_revision, 1);

        let mut dialog = editor_dialog(text_kind(), &"x".repeat(32_000), true);
        dialog.state_mut().caret = 20_000;
        let (_, measured) = editor_layout(dialog.state(), Style::default(), 64);
        assert_eq!(
            measured.caret_row,
            reference_editor_caret(&dialog.state().input, 20_000, 64).0
        );
        assert!(
            measured.rows > 32,
            "the document is much taller than the scratch buffer"
        );
    }

    #[test]
    fn actual_wrapped_caret_is_visible_in_text_and_other_at_narrow_sizes() {
        let fixtures = [
            ("ab 123456789 tail".to_string(), 3),
            ("x".repeat(512), 1),
            ("x".repeat(512), 255),
            (format!("ab{}tail", " ".repeat(80)), 33),
            ("e\u{301} 世界 👩‍💻 words".into(), "e\u{301} 世".len()),
            ("e\u{301} 世界 👩‍💻 words".into(), "e\u{301} ".len()),
            (
                format!("{} target {}", "▌".repeat(40), "▌".repeat(40)),
                "▌".repeat(40).len() + 2,
            ),
        ];
        for kind in editor_kinds() {
            for (input, caret) in &fixtures {
                for (width, height, mode) in [
                    (36, 5, QuestionLayoutMode::Pinned),
                    (14, 5, QuestionLayoutMode::Pinned),
                    (9, 5, QuestionLayoutMode::Pinned),
                    (6, 5, QuestionLayoutMode::Pinned),
                    (36, 3, QuestionLayoutMode::WholeBody),
                    (14, 3, QuestionLayoutMode::WholeBody),
                    (9, 3, QuestionLayoutMode::WholeBody),
                    (6, 3, QuestionLayoutMode::WholeBody),
                ] {
                    let mut dialog = editor_dialog(kind.clone(), input, true);
                    dialog.state_mut().caret = *caret;
                    let buffer = render_buffer(&mut dialog, width, height);
                    assert_eq!(dialog.state().last_layout.unwrap().mode, mode);
                    assert_caret_visible(&dialog, &buffer);
                    assert_focus_reachable(&dialog);
                }
            }
        }
    }

    #[test]
    fn selected_background_covers_visible_editor_rows_after_the_caret() {
        for kind in editor_kinds() {
            for whole_body in [false, true] {
                let mut dialog = editor_dialog(kind.clone(), &"word ".repeat(80), true);
                if whole_body {
                    dialog.request.questions[0].question = "long prompt ".repeat(30);
                }
                render_buffer(&mut dialog, 36, 8);
                dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
                let buffer = render_buffer(&mut dialog, 36, 8);
                let caret_y = assert_caret_visible(&dialog, &buffer);
                let key = dialog.state().last_layout.unwrap();
                assert_eq!(key.mode == QuestionLayoutMode::WholeBody, whole_body);
                let inner_x = (36 - question_width(buffer.area)) / 2 + 2;
                let bottom = (8 - key.popup_height) / 2 + key.popup_height - 1;
                assert!(caret_y + 1 < usize::from(bottom));
                for y in rows_to_u16(caret_y + 1)..bottom {
                    for x in inner_x..inner_x + key.inner_width {
                        assert_eq!(
                            buffer[(x, y)].bg,
                            selection_style().bg.unwrap(),
                            "selection must include suffix rows and their blank right-hand cells"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn editor_boundary_noops_preserve_pan_but_movement_and_mutation_reveal_again() {
        for kind in editor_kinds() {
            let mut dialog = editor_dialog(kind, &"word ".repeat(80), true);
            let buffer = render_buffer(&mut dialog, 36, 5);
            assert_caret_visible(&dialog, &buffer);
            let cached = dialog.state().editor_measurement.get();
            let caret = dialog.state().caret;
            let input = dialog.state().input.clone();
            dialog.handle_key(KeyCode::PageUp, KeyModifiers::NONE);
            let pan = dialog.state().viewport.top();
            for (code, modifiers) in [
                (KeyCode::Right, KeyModifiers::NONE),
                (KeyCode::Right, KeyModifiers::CONTROL),
                (KeyCode::End, KeyModifiers::NONE),
                (KeyCode::Delete, KeyModifiers::NONE),
            ] {
                dialog.handle_key(code, modifiers);
                assert!(!render_text(&mut dialog, 36, 5).contains('▌'));
                assert_eq!(dialog.state().viewport.top(), pan);
                assert_eq!(dialog.state().editor_measurement.get(), cached);
                assert_eq!(dialog.state().caret, caret);
                assert_eq!(dialog.state().input, input);
                assert!(dialog.state().answer.is_none());
            }
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
            let buffer = render_buffer(&mut dialog, 36, 5);
            assert_caret_visible(&dialog, &buffer);
            dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
            let buffer = render_buffer(&mut dialog, 36, 5);
            assert_caret_visible(&dialog, &buffer);
            dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
            let pan = dialog.state().viewport.top();
            for (code, modifiers) in [
                (KeyCode::Left, KeyModifiers::NONE),
                (KeyCode::Left, KeyModifiers::CONTROL),
                (KeyCode::Home, KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
            ] {
                dialog.handle_key(code, modifiers);
                assert!(!render_text(&mut dialog, 36, 5).contains('▌'));
                assert_eq!(dialog.state().viewport.top(), pan);
            }
            dialog.handle_key(KeyCode::Right, KeyModifiers::CONTROL);
            let buffer = render_buffer(&mut dialog, 36, 5);
            assert_caret_visible(&dialog, &buffer);
            dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
            type_text(&mut dialog, "!");
            let buffer = render_buffer(&mut dialog, 36, 5);
            assert_caret_visible(&dialog, &buffer);
        }
    }

    #[test]
    fn caret_reveal_reconciles_resize_and_yields_to_whole_body_validation() {
        let kind = QuestionPromptKind::Text {
            min_length: None,
            max_length: Some(1),
        };
        let mut dialog = editor_dialog(kind, &"word ".repeat(40), true);
        dialog.request.questions[0].question = "prompt ".repeat(15);
        dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let error = render_text(&mut dialog, 36, 3);
        assert!(error.contains("characters."));
        assert!(
            !error.contains('▌'),
            "whole-body errors take reveal priority"
        );
        assert_focus_reachable(&dialog);
        dialog.handle_key(KeyCode::Right, KeyModifiers::CONTROL);
        let buffer = render_buffer(&mut dialog, 36, 3);
        assert_caret_visible(&dialog, &buffer);
        let caret = dialog.state().caret;
        let cache = dialog.state().editor_measurement.get().unwrap();
        let buffer = render_buffer(&mut dialog, 96, 20);
        assert_caret_visible(&dialog, &buffer);
        assert_eq!(
            dialog.state().last_layout.unwrap().mode,
            QuestionLayoutMode::Pinned
        );
        assert_ne!(
            dialog.state().editor_measurement.get().unwrap().key.width,
            cache.key.width
        );
        assert_eq!(dialog.state().caret, caret);
    }

    #[test]
    fn editors_handle_zero_and_minimal_dimensions_without_panics() {
        for kind in editor_kinds() {
            let mut dialog = editor_dialog(kind, "wide 界 👩‍💻 text", true);
            for (width, height) in [
                (0, 0),
                (0, 5),
                (5, 0),
                (1, 1),
                (2, 2),
                (4, 4),
                (5, 3),
                (6, 5),
            ] {
                render_buffer(&mut dialog, width, height);
                dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
                render_buffer(&mut dialog, width, height);
                dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
            }
        }
    }

    #[test]
    fn editor_titles_are_contextual_and_keep_primary_actions_at_compact_widths() {
        for kind in editor_kinds() {
            let mut dialog = editor_dialog(kind, "text", false);
            let full = crate::hints::help_lines(dialog.context(), &dialog.hint_eligibility())
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            for hint in ["← left", "Ctrl+← word left", "Home start", "End end"] {
                assert!(full.contains(hint), "missing {hint}: {full}");
            }
            assert!(!full.contains("previous"));
            for width in [0u16, 1, 2, 12, 24, 42, 56, 80, 110, 300] {
                let title = dialog.title(width);
                assert!(display_width(&title) <= usize::from(width.saturating_sub(2)));
                let footer = hint_line(
                    dialog.context(),
                    &dialog.hint_eligibility(),
                    usize::from(width.saturating_sub(2)),
                )
                .to_string();
                assert!(display_width(&footer) <= usize::from(width.saturating_sub(2)));
                if width >= 56 {
                    assert!(footer.contains("Enter"));
                    assert!(footer.contains("Esc"));
                }
                assert!(!footer.contains("? help"));
            }
            if !dialog.state().editing_other {
                dialog
                    .request
                    .questions
                    .insert(0, request().questions.remove(0));
                dialog
                    .states
                    .insert(0, PromptState::new(&dialog.request.questions[0]));
                dialog.question_index = 1;
                assert!(dialog.title(300).contains("2/2"));
                dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
                let footer =
                    hint_line(dialog.context(), &dialog.hint_eligibility(), 54).to_string();
                assert!(footer.contains("Enter skip"));
                assert!(footer.contains("Esc dismiss"));
            }
        }
    }

    fn assert_focus_reachable(dialog: &QuestionDialog) {
        let state = dialog.state();
        let key = state.last_layout.expect("the dialog was rendered");
        let content = QuestionContent::new(
            dialog.current(),
            state,
            dialog.validation_error.as_deref(),
            key.inner_width,
        );
        let target = match key.mode {
            QuestionLayoutMode::Pinned => content.focus_target(state.cursor),
            QuestionLayoutMode::WholeBody => content.whole_body(state.cursor).focus_target,
        }
        .expect("the prompt has a focus target");
        let visible = state.viewport.visible_range();
        match target.anchor {
            FocusAnchor::Full if target.range.len() <= state.viewport.visible_rows() => {
                assert!(
                    visible.start() <= target.range.start() && visible.end() >= target.range.end(),
                    "focused range {:?} was outside {:?}",
                    target.range,
                    visible
                );
            }
            FocusAnchor::Full => assert!(
                visible.start() <= target.range.start() && target.range.start() < visible.end(),
                "oversized target start {:?} was outside {:?}",
                target.range,
                visible
            ),
            FocusAnchor::Row(offset) => {
                let row = target.range.start().saturating_add(offset);
                assert!(
                    visible.start() <= row && row < visible.end(),
                    "caret row {row} was outside {visible:?}"
                );
            }
            FocusAnchor::End => {
                let end_row = target.range.end().saturating_sub(1);
                assert!(
                    visible.start() <= end_row && end_row < visible.end(),
                    "target end {:?} was outside {:?}",
                    target.range,
                    visible
                );
            }
        }
    }

    #[test]
    fn empty_option_lists_use_the_free_form_other_path() {
        let mut empty_request = request();
        empty_request.questions.truncate(1);
        empty_request.questions[0].options.clear();
        empty_request.questions[0].kind = QuestionPromptKind::Text {
            min_length: Some(1),
            max_length: None,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(1), empty_request);

        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert_eq!(dialog.state().input, "");
        dialog.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("request-1"),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope".to_string(),
                        answer: Some(QuestionAnswerValue::String("x".to_string())),
                    }],
                },
            }
        );
    }

    #[test]
    fn large_option_lists_scroll_the_selected_option_into_view() {
        let mut large_request = request();
        large_request.questions.truncate(1);
        large_request.questions[0].options = (0..12)
            .map(|index| QuestionOption {
                label: format!("Option {index}"),
                description: format!("Description {index}."),
            })
            .collect();
        let mut dialog = QuestionDialog::new(TurnId::new(1), large_request);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");

        for _ in 0..11 {
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        }
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area);
            })
            .expect("render large option list");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Option 11"));

        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area);
            })
            .expect("render other option");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Other"));
    }

    #[test]
    fn step_by_step_answers_can_be_revised() {
        let mut dialog = QuestionDialog::new(TurnId::new(1), request());
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert_eq!(dialog.question_index, 1);
        assert_eq!(
            dialog.handle_key(KeyCode::Left, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let action = dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            action,
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("request-1"),
                response: QuestionResponse::Answered {
                    answers: vec![
                        QuestionAnswer {
                            id: "scope".to_string(),
                            answer: Some(QuestionAnswerValue::String("Broad".to_string())),
                        },
                        QuestionAnswer {
                            id: "tests".to_string(),
                            answer: Some(QuestionAnswerValue::String("Focused".to_string())),
                        },
                    ],
                },
            }
        );
    }

    #[test]
    fn other_editor_escapes_to_choices_and_option_escape_dismisses() {
        let mut dialog = QuestionDialog::new(TurnId::new(1), request());
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(dialog.state().editing_other);
        dialog.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert!(!dialog.state().editing_other);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(dialog.state().input, "x", "the draft survives Esc");
        dialog.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(dialog.question_index, 1);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("request-1"),
                response: QuestionResponse::Answered {
                    answers: vec![
                        QuestionAnswer {
                            id: "scope".to_string(),
                            answer: Some(QuestionAnswerValue::String("xy".to_string())),
                        },
                        QuestionAnswer {
                            id: "tests".to_string(),
                            answer: Some(QuestionAnswerValue::String("Focused".to_string())),
                        },
                    ],
                },
            }
        );

        let mut dialog = QuestionDialog::new(TurnId::new(1), request());
        assert_eq!(
            dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("request-1"),
                response: QuestionResponse::Dismissed,
            }
        );
    }

    #[test]
    fn single_other_default_opens_one_prefilled_editor_without_auto_answering() {
        let mut request = request();
        request.questions[0].default = Some(QuestionAnswerValue::String(" custom scope ".into()));
        let mut dialog = QuestionDialog::new(TurnId::new(1), request);
        assert_eq!(dialog.state().cursor, 2);
        assert_eq!(dialog.state().input, " custom scope ");
        assert!(!dialog.state().editing_other);
        assert!(dialog.state().answer.is_none());
        let rendered = render_text(&mut dialog, 120, 24);
        assert_eq!(rendered.matches("Other").count(), 1);
        assert!(!rendered.contains("Skip"));
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert!(dialog.state().editing_other);
        assert_eq!(
            dialog.question_index, 0,
            "Enter opens the editor, not another question"
        );
        assert!(render_text(&mut dialog, 120, 24).contains("custom scope"));
        dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!dialog.state().editing_other);
        assert_eq!(dialog.state().input, " custom scope ");
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        while !dialog.state().input.is_empty() {
            dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        }
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert!(
            dialog.state().editing_other,
            "blank text cannot be accepted"
        );
        for character in "revised ".chars() {
            dialog.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
        }
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(dialog.question_index, 1);
        assert_eq!(
            dialog.states[0].answer,
            Some(Some(QuestionAnswerValue::String("revised".into())))
        );
        assert_eq!(
            dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("request-1"),
                response: QuestionResponse::Dismissed
            }
        );
    }

    #[test]
    fn ordinary_choice_advances_past_other_directly_to_next_real_question() {
        let mut request = request();
        request.questions[0].default = Some(QuestionAnswerValue::String("Custom".into()));
        request.questions[1].required = false;
        request.questions[1].default = Some(QuestionAnswerValue::String("Full".into()));
        let mut dialog = QuestionDialog::new(TurnId::new(1), request);
        dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert_eq!(dialog.question_index, 1);
        assert_eq!(dialog.current().id, "tests");
        assert!(!dialog.state().editing_other);
        assert_eq!(dialog.state().cursor, 1, "ordinary defaults are unchanged");
        let rendered = render_text(&mut dialog, 120, 24);
        assert_eq!(rendered.matches("Other").count(), 1);
        assert!(rendered.contains("Skip"));
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("request-1"),
                response: QuestionResponse::Answered {
                    answers: vec![
                        QuestionAnswer {
                            id: "scope".into(),
                            answer: Some(QuestionAnswerValue::String("Focused".into()))
                        },
                        QuestionAnswer {
                            id: "tests".into(),
                            answer: None
                        },
                    ]
                },
            }
        );
    }

    #[test]
    fn multi_other_default_preselects_one_custom_value_and_preserves_limits_and_skip() {
        let mut request = request();
        request.questions.truncate(1);
        request.questions[0].kind = QuestionPromptKind::MultiSelect {
            min_selections: Some(2),
            max_selections: Some(2),
            allow_other: true,
        };
        request.questions[0].default = Some(QuestionAnswerValue::Strings(vec![
            "Focused".into(),
            " custom ".into(),
        ]));
        let mut dialog = QuestionDialog::new(TurnId::new(1), request.clone());
        assert_eq!(dialog.state().multi_selected, [true, false]);
        assert!(dialog.state().other_selected);
        assert_eq!(dialog.state().input, " custom ");
        assert!(!dialog.state().editing_other);
        let rendered = render_text(&mut dialog, 120, 24);
        assert_eq!(rendered.matches("Other").count(), 1);
        assert!(!rendered.contains("Skip"));
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert_eq!(
            dialog.validation_error.as_deref(),
            Some("Select no more than 2 choices.")
        );
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!dialog.state().other_selected);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(dialog.state().editing_other);
        assert_eq!(
            dialog.state().input,
            " custom ",
            "toggling retains the custom draft"
        );
        dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!dialog.state().editing_other);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert!(!dialog.state().editing_other);
        assert!(dialog.state().other_selected);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: request.id.clone(),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope".into(),
                        answer: Some(QuestionAnswerValue::Strings(vec![
                            "Focused".into(),
                            "custom!".into()
                        ])),
                    }]
                },
            }
        );
        request.questions[0].required = false;
        let mut dialog = QuestionDialog::new(TurnId::new(2), request.clone());
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: request.id,
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope".into(),
                        answer: None
                    }]
                },
            }
        );
    }

    #[test]
    fn custom_defaults_do_not_enable_other_when_disallowed() {
        for kind in [
            QuestionPromptKind::SingleSelect { allow_other: false },
            QuestionPromptKind::MultiSelect {
                min_selections: None,
                max_selections: None,
                allow_other: false,
            },
        ] {
            let mut prompt = request().questions.remove(0);
            prompt.default = Some(if matches!(kind, QuestionPromptKind::SingleSelect { .. }) {
                QuestionAnswerValue::String("Custom".into())
            } else {
                QuestionAnswerValue::Strings(vec!["Custom".into()])
            });
            prompt.kind = kind;
            let state = PromptState::new(&prompt);
            assert_eq!(state.cursor, 0);
            assert!(state.input.is_empty());
            assert!(!state.other_selected);
            assert!(!state.editing_other);
            assert_eq!(state.multi_selected, [false, false]);
        }
    }

    #[test]
    fn acp_multi_select_defaults_bounds_skip_and_source_label_are_supported() {
        let multi = QuestionRequest {
            id: QuestionRequestId::new("multi"),
            questions: vec![QuestionPrompt {
                id: "targets".to_string(),
                header: "Targets".to_string(),
                question: "Which targets?".to_string(),
                options: vec![
                    QuestionOption {
                        label: "Core".to_string(),
                        description: "Core crate.".to_string(),
                    },
                    QuestionOption {
                        label: "TUI".to_string(),
                        description: "Terminal crate.".to_string(),
                    },
                ],
                kind: QuestionPromptKind::MultiSelect {
                    min_selections: Some(2),
                    max_selections: Some(2),
                    allow_other: false,
                },
                required: true,
                default: Some(QuestionAnswerValue::Strings(vec!["Core".to_string()])),
            }],
            source_label: Some("Claude Code".to_string()),
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(3), multi);
        assert!(dialog.state().multi_selected[0]);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert_eq!(
            dialog.validation_error.as_deref(),
            Some("Select at least 2 choices.")
        );
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("multi"),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "targets".to_string(),
                        answer: Some(QuestionAnswerValue::Strings(vec![
                            "Core".to_string(),
                            "TUI".to_string(),
                        ])),
                    }],
                },
            }
        );

        let mut dialog = QuestionDialog::new(
            TurnId::new(4),
            QuestionRequest {
                id: QuestionRequestId::new("skip"),
                questions: vec![QuestionPrompt {
                    id: "optional".to_string(),
                    header: "Optional".to_string(),
                    question: "Choose or skip".to_string(),
                    options: vec![QuestionOption {
                        label: "Use it".to_string(),
                        description: String::new(),
                    }],
                    kind: QuestionPromptKind::SingleSelect { allow_other: false },
                    required: false,
                    default: None,
                }],
                source_label: Some("Codex".to_string()),
                dismissible: true,
            },
        );
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("skip"),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "optional".to_string(),
                        answer: None,
                    }],
                },
            }
        );

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area);
            })
            .expect("render sourced question");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Codex · Optional"));
        assert!(text.contains("Skip"));
    }

    #[test]
    fn multi_select_other_returns_to_choices_and_clears_stale_validation() {
        let request = QuestionRequest {
            id: QuestionRequestId::new("multi-other"),
            questions: vec![QuestionPrompt {
                id: "targets".to_string(),
                header: "Targets".to_string(),
                question: "Which targets?".to_string(),
                options: vec![
                    QuestionOption {
                        label: "Core".to_string(),
                        description: String::new(),
                    },
                    QuestionOption {
                        label: "TUI".to_string(),
                        description: String::new(),
                    },
                ],
                kind: QuestionPromptKind::MultiSelect {
                    min_selections: Some(2),
                    max_selections: None,
                    allow_other: true,
                },
                required: true,
                default: None,
            }],
            source_label: Some("Claude Code".to_string()),
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(5), request);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert!(dialog.validation_error.is_some());

        dialog.state_mut().cursor = 2;
        dialog.state_mut().editing_other = true;
        dialog.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(dialog.validation_error, None);
        for character in "ustom".chars() {
            dialog.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
        }
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled,
            "accepting Other should return to the checkbox list"
        );
        assert!(dialog.state().other_selected);
        assert!(!dialog.state().editing_other);

        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Respond {
                request_id: QuestionRequestId::new("multi-other"),
                response: QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "targets".to_string(),
                        answer: Some(QuestionAnswerValue::Strings(vec![
                            "TUI".to_string(),
                            "custom".to_string(),
                        ])),
                    }],
                },
            }
        );
    }

    #[test]
    fn wrapped_options_keep_every_selectable_row_reachable() {
        let option_count = 10;
        let wrapped = QuestionRequest {
            id: QuestionRequestId::new("wrapped"),
            questions: vec![QuestionPrompt {
                id: "wrapped-options".to_string(),
                header: "Wrapped".to_string(),
                question: "PINNED-PROMPT Choose an option while this question itself wraps across several narrow terminal rows.".to_string(),
                options: (0..option_count)
                    .map(|index| QuestionOption {
                        label: format!(
                            "MARKER-{index} label with enough words to wrap on a narrow terminal"
                        ),
                        description: format!(
                            "Description {index} also wraps while keeping the whole focused item reachable."
                        ),
                    })
                    .collect(),
                kind: QuestionPromptKind::SingleSelect { allow_other: true },
                required: false,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(7), wrapped);
        let selectable_rows = option_count + 2;
        let mut overflow_render = String::new();

        for index in 0..selectable_rows {
            let rendered = render_text(&mut dialog, 44, 12);
            assert!(rendered.contains("PINNED-PROMPT"));
            assert_focus_reachable(&dialog);
            if index < option_count {
                assert!(rendered.contains(&format!("MARKER-{index}")));
            } else if index == option_count {
                assert!(rendered.contains("Other"));
            } else {
                assert!(rendered.contains("Skip"));
                overflow_render = rendered;
            }
            if index + 1 < selectable_rows {
                dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
            }
        }

        assert!(
            overflow_render.contains('█'),
            "overflow should draw a thumb"
        );
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, selectable_rows - 1);
        dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, 0);
        render_text(&mut dialog, 44, 12);
        assert_focus_reachable(&dialog);
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, selectable_rows - 1);
        render_text(&mut dialog, 44, 12);
        assert_focus_reachable(&dialog);
    }

    #[test]
    fn page_panning_is_independent_and_navigation_reveals_again() {
        let mut wrapped = request();
        wrapped.questions.truncate(1);
        wrapped.questions[0].options = (0..12)
            .map(|index| QuestionOption {
                label: format!("Option {index}"),
                description: "A description long enough to occupy wrapped answer rows.".to_string(),
            })
            .collect();
        wrapped.questions[0].required = false;
        let mut dialog = QuestionDialog::new(TurnId::new(8), wrapped);

        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        render_text(&mut dialog, 42, 11);
        let selected = dialog.state().cursor;
        let answer = dialog.state().answer.clone();
        let input = dialog.state().input.clone();
        let bottom = dialog.state().viewport.top();
        dialog.handle_key(KeyCode::PageUp, KeyModifiers::NONE);
        render_text(&mut dialog, 42, 11);
        assert!(dialog.state().viewport.top() < bottom);
        assert_eq!(dialog.state().cursor, selected);
        assert_eq!(dialog.state().answer, answer);
        assert_eq!(dialog.state().input, input);

        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        render_text(&mut dialog, 42, 11);
        assert_eq!(dialog.state().cursor, selected - 1);
        assert_focus_reachable(&dialog);

        dialog.handle_key(KeyCode::Home, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, 0, "the first row is bounded");
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, 13, "the last row is bounded");
    }

    #[test]
    fn one_row_page_panning_moves_by_one_without_changing_selection() {
        let mut one_row = request();
        one_row.questions.truncate(1);
        let mut dialog = QuestionDialog::new(TurnId::new(19), one_row);
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        render_text(&mut dialog, 40, 5);

        let layout = dialog.state().last_layout.expect("one-row pinned layout");
        assert_eq!(layout.mode, QuestionLayoutMode::Pinned);
        assert_eq!(layout.visible_rows, 1);
        let selected = dialog.state().cursor;
        let bottom = dialog.state().viewport.top();
        assert!(bottom > 0);

        dialog.handle_key(KeyCode::PageUp, KeyModifiers::NONE);
        render_text(&mut dialog, 40, 5);
        assert_eq!(dialog.state().viewport.top(), bottom - 1);
        assert_eq!(dialog.state().cursor, selected);

        dialog.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
        render_text(&mut dialog, 40, 5);
        assert_eq!(dialog.state().viewport.top(), bottom);
        assert_eq!(dialog.state().cursor, selected);
    }

    #[test]
    fn tiny_pinned_and_fallback_viewports_keep_question_scrollbars_visible() {
        for (height, mode, visible_rows) in [
            (5, QuestionLayoutMode::Pinned, 1),
            (6, QuestionLayoutMode::Pinned, 2),
            (3, QuestionLayoutMode::WholeBody, 1),
            (4, QuestionLayoutMode::WholeBody, 2),
        ] {
            let mut tiny = request();
            tiny.questions.truncate(1);
            let mut dialog = QuestionDialog::new(TurnId::new(20), tiny);
            dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
            let rendered = render_text(&mut dialog, 40, height);
            let layout = dialog.state().last_layout.expect("tiny question layout");
            assert_eq!(layout.mode, mode, "terminal height {height}");
            assert_eq!(
                layout.visible_rows, visible_rows,
                "terminal height {height}"
            );
            assert!(
                rendered.contains('█'),
                "overflowing viewport needs a thumb at terminal height {height}"
            );
        }
    }

    #[test]
    fn optional_text_navigation_is_directional_and_j_k_remain_text() {
        let text_request = QuestionRequest {
            id: QuestionRequestId::new("text-navigation"),
            questions: vec![QuestionPrompt {
                id: "text".to_string(),
                header: "Text".to_string(),
                question: "Enter optional text".to_string(),
                options: Vec::new(),
                kind: QuestionPromptKind::Text {
                    min_length: None,
                    max_length: None,
                },
                required: false,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(9), text_request);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, 1);
        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(dialog.state().cursor, 0);
        dialog.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(dialog.state().input, "jk");
        assert_eq!(dialog.state().cursor, 0);
    }

    #[test]
    fn pinned_prompt_and_validation_remain_visible_while_answers_scroll() {
        let validation_request = QuestionRequest {
            id: QuestionRequestId::new("validation"),
            questions: vec![QuestionPrompt {
                id: "multi".to_string(),
                header: "Validation".to_string(),
                question: "PINNED-VALIDATION-PROMPT Which choices should be used?".to_string(),
                options: (0..10)
                    .map(|index| QuestionOption {
                        label: format!("Choice {index}"),
                        description: "Wrapped choice description for the scrolling viewport."
                            .to_string(),
                    })
                    .collect(),
                kind: QuestionPromptKind::MultiSelect {
                    min_selections: Some(2),
                    max_selections: None,
                    allow_other: false,
                },
                required: true,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(10), validation_request);
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        let rendered = render_text(&mut dialog, 48, 14);
        assert_eq!(
            dialog.state().last_layout.map(|layout| layout.mode),
            Some(QuestionLayoutMode::Pinned)
        );
        assert!(rendered.contains("PINNED-VALIDATION-PROMPT"));
        assert!(rendered.contains("Select at least 2 choices."));
        assert!(rendered.contains('█'));
        assert_focus_reachable(&dialog);
    }

    #[test]
    fn oversized_options_and_long_editors_keep_their_anchor_reachable() {
        let tall_request = QuestionRequest {
            id: QuestionRequestId::new("tall-option"),
            questions: vec![QuestionPrompt {
                id: "tall".to_string(),
                header: "Tall".to_string(),
                question: "Choose the tall option".to_string(),
                options: vec![QuestionOption {
                    label: format!("TALL-OPTION {}", "label ".repeat(60)),
                    description: "description ".repeat(60),
                }],
                kind: QuestionPromptKind::SingleSelect { allow_other: false },
                required: true,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut tall = QuestionDialog::new(TurnId::new(11), tall_request);
        let rendered = render_text(&mut tall, 36, 8);
        assert!(rendered.contains("TALL-OPTION"));
        assert_focus_reachable(&tall);

        let text_request = QuestionRequest {
            id: QuestionRequestId::new("long-text"),
            questions: vec![QuestionPrompt {
                id: "text".to_string(),
                header: "Text".to_string(),
                question: "Enter text".to_string(),
                options: Vec::new(),
                kind: QuestionPromptKind::Text {
                    min_length: None,
                    max_length: None,
                },
                required: true,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut text = QuestionDialog::new(TurnId::new(12), text_request);
        for character in "long-input ".repeat(30).chars() {
            text.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
        }
        let rendered = render_text(&mut text, 36, 8);
        assert!(rendered.contains('▌'));
        assert_focus_reachable(&text);
        let caret_bottom = text.state().viewport.top();
        text.handle_key(KeyCode::PageUp, KeyModifiers::NONE);
        render_text(&mut text, 36, 8);
        assert!(text.state().viewport.top() < caret_bottom);
        text.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        render_text(&mut text, 36, 8);
        assert_focus_reachable(&text);

        let mut other_request = request();
        other_request.questions.truncate(1);
        let mut other = QuestionDialog::new(TurnId::new(13), other_request);
        other.handle_key(KeyCode::End, KeyModifiers::NONE);
        other.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for character in "other-input ".repeat(30).chars() {
            other.handle_key(KeyCode::Char(character), KeyModifiers::NONE);
        }
        let rendered = render_text(&mut other, 36, 8);
        assert!(rendered.contains('▌'));
        assert_focus_reachable(&other);
    }

    #[test]
    fn adaptive_height_and_tiny_screen_fallback_reconcile_on_resize() {
        let mut fitting = QuestionDialog::new(TurnId::new(14), request());
        let rendered = render_text(&mut fitting, 80, 30);
        let fit_layout = fitting.state().last_layout.expect("fitting layout");
        let fit_content = QuestionContent::new(
            fitting.current(),
            fitting.state(),
            None,
            fit_layout.inner_width,
        );
        assert_eq!(
            usize::from(fit_layout.popup_height),
            fit_content
                .prompt_rows
                .saturating_add(fit_content.answer_rows)
                .saturating_add(2)
        );
        assert!(!rendered.contains('█'), "fitting answers need no scrollbar");

        let fallback_request = QuestionRequest {
            id: QuestionRequestId::new("fallback"),
            questions: vec![QuestionPrompt {
                id: "fallback-text".to_string(),
                header: "Fallback".to_string(),
                question: format!("FALLBACK-PROMPT {}", "wrapped prompt ".repeat(12)),
                options: Vec::new(),
                kind: QuestionPromptKind::Text {
                    min_length: Some(1),
                    max_length: None,
                },
                required: true,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut fallback = QuestionDialog::new(TurnId::new(15), fallback_request);
        fallback.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let tiny = render_text(&mut fallback, 40, 5);
        assert_eq!(
            fallback.state().last_layout.map(|layout| layout.mode),
            Some(QuestionLayoutMode::WholeBody)
        );
        assert!(tiny.contains("Enter at least 1"));
        assert!(tiny.contains("characters."));
        assert_focus_reachable(&fallback);

        let resized = render_text(&mut fallback, 40, 20);
        assert_eq!(
            fallback.state().last_layout.map(|layout| layout.mode),
            Some(QuestionLayoutMode::Pinned)
        );
        assert!(resized.contains("FALLBACK-PROMPT"));
        assert!(resized.contains("Enter at least 1"));
        assert!(resized.contains("characters."));
    }

    #[test]
    fn prompt_viewports_are_independent_across_question_transitions() {
        let options = (0..10)
            .map(|index| QuestionOption {
                label: format!("Question option {index}"),
                description: "A wrapped description that makes the answer list overflow."
                    .to_string(),
            })
            .collect::<Vec<_>>();
        let transition_request = QuestionRequest {
            id: QuestionRequestId::new("transitions"),
            questions: vec![
                QuestionPrompt {
                    id: "first".to_string(),
                    header: "First".to_string(),
                    question: "First question".to_string(),
                    options: options.clone(),
                    kind: QuestionPromptKind::SingleSelect { allow_other: false },
                    required: true,
                    default: None,
                },
                QuestionPrompt {
                    id: "second".to_string(),
                    header: "Second".to_string(),
                    question: "Second question".to_string(),
                    options,
                    kind: QuestionPromptKind::SingleSelect { allow_other: false },
                    required: true,
                    default: None,
                },
            ],
            source_label: None,
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(16), transition_request);
        dialog.handle_key(KeyCode::End, KeyModifiers::NONE);
        render_text(&mut dialog, 42, 10);
        let first_top = dialog.states[0].viewport.top();
        assert!(first_top > 0);
        dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        render_text(&mut dialog, 42, 10);
        assert_eq!(dialog.question_index, 1);
        assert_eq!(dialog.states[1].viewport.top(), 0);
        dialog.handle_key(KeyCode::Left, KeyModifiers::NONE);
        render_text(&mut dialog, 42, 10);
        assert_eq!(dialog.question_index, 0);
        assert_eq!(dialog.states[0].viewport.top(), first_top);
        assert_focus_reachable(&dialog);
    }

    #[test]
    fn zero_and_minimal_terminal_dimensions_are_panic_free() {
        for (width, height) in [(0, 0), (1, 1), (2, 2), (10, 2)] {
            let mut dialog = QuestionDialog::new(TurnId::new(17), request());
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    dialog.render(frame, area);
                })
                .expect("tiny question render");
        }
    }

    #[test]
    fn malformed_empty_select_is_defensive_even_when_constructed_in_memory() {
        let request = QuestionRequest {
            id: QuestionRequestId::new("empty-select"),
            questions: vec![QuestionPrompt {
                id: "empty".to_string(),
                header: "Empty".to_string(),
                question: "Choose".to_string(),
                options: Vec::new(),
                kind: QuestionPromptKind::SingleSelect { allow_other: false },
                required: true,
                default: None,
            }],
            source_label: None,
            dismissible: true,
        };
        let mut dialog = QuestionDialog::new(TurnId::new(6), request);
        assert_eq!(
            dialog.handle_key(KeyCode::Down, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
        assert_eq!(dialog.state().cursor, 0);
        assert_eq!(
            dialog.validation_error.as_deref(),
            Some("No choices are available for this question.")
        );
        assert_eq!(
            dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            QuestionDialogAction::Handled
        );
    }
}
