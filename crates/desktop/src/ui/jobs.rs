//! Session commands, presented as a live list above a bounded output tail.
use super::{
    status, style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::{actions, controller::SessionController};
use gpui_kit::{
    component::{IconName, button::Button},
    prelude::*,
    *,
};
use std::collections::HashMap;
use zevria_foundation::CommandJobId;
use zevria_presentation::jobs::{
    JobOutput, JobRow, JobsInspector, JobsMessage, summary_title, tail_byte_label,
    terminal_tail_text,
};

pub(super) struct JobsPanelView {
    pub(super) visible: bool,
    pub(super) focus: FocusHandle,
    pub(super) list_focus: FocusHandle,
    row_focus: HashMap<CommandJobId, FocusHandle>,
    list_scroll: ScrollHandle,
    output_scroll: ScrollHandle,
    selected: Option<CommandJobId>,
    copy_feedback: style::CopyFeedback,
}

impl JobsPanelView {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            visible: false,
            focus: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            row_focus: HashMap::new(),
            list_scroll: ScrollHandle::new(),
            output_scroll: ScrollHandle::new(),
            selected: None,
            copy_feedback: style::CopyFeedback::default(),
        }
    }

    pub(super) fn sync_selection(&mut self, inspector: Option<&JobsInspector>) {
        let selected = inspector.and_then(JobsInspector::inspected);
        if self.selected.as_ref() == selected {
            return;
        }
        self.selected = selected.cloned();
        self.copy_feedback.clear();
        self.output_scroll = ScrollHandle::new();
        if let Some(inspector) = inspector
            && let Some(index) = inspector
                .jobs()
                .iter()
                .position(|job| Some(&job.id) == selected)
        {
            self.list_scroll.scroll_to_item(index);
        }
    }
}

