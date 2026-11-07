use super::{theme::palette, window::DesktopWindow};
use gpui_kit::component::{Disableable, button::Button, text::TextView};
use gpui_kit::{prelude::*, *};
use zevria_workflow::PlanDecision;
impl DesktopWindow {
    pub(super) fn plan_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let model = &self.controller.read(cx).model.root;
        let dialog = model.workflow.dialog()?;
        let artifact = model.workflow.dialog_artifact()?;
        let busy = model.is_busy();
        let markdown = artifact.markdown.clone();
        let title = artifact.title.clone();
        let (_, text) = self.plan.as_ref()?;
        let panel = div()
            .id("plan-review")
            .w(px(430.))
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .border_l_1()
            .border_color(palette(cx).surfaces.border)
            .bg(palette(cx).surfaces.panel)
            .child(div().text_lg().child(title))
            .child(div().flex_1().min_h_0().child(TextView::new(text)))
            .child(
                Button::new("plan-implement")
                    .label("Implement")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.plan_decision(PlanDecision::ImplementCurrent, cx)
                        })
                    })),
            )
            .child(
                Button::new("plan-fresh")
                    .label("Implement in fresh session")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.plan_decision(PlanDecision::ImplementFresh, cx)
                        })
                    })),
            )
            .child(
                Button::new("plan-revise")
                    .label(if dialog.recovering() {
                        "Close"
                    } else {
                        "Revise"
                    })
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            if dialog.recovering() {
                                this.model.root.workflow.hide();
                                cx.notify();
                            } else {
                                this.plan_decision(PlanDecision::Revise, cx);
                            }
                        })
                    })),
            )
            .child(
                Button::new("plan-copy")
                    .label("Copy Plan")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(markdown.clone()))
                    }),
            );
        Some(panel.into_any_element())
    }
}
