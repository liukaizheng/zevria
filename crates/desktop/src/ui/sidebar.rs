use super::{
    style,
    theme::{palette, tones},
    window::{DesktopWindow, Replacement},
};
use gpui_kit::component::{
    Disableable, IconName, Sizable,
    button::{Button, ButtonVariants},
    tooltip::Tooltip,
};
use gpui_kit::{prelude::*, *};
use std::time::SystemTime;
use zevria_presentation::{
    blocks::{PresentationBlockKind, PresentationRole, subtask_icon},
    conversation::HistoryEntry,
    status_icon::StatusIcon,
};
impl DesktopWindow {
    pub(super) fn workspace_name(&self, cx: &App) -> String {
        self.controller
            .read(cx)
            .workspace
            .as_deref()
            .and_then(|p| p.file_name())
            .map_or_else(|| "Zevria".into(), |n| n.to_string_lossy().into_owned())
    }
    pub(super) fn sidebar(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let count = 1
            + self.controller.read(cx).model.children.len()
            + self.controller.read(cx).sessions.len();
        self.sidebar_focus.resize_with(count, || cx.focus_handle());
        let controller = self.controller.read(cx);
        let t = palette(cx);
        let tone = tones(cx);
        let transitioning = matches!(
            controller.phase,
            crate::controller::Phase::Starting | crate::controller::Phase::Stopping
        );
        let workspace = controller
            .workspace
            .as_ref()
            .map_or_else(|| "No workspace".into(), |p| p.display().to_string());
        let full_path = workspace.clone();
        let side = div()
            .id("sidebar")
            .w(px(style::SIDEBAR))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(16.))
            .bg(t.surfaces.panel)
            .border_r_1()
            .border_color(tone.hairline)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .line_height(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(self.workspace_name(cx)),
                            )
                            .child(
                                style::meta()
                                    .id("workspace-path")
                                    .font_family(style::MONO_FONT)
                                    .truncate()
                                    .text_color(t.text.muted)
                                    .tooltip(move |w, cx| {
                                        Tooltip::new(full_path.clone()).build(w, cx)
                                    })
                                    .child(middle_path(&workspace, 34)),
                            ),
                    )
                    .child(
                        style::icon_button(
                            "open-workspace",
                            IconName::FolderOpen,
                            "Open workspace… ⌘O",
                        )
                        .disabled(transitioning)
                        .on_click(cx.listener(|this, _, _, cx| this.open_workspace(cx))),
                    ),
            )
            .child(
                Button::new("new-session")
                    .ghost()
                    .small()
                    .w_full()
                    .justify_start()
                    .disabled(transitioning)
                    .icon(IconName::Plus)
                    .label("New session")
                    .child(div().flex_1())
                    .child(style::key_hint("cmd-n", "", cx))
                    .on_click(
                        cx.listener(|this, _, _, cx| this.request_replace(Replacement::New, cx)),
                    ),
            );
        let title = controller
            .model
            .root
            .conversation
            .history()
            .iter()
            .find_map(|entry| {
                let HistoryEntry::Conversation(entry) = entry else {
                    return None;
                };
                entry.blocks.iter().find_map(|block| match &block.kind {
                    PresentationBlockKind::Text { text, .. }
                        if block.role == Some(PresentationRole::User) =>
                    {
                        text.lines()
                            .find(|line| !line.trim().is_empty())
                            .map(|line| line.trim().to_owned())
                    }
                    _ => None,
                })
            })
            .unwrap_or_else(|| "New session".into());
        let selected = controller.model.selected_child.is_none();
        let mode_ink = super::status::accent(&controller.model.root, cx);
        let mut listing = div()
            .id("session-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.))
            .px(px(4.))
            .py(px(4.))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let step = match event.keystroke.key.as_str() {
                    "up" => -1isize,
                    "down" => 1,
                    _ => return,
                };
                let current = this
                    .sidebar_focus
                    .iter()
                    .position(|f| f.is_focused(window))
                    .unwrap_or(0);
                let next = (current as isize + step)
                    .clamp(0, this.sidebar_focus.len().saturating_sub(1) as isize)
                    as usize;
                if let Some(focus) = this.sidebar_focus.get(next) {
                    window.focus(focus, cx);
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(style::group_label("This session", cx))
            .child(
                Button::new("root-pane")
                    .ghost()
                    .small()
                    .w_full()
                    .justify_start()
                    .track_focus(&self.sidebar_focus[0])
                    .accessibility_label(format!("This session: {title}"))
                    .border_l_2()
                    .border_color(if selected { mode_ink } else { t.surfaces.panel })
                    .when(selected, |b| b.bg(tone.list_active))
                    .child(style::status_glyph(
                        if controller.model.root.is_busy() {
                            StatusIcon::Running
                        } else {
                            StatusIcon::Pending
                        },
                        mode_ink,
                        cx,
                    ))
                    .child(div().min_w_0().truncate().text_size(px(14.)).child(title))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller.model.selected_child = None;
                            cx.notify();
                        })
                    })),
            );
        for (index, child) in controller.model.children.iter().enumerate() {
            let id = child.id.clone();
            let selected = controller.model.selected_child.as_ref() == Some(&id);
            let ink = match child.kind {
                zevria_foundation::SubtaskKind::Explore => t.workflow.explore,
                zevria_foundation::SubtaskKind::Build => t.workflow.build,
            };
            let status = child
                .status
                .map(subtask_icon)
                .unwrap_or(StatusIcon::Pending);
            listing = listing.child(
                div().ml(px(12.)).child(
                    Button::new(("subtask", index))
                        .ghost()
                        .w_full()
                        .h_auto()
                        .px(px(8.))
                        .py(px(8.))
                        .track_focus(&self.sidebar_focus[index + 1])
                        .accessibility_label(format!("Inspect {}: {}", child.kind, child.title))
                        .border_l_2()
                        .border_color(if selected { ink } else { tone.hairline })
                        .when(selected, |b| b.bg(tone.list_active))
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(style::status_glyph(status, ink, cx))
                                .child(
                                    style::label()
                                        .text_color(ink)
                                        .child(style::kind_label(child.kind)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(14.))
                                        .child(child.title.clone()),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.controller.update(cx, |controller, cx| {
                                controller.model.selected_child = Some(id.clone());
                                cx.notify();
                            })
                        })),
                ),
            );
        }
        listing = listing.child(
            style::group_label(
                if controller.show_sessions {
                    "Choose a session to resume"
                } else {
                    "Earlier sessions"
                },
                cx,
            )
            .when(controller.show_sessions, |l| l.text_color(t.workflow.plan)),
        );
        if controller.sessions.is_empty() {
            listing = listing.child(
                style::meta()
                    .px(px(8.))
                    .text_color(t.text.muted)
                    .child("Earlier sessions in this workspace appear here."),
            );
        }
        for (index, session) in controller.sessions.iter().enumerate() {
            let path = session.path.clone();
            let title = session
                .preview
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Untitled session".into());
            listing = listing.child(
                Button::new(("resume", index))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .justify_start()
                    .px(px(8.))
                    .py(px(8.))
                    .track_focus(&self.sidebar_focus[1 + controller.model.children.len() + index])
                    .disabled(transitioning)
                    .tooltip(session.id.clone())
                    .accessibility_label(format!("Resume {title}"))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(14.))
                                    .line_height(px(20.))
                                    .child(title),
                            )
                            .child(
                                style::meta()
                                    .flex_shrink_0()
                                    .text_color(t.text.muted)
                                    .child(super::time::relative(
                                        SystemTime::now(),
                                        session.modified,
                                    )),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_replace(Replacement::Resume(path.clone()), cx)
                    })),
            );
        }
        side.child(listing).into_any_element()
    }
}
pub(super) fn middle_path(path: &str, limit: usize) -> String {
    let chars = path.chars().collect::<Vec<_>>();
    if chars.len() <= limit {
        return path.into();
    }
    let start = limit / 3;
    format!(
        "{}…{}",
        chars[..start].iter().collect::<String>(),
        chars[chars.len() - (limit - start - 1)..]
            .iter()
            .collect::<String>()
    )
}
