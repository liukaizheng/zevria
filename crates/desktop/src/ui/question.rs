use super::{style, theme::palette, window::DesktopWindow};
use gpui_kit::component::{
    Disableable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{InputEvent, Textarea, TextareaState},
    radio::Radio,
};
use gpui_kit::{prelude::*, *};
use zevria_foundation::QuestionPromptKind;
impl DesktopWindow {
    pub(super) fn sync_question(
        &mut self,
        question: Option<(String, Vec<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((id, values)) = question else {
            self.question_id.clear();
            self.question_inputs.clear();
            self.question_subscriptions.clear();
            return;
        };
        if id == self.question_id {
            return;
        }
        self.question_id = id;
        self.question_inputs.clear();
        self.question_subscriptions.clear();
        for (index, value) in values.into_iter().enumerate() {
            let request_id = self.question_id.clone();
            let input = cx.new(|cx| {
                let mut input = TextareaState::new(window, cx).auto_grow(1, 4);
                let end = value.len();
                input.set_value(value, window, cx);
                input.set_selected_range(end..end, cx);
                input
            });
            let subscription = cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        this.controller.update(cx, |this, cx| {
                            if let Some(question) = this.model.question.as_mut()
                                && question.request.id.as_str() == request_id.as_str()
                                && let Some(answer) = question.answers.get_mut(index)
                            {
                                answer.text = value;
                                question.error = None;
                            }
                            cx.notify();
                        });
                    }
                },
            );
            self.question_inputs.push(input);
            self.question_subscriptions.push(subscription);
        }
    }
    pub(super) fn question_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.controller.read(cx).model.question.as_ref()?;
        let t = palette(cx);
        let pending = form.pending;
        let mut fields = div()
            .id("question-fields")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4();
        for (index, (question, answer)) in
            form.request.questions.iter().zip(&form.answers).enumerate()
        {
            let mut field = div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(question.header.clone()),
                )
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(t.text.secondary)
                        .child(question.question.clone()),
                );
            let multiple = matches!(question.kind, QuestionPromptKind::MultiSelect { .. });
            for (option_index, option) in question.options.iter().enumerate() {
                let label = div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(option.label.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(t.text.muted)
                            .child(option.description.clone()),
                    );
                let checked =
                    answer.selected.contains(&option_index) && (multiple || !answer.other);
                let on_change = cx.listener(move |this, value: &bool, _: &mut Window, cx| {
                    this.controller.update(cx, |this, cx| {
                        if let Some(form) = this.model.question.as_mut() {
                            let answer = &mut form.answers[index];
                            if multiple {
                                answer.selected.retain(|i| *i != option_index);
                                if *value {
                                    answer.selected.push(option_index);
                                }
                            } else {
                                answer.selected = if *value { vec![option_index] } else { vec![] };
                            }
                            if !multiple {
                                answer.other = false;
                            }
                            answer.skip = false;
                            form.error = None;
                        }
                        cx.notify();
                    })
                });
                field = if multiple {
                    field.child(
                        Checkbox::new(("question-option", index * 1000 + option_index))
                            .checked(checked)
                            .disabled(pending)
                            .w_full()
                            .py(px(4.))
                            .accessibility_label(option.label.clone())
                            .child(label)
                            .on_change(on_change),
                    )
                } else {
                    field.child(
                        Radio::new(("question-option", index * 1000 + option_index))
                            .checked(checked)
                            .disabled(pending)
                            .w_full()
                            .py(px(4.))
                            .accessibility_label(option.label.clone())
                            .child(label)
                            .on_change(on_change),
                    )
                };
            }
            let allow_other = matches!(
                question.kind,
                QuestionPromptKind::SingleSelect { allow_other: true }
                    | QuestionPromptKind::MultiSelect {
                        allow_other: true,
                        ..
                    }
            );
            if allow_other {
                field = field.child(
                    Checkbox::new(("question-other", index))
                        .label("Other")
                        .checked(answer.other)
                        .disabled(pending)
                        .on_change(cx.listener(move |this, value, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                if let Some(form) = this.model.question.as_mut() {
                                    form.answers[index].other = *value;
                                    form.answers[index].skip = false;
                                }
                                cx.notify();
                            })
                        })),
                );
            }
            if (matches!(question.kind, QuestionPromptKind::Text { .. })
                || (allow_other && answer.other))
                && let Some(input) = self.question_inputs.get(index)
            {
                field = field.child(Textarea::new(input).disabled(pending));
            }
            if !question.required {
                field = field.child(
                    Checkbox::new(("question-skip", index))
                        .label("Skip this question")
                        .checked(answer.skip)
                        .disabled(pending)
                        .on_change(cx.listener(move |this, value, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                if let Some(form) = this.model.question.as_mut() {
                                    form.answers[index].skip = *value;
                                }
                                cx.notify();
                            })
                        })),
                );
            }
            fields = fields.child(field);
        }
        let count = form.request.questions.len();
        let mut dialog = style::sheet(cx)
            .w(px((f32::from(window.viewport_size().width) - 64.).min(600.)))
            .max_h(px(
                (f32::from(window.viewport_size().height) - 96.).max(180.)
            ))
            .child(
                div()
                    .text_size(px(18.))
                    .line_height(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if count == 1 {
                        "Zevria has a question".into()
                    } else {
                        format!("Zevria has {count} questions")
                    }),
            );
        if let Some(source) = &form.request.source_label {
            dialog = dialog.child(
                style::label()
                    .text_color(t.text.muted)
                    .child(format!("Asked by {source}")),
            );
        }
        dialog = dialog.child(fields);
        if let Some(error) = &form.error {
            dialog = dialog.child(
                div()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .text_color(t.feedback.error)
                    .child(error.clone()),
            );
        }
        let answer_button = Button::new("answer-question")
            .primary()
            .label(if pending {
                "Sending…"
            } else if count == 1 {
                "Send answer"
            } else {
                "Send answers"
            })
            .disabled(pending)
            .on_click(cx.listener(|this, _, _, cx| {
                this.controller
                    .update(cx, |this, cx| this.answer_question(false, cx))
            }));
        let mut buttons = div().flex().items_center().justify_end().gap(px(8.));
        if form.request.dismissible {
            buttons = buttons.child(
                Button::new("dismiss-question")
                    .ghost()
                    .label("Dismiss")
                    .disabled(pending)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |this, cx| this.answer_question(true, cx))
                    })),
            );
        }
        Some(
            style::scrim(cx)
                .id("question-dialog")
                .child(dialog.child(buttons.child(answer_button)))
                .into_any_element(),
        )
    }
}
