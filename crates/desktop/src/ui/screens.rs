use super::{theme::palette, window::DesktopWindow};
use crate::controller::Phase;
use gpui_kit::component::button::Button;
use gpui_kit::{prelude::*, *};
impl DesktopWindow {
    pub(super) fn screen(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        let t = palette(cx);
        let mut screen = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .p_8();
        if let Some(error) = &controller.config_error {
            screen = screen
                .child(div().text_xl().child("Configuration needs attention"))
                .child(
                    div()
                        .max_w(px(760.))
                        .text_color(t.feedback.warning)
                        .child(error.clone()),
                )
                .child(
                    Button::new("retry-config")
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |this, cx| this.retry_config(cx))
                        })),
                )
                .child(
                    Button::new("reveal-config")
                        .label("Reveal config")
                        .on_click(|_, _, cx| {
                            if let Ok(path) = zevria_foundation::config::config_path() {
                                cx.reveal_path(&path);
                            }
                        }),
                );
        } else if controller.workspace.is_none() {
            screen = screen.child("Choose a workspace to start Zevria").child(
                Button::new("choose-workspace")
                    .label("Open Workspace…")
                    .on_click(cx.listener(|this, _, _, cx| this.open_workspace(cx))),
            );
        } else {
            match &controller.phase {
                Phase::Starting => screen = screen.child("Connecting and restoring session…"),
                Phase::Stopping => screen = screen.child("Stopping engine and supervisors…"),
                Phase::Failed(error) => {
                    screen = screen
                        .child(div().text_lg().child("Session engine stopped"))
                        .child(error.clone())
                        .child(
                            Button::new("restart-session")
                                .label("Start new session")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |this, cx| this.new_session(cx))
                                })),
                        )
                }
                Phase::Idle => {
                    screen = screen.child("Ready to start").child(
                        Button::new("start-session")
                            .label("Start session")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller
                                    .update(cx, |this, cx| this.start_initial(false, cx))
                            })),
                    )
                }
                Phase::Live => return None,
            }
        }
        Some(screen.into_any_element())
    }
    pub(super) fn confirm_replace(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("confirm-replacement")
            .absolute()
            .inset_0()
            .bg(palette(cx).surfaces.canvas)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .bg(palette(cx).surfaces.overlay)
                    .child("A turn is active. Stop it and replace this session?")
                    .child(
                        Button::new("confirm-replace")
                            .label("Stop and replace")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(replacement) = this.pending_replace.take() {
                                    this.perform_replace(replacement, cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cancel-replace")
                            .label("Keep working")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pending_replace = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}
