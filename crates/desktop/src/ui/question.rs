use super::{
    blocks::code_block_header,
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::{actions::Shortcut, session::question::QuestionForm};
use gpui_kit::base::text::TextView;
use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{InputEvent, Textarea, TextareaState},
    radio::Radio,
    text::TextViewState,
};
use gpui_kit::{prelude::*, *};
use zevria_foundation::{QuestionPromptKind, QuestionRequestId};

#[derive(Clone, PartialEq, Eq)]
struct ChoiceNoteTarget {
    request_id: QuestionRequestId,
    prompt: usize,
    option: usize,
}

impl ChoiceNoteTarget {
    fn draft<'a>(&self, form: &'a QuestionForm) -> Option<&'a str> {
        if form.request.id != self.request_id {
            return None;
        }
        form.choice_note_draft(self.prompt, self.option)
    }
}

pub(super) struct ChoiceNoteEditor {
    target: ChoiceNoteTarget,
    pub(super) input: Entity<TextareaState>,
    _subscription: Subscription,
}

impl DesktopWindow {
    pub(super) fn sync_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.controller.read(cx).model.controls.question.as_ref() else {
            if !self.question_id.is_empty() {
                self.composer
                    .update(cx, |input, cx| input.focus(window, cx));
            }
            self.question_id.clear();
            self.question_inputs.clear();
            self.question_details.clear();
            self.question_subscriptions.clear();
            self.question_note_editor = None;
            return;
        };
        if form.request.id.as_str() == self.question_id {
            if let Some(editor) = &self.question_note_editor
                && editor.target.draft(form).is_none()
            {
                if editor.input.focus_handle(cx).is_focused(window) {
                    window.focus(&self.question_focus, cx);
                }
                self.question_note_editor = None;
            }
            return;
        }
        // Draft text and Markdown are only copied on a new request. The
        // retained text views survive preview and selection changes.
        self.question_id = form.request.id.to_string();
        let values = form
            .answers
            .iter()
            .map(|draft| draft.text.clone())
            .collect::<Vec<_>>();
        let details = form
            .request
            .questions
            .iter()
            .map(|prompt| {
                prompt
                    .options
                    .iter()
                    .map(|option| option.detail.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        self.question_details = details
            .into_iter()
            .map(|options| {
                options
                    .into_iter()
                    .map(|detail| {
                        detail.map(|markdown| cx.new(|cx| TextViewState::markdown(&markdown, cx)))
                    })
                    .collect()
            })
            .collect();
        self.question_note_editor = None;
        window.focus(&self.question_focus, cx);
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
                            if let Some(question) = this.model.controls.question.as_mut()
                                && question.request.id.as_str() == request_id.as_str()
                                && !question.pending
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
    fn open_choice_note(
        &mut self,
        target: ChoiceNoteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self
            .controller
            .read(cx)
            .model
            .controls
            .question
            .as_ref()
            .filter(|form| !form.pending)
            .and_then(|form| target.draft(form))
            .map(str::to_owned)
        else {
            return;
        };
        let input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx)
                .auto_grow(1, 4)
                .placeholder("Add a note to this choice");
            let end = draft.len();
            input.set_value(draft, window, cx);
            input.set_selected_range(end..end, cx);
            input
        });
        let request_target = target.clone();
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, _, cx| {
                if !matches!(event, InputEvent::Change)
                    || this
                        .question_note_editor
                        .as_ref()
                        .is_none_or(|editor| editor.input.entity_id() != input.entity_id())
                {
                    return;
                }
                let value = input.read(cx).value().to_string();
                this.controller.update(cx, |this, cx| {
                    if let Some(form) = this.model.controls.question.as_mut()
                        && form.request.id == request_target.request_id
                        && form.set_choice_note_draft(
                            request_target.prompt,
                            request_target.option,
                            value,
                        )
                    {
                        cx.notify();
                    }
                });
            },
        );
        input.update(cx, |input, cx| input.focus(window, cx));
        self.question_note_editor = Some(ChoiceNoteEditor {
            target,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn question_preview(
        &self,
        prompt: usize,
        option: Option<usize>,
        label: Option<&str>,
        cx: &App,
    ) -> impl IntoElement {
        let t = palette(cx);
        let pane = div()
            .id(("question-preview-pane", prompt))
            .test_support()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .border_color(tones(cx).hairline);
        let pane = if self.compact {
            pane.w_full().border_t_1().pt(px(12.))
        } else {
            pane.flex_1().border_l_1().pl(px(16.))
        };
        let state =
            option.and_then(|option| self.question_details.get(prompt)?.get(option)?.as_ref());
        let content = div()
            .id(("question-preview-scroll", option.unwrap_or(usize::MAX)))
            .max_h(px(if self.compact { 208. } else { 288. }))
            .min_w_0()
            .overflow_y_scroll();
        pane.child(style::label().child(label.unwrap_or("Preview").to_owned()))
            .child(if let Some(state) = state {
                content.child(
                    style::READING
                        .apply(TextView::new(state))
                        .style(style::markdown(cx, false))
                        .code_block_actions(code_block_header)
                        .scrollable(false)
                        .font_family(style::UI_FONT),
                )
            } else {
                content.child(
                    style::meta()
                        .text_color(t.text.muted)
                        .child("No additional detail."),
                )
            })
    }

    pub(super) fn question_dialog(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.controller.read(cx).model.controls.question.as_ref()?;
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
            let has_details = question.has_details();
            let preview = form.preview(index);
            if multiple {
                field = field.child(
                    style::meta()
                        .text_color(t.text.muted)
                        .child("Select one or more"),
                );
            }
            let mut choices = div().flex().flex_col().gap_2().min_w_0();
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
                let request_id = form.request.id.clone();
                let on_change = cx.listener(move |this, value: &bool, _: &mut Window, cx| {
                    this.controller.update(cx, |this, cx| {
                        if let Some(form) = this.model.controls.question.as_mut()
                            && form.request.id == request_id
                            && form.choose(index, option_index, *value)
                        {
                            cx.notify();
                        }
                    })
                });
                let choice = if multiple {
                    Checkbox::new(("question-option", index * 1000 + option_index))
                        .checked(checked)
                        .disabled(pending)
                        .w_full()
                        .py(px(4.))
                        .accessibility_label(option.label.clone())
                        .child(label)
                        .on_change(on_change)
                        .into_any_element()
                } else {
                    Radio::new(("question-option", index * 1000 + option_index))
                        .checked(checked)
                        .disabled(pending)
                        .w_full()
                        .py(px(4.))
                        .accessibility_label(option.label.clone())
                        .child(label)
                        .on_change(on_change)
                        .into_any_element()
                };
                if has_details {
                    let mut row = div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .px(px(6.))
                        .rounded(px(6.))
                        .when(preview == Some(option_index), |row| {
                            row.bg(tones(cx).list_active)
                        })
                        .child(div().flex_1().min_w_0().child(choice));
                    if option.detail.is_some() {
                        let request_id = form.request.id.clone();
                        row = row.child(
                            Button::new(("question-preview-option", index * 1000 + option_index))
                                .ghost()
                                .small()
                                .flex_shrink_0()
                                .label("Preview")
                                .accessibility_label(format!("Preview {}", option.label))
                                .disabled(pending)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.controller.update(cx, |this, cx| {
                                        if let Some(form) = this.model.controls.question.as_mut()
                                            && form.request.id == request_id
                                            && form.set_preview(index, option_index)
                                        {
                                            cx.notify();
                                        }
                                    });
                                })),
                        );
                    }
                    choices = choices.child(row);
                } else {
                    choices = choices.child(choice);
                }
                if let Some(draft) = form.choice_note_draft(index, option_index) {
                    let target = ChoiceNoteTarget {
                        request_id: form.request.id.clone(),
                        prompt: index,
                        option: option_index,
                    };
                    let inline = div().pl(px(24.));
                    choices = choices.child(
                        if let Some(editor) = &self.question_note_editor
                            && editor.target == target
                        {
                            inline.child(
                                Textarea::new(&editor.input)
                                    .small()
                                    .w_full()
                                    .aria_label("Choice note")
                                    .accessibility_id("choice-note-input")
                                    .disabled(pending),
                            )
                        } else {
                            inline.child(
                                Button::new(("question-note", index))
                                    .ghost()
                                    .small()
                                    .label(if draft.trim().is_empty() {
                                        "Add note"
                                    } else {
                                        "Edit note"
                                    })
                                    .disabled(pending)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_choice_note(target.clone(), window, cx)
                                    })),
                            )
                        },
                    );
                }
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
                let request_id = form.request.id.clone();
                choices = choices.child(
                    Checkbox::new(("question-other", index))
                        .label("Other")
                        .checked(answer.other)
                        .disabled(pending)
                        .on_change(cx.listener(move |this, value, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                if let Some(form) = this.model.controls.question.as_mut()
                                    && form.request.id == request_id
                                    && !form.pending
                                {
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
                choices = choices.child(Textarea::new(input).disabled(pending));
            }
            if !question.required {
                let request_id = form.request.id.clone();
                choices = choices.child(
                    Checkbox::new(("question-skip", index))
                        .label("Skip this question")
                        .checked(answer.skip)
                        .disabled(pending)
                        .on_change(cx.listener(move |this, value, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                if let Some(form) = this.model.controls.question.as_mut()
                                    && form.request.id == request_id
                                    && !form.pending
                                {
                                    form.answers[index].skip = *value;
                                }
                                cx.notify();
                            })
                        })),
                );
            }
            let content = if has_details {
                let row = div()
                    .id(("question-master-detail", index))
                    .flex()
                    .gap(px(16.));
                let row = if self.compact {
                    row.flex_col().child(choices.w_full())
                } else {
                    row.items_start().child(choices.w(px(320.)).flex_shrink_0())
                };
                let option = preview.and_then(|option| question.options.get(option));
                row.child(self.question_preview(
                    index,
                    preview,
                    option.map(|option| option.label.as_str()),
                    cx,
                ))
                .into_any_element()
            } else {
                choices.into_any_element()
            };
            fields = fields.child(field.child(content));
        }
        let count = form.request.questions.len();
        let height_ratio = if form.request.has_details() {
            0.70
        } else {
            0.55
        };
        let mut dialog = style::sheet(cx)
            .w_full()
            .max_w(px(style::COLUMN))
            .max_h(px((f32::from(window.viewport_size().height)
                * height_ratio)
                .max(180.)))
            .p(px(16.))
            .track_focus(&self.question_focus)
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
            .custom(style::mode_button(
                self.controller.read(cx).model.root.next_mode(),
                cx,
            ))
            .label(if pending {
                "Sending…"
            } else if count == 1 {
                "Send answer"
            } else {
                "Send answers"
            })
            .disabled(pending)
            .tooltip(format!("Send answer {}", Shortcut::CONFIRM.label()))
            .on_click(cx.listener(|this, _, _, cx| {
                this.controller
                    .update(cx, |this, cx| this.answer_question(false, cx))
            }));
        let cancelling = self.controller.read(cx).cancelling;
        let mut buttons = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(px(8.))
            .child(
                Button::new("stop-question-turn")
                    .ghost()
                    .small()
                    .label(if cancelling { "Stopping" } else { "Stop" })
                    .tooltip(format!("Stop the current turn {}", Shortcut::STOP.label()))
                    .disabled(cancelling)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |controller, cx| controller.cancel(cx))
                    })),
            )
            .child(div().flex_1());
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
            div()
                .id("question-dialog")
                .test_support()
                .flex_shrink_0()
                .px(px(style::COLUMN_INSET))
                .py(px(12.))
                .child(
                    style::Measure::column().child(
                        dialog.child(
                            buttons
                                .child(style::key_hint(Shortcut::CONFIRM, "Send answer", cx))
                                .child(answer_button),
                        ),
                    ),
                )
                .into_any_element(),
        )
    }
}
