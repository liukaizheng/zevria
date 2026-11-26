use super::{
    math, style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::actions::Shortcut;
use gpui_kit::base::text::TextView;
use gpui_kit::component::{
    Disableable, Sizable, Theme,
    button::{Button, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use zevria_workflow::PlanDecision;

impl DesktopWindow {
    fn copy_plan(&mut self, cx: &mut Context<Self>) {
        let workflow = &self.controller.read(cx).model.root.workflow;
        if let Some(artifact) = workflow.dialog_artifact() {
            cx.write_to_clipboard(ClipboardItem::new_string(artifact.markdown.clone()));
            self.plan_copy_feedback
                .trigger((), cx, |this| &mut this.plan_copy_feedback);
        }
    }

    pub(super) fn plan_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = &self.controller.read(cx).model.root;
        let dialog = model.workflow.dialog()?;
        let artifact = model.workflow.dialog_artifact()?;
        let title = artifact.title.clone();
        let revision = artifact.version.revision;
        let busy = model.is_busy();
        let recovering = dialog.recovering();
        let (_, text) = self.plan.as_ref()?;
        let t = palette(cx);
        let window_focus = self.window_focus.clone();
        let panel = div()
            .id("plan-review")
            .test_support()
            .w(px(self.plan_width))
            .flex_shrink_0()
            .h_full()
            .when(self.plan_overlay, |panel| {
                panel
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .shadow_lg()
                    .occlude()
            })
            .min_h_0()
            .flex()
            .flex_col()
            .bg(t.surfaces.overlay)
            .rounded(px(14.))
            .border_l_1()
            .border_color(tones(cx).hairline)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                // The overlay occludes the root; descendants still claim focus first.
                if !window.default_prevented() {
                    window.focus(&window_focus, cx);
                }
            })
            .child(
                div()
                    .p(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                style::label()
                                    .text_color(if recovering {
                                        t.feedback.warning
                                    } else {
                                        t.workflow.plan
                                    })
                                    .child(if recovering {
                                        "Recovery"
                                    } else {
                                        "Ready for review"
                                    }),
                            )
                            .child(
                                style::copy_button(
                                    "plan-copy",
                                    self.plan_copy_feedback.is_active(()),
                                    "Copy plan as Markdown",
                                    "Copied plan",
                                    cx,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.copy_plan(cx))),
                            ),
                    )
                    .child(
                        div()
                            .max_h(px(48.))
                            .overflow_hidden()
                            .text_size(px(18.))
                            .line_height(px(24.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(style::tag(
                        format!("Revision {revision}"),
                        t.text.secondary,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px(px(24.))
                    .pb(px(24.))
                    .child(
                        TextView::new(text)
                            .markdown_extensions(math::extensions())
                            .size_full()
                            .style(style::markdown(cx, true))
                            .font_family(style::DOCUMENT_FONT)
                            .map(|view| style::DOCUMENT.apply(view)),
                    ),
            )
            .child(
                div()
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .border_t_1()
                    .border_color(tones(cx).hairline)
                    .child(
                        Button::new("plan-implement")
                            .custom(style::mode_button(zevria_foundation::SessionMode::Plan, cx))
                            .label("Implement")
                            .tooltip(format!("Implement {}", Shortcut::CONFIRM.label()))
                            .disabled(busy)
                            .when(busy, |b| b.tooltip("Wait for the current turn to finish"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |this, cx| {
                                    this.plan_decision(PlanDecision::ImplementCurrent, cx)
                                })
                            })),
                    )
                    .child(
                        Button::new("plan-fresh")
                            .outline()
                            .label("Implement in fresh session")
                            .disabled(busy)
                            .when(busy, |b| b.tooltip("Wait for the current turn to finish"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |this, cx| {
                                    this.plan_decision(PlanDecision::ImplementFresh, cx)
                                })
                            })),
                    )
                    .child(
                        Button::new("plan-revise")
                            .ghost()
                            .small()
                            .label(if recovering { "Close" } else { "Revise" })
                            .disabled(busy)
                            .when(busy, |b| b.tooltip("Wait for the current turn to finish"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.controller.update(cx, |this, cx| {
                                    if recovering {
                                        this.model.root.workflow.hide();
                                        cx.notify();
                                    } else {
                                        this.plan_decision(PlanDecision::Revise, cx);
                                    }
                                })
                            })),
                    ),
            );
        Some(if cx.reduce_motion() {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    "plan-reveal",
                    Animation::new(Theme::global(cx).motion_tokens().duration_fast),
                    |panel, progress| panel.opacity(progress),
                )
                .into_any_element()
        })
    }
}
