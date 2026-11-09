use super::{style, theme::palette, window::DesktopWindow};
use crate::controller::Phase;
use gpui_kit::base::text::TextView;
use gpui_kit::component::{
    Icon, IconName,
    button::{Button, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use zevria_presentation::status_icon::StatusIcon;
impl DesktopWindow {
    pub(super) fn screen(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let controller = self.controller.read(cx);
        let t = palette(cx);
        let mut block = div()
            .w_full()
            .max_w(px(480.))
            .flex()
            .flex_col()
            .items_start()
            .gap(px(16.));
        let (title, body, icon) = if controller.config_error.is_some() {
            (
                "Finish setting up Zevria",
                "Zevria needs a model provider before it can start a session. Fill in the configuration files, then try again.",
                IconName::Settings,
            )
        } else if controller.workspace.is_none() {
            (
                "Open a workspace",
                "Zevria works inside one project folder at a time.",
                IconName::FolderOpen,
            )
        } else {
            match &controller.phase {
                Phase::Starting => (
                    "Starting session",
                    "Restoring the transcript and connecting to the model.",
                    IconName::LoaderCircle,
                ),
                Phase::Stopping => (
                    "Stopping session",
                    "Waiting for running tools to finish.",
                    IconName::LoaderCircle,
                ),
                Phase::Failed(_) => (
                    "The session stopped",
                    "Details are in the Zevria desktop log.",
                    IconName::CircleAlert,
                ),
                Phase::Idle => ("Ready to start", "", IconName::Folder),
                Phase::Live => return None,
            }
        };
        if matches!(controller.phase, Phase::Starting | Phase::Stopping) {
            block = block.child(
                style::status_glyph(StatusIcon::Running, t.text.secondary, cx)
                    .text_size(px(24.))
                    .line_height(px(32.)),
            );
        } else {
            block = block.child(Icon::new(icon).size(px(24.)).text_color(t.text.secondary));
        }
        block = block.child(
            div()
                .text_size(px(24.))
                .line_height(px(30.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        );
        if !body.is_empty() {
            block = block.child(style::body().text_color(t.text.secondary).child(body));
        }
        if let Some((_, text)) = &self.screen_error {
            block = block.child(
                div()
                    .w_full()
                    .max_h(px(180.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(t.surfaces.panel)
                    .child(
                        TextView::new(text)
                            .style(style::markdown(cx, false))
                            .font_family(style::MONO_FONT)
                            .map(|view| style::CODE.apply(view)),
                    ),
            );
        }
        let mut actions = div().flex().items_center().gap(px(8.));
        if controller.config_error.is_some() {
            actions = actions
                .child(
                    Button::new("reveal-config")
                        .primary()
                        .label("Show config in Finder")
                        .on_click(|_, _, cx| {
                            if let Ok(path) = zevria_foundation::config::config_path() {
                                cx.reveal_path(&path);
                            }
                        }),
                )
                .child(
                    Button::new("retry-config")
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.update(cx, |this, cx| this.retry_config(cx))
                        })),
                );
        } else if controller.workspace.is_none() {
            actions = actions.child(
                Button::new("choose-workspace")
                    .primary()
                    .label("Open workspace…")
                    .child(style::kbd_hint("⌘O", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.open_workspace(cx))),
            );
        } else {
            match controller.phase {
                Phase::Failed(_) => {
                    actions = actions.child(
                        Button::new("restart-session")
                            .primary()
                            .label("Start new session")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |this, cx| this.new_session(cx))
                            })),
                    )
                }
                Phase::Idle => {
                    actions = actions.child(
                        Button::new("start-session")
                            .primary()
                            .label("Start session")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller
                                    .update(cx, |this, cx| this.start_initial(false, cx))
                            })),
                    )
                }
                _ => {}
            }
        }
        Some(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(32.))
                .child(block.child(actions))
                .into_any_element(),
        )
    }
    pub(super) fn confirm_replace(&self, cx: &mut Context<Self>) -> AnyElement {
        style::scrim(cx).id("confirm-replacement")
            .child(style::sheet(cx).w(px(420.))
                .child(div().text_size(px(18.)).line_height(px(24.)).font_weight(FontWeight::SEMIBOLD).child("Stop the current turn?"))
                .child(div().text_size(px(14.)).line_height(px(20.)).text_color(palette(cx).text.secondary)
                    .child("Zevria is still working. Switching sessions stops this turn first."))
                .child(div().flex().justify_end().gap(px(8.))
                    .child(Button::new("cancel-replace").ghost().label("Keep working")
                        .on_click(cx.listener(|this,_,_,cx| { this.pending_replace = None; cx.notify(); })))
                    .child(Button::new("confirm-replace").danger().label("Stop and switch")
                        .on_click(cx.listener(|this,_,_,cx| {
                            if let Some(replacement) = this.pending_replace.take() { this.perform_replace(replacement,cx); }
                            cx.notify();
                        })))))
            .into_any_element()
    }
}
