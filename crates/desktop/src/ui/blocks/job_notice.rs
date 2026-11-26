//! A completion notice is a record; only the current inspector can make an
//! "also running" row live or actionable.
use super::{BlockKind, BlockView, Phase, jobs, settled_job_badge, style};
use crate::ui::{
    status,
    theme::{palette, tones},
};
use gpui_kit::{
    assets::IconName as Lucide,
    base::TestTextExt,
    component::{IconName, tooltip::Tooltip},
    prelude::*,
    *,
};
use zevria_foundation::CommandJobNoticeKind;
use zevria_presentation::{
    jobs::{
        JobsInspector, NoticeJobOutput, NoticeJobRow, format_job_duration, job_status_icon,
        job_status_text,
    },
    status_icon::StatusIcon,
};

const PREVIEW_LINES: usize = 16;
const SNAPSHOT_TOOLTIP: &str = "Status recorded when this notice was delivered. Background processes do not persist across sessions.";

struct ActionFocus {
    handle: FocusHandle,
    _watch: [Subscription; 2],
}

impl ActionFocus {
    fn new(window: &mut Window, cx: &mut Context<BlockView>) -> Self {
        let handle = cx.focus_handle();
        let focus = cx.on_focus(&handle, window, |_, _, cx| cx.notify());
        let blur = cx.on_blur(&handle, window, |_, _, cx| cx.notify());
        Self {
            handle,
            _watch: [focus, blur],
        }
    }

    fn is_focused(&self, window: &Window) -> bool {
        self.handle.is_focused(window)
    }
}

pub(super) struct NoticeJobFocus {
    show: ActionFocus,
    stop: Option<ActionFocus>,
}

impl NoticeJobFocus {
    pub(super) fn is_focused(&self, window: &Window) -> bool {
        self.show.is_focused(window)
            || self
                .stop
                .as_ref()
                .is_some_and(|stop| stop.is_focused(window))
    }
}

