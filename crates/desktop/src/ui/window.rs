use super::{
    style,
    theme::{palette, tones},
    transcript::TranscriptView,
};
use crate::{
    actions,
    controller::{Phase, SessionController, SessionRequest},
};
use gpui_kit::component::{
    IconName, Sizable, TitleBar,
    button::{Button, ButtonVariants},
    input::{InputEvent, TextareaState},
    text::TextViewState,
    tooltip::Tooltip,
};
use gpui_kit::{prelude::*, *};
use std::path::PathBuf;

pub(super) enum Replacement {
    New,
    Resume(PathBuf),
    Workspace(PathBuf),
}
pub struct DesktopWindow {
    pub controller: Entity<SessionController>,
    pub(super) composer: Entity<TextareaState>,
    composer_mode: zevria_foundation::SessionMode,
    pub(super) transcript: Entity<TranscriptView>,
    pub(super) plan: Option<(zevria_workflow::PlanVersion, Entity<TextViewState>)>,
    pub(super) screen_error: Option<(String, Entity<TextViewState>)>,
    pub(super) question_id: String,
    pub(super) question_inputs: Vec<Entity<TextareaState>>,
    pub(super) question_subscriptions: Vec<Subscription>,
    pub(super) pending_replace: Option<Replacement>,
    pub(super) completion_dismissed: Option<(String, usize)>,
    pub(super) completion_selected: usize,
    pub(super) sidebar_override: Option<bool>,
    pub(super) sidebar_focus: Vec<FocusHandle>,
    pub(super) compact: bool,
    pub(super) plan_width: f32,
    pub(super) plan_overlay: bool,
    pub(super) question_focus: FocusHandle,
    picker: Option<Task<()>>,
    was_live: bool,
    _input: Subscription,
    _controller: Subscription,
    _appearance: Subscription,
}
impl DesktopWindow {
    pub fn new(
        controller: Entity<SessionController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        super::theme::follow_appearance(window, cx);
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            super::theme::follow_appearance(window, cx);
            cx.notify();
        });
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 10)
                .placeholder("Ask Zevria to make a change")
                .submit_on_enter(true)
        });
        let input = cx.subscribe_in(
            &composer,
            window,
            |this, input, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        let draft = input.read(cx).value().to_string();
                        this.controller.update(cx, |controller, cx| {
                            controller.edit_draft(draft, cx);
                        });
                        this.completion_selected = 0;
                        this.completion_dismissed = None;
                    }
                    InputEvent::PressEnter {
                        shift: false,
                        secondary: false,
                    } => {
                        if this.completions(cx).is_empty() {
                            this.submit(window, cx);
                        } else {
                            this.accept_completion(window, cx);
                            let prompt = zevria_content::UserPrompt::from_text(
                                this.composer.read(cx).value().to_string(),
                            );
                            if matches!(
                                this.controller
                                    .read(cx)
                                    .model
                                    .registry
                                    .classify_prompt(&prompt),
                                Ok(zevria_presentation::command::ClassifiedInput::Builtin(_))
                            ) {
                                this.submit(window, cx);
                            }
                        }
                    }
                    _ => {}
                }
                cx.notify();
            },
        );
        let watch = cx.observe_in(&controller, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| TranscriptView::new(controller.clone(), cx));
        let weak = controller.downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = weak.update(cx, |this, cx| this.quit(cx));
            false
        });
        let mut this = Self {
            controller,
            composer,
            composer_mode: zevria_foundation::SessionMode::Build,
            transcript,
            plan: None,
            screen_error: None,
            question_id: String::new(),
            question_inputs: Vec::new(),
            question_subscriptions: Vec::new(),
            pending_replace: None,
            completion_dismissed: None,
            completion_selected: 0,
            sidebar_override: None,
            sidebar_focus: Vec::new(),
            compact: false,
            plan_width: 400.,
            plan_overlay: false,
            question_focus: cx.focus_handle(),
            picker: None,
            was_live: false,
            _input: input,
            _controller: watch,
            _appearance: appearance,
        };
        this.sync(window, cx);
        this
    }
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::theme::follow_appearance(window, cx);
        let controller = self.controller.read(cx);
        let draft = controller.draft.clone();
        let mode = controller.model.root.next_mode();
        let live = controller.phase == Phase::Live;
        let plan = controller
            .model
            .root
            .workflow
            .dialog_artifact()
            .filter(|a| {
                self.plan
                    .as_ref()
                    .is_none_or(|(current, _)| *current != a.version)
            })
            .map(|a| (a.version, a.markdown.clone()));
        let screen_error = controller
            .config_error
            .clone()
            .or_else(|| match &controller.phase {
                Phase::Failed(error) => Some(error.clone()),
                _ => None,
            });
        let question = controller.model.question.as_ref().map(|q| {
            (
                q.request.id.to_string(),
                q.answers.iter().map(|a| a.text.clone()).collect::<Vec<_>>(),
            )
        });
        if self.composer_mode != mode {
            self.composer_mode = mode;
            self.composer.update(cx, |input, cx| {
                input.set_placeholder(
                    if mode == zevria_foundation::SessionMode::Build {
                        "Ask Zevria to make a change"
                    } else {
                        "Describe what Zevria should plan"
                    },
                    window,
                    cx,
                )
            });
        }
        if self.composer.read(cx).value().as_ref() != draft {
            self.composer.update(cx, |input, cx| {
                let end = draft.len();
                input.set_value(draft, window, cx);
                input.set_selected_range(end..end, cx);
            });
        }
        if live && !self.was_live {
            self.composer
                .update(cx, |input, cx| input.focus(window, cx));
        }
        self.was_live = live;
        if let Some((version, markdown)) = plan
            && self
                .plan
                .as_ref()
                .is_none_or(|(current, _)| *current != version)
        {
            self.plan = Some((version, cx.new(|cx| TextViewState::markdown(&markdown, cx))));
        }
        if let Some(error) = screen_error {
            if self
                .screen_error
                .as_ref()
                .is_none_or(|(current, _)| current != &error)
            {
                let html = format!(
                    "<p>{}</p>",
                    error
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                        .replace('\n', "<br>")
                );
                self.screen_error = Some((error, cx.new(|cx| TextViewState::html(&html, cx))));
            }
        } else {
            self.screen_error = None;
        }
        self.sync_question(question, window, cx);
    }
    pub(super) fn request_replace(&mut self, replacement: Replacement, cx: &mut Context<Self>) {
        if self.controller.read(cx).model.root.is_busy() {
            self.pending_replace = Some(replacement);
            cx.notify();
        } else {
            self.perform_replace(replacement, cx);
        }
    }
    pub(super) fn perform_replace(&mut self, replacement: Replacement, cx: &mut Context<Self>) {
        match replacement {
            Replacement::New => self.controller.update(cx, |this, cx| this.new_session(cx)),
            Replacement::Resume(path) => self.controller.update(cx, |this, cx| {
                if let Some(workspace) = this.workspace.clone() {
                    this.replace(
                        SessionRequest {
                            workspace,
                            start: zevria_host::SessionStart::Resume(path),
                        },
                        cx,
                    );
                }
            }),
            Replacement::Workspace(workspace) => self.controller.update(cx, |this, cx| {
                this.replace(
                    SessionRequest {
                        workspace,
                        start: zevria_host::SessionStart::New {
                            inherited_models: None,
                        },
                    },
                    cx,
                )
            }),
        }
    }
    pub(super) fn open_workspace(&mut self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open workspace".into()),
        });
        self.picker = Some(cx.spawn(async move |owner, cx| match prompt.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = owner.update(cx, |this, cx| {
                        this.request_replace(Replacement::Workspace(path), cx)
                    });
                }
            }
            Ok(Ok(None)) => {}
            error => {
                let error = match error {
                    Ok(Err(error)) => format!("{error:#}"),
                    Err(error) => error.to_string(),
                    Ok(Ok(_)) => return,
                };
                let _ = owner.update(cx, |this, cx| {
                    this.controller.update(cx, |controller, cx| {
                        controller
                            .model
                            .root
                            .conversation
                            .push_error(format!("Couldn't open the folder picker: {error}"));
                        cx.notify();
                    });
                });
            }
        }));
    }
    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.pending_replace.take().is_some() {
            cx.notify();
            return;
        }
        let controller = self.controller.read(cx);
        if let Some(question) = &controller.model.question {
            if !question.request.dismissible {
                return;
            }
            self.controller
                .update(cx, |this, cx| this.answer_question(true, cx));
            return;
        }
        if !self.completions(cx).is_empty() {
            self.completion_dismissed = Some((
                self.composer.read(cx).value().to_string(),
                self.composer.read(cx).cursor(),
            ));
            cx.notify();
            return;
        }
        if controller.model.root.workflow.dialog().is_some() {
            self.controller.update(cx, |this, cx| {
                this.model.root.workflow.hide();
                cx.notify();
            });
            return;
        }
        self.controller.update(cx, |this, cx| this.cancel(cx));
    }
}
impl DesktopWindow {
    fn disclose(&mut self, expanded: bool, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        let from_composer = self.composer.focus_handle(cx).contains_focused(window, cx);
        self.transcript.update(cx, |view, cx| {
            view.disclose(expanded, all, from_composer, window, cx)
        });
    }
    fn confirm_decision(&mut self, cx: &mut Context<Self>) {
        if self.pending_replace.is_some() {
            return;
        }
        if self.controller.read(cx).model.question.is_some() {
            self.controller
                .update(cx, |controller, cx| controller.answer_question(false, cx));
        } else if self
            .controller
            .read(cx)
            .model
            .root
            .workflow
            .dialog()
            .is_some()
        {
            self.controller.update(cx, |controller, cx| {
                controller.plan_decision(zevria_workflow::PlanDecision::ImplementCurrent, cx)
            });
        }
    }
    pub(super) fn sidebar_visible(&self, width: f32) -> bool {
        self.sidebar_override.unwrap_or(width >= 900.)
    }
    fn title_strip(&self, visible: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = palette(cx);
        let tone = tones(cx);
        let path = self
            .controller
            .read(cx)
            .workspace
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(16.))
            .bg(t.surfaces.canvas)
            .border_b_1()
            .border_color(tone.hairline);
        let model = &self.controller.read(cx).model;
        if let Some(child) = model
            .selected_child
            .as_ref()
            .and_then(|id| model.children.iter().find(|c| &c.id == id))
        {
            main = main
                .child(
                    Button::new("back-to-root")
                        .ghost()
                        .small()
                        .label("Conversation")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |this, cx| {
                                this.model.selected_child = None;
                                cx.notify();
                            })
                        })),
                )
                .child(style::label().text_color(t.text.muted).child("›"))
                .child(div().min_w_0().truncate().text_size(px(14.)).child(format!(
                    "{}: {}",
                    style::kind_label(child.kind),
                    child.title
                )))
                .child(style::tag("Read-only", t.text.secondary, cx));
        } else {
            main = main.child(
                div()
                    .id("workspace-title")
                    .min_w_0()
                    .truncate()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .tooltip(move |w, cx| Tooltip::new(path.clone()).build(w, cx))
                    .child(self.workspace_name(cx)),
            );
        }
        main = main.child(div().flex_1());
        if model.root.workflow.submitted_artifact().is_some()
            && model.root.workflow.dialog().is_none()
        {
            main = main.child(
                Button::new("review-plan")
                    .ghost()
                    .small()
                    .child(div().size(px(5.)).rounded_full().bg(t.workflow.plan))
                    .label("Review plan")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.model.root.workflow.open_review();
                            cx.notify();
                        })
                    })),
            );
        }
        TitleBar::new()
            .h(px(style::TITLE))
            .pl_0()
            .border_b_0()
            .bg(t.surfaces.canvas)
            .child(
                div()
                    .w(px(if visible { style::SIDEBAR } else { 120. }))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .bg(if visible {
                        t.surfaces.panel
                    } else {
                        t.surfaces.canvas
                    })
                    .when(visible, |d| d.border_r_1().border_color(tone.hairline))
                    .child(div().w(px(80.)).flex_shrink_0())
                    .child(
                        style::icon_button(
                            "toggle-sidebar",
                            IconName::PanelLeft,
                            if visible {
                                "Hide sidebar ⌃⌘S"
                            } else {
                                "Show sidebar ⌃⌘S"
                            },
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.sidebar_override = Some(
                                !this.sidebar_visible(f32::from(window.viewport_size().width)),
                            );
                            cx.notify();
                        })),
                    ),
            )
            .child(main)
            .into_any_element()
    }
}
impl Render for DesktopWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
        let width = f32::from(window.viewport_size().width);
        let visible = self.sidebar_visible(width);
        self.plan_width = (width * 0.36).clamp(400., 560.);
        let plan_open = self
            .controller
            .read(cx)
            .model
            .root
            .workflow
            .dialog()
            .is_some();
        let main_width = width - if visible { style::SIDEBAR } else { 0. };
        self.plan_overlay = plan_open && main_width - self.plan_width < 560.;
        self.plan_width = self.plan_width.min(main_width);
        let compact = main_width
            - if plan_open && !self.plan_overlay {
                self.plan_width
            } else {
                0.
            }
            < 760.;
        if compact != self.compact {
            self.compact = compact;
            self.transcript
                .update(cx, |view, cx| view.set_compact(compact, cx));
        }
        let mut root = div()
            .id("zevria-desktop")
            .key_context(if self.completions(cx).is_empty() {
                "ZevriaDesktop"
            } else {
                "ZevriaDesktop ZevriaCompletion"
            })
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.surfaces.canvas)
            .text_color(t.text.primary)
            .on_action(cx.listener(|this, _: &actions::Quit, _, cx| {
                this.controller.update(cx, |this, cx| this.quit(cx))
            }))
            .on_action(
                cx.listener(|this, _: &actions::OpenWorkspace, _, cx| this.open_workspace(cx)),
            )
            .on_action(cx.listener(|this, _: &actions::NewSession, _, cx| {
                this.request_replace(Replacement::New, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleSidebar, window, cx| {
                this.sidebar_override =
                    Some(!this.sidebar_visible(f32::from(window.viewport_size().width)));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &actions::Cancel, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &actions::Stop, _, cx| {
                this.controller
                    .update(cx, |controller, cx| controller.cancel(cx));
            }))
            .on_action(cx.listener(|this, _: &actions::ExpandBlock, window, cx| {
                this.disclose(true, false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::CollapseBlock, window, cx| {
                this.disclose(false, false, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::ExpandAllThinking, window, cx| {
                    this.disclose(true, true, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::CollapseAllThinking, window, cx| {
                    this.disclose(false, true, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::JumpToLatest, _, cx| {
                this.transcript
                    .update(cx, |view, cx| view.jump_to_latest(cx))
            }))
            .on_action(
                cx.listener(|this, _: &actions::ConfirmDecision, _, cx| this.confirm_decision(cx)),
            )
            .on_action(cx.listener(|this, _: &actions::ToggleMode, _, cx| {
                let mode = if this.controller.read(cx).model.root.next_mode()
                    == zevria_foundation::SessionMode::Build
                {
                    zevria_foundation::SessionMode::Plan
                } else {
                    zevria_foundation::SessionMode::Build
                };
                this.controller
                    .update(cx, |controller, cx| controller.select_mode(mode, cx));
            }))
            .on_action(cx.listener(|this, _: &actions::Submit, window, cx| this.submit(window, cx)))
            .on_action(cx.listener(|this, _: &actions::Complete, window, cx| {
                this.accept_completion(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::CompletionUp, _, cx| {
                this.completion_selected = this.completion_selected.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &actions::CompletionDown, _, cx| {
                this.completion_selected = (this.completion_selected + 1)
                    .min(this.completions(cx).len().saturating_sub(1));
                cx.notify();
            }));
        root = root.child(self.title_strip(visible, cx));
        if let Some(screen) = self.screen(cx) {
            root = root.child(div().flex_1().min_h_0().child(screen));
        } else {
            let mut center = div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(self.transcript.clone()));
            if let Some(question) = self.question_dialog(window, cx) {
                center = center.child(question);
            } else if self.controller.read(cx).model.selected_child.is_none() {
                center = center.child(self.composer_view(window, cx));
            }
            center = center.child(self.status_bar(cx));
            let mut body = div().flex_1().min_h_0().flex().relative();
            if visible {
                body = body.child(self.sidebar(window, cx));
            }
            body = body.child(center);
            if let Some(plan) = self.plan_panel(cx) {
                body = body.child(plan);
            }
            root = root.child(body);
        }
        if self.pending_replace.is_some() {
            root = root.child(self.confirm_replace(cx));
        }
        root
    }
}
