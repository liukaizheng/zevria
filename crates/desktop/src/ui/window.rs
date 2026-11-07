use super::{theme::palette, transcript::TranscriptView};
use crate::{
    actions,
    controller::{Phase, SessionController, SessionRequest},
};
use gpui_kit::component::{
    input::{InputEvent, TextareaState},
    text::TextViewState,
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
    pub(super) transcript: Entity<TranscriptView>,
    pub(super) plan: Option<(zevria_workflow::PlanVersion, Entity<TextViewState>)>,
    pub(super) question_id: String,
    pub(super) question_inputs: Vec<Entity<TextareaState>>,
    pub(super) question_subscriptions: Vec<Subscription>,
    pub(super) pending_replace: Option<Replacement>,
    pub(super) completion_dismissed: Option<(String, usize)>,
    pub(super) completion_selected: usize,
    picker: Option<Task<()>>,
    was_live: bool,
    _input: Subscription,
    _controller: Subscription,
}
impl DesktopWindow {
    pub fn new(
        controller: Entity<SessionController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
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
            transcript,
            plan: None,
            question_id: String::new(),
            question_inputs: Vec::new(),
            question_subscriptions: Vec::new(),
            pending_replace: None,
            completion_dismissed: None,
            completion_selected: 0,
            picker: None,
            was_live: false,
            _input: input,
            _controller: watch,
        };
        this.sync(window, cx);
        this
    }
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let controller = self.controller.read(cx);
        let draft = controller.draft.clone();
        let live = controller.phase == Phase::Live;
        let plan = controller
            .model
            .root
            .workflow
            .dialog_artifact()
            .map(|a| (a.version, a.markdown.clone()));
        let question = controller.model.question.as_ref().map(|q| {
            (
                q.request.id.to_string(),
                q.answers.iter().map(|a| a.text.clone()).collect::<Vec<_>>(),
            )
        });
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
                let _ = owner.update(cx, |this, cx| {
                    this.controller.update(cx, |controller, cx| {
                        controller
                            .model
                            .root
                            .conversation
                            .push_error(format!("Folder picker failed: {error:?}"));
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
        if controller.model.question.is_some() {
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
impl Render for DesktopWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
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
            .on_action(cx.listener(|this, _: &actions::Cancel, _, cx| this.cancel(cx)))
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
        if let Some(screen) = self.screen(cx) {
            root = root.child(screen);
        } else {
            let mut center = div().flex_1().min_w_0().h_full().flex().flex_col();
            let workspace = &self.controller.read(cx).model;
            if let Some(child) = workspace
                .selected_child
                .as_ref()
                .and_then(|id| workspace.children.iter().find(|child| &child.id == id))
            {
                let title = format!(
                    "{} · {} · {}",
                    child.kind,
                    child.title,
                    child.workspace.as_deref().unwrap_or("read-only inspection")
                );
                center = center.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .p_3()
                        .border_b_1()
                        .border_color(t.surfaces.border)
                        .child(
                            gpui_kit::component::button::Button::new("back-to-root")
                                .label("← Conversation")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |this, cx| {
                                        this.model.selected_child = None;
                                        cx.notify();
                                    })
                                })),
                        )
                        .child(title),
                );
            }
            center = center.child(div().flex_1().min_h_0().child(self.transcript.clone()));
            if self.controller.read(cx).model.selected_child.is_none() {
                center = center.child(self.composer_view(cx));
            }
            let mut body = div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.sidebar(cx))
                .child(center);
            if let Some(plan) = self.plan_panel(cx) {
                body = body.child(plan);
            }
            root = root.child(body).child(self.status_bar(cx));
        }
        if let Some(question) = self.question_dialog(window, cx) {
            root = root.child(question);
        }
        if self.pending_replace.is_some() {
            root = root.child(self.confirm_replace(cx));
        }
        root
    }
}
