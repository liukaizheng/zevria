use super::{
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::{actions::Shortcut, controller::Phase};
use gpui_kit::component::{
    Disableable, Icon, IconName, Sizable, ThemeStyled,
    button::{Button, ButtonVariants, Toggle, ToggleGroup},
    input::Textarea,
    switch::Switch,
};
use gpui_kit::{assets::IconName as Lucide, prelude::*, *};
use zevria_foundation::SessionMode;
impl DesktopWindow {
    pub(super) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let controller = self.controller.read(cx);
        if controller.model.controls.models.is_open() || controller.edit.is_awaiting_acceptance() {
            return;
        }
        let draft = self.composer.read(cx).value().to_string();
        self.controller.update(cx, |this, cx| {
            if this.draft != draft {
                this.edit_draft(draft, cx);
            }
            this.submit(cx);
        });
        // Reflect admission before handling any subsequent native editor input.
        self.sync_composer_draft(false, window, cx);
        if self.controller.read(cx).edit.is_awaiting_acceptance() {
            self.transcript
                .update(cx, |view, cx| view.jump_to_latest(cx));
            // Resend disappears while the edit is pending. Keep the focus
            // in the composer so Escape can still stop the operation.
            self.composer
                .update(cx, |input, cx| input.focus(window, cx));
        }
    }
    pub(super) fn completions(&self, cx: &App) -> Vec<(String, String)> {
        let controller = self.controller.read(cx);
        if controller.model.controls.models.is_open() || controller.edit.is_awaiting_acceptance() {
            return Vec::new();
        }
        let text = self.composer.read(cx).value();
        let cursor = self.composer.read(cx).cursor();
        if self.completion_dismissed.as_ref() == Some(&(text.to_string(), cursor)) {
            return Vec::new();
        }
        self.controller
            .read(cx)
            .model
            .controls
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
    fn edit_banner(&self, applying: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
        let title = if applying {
            "Applying edited message…"
        } else {
            "Editing earlier message"
        };
        let description = if self.compact {
            "Replaces this and later turns"
        } else {
            "Sending replaces this message and later turns"
        };
        div()
            .id("composer-edit-banner")
            .test_support()
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(tones(cx).hairline)
            .pb(px(6.))
            .child(
                Icon::new(Lucide::Pencil)
                    .size(px(14.))
                    .text_color(t.roles.you),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(8.))
                    .when(self.compact, |d| d.flex_col().items_start())
                    .child(style::label().child(style::observed_text(title)))
                    .when(!applying, |d| {
                        d.child(style::meta().text_color(t.text.muted).child(description))
                    }),
            )
            .when(!applying, |d| {
                d.child(
                    Button::new("cancel-edit")
                        .ghost()
                        .xsmall()
                        .label("Cancel")
                        .tooltip("Cancel editing (Esc)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |controller, cx| {
                                controller.cancel_recall(cx);
                            });
                        })),
                )
            })
    }

    fn composer_attachments(&self, editable: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
        let mut strip = div()
            .id("composer-attachments")
            .test_support()
            .flex()
            .flex_wrap()
            .gap(px(4.));
        for (index, image) in self.controller.read(cx).draft_images.iter().enumerate() {
            let (width, height) = image.dimensions();
            let format = image.mime_type().trim_start_matches("image/");
            let label = format!("Image {} · {format} · {width}×{height}", index + 1);
            strip = strip.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .pl(px(6.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(tones(cx).hairline)
                    .child(
                        style::meta()
                            .text_color(t.text.muted)
                            .child(style::observed_text(label)),
                    )
                    .child(
                        style::icon_button(
                            ("remove-attachment", index),
                            Lucide::X,
                            format!("Remove image {}", index + 1),
                        )
                        .disabled(!editable)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.controller.update(cx, |controller, cx| {
                                controller.remove_draft_image(index, cx)
                            });
                        })),
                    ),
            );
        }
        strip
    }

    pub(super) fn composer_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = palette(cx);
        let tone = tones(cx);
        let controller = self.controller.read(cx);
        let busy = controller.model.root.is_busy();
        let mode = controller.model.root.next_mode();
        let recalling = controller.edit.is_recalling();
        let applying_edit = controller.edit.is_awaiting_acceptance();
        let editable = controller.phase == Phase::Live
            && controller.model.root.workflow.dialog().is_none()
            && controller.model.controls.question.is_none()
            && !controller.model.controls.models.is_open()
            && !controller.capturing_dialog()
            && !applying_edit;
        let orchestrate = controller.orchestrate;
        let mode_disabled = !editable || busy || recalling;
        let cancelling = controller.cancelling;
        let recovery = controller.edit.is_none() && !controller.draft_slot.recovery().is_empty();
        let has_images = !controller.draft_images.is_empty();
        let empty = controller.draft.is_empty() && !has_images;
        let send_label = if recalling { "Resend" } else { "Send" };
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
        let focused = focus.contains_focused(window, cx);
        let hints = !applying_edit && (focused || empty);
        let mut field = div()
            .id("composer-panel")
            .test_support()
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.))
            .bg(t.surfaces.panel)
            .rounded(px(12.))
            .border_1()
            .border_color(tone.hairline)
            .track_focus(&focus)
            .px(px(14.))
            .py(px(12.))
            .when(recalling || applying_edit, |d| {
                d.child(self.edit_banner(applying_edit, cx))
            })
            .when(has_images, |d| {
                d.child(self.composer_attachments(editable, cx))
            })
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
        if mode == SessionMode::Build {
            footer = footer.child(
                Switch::new("orchestrate")
                    .small()
                    .label(if self.compact {
                        "Orchestrate"
                    } else {
                        "Orchestrate subtasks"
                    })
                    .checked(orchestrate)
                    .disabled(mode_disabled)
                    .on_change(cx.listener(|this, value, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            if this.edit.is_none() {
                                this.orchestrate = *value;
                                cx.notify();
                            }
                        })
                    })),
            );
        }
        footer = footer.child(div().flex_1());
        if !self.compact && hints {
            footer = footer
                .child(style::key_hint(Shortcut::ENTER, send_label, cx))
                .child(style::key_hint(Shortcut::SHIFT_ENTER, "New line", cx))
                .when(recalling, |d| {
                    d.child(style::key_hint(Shortcut::ESCAPE, "Cancel", cx))
                });
        }
        if recovery {
            footer = footer.child(
                Button::new("recover-draft")
                    .ghost()
                    .small()
                    .label("Recover draft")
                    .disabled(!editable || !empty)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |controller, cx| controller.recover_draft(cx));
                    })),
            );
        }
        if busy {
            footer = footer.child(
                Button::new("stop")
                    .outline()
                    .small()
                    .icon(IconName::Square)
                    .label(if cancelling { "Stopping" } else { "Stop" })
                    .tooltip(format!("Stop {}", Shortcut::STOP.label()))
                    .disabled(cancelling)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| this.cancel(cx))
                    })),
            );
        } else {
            footer = footer.child(
                Button::new("send")
                    .custom(style::mode_button(mode, cx))
                    .small()
                    .label(send_label)
                    .disabled(!editable)
                    .when(!editable, |b| b.opacity(0.5))
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            );
        }
        field = field.child(footer);
        if self.compact && hints {
            field = field.child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(style::key_hint(Shortcut::ENTER, send_label, cx))
                    .child(style::key_hint(Shortcut::SHIFT_ENTER, "New line", cx))
                    .when(recalling, |d| {
                        d.child(style::key_hint(Shortcut::ESCAPE, "Cancel", cx))
                    }),
            );
        }
        if focused {
            field = field.focus_ring_style(window, cx);
        }
        div()
            .px(px(style::COLUMN_INSET))
            .pt(px(12.))
            .pb(px(16.))
            .child(
                style::Measure::column()
                    .id("composer-column")
                    .test_support()
                    .child(field),
            )
            .into_any_element()
    }
}
