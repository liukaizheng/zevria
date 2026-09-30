//! Configured-model picker and correlated conversion confirmation.
use crate::viewport::render_scrollbar;
use ratatui::{
    Frame,
    crossterm::event::KeyEvent,
    layout::Rect,
    text::Line,
    widgets::{List, ListItem, Paragraph, Wrap},
};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};
use zevria_foundation::SessionMode;
use zevria_model::models::ModelManagementRequest as Request;
use zevria_model::models::ModelManagementResult as Result;
use zevria_model::models::ModelSelectionPreview;
use zevria_model::models::ModelSelectionScope;
use zevria_model::models::mode_role;
use zevria_model::models::{ModelCandidate, ModelSelection};
use zevria_session_api::SessionCommand;

use crate::hints::{Eligibility, hint_line};
use crate::input::{Action, ChordState, KeyContext};
use zevria_tui_widgets::overlay::{ListNav, modal};

#[cfg(test)]
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionStep {
    Model,
    Reasoning { profile: usize, selected: usize },
}

/// One modal lifecycle. Cancellation retains the submitted target until its
/// correlated result settles; a late catalog/preview cannot reopen selection.
#[derive(Default)]
enum ModelPhase {
    #[default]
    Settled,
    Loading,
    Selecting(SelectionStep),
    Conversion {
        preview: ModelSelectionPreview,
        back: SelectionStep,
    },
    PendingChange {
        target: ModelSelection,
        back: SelectionStep,
    },
    Cancelling {
        target: Option<ModelSelection>,
    },
    Error {
        message: String,
        back: SelectionStep,
    },
}

impl ModelPhase {
    fn step(&self) -> SelectionStep {
        match self {
            Self::Selecting(step) => *step,
            Self::Conversion { back, .. }
            | Self::PendingChange { back, .. }
            | Self::Error { back, .. } => *back,
            _ => SelectionStep::Model,
        }
    }

