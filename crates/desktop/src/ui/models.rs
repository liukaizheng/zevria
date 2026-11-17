//! Native model sheet. Only view-local search, navigation and focus live here.
use super::{
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::{actions, controller::Phase};
use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    input::{InputEvent, Textarea, TextareaState},
};
use gpui_kit::{prelude::*, *};
use zevria_model::models::ModelSelectionScope;
use zevria_presentation::models::{ModelPhase, SelectionStep};

pub(super) struct ModelPickerView {
    request_id: String,
    search: Entity<TextareaState>,
    selected: usize,
    catalog_loaded: bool,
    scroll: ScrollHandle,
    focus: FocusHandle,
    cancel_focus: FocusHandle,
    apply_focus: FocusHandle,
    return_focus: Option<WeakFocusHandle>,
    _search: Subscription,
}
impl DesktopWindow {
    pub(super) fn open_models(&mut self, cx: &mut Context<Self>) {
        self.controller.update(cx, |this, cx| {
            this.open_model_picker(ModelSelectionScope::SessionOnly, cx);
        });
    }
    pub(super) fn sync_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = &self.controller.read(cx).model.models;
        if !picker.is_open() {
            if let Some(view) = self.model_ui.take()
                && self.controller.read(cx).phase == Phase::Live
            {
                if let Some(focus) = view
                    .return_focus
                    .and_then(|f| f.upgrade())
                    .filter(|f| self.window_focus.contains(f, window))
                {
                    window.focus(&focus, cx);
                } else {
                    self.composer
                        .update(cx, |input, cx| input.focus(window, cx));
                }
            }
            return;
        }
        if let Some(view) = &mut self.model_ui
            && view.request_id == picker.request_id
        {
            if !view.catalog_loaded
                && let Some(current) = &picker.current
            {
                view.selected = picker
                    .profiles
                    .iter()
                    .position(|p| p.context.profile == current.profile)
                    .unwrap_or(0);
                view.scroll.scroll_to_item(view.selected);
                view.catalog_loaded = true;
                if !picker.pending() && picker.preview().is_none() {
                    view.search.update(cx, |input, cx| input.focus(window, cx));
                }
            }
            return;
        }
        let request_id = picker.request_id.clone();
        let focus_sheet = picker.pending() || picker.preview().is_some();
        let return_focus = window.focused(cx).map(|f| f.downgrade());
        let search = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 1)
                .placeholder("Search configured models")
                .submit_on_enter(true)
        });
        let subscription = cx.subscribe_in(&search, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                if let Some(view) = &mut this.model_ui {
                    view.selected = 0;
                    view.scroll.scroll_to_item(0);
                }
                cx.notify();
            }
        });
        let focus = cx.focus_handle();
        if focus_sheet {
            window.focus(&focus, cx);
        } else {
            search.update(cx, |input, cx| input.focus(window, cx));
        }
        self.model_ui = Some(ModelPickerView {
            request_id,
            search,
            selected: 0,
            catalog_loaded: false,
            scroll: ScrollHandle::new(),
            focus,
            cancel_focus: cx.focus_handle(),
            apply_focus: cx.focus_handle(),
            return_focus,
            _search: subscription,
        });
    }
    fn focus_model_control(&self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.model_ui else {
            return;
        };
        if next {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        if !view.focus.contains_focused(window, cx) {
            let picker = &self.controller.read(cx).model.models;
            if next {
                window.focus(&view.focus, cx);
                window.focus_next(cx);
                if !view.focus.contains_focused(window, cx) {
                    window.focus(&view.focus, cx);
                }
            } else if !picker.pending()
                && (picker.preview().is_some()
                    || matches!(picker.phase.step(), SelectionStep::Reasoning { .. }))
            {
                window.focus(&view.apply_focus, cx);
            } else if !matches!(picker.phase, ModelPhase::Cancelling { .. }) {
                window.focus(&view.cancel_focus, cx);
            } else {
                window.focus(&view.focus, cx);
            }
        }
    }
    #[cfg(test)]
    pub(super) fn model_picker_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.model_ui
            .as_ref()
            .is_some_and(|v| v.focus.contains_focused(window, cx))
    }
    fn filtered_models(&self, cx: &App) -> Vec<usize> {
        let Some(view) = &self.model_ui else {
            return Vec::new();
        };
        let query = view.search.read(cx).value().to_lowercase();
        self.controller
            .read(cx)
            .model
            .models
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                model
                    .context
                    .profile
                    .to_string()
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }
    fn navigate_models(&mut self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        let picker = &self.controller.read(cx).model.models;
        if picker.pending() || picker.preview().is_some() {
            return;
        }
        let Some(view) = &self.model_ui else {
            return;
        };
        if !view.search.focus_handle(cx).contains_focused(window, cx)
            && let SelectionStep::Reasoning { profile, selected } = picker.phase.step()
        {
            let len = picker.profiles[profile].reasoning_levels.len();
            let effort = if next {
                (selected + 1).min(len.saturating_sub(1))
            } else {
                selected.saturating_sub(1)
            };
            self.controller
                .update(cx, |this, cx| this.select_model_effort(effort, cx));
        } else {
            let len = self.filtered_models(cx).len();
            let view = self.model_ui.as_mut().unwrap();
            view.selected = if next {
                (view.selected + 1).min(len.saturating_sub(1))
            } else {
                view.selected.saturating_sub(1)
            };
            view.scroll.scroll_to_item(view.selected);
            cx.notify();
        }
    }
    fn accept_model_choice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = &self.controller.read(cx).model.models;
        if picker.pending() {
            return;
        }
        if picker.preview().is_some() {
            self.controller
                .update(cx, |this, cx| this.confirm_model_conversion(cx));
        } else if self
            .model_ui
            .as_ref()
            .is_some_and(|view| view.search.focus_handle(cx).contains_focused(window, cx))
            || matches!(picker.phase.step(), SelectionStep::Model)
        {
            let selected = self.model_ui.as_ref().map_or(0, |view| view.selected);
            if let Some(profile) = self.filtered_models(cx).get(selected).copied() {
                self.controller
                    .update(cx, |this, cx| this.select_model(profile, cx));
                if let Some(view) = &self.model_ui {
                    window.focus(&view.focus, cx);
                }
            }
        } else {
            self.controller
                .update(cx, |this, cx| this.apply_model_selection(cx));
        }
    }
    pub(super) fn model_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.model_ui.as_ref()?;
        let picker = &self.controller.read(cx).model.models;
        if !picker.is_open() {
            return None;
        }
        let t = palette(cx);
        let tone = tones(cx);
        let pending = picker.pending();
        let cancelling = matches!(picker.phase, ModelPhase::Cancelling { .. });
        let preview = picker.preview().cloned();
        let step = picker.phase.step();
        let filtered = self.filtered_models(cx);
        let selected = view.selected.min(filtered.len().saturating_sub(1));
        let mode = picker.mode.map_or("", |m| match m {
            zevria_foundation::SessionMode::Build => "Build",
            zevria_foundation::SessionMode::Plan => "Plan",
        });
        let mut sheet = style::sheet(cx)
            .id("model-picker")
            .test_support()
            .role(Role::Dialog)
            .aria_label(format!("Change {mode} model and thinking effort"))
            .key_context("ZevriaModels")
            .track_focus(&view.focus)
            .tab_group()
            .font_family(style::UI_FONT)
            .w(px(560.))
            .max_w(relative(0.92))
            .max_h(px(f32::from(window.viewport_size().height) - 48.))
            .min_h_0()
            // Search uses explicit Input-context bindings. Native buttons must
            // retain Enter/Space activation (especially Cancel); only the
            // sheet's own focus uses these selection/navigation defaults.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if !this
                    .model_ui
                    .as_ref()
                    .is_some_and(|v| v.focus.is_focused(window))
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "enter" | "up" | "down" if event.is_held => {
                        cx.stop_propagation();
                    }
                    "enter" => {
                        this.accept_model_choice(window, cx);
                        cx.stop_propagation();
                    }
                    "up" => {
                        this.navigate_models(false, window, cx);
                        cx.stop_propagation();
                    }
                    "down" => {
                        this.navigate_models(true, window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .on_action(cx.listener(|this, _: &actions::ModelPrevious, window, cx| {
                this.navigate_models(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ModelNext, window, cx| {
                this.navigate_models(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ModelAccept, window, cx| {
                this.accept_model_choice(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ModelFocusNext, window, cx| {
                    this.focus_model_control(true, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::ModelFocusPrevious, window, cx| {
                    this.focus_model_control(false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ModelBack, window, cx| {
                let picker = &this.controller.read(cx).model.models;
                if picker.pending() || picker.preview().is_some() {
                    return;
                }
                this.controller.update(cx, |this, cx| {
                    this.model.models.back();
                    cx.notify();
                });
                if let Some(view) = &this.model_ui {
                    view.search.update(cx, |input, cx| input.focus(window, cx));
                }
            }))
            .child(
                style::HEADINGS[0]
                    .apply(div())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Change {mode} model")),
            )
            .child(
                style::label()
                    .text_color(t.text.secondary)
                    .child(picker.scope_label()),
            )
            .child(
                style::label()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text.secondary)
                    .child(format!(
                        "Current: {}",
                        picker
                            .current
                            .as_ref()
                            .map_or_else(|| "Loading…".into(), ToString::to_string)
                    )),
            );
        let mut body = div()
            .id("model-picker-body")
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.));
        if pending {
            body = body.child(style::body().child(if cancelling {
                "Cancelling model selection…"
            } else if matches!(picker.phase, ModelPhase::Loading) {
                "Loading configured models…"
            } else {
                "Applying model selection…"
            }));
        } else if let Some(preview) = &preview {
            body = body
                .child(style::HEADINGS[1].apply(div()).font_weight(FontWeight::SEMIBOLD).child("Convert conversation context?"))
                .child(style::body().child(format!("{} → {}", preview.source, preview.target)))
                .child(style::body().text_color(t.text.secondary).child(preview.reason.clone()))
                .child(style::body().text_color(t.text.secondary).child("Conversion calls the source model to summarize shared root context. It costs tokens and can lose detail. No model change is applied until you confirm."));
        } else {
            body = body.child(
                Textarea::new(&view.search)
                    .aria_label("Search configured models")
                    .text_size(px(style::READING.size))
                    .line_height(px(style::READING.leading)),
            );
            let mut list = div()
                .id("model-options")
                .track_scroll(&view.scroll)
                .role(Role::ListBox)
                .aria_label("Configured models")
                .flex()
                .flex_col()
                .gap(px(2.))
                .max_h(px(200.))
                .overflow_y_scroll();
            for (position, index) in filtered.iter().copied().enumerate() {
                let candidate = &picker.profiles[index];
                let current = picker
                    .current
                    .as_ref()
                    .is_some_and(|current| current.profile == candidate.context.profile);
                let chosen =
                    matches!(step, SelectionStep::Reasoning { profile, .. } if profile == index);
                let label = format!(
                    "{}{}",
                    candidate.context.profile,
                    if current { " (current)" } else { "" }
                );
                list = list.child(
                    div()
                        .id(("model-option", index))
                        .test_support()
                        .role(Role::ListBoxOption)
                        .aria_selected(
                            chosen
                                || (matches!(step, SelectionStep::Model) && position == selected),
                        )
                        .rounded(px(6.))
                        .when(chosen || position == selected, |d| d.bg(tone.list_active))
                        .child(
                            Button::new(("choose-model", index))
                                .ghost()
                                .w_full()
                                .h_auto()
                                .py(px(8.))
                                .accessibility_label(label.clone())
                                .disabled(candidate.reasoning_levels.is_empty())
                                .child(div().w_full().text_left().min_w_0().truncate().child(label))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.controller
                                        .update(cx, |this, cx| this.select_model(index, cx));
                                    if let Some(view) = &this.model_ui {
                                        window.focus(&view.focus, cx);
                                    }
                                })),
                        ),
                );
            }
            body = body.child(list);
            if filtered.is_empty() {
                body = body.child(style::body().text_color(t.text.secondary).child(if picker.profiles.is_empty() { "No configured models are available. Add a model to your configuration, then reopen this picker." } else { "No models match. Try another search." }));
            }
            if let SelectionStep::Reasoning { profile, selected } = step {
                let candidate = &picker.profiles[profile];
                let mut efforts = div()
                    .id("model-efforts")
                    .test_support()
                    .role(Role::ListBox)
                    .aria_label("Supported thinking efforts")
                    .flex()
                    .flex_wrap()
                    .gap(px(8.));
                for (index, level) in candidate.reasoning_levels.iter().enumerate() {
                    let active = picker.current.as_ref().is_some_and(|c| {
                        c.profile == candidate.context.profile && c.reasoning_level == *level
                    });
                    let label = format!(
                        "{}{}{}",
                        if index == selected { "✓ " } else { "" },
                        level,
                        if active { " (current)" } else { "" }
                    );
                    efforts = efforts.child(
                        Button::new(("model-effort", index))
                            .ghost()
                            .small()
                            .label(label)
                            .when(index == selected, |b| b.bg(tone.list_active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.controller
                                    .update(cx, |this, cx| this.select_model_effort(index, cx))
                            })),
                    );
                }
                body = body.child(style::label().child(format!("Thinking effort for {}", candidate.context.profile))).child(efforts)
                    .child(style::label().text_color(t.text.secondary).child("Changing only effort preserves thinking, tool records and native context. Normal next-turn capacity checks still apply."));
            } else {
                body = body.child(
                    style::label()
                        .text_color(t.text.secondary)
                        .child("Choose a model, then confirm a supported thinking effort."),
                );
            }
        }
        if let Some(error) = picker.error() {
            body = body.child(
                style::body()
                    .id("model-error")
                    .test_support()
                    .text_color(t.feedback.error)
                    .child(error.to_owned()),
            );
        }
        sheet = sheet.child(body).child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(8.))
                .child(
                    Button::new("model-cancel")
                        .track_focus(&view.cancel_focus)
                        .ghost()
                        .label("Cancel")
                        .disabled(cancelling)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller
                                .update(cx, |this, cx| this.cancel_model_selection(cx))
                        })),
                )
                .child(
                    Button::new("model-apply")
                        .track_focus(&view.apply_focus)
                        .label(if preview.is_some() {
                            "Convert"
                        } else {
                            "Apply"
                        })
                        .disabled(
                            pending
                                || (preview.is_none()
                                    && !matches!(step, SelectionStep::Reasoning { .. })),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                if this.model.models.preview().is_some() {
                                    this.confirm_model_conversion(cx);
                                } else {
                                    this.apply_model_selection(cx);
                                }
                            })
                        })),
                ),
        );
        if view.focus.is_focused(window) {
            sheet = sheet.relative().child(style::focus_ring(14., cx));
        }
        Some(
            style::scrim(cx)
                .id("model-picker-scrim")
                .test_support()
                .child(sheet)
                .into_any_element(),
        )
    }
}
