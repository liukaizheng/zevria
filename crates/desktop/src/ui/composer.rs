use super::{
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::controller::Phase;
use gpui_kit::component::{
    Disableable, IconName, Sizable,
    button::{Button, ButtonVariants, Toggle, ToggleGroup},
    input::Textarea,
    switch::Switch,
};
use gpui_kit::{prelude::*, *};
use zevria_foundation::SessionMode;
impl DesktopWindow {
    pub(super) fn submit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let draft = self.composer.read(cx).value().to_string();
        self.controller.update(cx, |this, cx| {
            if this.draft != draft {
                this.edit_draft(draft, cx);
            }
            this.submit(cx);
        });
    }
    pub(super) fn completions(&self, cx: &App) -> Vec<(String, String)> {
        let text = self.composer.read(cx).value();
        let cursor = self.composer.read(cx).cursor();
        if self.completion_dismissed.as_ref() == Some(&(text.to_string(), cursor)) {
            return Vec::new();
        }
        self.controller
            .read(cx)
            .model
            .registry
            .matches(&text, cursor)
            .into_iter()
            .take(8)
            .map(|e| {
                (
                    format!("{}{}", e.sigil(), e.name()),
                    e.description().to_string(),
                )
            })
            .collect()
    }
    pub(super) fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.completions(cx);
        if let Some((value, _)) = matches.get(
            self.completion_selected
                .min(matches.len().saturating_sub(1)),
        ) {
            let text = self.composer.read(cx).value().to_string();
            let cursor = self.composer.read(cx).cursor();
            let Some(query) = zevria_presentation::completion::command_query(&text, cursor) else {
                return;
            };
            let caret = query.replacement.start + value.len() + 1;
            let mut replacement = text;
            replacement.replace_range(query.replacement(), &format!("{value} "));
            self.composer.update(cx, |input, cx| {
                input.set_value(replacement.clone(), window, cx);
                input.set_selected_range(caret..caret, cx);
            });
            self.controller
                .update(cx, |this, cx| this.edit_draft(replacement, cx));
        }
    }
    pub(super) fn composer_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = palette(cx);
        let tone = tones(cx);
        let controller = self.controller.read(cx);
        let busy = controller.model.root.is_busy();
        let mode = controller.model.root.next_mode();
        let editable = controller.phase == Phase::Live
            && controller.model.root.workflow.dialog().is_none()
            && controller.model.question.is_none();
        let orchestrate = controller.orchestrate;
        let mode_disabled = !editable || busy;
        let cancelling = controller.cancelling;
        let recovery = controller.recovery.last().is_some();
        let measure = style::Measure {
            compact: self.compact,
        };
        let mut modes = measure
            .gutter()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(4.))
            .pt(px(8.));
        for (id, label, value, ink) in [
            ("mode-build", "Build", SessionMode::Build, t.workflow.build),
            ("mode-plan", "Plan", SessionMode::Plan, t.workflow.plan),
        ] {
            modes = modes.child(
                Button::new(id)
                    .ghost()
                    .small()
                    .accessibility_label(label)
                    .disabled(mode_disabled)
                    .text_color(if value == mode { ink } else { t.text.muted })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .when(value == mode, |row| {
                                row.child(div().text_size(px(8.)).child("●"))
                            })
                            .child(
                                style::label()
                                    .font_weight(if value == mode {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .child(label),
                            ),
                    )
                    .tooltip(format!("Next turn runs in {label} mode"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller
                            .update(cx, |this, cx| this.select_mode(value, cx))
                    })),
            );
        }
        let mut completions = div()
            .id("completions")
            .role(Role::ListBox)
            .aria_label("Command and skill completions")
            .absolute()
            .bottom(relative(1.))
            .mb(px(8.))
            .left_0()
            .w_full()
            .p(px(4.))
            .rounded(px(10.))
            .bg(t.surfaces.overlay)
            .shadow_lg()
            .occlude()
            .flex()
            .flex_col();
        let matches = self.completions(cx);
        let selected = self
            .completion_selected
            .min(matches.len().saturating_sub(1));
        for (index, (name, description)) in matches.iter().enumerate() {
            let row = Button::new(("completion", index))
                .ghost()
                .w_full()
                .h_auto()
                .px(px(8.))
                .py(px(8.))
                .accessibility_label(format!("{name}, {description}"))
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(style::mono().child(name.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .line_height(px(16.))
                                .text_color(if index == selected {
                                    t.text.secondary
                                } else {
                                    t.text.muted
                                })
                                .child(description.clone()),
                        ),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.completion_selected = index;
                    this.accept_completion(window, cx);
                }));
            completions = completions.child(
                div()
                    .id(("completion-state", index))
                    .test_support()
                    .role(Role::ListBoxOption)
                    .aria_selected(index == selected)
                    .rounded(px(6.))
                    .when(index == selected, |d| d.bg(tone.list_active))
                    .child(row),
            );
        }
        let focus = self.composer.focus_handle(cx);
        let mut field = div()
            .id("composer-panel")
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.))
            .bg(t.surfaces.panel)
            .rounded(px(10.))
            .border_1()
            .border_color(t.surfaces.border)
            .track_focus(&focus)
            .focus(move |s| s.border_color(t.surfaces.border_strong))
            .px(px(14.))
            .py(px(12.))
            .child(
                div()
                    .id("composer")
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .child(
                        Textarea::new(&self.composer)
                            .appearance(false)
                            .disabled(!editable)
                            .aria_label("Message to Zevria")
                            .text_size(px(14.))
                            .line_height(px(20.)),
                    ),
            );
        if !matches.is_empty() {
            field = field.child(completions);
        }
        let mut footer = div().flex().flex_wrap().items_center().gap(px(8.));
        if self.compact {
            let mut group = ToggleGroup::new("mode-switch")
                .segmented()
                .small()
                .disabled(mode_disabled);
            for (id, label, value, ink, tint) in [
                (
                    "mode-build",
                    "Build",
                    SessionMode::Build,
                    t.workflow.build,
                    tone.build_tint,
                ),
                (
                    "mode-plan",
                    "Plan",
                    SessionMode::Plan,
                    t.workflow.plan,
                    tone.plan_tint,
                ),
            ] {
                group = group.child(
                    Toggle::new(id)
                        .label(label)
                        .checked(mode == value)
                        .tooltip(format!("Next turn runs in {label} mode"))
                        .text_size(px(12.))
                        .when(mode == value, |b| b.bg(tint).text_color(ink)),
                );
            }
            // Kit owns the item callbacks in a ToggleGroup. Resolve the one
            // changed segment here, including reselecting the active segment:
            // a mode switch must never become a multi-select or turnless state.
            footer = footer.child(group.on_click(cx.listener(
                move |this, values: &Vec<bool>, _, cx| {
                    let value = if values.first().copied() != Some(mode == SessionMode::Build) {
                        SessionMode::Build
                    } else {
                        SessionMode::Plan
                    };
                    this.controller
                        .update(cx, |this, cx| this.select_mode(value, cx));
                },
            )));
        }
        if mode == SessionMode::Build {
            footer = footer.child(
                Switch::new("orchestrate")
                    .small()
                    .label("Orchestrate subtasks")
                    .checked(orchestrate)
                    .disabled(mode_disabled)
                    .on_change(cx.listener(|this, value, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.orchestrate = *value;
                            cx.notify();
                        })
                    })),
            );
        }
        footer = footer.child(div().flex_1());
        if !self.compact {
            footer = footer.child(style::kbd_hint(
                "Enter to send, Shift-Enter for a new line",
                cx,
            ));
        }
        if recovery {
            footer = footer.child(
                Button::new("recover-draft")
                    .ghost()
                    .small()
                    .label("Recover draft")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            if this.draft.is_empty()
                                && let Some(draft) = this.recovery.pop()
                            {
                                this.draft = draft;
                            }
                            cx.notify();
                        })
                    })),
            );
        }
        if busy {
            footer = footer.child(
                Button::new("stop")
                    .danger()
                    .outline()
                    .small()
                    .icon(IconName::Square)
                    .label(if cancelling { "Stopping…" } else { "Stop" })
                    .disabled(cancelling)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| this.cancel(cx))
                    })),
            );
        } else {
            footer = footer.child(
                Button::new("send")
                    .primary()
                    .small()
                    .label("Send")
                    .disabled(!editable)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            );
        }
        field = field.child(footer);
        div()
            .px(px(16.))
            .pt(px(12.))
            .pb(px(16.))
            .child(
                measure
                    .row()
                    .when(!self.compact, |row| row.child(modes))
                    .child(field),
            )
            .into_any_element()
    }
}
