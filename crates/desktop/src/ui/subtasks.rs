//! Session pane switcher. The selected pane's transcript stays in the center.
use super::{
    status, style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::{actions, session::ChildPane};
use gpui_kit::{
    component::{
        IconName,
        button::{Button, ButtonVariants},
    },
    prelude::*,
    *,
};
use std::collections::HashMap;
use zevria_foundation::{SubtaskId, SubtaskKind};
use zevria_presentation::{blocks::subtask_icon, status_icon::StatusIcon, subtasks::summary_title};

const GLYPH_WIDTH: f32 = 16.;
const GLYPH_GAP: f32 = 8.;

pub(super) struct SubtasksPanelView {
    pub(super) visible: bool,
    pub(super) focus: FocusHandle,
    pub(super) list_focus: FocusHandle,
    row_focus: HashMap<Option<SubtaskId>, FocusHandle>,
    pub(super) list_scroll: ScrollHandle,
}

impl SubtasksPanelView {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            visible: false,
            focus: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            row_focus: HashMap::new(),
            list_scroll: ScrollHandle::new(),
        }
    }
}

impl DesktopWindow {
    pub(super) fn subtasks_panel(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let model = &self.controller.read(cx).model;
        if !model.subtasks_panel_visible() {
            return None;
        }
        let summary = summary_title(model.children.iter().map(|child| child.status));
        let summary = summary
            .strip_prefix("Subtasks · ")
            .unwrap_or(&summary)
            .to_owned();
        let t = palette(cx);
        let tone = tones(cx);
        let view = &mut self.subtasks_ui;
        view.row_focus.retain(|id, _| {
            id.as_ref()
                .is_none_or(|id| model.children.iter().any(|child| &child.id == id))
        });
        let mut list = div()
            .id("subtask-list")
            .test_support()
            .track_focus(&view.list_focus)
            .tab_stop(true)
            .track_scroll(&view.list_scroll)
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(12.))
            .pb(px(10.))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let delta = match event.keystroke.key.as_str() {
                    "up" => -1,
                    "down" => 1,
                    _ => return,
                };
                let view = &this.subtasks_ui;
                let model = &this.controller.read(cx).model;
                let focused = view
                    .row_focus
                    .iter()
                    .find(|(_, focus)| focus.is_focused(window))
                    .map(|(id, _)| id)
                    .unwrap_or(&model.selected_child);
                let current = focused
                    .as_ref()
                    .and_then(|id| model.children.iter().position(|child| &child.id == id))
                    .map_or(0, |index| index + 1);
                let index = current
                    .saturating_add_signed(delta)
                    .min(model.children.len());
                let id = index.checked_sub(1).map(|i| model.children[i].id.clone());
                if let Some(focus) = view.row_focus.get(&id) {
                    window.focus(focus, cx);
                }
                view.list_scroll.scroll_to_item(index);
                this.controller
                    .update(cx, |controller, cx| controller.select_pane(id, cx));
                cx.stop_propagation();
            }));
        for child in std::iter::once(None).chain(model.children.iter().map(Some)) {
            let id = child.map(|child| child.id.clone());
            let focus = view
                .row_focus
                .entry(id.clone())
                .or_insert_with(|| cx.focus_handle());
            let active = id == model.selected_child;
            let (element_id, accessible, ink, row) = match child {
                Some(child) => {
                    let ink = match child.kind {
                        SubtaskKind::Explore => t.workflow.explore,
                        SubtaskKind::Build => t.workflow.build,
                    };
                    (
                        format!("subtask-row-{}", child.id),
                        format!("Inspect {}: {}", child.kind, child.title),
                        ink,
                        subtask_row(child, ink, cx),
                    )
                }
                None => (
                    "subtask-conversation".into(),
                    "Conversation".into(),
                    status::accent(&model.root, cx),
                    style::body()
                        .w_full()
                        .text_left()
                        .pl(px(GLYPH_WIDTH + GLYPH_GAP))
                        .child("Conversation"),
                ),
            };
            let controller = self.controller.clone();
            let focus = focus.clone();
            list = list.child(
                Button::new(SharedString::from(element_id))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .flex_shrink_0()
                    .justify_start()
                    .px(px(8.))
                    .py(px(6.))
                    .track_focus(&focus)
                    .accessibility_label(accessible)
                    .border_l_2()
                    .border_color(if active || focus.is_focused(window) {
                        ink
                    } else {
                        t.surfaces.overlay
                    })
                    .when(active, |row| row.bg(tone.list_active))
                    .child(row)
                    .on_click(move |_, window, cx| {
                        window.focus(&focus, cx);
                        controller
                            .update(cx, |controller, cx| controller.select_pane(id.clone(), cx));
                    }),
            );
        }
        Some(
            style::inspector_panel("subtasks-panel", self.plan_width, self.plan_overlay, cx)
                .test_support()
                .track_focus(&view.focus)
                .key_context("ZevriaSubtasks")
                .on_action(cx.listener(|this, _: &actions::Cancel, _, cx| {
                    this.controller
                        .update(cx, |controller, cx| controller.toggle_subtasks(cx));
                    cx.stop_propagation();
                }))
                .child(style::inspector_header(
                    "Subtasks",
                    summary,
                    style::icon_button(
                        "close-subtasks",
                        IconName::PanelRightClose,
                        "Close Subtasks",
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |controller, cx| controller.toggle_subtasks(cx));
                    })),
                    cx,
                ))
                .child(list)
                .into_any_element(),
        )
    }
}

fn subtask_row(child: &ChildPane, ink: Hsla, cx: &App) -> Div {
    let icon = child.status.map_or(StatusIcon::Pending, subtask_icon);
    let status = child
        .status
        .map_or_else(|| "pending".into(), |status| status.to_string());
    let meta = match &child.workspace {
        Some(workspace) => format!("{status} · {workspace}"),
        None => status,
    };
    div()
        .flex()
        .items_start()
        .gap(px(GLYPH_GAP))
        .w_full()
        .min_w_0()
        .child(
            style::status_glyph(icon, ink, cx)
                .w(px(GLYPH_WIDTH))
                .flex_shrink_0(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            style::label()
                                .flex_shrink_0()
                                .text_color(ink)
                                .child(style::kind_label(child.kind)),
                        )
                        .child(
                            style::body()
                                .min_w_0()
                                .truncate()
                                .child(child.title.clone()),
                        ),
                )
                .child(
                    style::meta()
                        .truncate()
                        .text_color(palette(cx).text.muted)
                        .child(meta),
                ),
        )
}