impl BlockView {
    pub(super) fn notice_inspector<'a>(&self, cx: &'a App) -> Option<&'a JobsInspector> {
        // A resumed notice belongs to another runtime, even if a short job ID
        // happens to be reused by a process later in this session.
        if self.row.phase == Phase::Restored {
            return None;
        }
        self.jobs_inspector(cx)
    }

    pub(super) fn sync_notice_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut actions = Vec::new();
        if self.row.expanded
            && let BlockKind::Jobs {
                finished, running, ..
            } = &self.data.kind
            && let Some(inspector) = self.notice_inspector(cx)
        {
            actions.extend(
                finished
                    .iter()
                    .map(|row| (false, &row.id))
                    .chain(running.iter().map(|row| (true, &row.id)))
                    .filter_map(|(was_running, id)| {
                        let job = inspector.job(id)?;
                        let can_stop = was_running && JobsInspector::can_terminate(job);
                        Some(((was_running, id.clone()), can_stop))
                    }),
            );
        }
        let can_inspect = self.can_inspect_jobs(cx);
        let mut old = std::mem::take(&mut self.notice_job_focus);
        let mut lost_focus = false;
        for (key, can_stop) in actions {
            if !can_inspect && !can_stop {
                continue;
            }
            let mut focus = old.remove(&key).unwrap_or_else(|| NoticeJobFocus {
                show: ActionFocus::new(window, cx),
                stop: None,
            });
            if can_stop {
                focus
                    .stop
                    .get_or_insert_with(|| ActionFocus::new(window, cx));
            } else {
                lost_focus |= focus
                    .stop
                    .take()
                    .is_some_and(|stop| stop.is_focused(window));
            }
            lost_focus |= !can_inspect && focus.show.is_focused(window);
            self.notice_job_focus.insert(key, focus);
        }
        if lost_focus || old.values().any(|focus| focus.is_focused(window)) {
            window.focus(&self.disclosure_focus, cx);
        }
    }

    pub(super) fn jobs_notice(
        &self,
        kind: CommandJobNoticeKind,
        finished: &[NoticeJobRow],
        running: &[NoticeJobRow],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = palette(cx);
        let ink = status::accent(&self.controller.read(cx).model.root, cx);
        let inspector = self.notice_inspector(cx);
        let live_count = running
            .iter()
            .filter(|row| {
                inspector
                    .and_then(|inspector| inspector.job(&row.id))
                    .is_some_and(JobsInspector::can_terminate)
            })
            .count();
        let count = finished.len();
        let failed = finished
            .iter()
            .filter(|row| row.icon == StatusIcon::Failed)
            .count();
        let outcome = if failed > 0 {
            StatusIcon::Failed
        } else if finished.iter().any(|row| row.icon != StatusIcon::Done) {
            StatusIcon::Interrupted
        } else {
            StatusIcon::Done
        };
        let (label, summary) = if let [job] = finished {
            ("Background job", jobs::job_command(&job.command, cx))
        } else {
            let mut text = format!("{count} finished");
            let start = text.len();
            if failed > 0 {
                text.push_str(&format!(" · {failed} failed"));
            }
            let highlights = (failed > 0).then(|| (start..text.len(), t.feedback.error.into()));
            let text: SharedString = text.into();
            (
                "Background jobs",
                style::meta()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text.secondary)
                    .child(
                        StyledText::new(text.clone())
                            .with_highlights(highlights)
                            .test_text(text),
                    ),
            )
        };
        let mut header = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .child(style::step_icon(Lucide::SquareTerminal, t.text.muted)),
            )
            .child(
                style::label()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_color(t.text.secondary)
                    .child(label),
            )
            .when(kind == CommandJobNoticeKind::Continuation, |header| {
                header.child(
                    style::tag("Resumed", t.text.secondary, cx)
                        .whitespace_nowrap()
                        .flex_shrink_0()
                        .id("notice-resumed")
                        .test_support(),
                )
            })
            .child(summary.flex_1());
        if let [job] = finished {
            header = header
                .when_some(
                    settled_job_badge(&job.status_kind, cx)
                        .filter(|_| job.icon != StatusIcon::Done),
                    |header, (text, ink)| header.child(style::step_badge(text, ink)),
                )
                .when_some(job.recorded_duration.as_ref(), |header, duration| {
                    header.child(style::step_badge(duration.clone(), t.text.muted))
                });
        }
        header = header
            .when(live_count > 0, |header| {
                header.child(
                    style::step_badge(format!("{live_count} still running"), ink)
                        .id("notice-live-badge")
                        .test_support(),
                )
            })
            .child(div().flex().flex_shrink_0().child(style::step_status(
                outcome,
                t.text.muted,
                cx,
            )));
        let title = match finished {
            [job] => format!("{label}: {}", job.command.text),
            _ => format!("{label}: {count} finished"),
        };
        div()
            .id("timeline-jobs-notice")
            .test_support()
            .group("timeline-block")
            .min_w_0()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(tones(cx).hairline)
            .bg(t.surfaces.panel)
            .child(self.disclosure_header(title, header, window, cx))
            .when(self.row.expanded, |card| {
                card.child(self.notice_details(finished, running, live_count, ink, window, cx))
            })
    }

    fn notice_details(
        &self,
        finished: &[NoticeJobRow],
        running: &[NoticeJobRow],
        live_count: usize,
        ink: Hsla,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = palette(cx);
        let mut budget = if self.show_all_output {
            usize::MAX
        } else {
            PREVIEW_LINES
        };
        let mut lines = 0;
        let mut finished_rows =
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .min_w_0()
                .when(!running.is_empty(), |rows| {
                    rows.child(
                        style::label()
                            .text_color(t.text.secondary)
                            .child("Finished")
                            .id("notice-finished-heading")
                            .test_support(),
                    )
                });
        for row in finished {
            let mut item = div()
                .min_w_0()
                .child(self.notice_job_row(row, false, ink, window, cx));
            if let Some(output) = &row.output {
                lines += [&output.note, &output.stdout, &output.stderr]
                    .into_iter()
                    .flatten()
                    .map(|text| text.lines().count())
                    .sum::<usize>();
                item = item.children(captured_output(output, &mut budget, cx));
            }
            finished_rows = finished_rows.child(item);
        }
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap(px(8.))
            .pl(px(32.))
            .pr(px(12.))
            .pt(px(4.))
            .pb(px(10.))
            .child(finished_rows)
            .when(lines > PREVIEW_LINES, |details| {
                details.child(self.output_toggle(lines, cx))
            })
            .when(!running.is_empty(), |details| {
                details.child(div().h(px(1.)).bg(tones(cx).hairline)).child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            style::label()
                                .text_color(t.text.muted)
                                .child(if live_count > 0 {
                                    "Still running"
                                } else {
                                    "Also running at the time"
                                })
                                .id(if live_count > 0 {
                                    "notice-still-running"
                                } else {
                                    "notice-also-running"
                                })
                                .test_support(),
                        )
                        .children(
                            running
                                .iter()
                                .map(|row| self.notice_job_row(row, true, ink, window, cx)),
                        ),
                )
            })
    }

    fn notice_job_row(
        &self,
        row: &NoticeJobRow,
        was_running: bool,
        ink: Hsla,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let t = palette(cx);
        let live = self.notice_inspector(cx).and_then(|jobs| jobs.job(&row.id));
        let historical = was_running && live.is_none();
        let (icon, elapsed, status_text) = match (was_running, live) {
            (true, Some(job)) if JobsInspector::can_terminate(job) => (
                StatusIcon::Running,
                Some(format_job_duration(
                    job.started_at.elapsed().unwrap_or_default(),
                )),
                "running".into(),
            ),
            (true, Some(job)) => (
                job_status_icon(&job.status),
                row.recorded_duration
                    .as_ref()
                    .map(|time| format!("{time} at notice")),
                format!("now {}", job_status_text(&job.status)),
            ),
            (true, None) => (
                StatusIcon::Pending,
                row.recorded_duration
                    .as_ref()
                    .map(|time| format!("{time} at notice")),
                "was running".into(),
            ),
            (false, _) => (
                // A finished record must never animate, even for malformed fixture text.
                if row.icon == StatusIcon::Running {
                    StatusIcon::Pending
                } else {
                    row.icon
                },
                row.recorded_duration.clone(),
                row.status.clone(),
            ),
        };
        let facts = elapsed.map_or_else(
            || format!("{} · ", row.id),
            |time| format!("{} · {time} · ", row.id),
        );
        let actions = self.notice_job_focus.get(&(was_running, row.id.clone()));
        let can_inspect = actions.is_some() && self.can_inspect_jobs(cx);
        let section = if was_running { "running" } else { "finished" };
        let selector = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_start()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .pt(px(2.))
                    .child(if historical {
                        style::step_icon(Lucide::Clock, t.text.muted)
                    } else {
                        style::step_status(icon, ink, cx)
                    })
                    .id(match icon {
                        StatusIcon::Running => "notice-running-icon",
                        _ if historical => "notice-snapshot-clock",
                        _ => "notice-outcome-icon",
                    })
                    .test_support(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(jobs::job_command(&row.command, cx))
                    .child(
                        style::meta()
                            .flex()
                            .min_w_0()
                            .text_color(t.text.muted)
                            .child(style::observed_text(facts))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(style::glyph_color(icon, ink, cx))
                                    .child(style::observed_text(status_text)),
                            ),
                    ),
            )
            .id("notice-select-job")
            .test_support()
            .when(historical, |selector| {
                selector.tooltip(|w, cx| Tooltip::new(SNAPSHOT_TOOLTIP).build(w, cx))
            })
            .when(can_inspect, |selector| {
                let id = row.id.clone();
                let controller = self.controller.clone();
                selector.cursor_pointer().on_click(move |_, _, cx| {
                    controller.update(cx, |c, cx| c.inspect_job(id.clone(), cx));
                })
            });
        div()
            .id(format!("notice-row-{section}-{}", row.id))
            .test_support()
            .group("notice-job")
            .flex()
            .items_center()
            .gap(px(4.))
            .min_w_0()
            .rounded(px(4.))
            .py(px(2.))
            .when(can_inspect, |item| item.hover(|s| s.bg(tones(cx).hover)))
            .child(selector)
            .when_some(
                actions.filter(|focus| can_inspect || focus.stop.is_some()),
                |item, focus| {
                    let id = row.id.clone();
                    let controller = self.controller.clone();
                    item.child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(2.))
                            .opacity(if focus.is_focused(window) { 1. } else { 0. })
                            .group_hover("notice-job", |s| s.opacity(1.))
                            .when(can_inspect, |actions| {
                                actions.child(
                                    style::icon_button(
                                        "notice-show-in-jobs",
                                        IconName::ExternalLink,
                                        "Show in Jobs",
                                    )
                                    .track_focus(&focus.show.handle)
                                    .on_click(
                                        move |_, _, cx| {
                                            controller
                                                .update(cx, |c, cx| c.inspect_job(id.clone(), cx))
                                        },
                                    ),
                                )
                            })
                            .when_some(focus.stop.as_ref(), |actions, stop| {
                                actions.child(
                                    jobs::stop_button(row.id.clone(), self.controller.clone())
                                        .track_focus(&stop.handle),
                                )
                            }),
                    )
                },
            )
    }
}

