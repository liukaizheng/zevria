use super::{
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use gpui_kit::base::text::TextView;
use gpui_kit::component::{
    Disableable, IconName, Sizable, Theme,
    button::{Button, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use zevria_workflow::PlanDecision;
impl DesktopWindow {
    pub(super) fn plan_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = &self.controller.read(cx).model.root;
        let dialog = model.workflow.dialog()?;
        let artifact = model.workflow.dialog_artifact()?;
        let busy = model.is_busy();
        let markdown = artifact.markdown.clone();
        let recovering = dialog.recovering();
        let (_, text) = self.plan.as_ref()?;
        let t = palette(cx);
        let panel = div()
            .id("plan-review")
            .test_support()
            .w(px(self.plan_width))
            .flex_shrink_0()
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(t.surfaces.overlay)
            .border_l_1()
            .border_color(tones(cx).hairline)
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
                                style::icon_button(
                                    "plan-copy",
                                    IconName::Copy,
                                    "Copy plan as Markdown",
                                )
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        markdown.clone(),
                                    ))
                                }),
                            ),
                    )
                    .child(
                        div()
                            .max_h(px(48.))
                            .overflow_hidden()
                            .text_size(px(18.))
                            .line_height(px(24.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(artifact.title.clone()),
                    )
                    .child(
                        style::label()
                            .text_color(t.text.muted)
                            .child(format!("Revision {}", artifact.version.revision)),
                    ),
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
                            .size_full()
                            .style(style::markdown(cx, true))
                            .font_family(style::DOCUMENT_FONT)
                            .text_size(px(16.))
                            .line_height(px(27.)),
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
                            .primary()
                            .label("Implement")
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
