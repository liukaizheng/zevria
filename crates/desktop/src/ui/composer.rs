use super::{theme::palette, window::DesktopWindow};
use crate::controller::Phase;
use gpui_kit::component::{Disableable, button::Button, checkbox::Checkbox, input::Textarea};
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
            .map(|entry| {
                (
                    format!("{}{}", entry.sigil(), entry.name()),
                    entry.description().to_string(),
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
        let mut composer = div()
            .id("composer-panel")
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(t.surfaces.border)
            .bg(t.surfaces.panel);
        let mut completions = div().flex().flex_col().gap_1();
        for (index, (name, description)) in self.completions(cx).into_iter().enumerate() {
            completions = completions.child(
                Button::new(("completion", index))
                    .label(format!("{name} — {description}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.completion_selected = index;
                        this.accept_completion(window, cx);
                    })),
            );
        }
        composer = composer.child(completions).child(
            div()
                .id("composer")
                .child(Textarea::new(&self.composer).disabled(!editable)),
        );
        let mut controls = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("mode-build")
                    .label(if mode == SessionMode::Build {
                        "● Build"
                    } else {
                        "Build"
                    })
                    .disabled(mode_disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |this, cx| this.select_mode(SessionMode::Build, cx))
                    })),
            )
            .child(
                Button::new("mode-plan")
                    .label(if mode == SessionMode::Plan {
                        "● Plan"
                    } else {
                        "Plan"
                    })
                    .disabled(mode_disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |this, cx| this.select_mode(SessionMode::Plan, cx))
                    })),
            )
            .child(
                Checkbox::new("orchestrate")
                    .label("Orchestrate")
                    .checked(orchestrate)
                    .disabled(mode_disabled || mode != SessionMode::Build)
                    .on_change(cx.listener(|this, value, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.orchestrate = *value;
                            cx.notify();
                        })
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(t.text.muted)
                    .child("Enter to send · Shift-Enter for newline"),
            );
        if recovery {
            controls = controls.child(
                Button::new("recover-draft")
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
            controls = controls.child(
                Button::new("stop")
                    .label(if cancelling { "Cancelling…" } else { "Stop" })
                    .disabled(cancelling)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| this.cancel(cx))
                    })),
            );
        } else {
            controls = controls.child(
                Button::new("send")
                    .label("Send")
                    .disabled(!editable)
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            );
        }
        composer.child(controls).into_any_element()
    }
}
