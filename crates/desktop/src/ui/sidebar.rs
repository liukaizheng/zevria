use super::{
    theme::palette,
    window::{DesktopWindow, Replacement},
};
use gpui_kit::component::{Disableable, button::Button};
use gpui_kit::{prelude::*, *};
impl DesktopWindow {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let t = palette(cx);
        let transitioning = matches!(
            controller.phase,
            crate::controller::Phase::Starting | crate::controller::Phase::Stopping
        );
        let workspace = controller
            .workspace
            .as_ref()
            .map_or_else(|| "No workspace".into(), |p| p.display().to_string());
        let side = div()
            .id("sidebar")
            .w(px(245.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .bg(t.surfaces.panel)
            .border_r_1()
            .border_color(t.surfaces.border)
            .child(div().text_lg().child("Zevria"))
            .child(div().text_xs().child(workspace))
            .child(
                Button::new("open-workspace")
                    .label("Open Workspace…")
                    .disabled(transitioning)
                    .on_click(cx.listener(|this, _, _, cx| this.open_workspace(cx))),
            )
            .child(
                Button::new("new-session")
                    .label("New session")
                    .disabled(transitioning)
                    .on_click(
                        cx.listener(|this, _, _, cx| this.request_replace(Replacement::New, cx)),
                    ),
            )
            .child(
                Button::new("root-pane")
                    .label("← Conversation")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.model.selected_child = None;
                            cx.notify();
                        })
                    })),
            );
        let mut listing = div()
            .id("session-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        for (index, child) in controller.model.children.iter().enumerate() {
            let id = child.id.clone();
            listing = listing.child(
                Button::new(("subtask", index))
                    .label(format!(
                        "{} · {} · {}",
                        child.kind,
                        child.title,
                        child
                            .status
                            .map_or_else(|| "waiting".into(), |status| status.to_string())
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller.update(cx, |this, cx| {
                            this.model.selected_child = Some(id.clone());
                            cx.notify();
                        })
                    })),
            );
        }
        listing = listing.child(div().text_sm().text_color(t.text.muted).child(
            if controller.show_sessions {
                "Choose a previous session"
            } else {
                "Previous sessions"
            },
        ));
        for (index, session) in controller.sessions.iter().enumerate() {
            let path = session.path.clone();
            listing = listing.child(
                Button::new(("resume", index))
                    .label(format!(
                        "{} · {}",
                        session.id,
                        session.preview.as_deref().unwrap_or("No preview")
                    ))
                    .disabled(transitioning)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_replace(Replacement::Resume(path.clone()), cx)
                    })),
            );
        }
        side.child(listing).into_any_element()
    }
    pub(super) fn status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let model = controller.model.visible();
        let primary = super::status::primary(model, controller.cancelling);
        let telemetry = super::status::telemetry(model);
        let accent = super::status::accent(model, cx);
        let retained = controller
            .model
            .root
            .workflow
            .submitted_artifact()
            .is_some();
        let mut status = div()
            .id("status-bar")
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(palette(cx).surfaces.border)
            .text_sm()
            .child(div().text_color(accent).child(primary))
            .child(div().flex_1().text_xs().child(telemetry));
        if retained {
            status = status.child(Button::new("review-plan").label("Review Plan").on_click(
                cx.listener(|this, _, _, cx| {
                    this.controller.update(cx, |this, cx| {
                        this.model.root.workflow.open_review();
                        cx.notify();
                    })
                }),
            ));
        }
        status.into_any_element()
    }
}
