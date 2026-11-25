//! Configured-model picker and correlated conversion confirmation.
use crate::viewport::render_scrollbar;
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    text::Line,
    widgets::{List, ListItem, Paragraph, Wrap},
};
use std::ops::{Deref, DerefMut};
use zevria_foundation::SessionMode;
#[cfg(test)]
use zevria_model::models::ModelManagementRequest as Request;
use zevria_model::models::ModelManagementResult as Result;
#[cfg(test)]
use zevria_model::models::ModelSelectionPreview;
use zevria_model::models::ModelSelectionScope;
#[cfg(test)]
use zevria_model::models::mode_role;
#[cfg(test)]
use zevria_model::models::{ModelCandidate, ModelSelection};
use zevria_presentation::models::{ModelPhase, ModelPicker as PickerState, SelectionStep};
#[cfg(test)]
use zevria_session_api::SessionCommand;

use crate::hints::{Eligibility, hint_line};
use crate::input::{Action, ChordState, KeyContext};
use zevria_tui_widgets::overlay::{ListNav, modal};

#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;

#[derive(Default)]
pub(crate) struct ModelPicker {
    state: PickerState,
    filter: crate::composer::ComposerState,
    nav: ListNav,
    reasoning: ListNav,
    details: ListNav,
    keys: ChordState,
    filtering: bool,
}

impl Deref for ModelPicker {
    type Target = PickerState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
impl DerefMut for ModelPicker {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}
impl ModelPicker {
    pub(crate) fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
        self.reasoning.invalidate_geometry();
        self.details.invalidate_geometry();
    }

    pub fn show(&mut self, mode: SessionMode, scope: ModelSelectionScope) {
        if self.state.is_open() {
            return;
        }
        *self = Self::default();
        self.state.show(mode, scope);
    }
    pub fn accept(&mut self, request_id: &str, result: &Result) -> bool {
        if !self.state.accept(request_id, result) {
            return false;
        }
        match result {
            Result::Catalog { current, .. } => {
                self.nav.selected = self
                    .state
                    .profiles
                    .iter()
                    .position(|p| p.context.profile == current.profile)
                    .unwrap_or(0);
            }
            Result::ConfirmationRequired(_) => self.details = ListNav::default(),
            _ => {}
        }
        true
    }
    fn filtered(&self) -> Vec<usize> {
        self.state.filter_indices(self.filter.text())
    }
    pub(crate) fn context(&self) -> KeyContext {
        if self.filtering {
            KeyContext::TextEntry
        } else if self.preview().is_some() {
            KeyContext::ModelConfirm
        } else if matches!(self.state.phase.step(), SelectionStep::Reasoning { .. }) {
            KeyContext::ModelReasoning
        } else {
            KeyContext::ModelList
        }
    }

    pub(crate) fn hint_eligibility(&self) -> Eligibility {
        let mut hints = Eligibility::default();
        if self.pending() {
            hints.disabled = zevria_tui_input::keymap::bindings(self.context())
                .map(|binding| binding.action)
                .filter(|action| !matches!(action, Action::Close | Action::Cancel | Action::Help))
                .collect();
        } else if self.context() == KeyContext::ModelList && self.filtered().is_empty() {
            hints.disabled.push(Action::Confirm);
        }
        hints.labels.push((
            Action::Confirm,
            match self.context() {
                KeyContext::ModelList | KeyContext::TextEntry => "next",
                KeyContext::ModelConfirm => "convert",
                _ => "confirm",
            },
        ));
        if self.context() == KeyContext::ModelConfirm {
            hints
                .labels
                .extend([(Action::Up, "scroll up"), (Action::Down, "scroll down")]);
        }
        hints
    }

    pub fn handle_input(&mut self, key: KeyEvent) {
        let action = self.keys.resolve(self.context(), key);
        if action == Some(Action::Cancel) {
            self.cancel();
            return;
        }
        if self.filtering {
            match action {
                Some(Action::Close) => self.filtering = false,
                Some(Action::Confirm) => {
                    self.filtering = false;
                    self.handle_action(Action::Confirm);
                }
                Some(Action::Backspace) => {
                    self.filter.backspace();
                    self.nav.selected = 0;
                }
                Some(Action::Type(ch)) if self.filter.text().len() < 256 => {
                    self.filter.insert_character(ch);
                    self.nav.selected = 0;
                }
                _ => {}
            }
            return;
        }
        if let Some(action) = action {
            self.handle_action(action);
        }
    }

