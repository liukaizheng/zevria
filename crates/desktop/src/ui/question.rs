use super::{theme::palette, window::DesktopWindow};
use gpui_kit::component::{
    Disableable,
    button::Button,
    checkbox::Checkbox,
    input::{InputEvent, Textarea, TextareaState},
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
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.controller.read(cx).model.question.as_ref()?;
        let t = palette(cx);
        let pending = form.pending;
        let mut fields = div()
            .id("question-fields")
            .max_h(px(500.))
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
                .child(div().text_sm().child(question.header.clone()))
                .child(question.question.clone());
            let multiple = matches!(question.kind, QuestionPromptKind::MultiSelect { .. });
            for (option_index, option) in question.options.iter().enumerate() {
                field = field.child(
                    Checkbox::new(("question-option", index * 1000 + option_index))
                        .label(format!("{} — {}", option.label, option.description))
                        .checked(
                            answer.selected.contains(&option_index) && (multiple || !answer.other),
                        )
                        .disabled(pending)
                        .on_change(cx.listener(move |this, value, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                if let Some(form) = this.model.question.as_mut() {
                                    let answer = &mut form.answers[index];
                                    if multiple {
                                        answer.selected.retain(|i| *i != option_index);
                                        if *value {
                                            answer.selected.push(option_index);
                                        }
                                    } else {
                                        answer.selected =
                                            if *value { vec![option_index] } else { vec![] };
                                    }
                                    if !multiple {
                                        answer.other = false;
                                    }
                                    answer.skip = false;
                                    form.error = None;
                                }
                                cx.notify();
                            })
                        })),
                );
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
            if (matches!(question.kind, QuestionPromptKind::Text { .. }) || allow_other)
                && let Some(input) = self.question_inputs.get(index)
            {
                field = field.child(Textarea::new(input).disabled(pending));
            }
            if !question.required {
                field = field.child(
                    Checkbox::new(("question-skip", index))
                        .label("Skip this optional question")
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
        let mut dialog = div()
            .w(px(640.))
            .max_h(px(720.))
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .bg(t.surfaces.overlay)
            .border_1()
            .border_color(t.surfaces.border_strong)
            .rounded_lg()
            .child(div().text_lg().child("Zevria needs your input"))
            .child(fields);
        if let Some(error) = &form.error {
            dialog = dialog.child(div().text_color(t.feedback.error).child(error.clone()));
        }
        let mut buttons = div().flex().gap_2().child(
            Button::new("answer-question")
                .label(if pending { "Sending…" } else { "Answer" })
                .disabled(pending)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller
                        .update(cx, |this, cx| this.answer_question(false, cx))
                })),
        );
        if form.request.dismissible {
            buttons = buttons.child(
                Button::new("dismiss-question")
                    .label("Dismiss")
                    .disabled(pending)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |this, cx| this.answer_question(true, cx))
                    })),
            );
        }
        Some(
            div()
                .id("question-dialog")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.surfaces.canvas)
                .child(dialog.child(buttons))
                .into_any_element(),
        )
    }
}