fn captured_output(output: &NoticeJobOutput, budget: &mut usize, cx: &App) -> Option<Div> {
    let t = palette(cx);
    let mut take = |text: &Option<String>| {
        let text = text.as_deref()?;
        if *budget == 0 || text.is_empty() {
            return None;
        }
        if *budget == usize::MAX {
            return Some(text.to_owned());
        }
        let lines: Vec<_> = text.lines().take(*budget).collect();
        *budget -= lines.len();
        Some(lines.join("\n"))
    };
    let (note, stdout, stderr) = (
        take(&output.note),
        take(&output.stdout),
        take(&output.stderr),
    );
    (note.is_some() || stdout.is_some() || stderr.is_some()).then(|| {
        div()
            .min_w_0()
            .ml(px(22.))
            .mt(px(3.))
            .mb(px(3.))
            .flex()
            .flex_col()
            .gap(px(3.))
            .rounded(px(6.))
            .bg(t.surfaces.canvas)
            .px(px(10.))
            .py(px(7.))
            .when_some(note, |output, text| {
                output.child(
                    style::SECONDARY
                        .apply(div())
                        .text_color(t.text.secondary)
                        .child(style::observed_text(text))
                        .id("notice-output-note")
                        .test_support(),
                )
            })
            .when_some(stdout, |output, text| {
                output.child(
                    style::mono()
                        .child(style::observed_text(text))
                        .id("notice-stdout")
                        .test_support(),
                )
            })
            .when_some(stderr, |output, text| {
                output.child(
                    style::mono()
                        .text_color(t.feedback.warning)
                        .child(style::observed_text(text))
                        .id("notice-stderr")
                        .test_support(),
                )
            })
    })
}
