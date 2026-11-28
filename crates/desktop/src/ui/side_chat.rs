//! The Side Chat rail reuses native transcript cards and keeps a separate draft.
use super::{
    composer::attachments,
    images, style,
    theme::{palette, tones},
    transcript::TranscriptView,
    window::DesktopWindow,
};
use gpui_kit::component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    input::{InputEvent, Textarea, TextareaState},
};
use gpui_kit::{prelude::*, *};
use zevria_foundation::{
    SIDE_CHAT_EXCERPT_BYTES, SideChatId, SideChatStatus, bound_side_chat_text,
};

#[derive(Default)]
pub(super) struct SideChatPanelView {
    selected: Option<SideChatId>,
    input: Option<Entity<TextareaState>>,
    focus: Option<FocusHandle>,
    transcripts: std::collections::HashMap<SideChatId, Entity<TranscriptView>>,
    input_subscription: Option<Subscription>,
}

impl DesktopWindow {
    pub(super) fn sync_side_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let controller = self.controller.read(cx);
        let selected = controller
            .model
            .controls
            .side_chat_open
            .then_some(controller.model.controls.selected_side_chat)
            .flatten();
        self.side_ui.transcripts.retain(|id, _| {
            controller
                .model
                .side_chats
                .iter()
                .any(|pane| pane.descriptor.id == *id)
        });
        if selected == self.side_ui.selected {
            if let (Some(id), Some(input)) = (selected, self.side_ui.input.as_ref())
                && let Some(pane) = controller
                    .model
                    .side_chats
                    .iter()
                    .find(|pane| pane.descriptor.id == id)
                && input.read(cx).value().to_string() != pane.draft
            {
                let draft = pane.draft.clone();
                input.update(cx, |input, cx| input.set_value(draft, window, cx));
            }
            return;
        }
        self.side_ui.selected = selected;
        let Some(id) = selected else {
            let focused = self
                .side_ui
                .focus
                .as_ref()
                .is_some_and(|focus| focus.contains_focused(window, cx));
            self.side_ui.input = None;
            self.side_ui.input_subscription = None;
            if focused {
                self.composer
                    .update(cx, |input, cx| input.focus(window, cx));
            }
            return;
        };
        let draft = controller
            .model
            .side_chats
            .iter()
            .find(|pane| pane.descriptor.id == id)
            .map(|pane| pane.draft.clone())
            .unwrap_or_default();
        self.side_ui.focus.get_or_insert_with(|| cx.focus_handle());
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder("Ask a follow-up without changing the main chat")
                .submit_on_enter(true)
        });
        input.update(cx, |input, cx| {
            input.set_value(draft, window, cx);
            input.focus(window, cx);
        });
        let watch = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, _, cx| match event {
                InputEvent::Change => {
                    let draft = input.read(cx).value().to_string();
                    this.controller
                        .update(cx, |this, cx| this.update_side_draft(id, draft, cx));
                }
                InputEvent::PressEnter {
                    shift: false,
                    secondary: false,
                } => {
                    this.controller
                        .update(cx, |this, cx| this.submit_side_chat(id, cx));
                }
                _ => {}
            },
        );
        let controller = self.controller.clone();
        let focus = self.window_focus.clone();
        self.side_ui.transcripts.entry(id).or_insert_with(|| {
            cx.new(|cx| TranscriptView::new_side(controller, focus, id, window, cx))
        });
        self.side_ui.input = Some(input);
        self.side_ui.input_subscription = Some(watch);
    }

    pub(super) fn active_transcript_selection(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.composer.focus_handle(cx).contains_focused(window, cx)
            || self
                .side_ui
                .input
                .as_ref()
                .is_some_and(|input| input.focus_handle(cx).contains_focused(window, cx))
        {
            return None;
        }
        let text = self
            .transcript
            .read(cx)
            .selected_diff(window, cx)
            .unwrap_or_else(|| gpui_kit::base::TextSelection::selected_text(window, cx));
        let text = bound_side_chat_text(&text, SIDE_CHAT_EXCERPT_BYTES);
        (!text.is_empty()).then(|| text.to_owned())
    }

    pub(super) fn toggle_side_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let excerpt = self.active_transcript_selection(window, cx);
        self.controller.update(cx, |controller, cx| {
            if let Some(excerpt) = excerpt {
                controller.create_side_chat(None, Some(excerpt), cx);
            } else {
                controller.toggle_side_chat(cx);
            }
        });
    }

    pub(super) fn stop_side_if_focused(
        &mut self,
        close_if_idle: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.side_ui.selected else {
            return false;
        };
        if self
            .side_ui
            .focus
            .as_ref()
            .is_some_and(|focus| focus.contains_focused(window, cx))
        {
            let running = self
                .controller
                .read(cx)
                .model
                .side_chats
                .iter()
                .any(|pane| pane.descriptor.id == id && pane.is_running());
            if running {
                self.controller
                    .update(cx, |controller, cx| controller.cancel_side_chat(id, cx));
                true
            } else if close_if_idle {
                self.controller
                    .update(cx, |controller, cx| controller.close_side_chat_panel(cx));
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    pub(super) fn side_chat_panel(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = palette(cx);
        let tone = tones(cx);
        let controller = self.controller.read(cx);
        let capturing = controller.capturing_dialog();
        let m = &controller.model;
        if !m.controls.side_chat_open {
            return None;
        }
        let selected = m.controls.selected_side_chat;
        let tabs = m
            .side_chats
            .iter()
            .rev()
            .map(|pane| {
                (
                    pane.descriptor.id,
                    format!(
                        "{} {}",
                        if pane.is_running() {
                            "◐"
                        } else if pane.descriptor.status == SideChatStatus::Failed {
                            "!"
                        } else {
                            "○"
                        },
                        bound_side_chat_text(&pane.descriptor.title, 36)
                    ),
                    format!("{} ({})", pane.descriptor.title, pane.descriptor.short_id),
                )
            })
            .collect::<Vec<_>>();
        let current = m
            .side_chats
            .iter()
            .find(|pane| Some(pane.descriptor.id) == selected);
        let running = current.is_some_and(|pane| pane.is_running());
        let editable = !running && !capturing;
        let empty =
            current.is_none_or(|pane| pane.draft.trim().is_empty() && pane.draft_images.is_empty());
        let draft_images = current
            .map(|pane| pane.draft_images.clone())
            .unwrap_or_default();
        let banner = current.map(|pane| (pane.descriptor.title.clone(), pane.banner()));
        let excerpt = current
            .and_then(|pane| pane.quoted_excerpt.as_ref())
            .map(|text| bound_side_chat_text(text, 360).to_owned());
        let error = current.and_then(|pane| pane.error.clone());
        let header = div()
            .h(px(46.))
            .px(px(14.))
            .gap(px(8.))
            .flex()
            .items_center()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Side Chat"))
            .child(style::tag("Read-only", t.text.muted, cx))
            .child(div().flex_1())
            .child(
                Button::new("side-chat-new")
                    .ghost()
                    .small()
                    .label("+ New")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller.create_side_chat(None, None, cx)
                        })
                    })),
            )
            .child(
                Button::new("side-chat-close")
                    .ghost()
                    .small()
                    .label("Close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |controller, cx| controller.close_side_chat_panel(cx))
                    })),
            );
        let mut panel = div()
            .id("side-chat-panel")
            .w(px(self.plan_width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(tone.hairline)
            .bg(t.surfaces.canvas)
            .when_some(self.side_ui.focus.clone(), |d, focus| d.track_focus(&focus))
            .when(self.plan_overlay, |d| {
                d.absolute().right_0().top_0().occlude().shadow_lg()
            })
            .child(header);
        let mut thread_list = div()
            .id("side-chat-threads")
            .flex()
            .gap(px(4.))
            .overflow_x_scroll()
            .px(px(12.))
            .pb(px(6.));
        for (id, short, title) in tabs {
            let active = Some(id) == selected;
            let button = Button::new(SharedString::from(format!("side-tab-{id}")))
                .small()
                .label(short)
                .tooltip(title);
            let button = if active {
                button.primary()
            } else {
                button.ghost()
            };
            thread_list = thread_list.child(button.on_click(cx.listener(move |this, _, _, cx| {
                this.controller
                    .update(cx, |controller, cx| controller.select_side_chat(id, cx))
            })));
        }
        panel = panel.child(thread_list);
        if let Some((title, banner)) = banner {
            panel = panel.child(
                div()
                    .px(px(14.))
                    .py(px(7.))
                    .border_t_1()
                    .border_b_1()
                    .border_color(tone.hairline)
                    .child(div().text_sm().truncate().child(title))
                    .child(div().text_xs().text_color(t.text.muted).child(banner)),
            );
        }
        if let Some(excerpt) = excerpt {
            panel = panel.child(
                div()
                    .mx(px(14.))
                    .my(px(10.))
                    .pl(px(9.))
                    .border_l_1()
                    .border_color(tone.hairline)
                    .text_sm()
                    .text_color(t.text.muted)
                    .child(excerpt),
            );
        }
        let transcript = selected
            .and_then(|id| self.side_ui.transcripts.get(&id))
            .cloned();
        panel = panel.child(
            div()
                .flex_1()
                .min_h_0()
                .when_some(transcript, |div, view| div.child(view))
                .when(selected.is_none(), |d| {
                    d.p(px(20.))
                        .text_sm()
                        .text_color(t.text.muted)
                        .child("Start a side chat without interrupting the main conversation.")
                }),
        );
        if let Some(error) = error {
            panel = panel.child(
                div()
                    .px(px(14.))
                    .text_sm()
                    .text_color(t.feedback.error)
                    .child(error),
            );
        }
        if let (Some(id), Some(input)) = (selected, self.side_ui.input.as_ref()) {
            let send = Button::new("side-send")
                .small()
                .label(if running { "Stop" } else { "Send" })
                .disabled(capturing || (!running && empty))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller.update(cx, |controller, cx| {
                        if running {
                            controller.cancel_side_chat(id, cx);
                        } else {
                            controller.submit_side_chat(id, cx);
                        }
                    })
                }));
            let drop_target = self.controller.clone();
            let paste_target = self.controller.clone();
            panel = panel.child(
                div()
                    .id("side-composer-panel")
                    .test_support()
                    .when(editable, |d| {
                        d.on_drop(move |paths: &ExternalPaths, _, cx| {
                            images::drop_paths(&drop_target, Some(id), paths.0.to_vec(), cx)
                        })
                    })
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .border_t_1()
                    .border_color(tone.hairline)
                    .when(!draft_images.is_empty(), |d| {
                        d.child(attachments(
                            &self.controller,
                            Some(id),
                            &draft_images,
                            editable,
                            cx,
                        ))
                    })
                    .child(
                        Textarea::new(input)
                            .disabled(!editable)
                            .aria_label("Side Chat message")
                            .on_paste(move |clip, _, cx| {
                                editable && images::paste(&paste_target, Some(id), clip, cx)
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                images::attach_button("side-attach-image")
                                    .disabled(!editable)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        images::choose(this.controller.clone(), Some(id), cx);
                                    })),
                            )
                            .child(send),
                    ),
            );
        }
        Some(panel.into_any_element())
    }
}