    fn target(&self) -> Option<&ModelSelection> {
        match self {
            Self::Conversion { preview, .. } => Some(&preview.target),
            Self::PendingChange { target, .. } => Some(target),
            Self::Cancelling { target } => target.as_ref(),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct ModelPicker {
    phase: ModelPhase,
    pub commands: VecDeque<SessionCommand>,
    request_id: String,
    mode: Option<SessionMode>,
    scope: Option<ModelSelectionScope>,
    profiles: Vec<ModelCandidate>,
    current: Option<ModelSelection>,
    revision: String,
    filter: crate::composer::ComposerState,
    nav: ListNav,
    reasoning: ListNav,
    details: ListNav,
    keys: ChordState,
    filtering: bool,
}

impl ModelPicker {
    pub(crate) fn is_open(&self) -> bool {
        !matches!(self.phase, ModelPhase::Settled)
    }

    fn pending(&self) -> bool {
        matches!(
            self.phase,
            ModelPhase::Loading | ModelPhase::PendingChange { .. } | ModelPhase::Cancelling { .. }
        )
    }

    fn preview(&self) -> Option<&ModelSelectionPreview> {
        match &self.phase {
            ModelPhase::Conversion { preview, .. } => Some(preview),
            _ => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match &self.phase {
            ModelPhase::Error { message, .. } => Some(message),
            _ => None,
        }
    }

    pub(crate) fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
        self.reasoning.invalidate_geometry();
        self.details.invalidate_geometry();
    }

    pub fn show(&mut self, mode: SessionMode, scope: ModelSelectionScope) {
        *self = Self {
            phase: ModelPhase::Loading,
            mode: Some(mode),
            scope: Some(scope),
            request_id: format!("model-{}", REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)),
            ..Self::default()
        };
        self.send(Request::List { mode, scope });
    }
    fn send(&mut self, request: Request) {
        self.commands.push_back(SessionCommand::Manage(
            zevria_session_api::ManagementCommand::Models {
                request_id: self.request_id.clone(),
                request,
            },
        ));
    }
    pub fn cancel(&mut self) {
        if self.is_open() && !matches!(self.phase, ModelPhase::Cancelling { .. }) {
            self.phase = ModelPhase::Cancelling {
                target: self.phase.target().cloned(),
            };
            self.send(Request::Cancel);
        }
    }
    /// Late responses from an earlier picker/session cannot update telemetry.
    pub fn accept(&mut self, request_id: &str, result: &Result) -> bool {
        if !self.is_open() || self.request_id != request_id {
            return false;
        }
        // Validate captured mode/scope before changing any modal state or
        // allowing the runtime to install authoritative telemetry.
        let correlated = match result {
            Result::Catalog { mode, scope, .. } => {
                matches!(
                    self.phase,
                    ModelPhase::Loading | ModelPhase::Cancelling { .. }
                ) && Some(*mode) == self.mode
                    && Some(*scope) == self.scope
            }
            Result::ConfirmationRequired(preview) => {
                matches!(
                    self.phase,
                    ModelPhase::PendingChange { .. } | ModelPhase::Cancelling { .. }
                ) && preview.request_id == self.request_id
                    && Some(preview.mode) == self.mode
                    && Some(preview.scope) == self.scope
                    && preview.revision == self.revision
                    && self.phase.target() == Some(&preview.target)
            }
            Result::Changed {
                role,
                scope,
                context,
                reasoning_level,
                ..
            } => {
                matches!(
                    self.phase,
                    ModelPhase::PendingChange { .. } | ModelPhase::Cancelling { .. }
                ) && self.mode.map(mode_role) == Some(*role)
                    && Some(*scope) == self.scope
                    && self.phase.target().is_some_and(|target| {
                        target.profile == context.profile
                            && target.reasoning_level == *reasoning_level
                    })
            }
            Result::Cancelled | Result::Rejected { .. } => true,
        };
        if !correlated {
            return false;
        }
        let cancelling = matches!(self.phase, ModelPhase::Cancelling { .. });
        match result {
            Result::Catalog {
                current,
                profiles,
                revision,
                ..
            } => {
                self.profiles = profiles.clone();
                self.nav.selected = self
                    .profiles
                    .iter()
                    .position(|profile| profile.context.profile == current.profile)
                    .unwrap_or(0);
                self.current = Some(current.clone());
                self.revision = revision.clone();
                if !cancelling {
                    self.phase = ModelPhase::Selecting(SelectionStep::Model);
                }
            }
            Result::ConfirmationRequired(preview) => {
                if !cancelling {
                    self.details = ListNav::default();
                    self.phase = ModelPhase::Conversion {
                        preview: preview.clone(),
                        back: self.phase.step(),
                    };
                }
            }
            Result::Changed { .. } | Result::Cancelled => {
                self.phase = ModelPhase::Settled;
            }
            Result::Rejected {
                message,
                current_revision,
                ..
            } => {
                if let Some(revision) = current_revision
                    && self.scope == Some(ModelSelectionScope::SessionAndDefault)
                {
                    self.revision = revision.clone();
                }
                self.phase = if cancelling {
                    ModelPhase::Settled
                } else {
                    ModelPhase::Error {
                        message: message.clone(),
                        back: self.phase.step(),
                    }
                };
            }
        }
        true
    }
    fn filtered(&self) -> Vec<usize> {
        let query = self.filter.text().to_lowercase();
        self.profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| {
                profile
                    .context
                    .profile
                    .to_string()
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }
    pub(crate) fn context(&self) -> KeyContext {
        if self.filtering {
            KeyContext::TextEntry
        } else if self.preview().is_some() {
            KeyContext::ModelConfirm
        } else if matches!(self.phase.step(), SelectionStep::Reasoning { .. }) {
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
            self.phase,
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
        if let Some(preview) = self.preview() {
            if action == Action::Confirm {
                let preview = preview.clone();
                self.phase = ModelPhase::PendingChange {
                    target: preview.target.clone(),
                    back: self.phase.step(),
                };
                self.send(Request::Confirm { preview });
            } else if let Some(action) = action.list_action() {
                self.details.pan(action);
            }
            return;
        }
        if let SelectionStep::Reasoning { profile, selected } = self.phase.step() {
            self.reasoning.selected = selected;
            let candidate = &self.profiles[profile];
            match action {
                Action::Back => {
                    self.phase = ModelPhase::Selecting(SelectionStep::Model);
                    return;
                }
                Action::Confirm => {
                    if let Some(level) = candidate.reasoning_levels.get(selected) {
                        let target = ModelSelection::new(candidate.context.profile.clone(), *level);
                        self.phase = ModelPhase::PendingChange {
                            target: target.clone(),
                            back: SelectionStep::Reasoning { profile, selected },
                        };
                        self.send(Request::Select {
                            mode: self.mode.expect("open picker mode"),
                            scope: self.scope.expect("open picker scope"),
                            target,
                            revision: self.revision.clone(),
                        });
                    }
                    return;
                }
                _ => {
                    if let Some(action) = action.list_action() {
                        self.reasoning
                            .handle(action, candidate.reasoning_levels.len());
                    }
                }
            }
            self.phase = ModelPhase::Selecting(SelectionStep::Reasoning {
                profile,
                selected: self.reasoning.selected,
            });
            return;
        }
        let filtered = self.filtered();
        match action {
            Action::Filter => self.filtering = true,
            Action::Confirm => {
                if let Some(index) = filtered.get(self.nav.selected) {
                    let selected = self
                        .current
                        .as_ref()
                        .and_then(|current| {
                            self.profiles[*index]
                                .reasoning_levels
                                .iter()
                                .position(|level| *level == current.reasoning_level)
                        })
                        .unwrap_or(0);
                    self.reasoning = ListNav::default();
                    self.reasoning.selected = selected;
                    self.phase = ModelPhase::Selecting(SelectionStep::Reasoning {
                        profile: *index,
                        selected,
                    });
                }
            }
            _ => {
                if let Some(action) = action.list_action() {
                    self.nav.handle(action, filtered.len());
                }
            }
        }
    }
    fn scope_label(&self) -> &'static str {
        match self.scope {
            Some(ModelSelectionScope::SessionOnly) => {
                "session only — kept on /new, fresh handoff and resume; config unchanged"
            }
            Some(ModelSelectionScope::SessionAndDefault) => {
                "session and global default — kept on /new, fresh handoff and resume; updates config"
            }
            None => "",
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
            self.scope.map_or("Model", ModelSelectionScope::command),
            self.mode.map_or("", |mode| match mode {
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
                Paragraph::new(if matches!(self.phase, ModelPhase::Cancelling { .. }) {
                    "Cancelling model maintenance…"
                } else {
                    "Preparing model selection…"
                })
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
        if let SelectionStep::Reasoning { profile, selected } = self.phase.step() {
            let candidate = &self.profiles[profile];
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
                    ListItem::new(format!("  {level}")).style(if index == selected {
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
                self.current
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
                let profile = &self.profiles[*profile].context;
                let marker = if self
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