    pub fn paste(&mut self, text: &str) {
        if matches!(
            self.state.phase,
            ModelPhase::Selecting(SelectionStep::Model)
                | ModelPhase::Error {
                    back: SelectionStep::Model,
                    ..
                }
        ) {
            let text: String = text
                .chars()
                .filter(|ch| !ch.is_control())
                .take(256usize.saturating_sub(self.filter.text().chars().count()))
                .collect();
            self.filter.insert_text(&text);
            self.nav.selected = 0;
        }
    }

    #[cfg(test)]
    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        self.handle_input(KeyEvent::new(key, KeyModifiers::NONE));
    }

    fn handle_action(&mut self, action: Action) {
        if action == Action::Close {
            self.cancel();
            return;
        }
        if self.pending() || !self.is_open() {
            return;
        }
        if self.preview().is_some() {
            if action == Action::Confirm {
                self.state.confirm_conversion();
            } else if let Some(action) = action.list_action() {
                self.details.pan(action);
            }
            return;
        }
        if let SelectionStep::Reasoning { profile, selected } = self.state.phase.step() {
            self.reasoning.selected = selected;
            let candidate = &self.state.profiles[profile];
            match action {
                Action::Back => {
                    self.state.back();
                    return;
                }
                Action::Confirm => {
                    self.state.apply();
                    return;
                }
                _ => {
                    if let Some(action) = action.list_action() {
                        self.reasoning
                            .handle(action, candidate.reasoning_levels.len());
                    }
                }
            }
            self.state.choose_effort(self.reasoning.selected);
            return;
        }
        let filtered = self.filtered();
        match action {
            Action::Filter => self.filtering = true,
            Action::Confirm => {
                if let Some(index) = filtered.get(self.nav.selected) {
                    self.state.choose_model(*index);
                    self.reasoning = ListNav::default();
                    if let SelectionStep::Reasoning { selected, .. } = self.state.phase.step() {
                        self.reasoning.selected = selected;
                    }
                }
            }
            _ => {
                if let Some(action) = action.list_action() {
                    self.nav.handle(action, filtered.len());
                }
            }
        }
    }
    pub fn render(&mut self, frame: &mut Frame, body: Rect) {
        let width = body.width.saturating_sub(4).min(100);
        let height = body.height.min(18);
        let area = Rect::new(
            body.x + body.width.saturating_sub(width) / 2,
            body.y + body.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let style = crate::chrome::overlay_style();
        let title = format!(
            " {} — {} ",
            self.state
                .scope
                .map_or("Model", ModelSelectionScope::command),
            self.state.mode.map_or("", |mode| match mode {
                SessionMode::Build => "Build role",
                SessionMode::Plan => "Plan role",
            })
        );
        let hints = hint_line(
            self.context(),
            &self.hint_eligibility(),
            usize::from(area.width.saturating_sub(2)),
        );
        let inner = modal(frame, area, title, hints);
        if self.pending() {
            frame.render_widget(
                Paragraph::new(
                    if matches!(self.state.phase, ModelPhase::Cancelling { .. }) {
                        "Cancelling model maintenance…"
                    } else {
                        "Preparing model selection…"
                    },
                )
                .style(style)
                .wrap(Wrap { trim: false }),
                inner,
            );
            return;
        }
        if let Some(preview) = self.preview() {
            let text = format!(
                "Convert {} → {}?\n{}\n\n{}\n\nConfirmation makes a source-model call.",
                preview.source,
                preview.target,
                self.scope_label(),
                preview.reason
            );
            let paragraph = Paragraph::new(text).style(style).wrap(Wrap { trim: false });
            self.details
                .viewport
                .reconcile(paragraph.line_count(inner.width), usize::from(inner.height));
            frame.render_widget(
                paragraph.scroll((crate::viewport::rows_to_u16(self.details.viewport.top()), 0)),
                inner,
            );
            render_scrollbar(frame, inner, &self.details.viewport);
            return;
        }
        if let SelectionStep::Reasoning { profile, selected } = self.state.phase.step() {
            let candidate = &self.state.profiles[profile];
            // Keep at least one choice row when the protected body has any room.
            // Heading/help may be omitted; selection must never become invisible.
            let header_height = inner.height.saturating_sub(1).min(2);
            let header = Rect {
                height: header_height,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!(
                        "Step 2/2 — {} — choose reasoning",
                        candidate.context.profile
                    )),
                    Line::from(self.scope_label()),
                ])
                .style(style),
                header,
            );
            let rows = Rect::new(
                inner.x,
                header.bottom(),
                inner.width,
                inner.height.saturating_sub(header_height),
            );
            self.reasoning.selected = selected;
            self.reasoning
                .reconcile(candidate.reasoning_levels.len(), usize::from(rows.height));
            let visible = self.reasoning.viewport.visible_range();
            let choices = candidate
                .reasoning_levels
                .iter()
                .enumerate()
                .skip(visible.start())
                .take(visible.len())
                .map(|(index, level)| {
                    ListItem::new(format!(
                        "{} {level}",
                        if self
                            .state
                            .current
                            .as_ref()
                            .is_some_and(|c| c.profile == candidate.context.profile
                                && c.reasoning_level == *level)
                        {
                            "*"
                        } else {
                            " "
                        }
                    ))
                    .style(if index == selected {
                        crate::chrome::selection_style()
                    } else {
                        style
                    })
                })
                .collect::<Vec<_>>();
            frame.render_widget(List::new(choices).style(style), rows);
            self.reasoning.paint_selected(frame, rows);
            render_scrollbar(frame, rows, &self.reasoning.viewport);
            if let Some(error) = self.error() {
                // Error context belongs in the heading, never over the choices.
                frame.render_widget(
                    Paragraph::new(error).style(crate::chrome::error_style()),
                    header,
                );
            }
            return;
        }
        let header = Rect {
            height: inner.height.min(4),
            ..inner
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{}\nCurrent: {}\nStep 1/2 — Filter: {}{}",
                self.scope_label(),
                self.state
                    .current
                    .as_ref()
                    .map_or_else(|| "unavailable".into(), ToString::to_string),
                self.filter.text(),
                if self.filtering { "_" } else { "" }
            ))
            .style(style)
            .wrap(Wrap { trim: false }),
            header,
        );
        let error_height = if self.error().is_some() {
            inner.height.saturating_sub(header.height).min(4)
        } else {
            0
        };
        let rows = Rect::new(
            inner.x,
            inner.y.saturating_add(header.height),
            inner.width,
            inner
                .height
                .saturating_sub(header.height)
                .saturating_sub(error_height),
        );
        let filtered = self.filtered();
        self.nav.reconcile(filtered.len(), usize::from(rows.height));
        let visible = self.nav.viewport.visible_range();
        let items = filtered
            .iter()
            .enumerate()
            .skip(visible.start())
            .take(visible.len())
            .map(|(index, profile)| {
                let profile = &self.state.profiles[*profile].context;
                let marker = if self
                    .state
                    .current
                    .as_ref()
                    .is_some_and(|current| current.profile == profile.profile)
                {
                    "*"
                } else {
                    " "
                };
                let line = Line::from(format!(
                    "{marker} {} · input {} / context {} · retain {}",
                    profile.profile,
                    profile.input_token_limit,
                    profile.context_window_tokens,
                    profile.retained_user_tokens
                ));
                ListItem::new(line).style(if index == self.nav.selected {
                    crate::chrome::selection_style()
                } else {
                    style
                })
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items).style(style), rows);
        if !filtered.is_empty() {
            self.nav.paint_selected(frame, rows);
        }
        render_scrollbar(frame, rows, &self.nav.viewport);
        if let Some(error) = self.error() {
            frame.render_widget(
                Paragraph::new(error)
                    .style(crate::chrome::error_style())
                    .wrap(Wrap { trim: false }),
                Rect::new(inner.x, rows.bottom(), inner.width, error_height),
            );
        }
    }
}