impl DesktopWindow {
    fn copy_job_output(&mut self, id: &CommandJobId, cx: &mut Context<Self>) {
        let model = &self.controller.read(cx).model;
        let inspector = model.root.jobs_inspector();
        if !model.jobs_panel_visible() || inspector.inspected() != Some(id) {
            return;
        }
        let Some(output) = inspector.detail() else {
            return;
        };
        let mut copy = output.stdout_tail.clone();
        if !output.stderr_tail.is_empty() {
            if !copy.is_empty() && !copy.ends_with('\n') {
                copy.push('\n');
            }
            copy.push_str(&output.stderr_tail);
        }
        if !copy.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(copy));
            self.jobs_ui
                .copy_feedback
                .trigger((), cx, |this| &mut this.jobs_ui.copy_feedback);
        }
    }

    pub(super) fn jobs_panel(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let owner = cx.entity();
        let controller = self.controller.read(cx);
        if !controller.model.jobs_panel_visible() {
            return None;
        }
        let inspector = controller.model.root.jobs_inspector();
        let jobs = inspector.jobs();
        let selected = inspector.inspected();
        let summary = summary_title(jobs);
        let summary = summary
            .strip_prefix("Jobs · ")
            .unwrap_or(&summary)
            .to_owned();
        let selected_job = selected.and_then(|id| jobs.iter().find(|job| &job.id == id));
        let ink = status::accent(&controller.model.root, cx);
        let t = palette(cx);
        let tone = tones(cx);
        let view = &mut self.jobs_ui;
        view.row_focus
            .retain(|id, _| jobs.iter().any(|job| &job.id == id));
        let mut list = div()
            .id("job-list")
            .test_support()
            .track_focus(&view.list_focus)
            .tab_stop(true)
            .track_scroll(&view.list_scroll)
            .overflow_y_scroll()
            .max_h(relative(0.4))
            .flex_shrink_0()
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
                let inspector = this.controller.read(cx).model.root.jobs_inspector();
                let jobs = inspector.jobs();
                let current = jobs
                    .iter()
                    .position(|job| {
                        this.jobs_ui
                            .row_focus
                            .get(&job.id)
                            .is_some_and(|focus| focus.contains_focused(window, cx))
                    })
                    .or_else(|| {
                        jobs.iter()
                            .position(|job| Some(&job.id) == inspector.inspected())
                    })
                    .unwrap_or(0);
                let index = current
                    .saturating_add_signed(delta)
                    .min(jobs.len().saturating_sub(1));
                if let Some(job) = jobs.get(index) {
                    let id = job.id.clone();
                    if let Some(focus) = this.jobs_ui.row_focus.get(&id) {
                        window.focus(focus, cx);
                    }
                    this.controller
                        .update(cx, |controller, cx| controller.inspect_job(id, cx));
                }
                cx.stop_propagation();
            }));
        if jobs.is_empty() {
            list = list.child(
                style::meta()
                    .px(px(8.))
                    .py(px(10.))
                    .text_color(t.text.muted)
                    .child("No commands in this session."),
            );
        }
        for job in jobs {
            let id = job.id.clone();
            let row_focus = view
                .row_focus
                .entry(id.clone())
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let active = Some(&id) == selected;
            let focused = row_focus.contains_focused(window, cx);
            let show_controls = focused || (active && view.list_focus.is_focused(window));
            let row = JobRow::from(job);
            let select = self.controller.clone();
            let keyboard = select.clone();
            let key_id = id.clone();
            let key_focus = row_focus.clone();
            let mut item = div()
                .id(SharedString::from(format!("job-row-{id}")))
                .test_support()
                .group("job-row")
                .track_focus(&row_focus)
                .tab_stop(true)
                .rounded(px(5.))
                .border_l_2()
                .border_color(if focused || active {
                    ink
                } else {
                    t.surfaces.overlay
                })
                .when(active, |item| item.bg(tone.list_active))
                .hover(|item| item.bg(tone.hover))
                .focus_visible(|item| item.border_color(ink).bg(tone.hover))
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .on_key_down(move |event, window, cx| {
                    if key_focus.is_focused(window)
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        keyboard.update(cx, |controller, cx| {
                            controller.inspect_job(key_id.clone(), cx)
                        });
                        cx.stop_propagation();
                    }
                })
                .child(
                    div()
                        .id("select-job")
                        .test_support()
                        .cursor_pointer()
                        .flex_1()
                        .min_w_0()
                        .on_click(move |_, window, cx| {
                            window.focus(&row_focus, cx);
                            select.update(cx, |controller, cx| {
                                controller.inspect_job(id.clone(), cx)
                            });
                        })
                        .child(job_row(&row, ink, cx)),
                );
            if JobsInspector::can_terminate(job) {
                item = item.child(
                    div()
                        .flex_shrink_0()
                        .opacity(if show_controls { 1. } else { 0. })
                        .group_hover("job-row", |s| s.opacity(1.))
                        .child(control_buttons(
                            &job.id,
                            JobsInspector::can_background(job),
                            &self.controller,
                        )),
                );
            }
            list = list.child(item);
        }
        // Compare with the old extent before laying out the new tail. If the
        // reader has scrolled up, leave their offset alone until they reach the end.
        let scroll = &view.output_scroll;
        if scroll.max_offset().y + scroll.offset().y <= px(6.) {
            scroll.scroll_to_bottom();
        }
        let mut panel =
            style::inspector_panel("jobs-panel", self.plan_width, self.plan_overlay, cx)
                .test_support()
                .track_focus(&view.focus)
                .key_context("ZevriaJobs")
                .on_action(cx.listener(|this, _: &actions::Cancel, _, cx| {
                    this.controller
                        .update(cx, |controller, cx| controller.toggle_jobs(cx));
                    cx.stop_propagation();
                }))
                .child(style::inspector_header(
                    "Jobs",
                    summary,
                    style::icon_button("close-jobs", IconName::PanelRightClose, "Close Jobs")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller
                                .update(cx, |controller, cx| controller.toggle_jobs(cx));
                        })),
                    cx,
                ))
                .child(list);
        if let Some(id) = selected {
            panel = panel.child(detail(
                id,
                selected_job.is_some_and(JobsInspector::can_terminate),
                inspector.detail(),
                scroll,
                owner,
                view.copy_feedback.is_active(()),
                cx,
            ));
        }
        if let Some(JobsMessage::Rejected(message)) = inspector.message() {
            panel = panel.child(
                style::meta()
                    .flex_shrink_0()
                    .px(px(20.))
                    .py(px(8.))
                    .text_color(t.feedback.warning)
                    .child(message.clone()),
            );
        }
        Some(panel.into_any_element())
    }
}

pub(super) fn job_row(row: &JobRow, ink: Hsla, cx: &App) -> Div {
    let t = palette(cx);
    let spans: Vec<_> = row
        .command
        .spans
        .iter()
        .map(|(range, token)| {
            (
                range.clone(),
                HighlightStyle {
                    color: Some(style::shell_ink(*token, &t)),
                    ..Default::default()
                },
            )
        })
        .collect();
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .py(px(5.))
        .child(
            style::status_glyph(row.icon, ink, cx)
                .w(px(16.))
                .flex_shrink_0(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    style::mono()
                        .truncate()
                        .child(StyledText::new(row.command.text.clone()).with_highlights(spans)),
                )
                .child(
                    style::meta()
                        .flex()
                        .min_w_0()
                        .text_color(t.text.muted)
                        .child(format!("{} · {} · ", row.mode, row.duration))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(style::glyph_color(row.icon, ink, cx))
                                .child(row.status.clone()),
                        ),
                ),
        )
}

pub(super) fn stop_button(id: CommandJobId, controller: Entity<SessionController>) -> Button {
    style::icon_button("job-stop", IconName::Square, "Stop command").on_click(move |_, _, cx| {
        controller.update(cx, |controller, cx| {
            controller.terminate_job(id.clone(), cx)
        });
    })
}

pub(super) fn background_button(id: CommandJobId, controller: Entity<SessionController>) -> Button {
    style::icon_button(
        "job-background",
        IconName::ArrowDown,
        "Move command to background",
    )
    .on_click(move |_, _, cx| {
        controller.update(cx, |controller, cx| {
            controller.background_job(id.clone(), cx)
        });
    })
}

fn control_buttons(
    id: &CommandJobId,
    foreground: bool,
    controller: &Entity<SessionController>,
) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(2.))
        .child(stop_button(id.clone(), controller.clone()))
        .when(foreground, |buttons| {
            buttons.child(background_button(id.clone(), controller.clone()))
        })
}

fn detail(
    id: &CommandJobId,
    running: bool,
    view: Option<&JobOutput>,
    scroll: &ScrollHandle,
    owner: Entity<DesktopWindow>,
    copied: bool,
    cx: &App,
) -> Div {
    let t = palette(cx);
    let mut heading = div()
        .flex()
        .items_center()
        .justify_between()
        .flex_shrink_0()
        .px(px(20.))
        .py(px(10.))
        .child(style::tag(id.to_string(), t.text.muted, cx));
    let mut output = div()
        .id("job-output")
        .test_support()
        .track_scroll(scroll)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(20.))
        .pb(px(16.));
    if let Some(view) =
        view.filter(|view| !view.stdout_tail.is_empty() || !view.stderr_tail.is_empty())
    {
        let job_id = id.clone();
        heading = heading.child(
            style::copy_button(
                "job-copy-output",
                copied,
                "Copy output",
                "Copied output",
                cx,
            )
            .on_click(move |_, _, cx| {
                owner.update(cx, |this, cx| this.copy_job_output(&job_id, cx));
            }),
        );
        for (label, tail, bytes, ink) in [
            (
                "stdout",
                &view.stdout_tail,
                view.stdout_bytes,
                t.text.primary,
            ),
            (
                "stderr",
                &view.stderr_tail,
                view.stderr_bytes,
                t.feedback.warning,
            ),
        ] {
            if tail.is_empty() {
                continue;
            }
            output = output.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_shrink_0()
                    .child(
                        style::meta()
                            .text_color(t.text.secondary)
                            .child(format!("{label} · {}", tail_byte_label(bytes))),
                    )
                    .child(
                        style::mono()
                            .text_color(ink)
                            .child(terminal_tail_text(tail)),
                    ),
            );
        }
    } else {
        output = output.child(style::meta().text_color(t.text.muted).child(if running {
            "No output yet."
        } else {
            "No output."
        }));
    }
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .border_t_1()
        .border_color(tones(cx).hairline)
        .child(heading)
        .child(output)
}
