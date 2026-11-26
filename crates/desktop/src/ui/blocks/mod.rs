//! Retained renderers for the desktop timeline. Wire data is only used for copy.
mod job_notice;
pub(super) mod web;

use super::{
    jobs, math, style,
    theme::{palette, tones},
};
use crate::{
    controller::SessionController,
    disclosure::Phase,
    timeline::{
        Attachment, BlockData, BlockKind, NoticeLevel, Step, StepPosition, ThoughtGroup,
        TimelineRow,
        command::{CommandOutput, is_redundant_input},
        tools::{self, ToolDetail, ToolPresentation},
    },
};
use gpui_kit::component::{
    IconName, Sizable,
    button::{Button, ButtonVariants},
    diff::{Diff, DiffState},
    text::TextViewState,
};
use gpui_kit::{
    assets::IconName as Lucide,
    base::{
        AccordionTrigger,
        text::{CodeBlock, TextView},
    },
    prelude::*,
    *,
};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use zevria_content::{decode_image_base64, readable::sanitize_readable, tool_result_image};
use zevria_foundation::{
    CommandJobId, CommandJobSnapshot, CommandJobStatus, FileChange, READ_IMAGE_TOOL_NAME,
};
use zevria_presentation::{
    blocks::{PresentationBlock, PresentationBlockId, image_facts_label, image_resized_label},
    conversation::HistoryEntry,
    jobs::{JobsInspector, format_job_duration, job_status_icon},
    status_icon::StatusIcon,
};

pub use tools::tool_label;

pub(super) struct DisclosureChanged {
    pub expanded: bool,
}
pub(super) struct DetailResized;

fn plain_html(text: &str) -> String {
    format!(
        "<p>{}</p>",
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\n', "<br>")
    )
}

fn image_format(mime: &str) -> Option<ImageFormat> {
    Some(match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::Webp,
        "image/gif" => ImageFormat::Gif,
        _ => return None,
    })
}

fn image_preview(image: Arc<Image>, (width, height): (u32, u32), cx: &App) -> Div {
    // Reserve the actual displayed size before decoding. Natural image sizing
    // can arrive after the virtual list has measured the row.
    let (width, height) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (480. / width).min(240. / height).min(1.);
    div()
        .w(px(width * scale))
        .h(px(height * scale))
        .max_w_full()
        .overflow_hidden()
        .rounded(px(6.))
        .border_1()
        .border_color(tones(cx).hairline)
        .child(img(image).size_full().object_fit(ObjectFit::Contain))
}

type PromptAttachment = (String, (u32, u32), Arc<Image>);

const JOB_ACTION_SIZE: f32 = 20.;
const JOB_ACTION_GAP: f32 = 2.;
const LIVE_CONTROLS_RIGHT: f32 = 10. + 14. + 8.; // Trigger padding, status icon, header gap.
const SHOW_IN_JOBS_ACTION: usize = 3;
const EDIT_PROMPT_ACTION: usize = 4;

fn live_controls_width(job: &CommandJobSnapshot) -> f32 {
    if JobsInspector::can_background(job) {
        JOB_ACTION_SIZE * 2. + JOB_ACTION_GAP
    } else {
        JOB_ACTION_SIZE
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CopyTarget {
    Primary,
    Output,
}

pub struct BlockView {
    row: TimelineRow,
    data: Arc<BlockData>,
    text: Entity<TextViewState>,
    text_source: String,
    plain: bool,
    show_all_output: bool,
    command_clipped: bool,
    show_all_sources: bool,
    diff: Option<Entity<DiffState>>,
    diff_error: Option<String>,
    diff_task: Option<Task<()>>,
    preview: Option<Arc<Image>>,
    preview_task: Option<Task<()>>,
    attachments: Vec<PromptAttachment>,
    _text: Subscription,
    block_focus: FocusHandle,
    disclosure_focus: FocusHandle,
    action_focus: [FocusHandle; 5],
    copy_feedback: style::CopyFeedback<CopyTarget>,
    job_action_focus: [FocusHandle; 2], // Stop, background.
    open_jobs_focus: FocusHandle,
    notice_job_focus: HashMap<(bool, CommandJobId), job_notice::NoticeJobFocus>,
    focus_subscriptions: Vec<Subscription>,
    controller: Entity<SessionController>,
}
impl EventEmitter<DetailResized> for BlockView {}
impl EventEmitter<DisclosureChanged> for BlockView {}

impl BlockView {
    pub fn key(&self) -> &str {
        &self.data.key
    }
    pub(super) fn disclosure_focus(&self) -> FocusHandle {
        self.disclosure_focus.clone()
    }
    pub(super) fn has_disclosure_focus(&self, window: &Window) -> bool {
        self.data.is_disclosure() && self.disclosure_focus.is_focused(window)
    }
    pub(super) fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.block_focus.contains_focused(window, cx)
            || self.disclosure_focus.is_focused(window)
            || self.open_jobs_focus.is_focused(window)
            || self
                .action_focus
                .iter()
                .chain(&self.job_action_focus)
                .any(|f| f.is_focused(window))
            || self.notice_job_focus.values().any(|f| f.is_focused(window))
    }
    #[cfg(test)]
    pub(super) fn text_id(&self) -> EntityId {
        self.text.entity_id()
    }
    #[cfg(test)]
    pub(super) fn test_prepared(&self) -> bool {
        self.preview_task.is_some()
            || self.preview.is_some()
            || self.diff_task.is_some()
            || self.diff.is_some()
    }

    pub fn new(
        row: TimelineRow,
        controller: Entity<SessionController>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (text_source, plain) = Self::text_source(&row).unwrap_or_default();
        let text = cx.new(|cx| Self::parse_text(&text_source, plain, cx));
        let watch = Self::watch_text(&text, cx);
        let mut this = Self {
            data: row.data.clone(),
            attachments: Self::attachments(&row.data),
            row,
            text,
            text_source,
            plain,
            show_all_output: false,
            command_clipped: false,
            show_all_sources: false,
            diff: None,
            diff_error: None,
            diff_task: None,
            preview: None,
            preview_task: None,
            _text: watch,
            block_focus: cx.focus_handle(),
            disclosure_focus: cx.focus_handle(),
            action_focus: std::array::from_fn(|_| cx.focus_handle()),
            copy_feedback: style::CopyFeedback::default(),
            job_action_focus: std::array::from_fn(|_| cx.focus_handle()),
            open_jobs_focus: cx.focus_handle(),
            notice_job_focus: HashMap::new(),
            focus_subscriptions: vec![],
            controller,
        };
        if matches!(&this.data.kind, BlockKind::Step(Step::Tool(tool)) if matches!(tool.detail, ToolDetail::Image(_)))
            || !this.attachments.is_empty()
        {
            cx.on_release(|this, cx| {
                this.clear_preview(cx);
                for (_, _, image) in this.attachments.drain(..) {
                    image.remove_asset(cx);
                }
            })
            .detach();
        }
        this.prepare_details(cx);
        this
    }

    fn attachments(data: &BlockData) -> Vec<PromptAttachment> {
        let attachments: &[Attachment] = match &data.kind {
            BlockKind::Prompt { images, .. } => images,
            BlockKind::Image(image) => std::slice::from_ref(image),
            _ => &[],
        };
        attachments
            .iter()
            .filter_map(|image| {
                let format = image_format(image.image.mime_type())?;
                Some((
                    image.label.trim_matches(['[', ']']).to_owned(),
                    image.image.dimensions(),
                    Arc::new(Image::from_bytes(
                        format,
                        image.image.encoded_bytes().to_vec(),
                    )),
                ))
            })
            .collect()
    }

    fn watch_text(text: &Entity<TextViewState>, cx: &mut Context<Self>) -> Subscription {
        cx.observe(text, |_, _, cx| {
            cx.emit(DetailResized);
            cx.notify();
        })
    }
    fn parse_text(body: &str, plain: bool, cx: &mut Context<TextViewState>) -> TextViewState {
        if plain {
            TextViewState::html(&plain_html(body), cx)
        } else {
            TextViewState::markdown(body, cx)
        }
    }
    fn text_source(row: &TimelineRow) -> Option<(String, bool)> {
        match &row.data.kind {
            BlockKind::Prose { plain } => Some((row.data.body.to_string(), *plain)),
            BlockKind::Document { .. } => Some((row.data.body.to_string(), false)),
            BlockKind::ThoughtGroup(thought) if row.expanded => Some((thought.text(), false)),
            BlockKind::Step(Step::Tool(tool))
                if row.expanded
                    && matches!(
                        tool.detail,
                        ToolDetail::Standard { markdown: true, .. }
                            | ToolDetail::WebSearch {
                                completed: Some(_),
                                ..
                            }
                    ) =>
            {
                Some((tool.body.clone(), false))
            }
            _ => None,
        }
    }

    pub fn sync(&mut self, row: TimelineRow, window: &mut Window, cx: &mut Context<Self>) {
        let changed = row.fingerprint != self.row.fingerprint;
        let content_changed = row.data.revision != self.data.revision;
        let expanded_changed = row.expanded != self.row.expanded;
        if content_changed {
            let before = self.tool().map(|t| &t.changes);
            let after = match &row.data.kind {
                BlockKind::Step(Step::Tool(t)) => Some(&t.changes),
                _ => None,
            };
            if before != after {
                self.diff = None;
                self.diff_error = None;
                self.diff_task = None;
            }
            if matches!(
                row.data.kind,
                BlockKind::Prompt { .. } | BlockKind::Image(_)
            ) {
                for (_, _, image) in self.attachments.drain(..) {
                    image.remove_asset(cx);
                }
                self.attachments = Self::attachments(&row.data);
            }
        }
        if !row.expanded || content_changed {
            self.clear_preview(cx);
        }
        let collapsed_with_panel_focus = !row.expanded
            && self.row.expanded
            && self.block_focus.contains_focused(window, cx)
            && !self.disclosure_focus.is_focused(window)
            && !self
                .action_focus
                .iter()
                .chain(&self.job_action_focus)
                .any(|f| f.is_focused(window));
        self.data = row.data.clone();
        self.row = row;
        self.sync_job_focus(window, cx);
        if collapsed_with_panel_focus {
            window.focus(&self.disclosure_focus, cx);
        }
        if !changed {
            return;
        }
        if (content_changed || expanded_changed)
            && let Some((text, plain)) = Self::text_source(&self.row)
        {
            if plain != self.plain {
                self.text = cx.new(|cx| Self::parse_text(&text, plain, cx));
                self._text = Self::watch_text(&self.text, cx);
                self.plain = plain;
            } else if self.text_source != text {
                let source = if plain {
                    Cow::Owned(plain_html(&text))
                } else {
                    Cow::Borrowed(text.as_str())
                };
                self.text.update(cx, |view, cx| view.set_text(&source, cx));
            }
            self.text_source = text;
        }
        self.prepare_details(cx);
        cx.notify();
    }

    fn tool(&self) -> Option<&ToolPresentation> {
        match &self.data.kind {
            BlockKind::Step(Step::Tool(tool)) => Some(tool),
            _ => None,
        }
    }
    fn jobs_inspector<'a>(&self, cx: &'a App) -> Option<&'a JobsInspector> {
        let model = &self.controller.read(cx).model;
        model
            .controls
            .selected_child
            .is_none()
            .then(|| model.root.jobs_inspector())
    }

    fn tool_runtime_job<'a>(&self, cx: &'a App) -> Option<&'a CommandJobSnapshot> {
        self.jobs_inspector(cx)?.job(&self.tool()?.job.as_ref()?.id)
    }

    fn running_job<'a>(&self, cx: &'a App) -> Option<&'a CommandJobSnapshot> {
        self.tool_runtime_job(cx)
            .filter(|job| JobsInspector::can_terminate(job))
    }

    fn can_inspect_jobs(&self, cx: &App) -> bool {
        self.controller.read(cx).can_inspect_jobs()
    }

    fn inspectable_job<'a>(&self, cx: &'a App) -> Option<&'a CommandJobId> {
        if !self.can_inspect_jobs(cx) {
            return None;
        }
        match &self.data.kind {
            BlockKind::Jobs { finished, .. } if finished.len() == 1 => self
                .notice_inspector(cx)?
                .job(&finished[0].id)
                .map(|job| &job.id),
            BlockKind::Step(Step::Tool(tool)) => {
                let job = self.tool_runtime_job(cx)?;
                (tool.background || JobsInspector::can_terminate(job)).then_some(&job.id)
            }
            _ => None,
        }
    }

    fn waiting_command_job<'a>(&self, cx: &'a App) -> Option<&'a CommandJobSnapshot> {
        let tool = self.tool()?;
        if self.row.expanded
            && matches!(tool.detail, ToolDetail::Command { .. })
            && tool.status == StatusIcon::Running
            && self.data.secondary.is_none()
            && self.can_inspect_jobs(cx)
        {
            self.tool_runtime_job(cx)
        } else {
            None
        }
    }

    fn sync_job_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let job = self.running_job(cx);
        let lost_focus = (job.is_none() && self.job_action_focus[0].is_focused(window))
            || (!job.is_some_and(JobsInspector::can_background)
                && self.job_action_focus[1].is_focused(window))
            || (self.action_focus[SHOW_IN_JOBS_ACTION].is_focused(window)
                && self.inspectable_job(cx).is_none())
            || (self.open_jobs_focus.is_focused(window) && self.waiting_command_job(cx).is_none());
        if lost_focus {
            window.focus(&self.disclosure_focus, cx);
        }
        self.sync_notice_focus(window, cx);
    }

    pub(super) fn refresh_jobs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_job_focus(window, cx);
        cx.notify();
    }
    fn prepare_details(&mut self, cx: &mut Context<Self>) {
        if self.row.expanded {
            self.prepare_diff(cx);
            self.prepare_preview(cx);
        }
    }
    fn clear_preview(&mut self, cx: &mut App) {
        self.preview_task = None;
        if let Some(image) = self.preview.take() {
            image.remove_asset(cx);
        }
    }
    fn prepare_preview(&mut self, cx: &mut Context<Self>) {
        if self.preview.is_some()
            || self.preview_task.is_some()
            || !self
                .tool()
                .is_some_and(|tool| matches!(tool.detail, ToolDetail::Image(Some(_))))
        {
            return;
        }
        let Some((index, source)) = self.data.source else {
            return;
        };
        let conversation = &self.controller.read(cx).model.visible().conversation;
        let Some(HistoryEntry::Conversation(entry)) = conversation.entry(index) else {
            return;
        };
        let result = entry
            .blocks
            .iter()
            .find(|b| b.id == source)
            .and_then(PresentationBlock::native_tool)
            .and_then(|(_, state)| state.result.as_ref());
        let Some((mime, data)) = result.and_then(tool_result_image) else {
            return;
        };
        let Some(format) = image_format(mime) else {
            return;
        };
        let data = data.to_owned();
        let job = cx.background_executor().spawn(async move {
            decode_image_base64(&data).map(|bytes| Arc::new(Image::from_bytes(format, bytes)))
        });
        self.preview_task = Some(cx.spawn(async move |owner, cx| {
            let preview = job.await;
            let _ = owner.update(cx, |this, cx| {
                this.preview = preview;
                cx.emit(DetailResized);
                cx.notify();
            });
        }));
    }
    fn prepare_diff(&mut self, cx: &mut Context<Self>) {
        if self.diff.is_some() || self.diff_task.is_some() {
            return;
        }
        let Some(tool) = self.tool() else { return };
        if !tool
            .changes
            .iter()
            .any(|f| !matches!(f.change, FileChange::Omitted { .. }))
        {
            return;
        }
        let changes = tool.changes.clone();
        let job = cx
            .background_executor()
            .spawn(async move { tools::parse(&changes) });
        self.diff_task = Some(cx.spawn(async move |owner, cx| {
            let result = job.await;
            let _ = owner.update(cx, |this, cx| {
                match result {
                    Ok(files) => this.diff = Some(cx.new(|cx| DiffState::new(files, cx))),
                    Err(error) => this.diff_error = Some(error),
                }
                cx.emit(DetailResized);
                cx.notify();
            });
        }));
    }

    fn disclosure(
        &self,
        label: String,
        contents: Div,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AccordionTrigger {
        let owner = cx.entity();
        let focus = self.disclosure_focus.clone();
        let open = self.row.expanded;
        let focused = focus.is_focused(window);
        AccordionTrigger::new("block-disclosure")
            .open(open)
            .key_context("ZevriaDisclosure")
            .track_focus(&focus)
            .tab_stop(true)
            .aria_label(label)
            .relative()
            .flex_1()
            .min_w_0()
            .min_h(px(32.))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(tones(cx).hover))
            .active(|s| s.bg(tones(cx).pressed))
            .on_change(move |expanded, _, window, cx| {
                window.focus(&focus, cx);
                owner.update(cx, |_, cx| cx.emit(DisclosureChanged { expanded }));
            })
            .on_key_down(cx.listener(move |_, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.emit(DisclosureChanged { expanded: !open });
                    cx.stop_propagation();
                }
            }))
            .child(div().flex_shrink_0().flex().child(style::step_icon(
                if open {
                    Lucide::ChevronDown
                } else {
                    Lucide::ChevronRight
                },
                palette(cx).text.muted,
            )))
            .child(contents)
            .when(focused, |trigger| {
                trigger.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(6.))
                        .border_2()
                        .border_color(tones(cx).focus),
                )
            })
    }

    fn has_copy_feedback(&self) -> bool {
        self.copy_feedback.is_active(CopyTarget::Primary)
            || (self.copy_feedback.is_active(CopyTarget::Output) && self.data.secondary.is_some())
    }

    fn copy_primary(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.data.primary_copy()));
        self.copy_feedback
            .trigger(CopyTarget::Primary, cx, |this| &mut this.copy_feedback);
    }

    fn copy_output(&mut self, cx: &mut Context<Self>) {
        if let Some(output) = &self.data.secondary {
            cx.write_to_clipboard(ClipboardItem::new_string(output.to_string()));
            self.copy_feedback
                .trigger(CopyTarget::Output, cx, |this| &mut this.copy_feedback);
        }
    }

    fn editable_prompt_source(&self, cx: &App) -> Option<(usize, PresentationBlockId)> {
        self.data.source.filter(|&source| {
            matches!(
                self.data.kind,
                BlockKind::Prompt {
                    preparing: false,
                    ..
                }
            ) && self.controller.read(cx).is_prompt_editable(source)
        })
    }

    fn is_active_edit_target(&self, cx: &App) -> bool {
        matches!(self.data.kind, BlockKind::Prompt { .. })
            && self.data.source.is_some_and(|(index, _)| {
                self.controller.read(cx).active_edit_history_index() == Some(index)
            })
    }

    pub(super) fn refresh_edit_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.action_focus[EDIT_PROMPT_ACTION].is_focused(window)
            && (self.is_active_edit_target(cx) || self.editable_prompt_source(cx).is_none())
        {
            window.focus(&self.action_focus[0], cx);
        }
        cx.notify();
    }

    fn actions(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let (idle_label, copied_label) = match &self.data.kind {
            BlockKind::Step(Step::Tool(_)) => ("Copy input", "Copied input"),
            BlockKind::Step(Step::Web(_)) => ("Copy web activity", "Copied web activity"),
            BlockKind::Jobs { .. } => ("Copy notice", "Copied notice"),
            _ => ("Copy text", "Copied text"),
        };
        let focused = self.action_focus.iter().any(|f| f.is_focused(window));
        let mut actions = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap(px(2.))
            .opacity(if focused || self.has_copy_feedback() {
                1.
            } else {
                0.
            })
            .group_hover("timeline-block", |s| s.opacity(1.))
            .child(
                style::copy_button(
                    "copy",
                    self.copy_feedback.is_active(CopyTarget::Primary),
                    idle_label,
                    copied_label,
                    cx,
                )
                .track_focus(&self.action_focus[0])
                .on_click(cx.listener(|this, _, _, cx| this.copy_primary(cx))),
            );
        if !self.is_active_edit_target(cx)
            && let Some(source) = self.editable_prompt_source(cx)
        {
            let controller = self.controller.clone();
            actions = actions.child(
                style::icon_button("edit-prompt", Lucide::Pencil, "Edit and resend")
                    .track_focus(&self.action_focus[EDIT_PROMPT_ACTION])
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |controller, cx| {
                            controller.recall_prompt(source, cx);
                        });
                    }),
            );
        }
        if self.data.secondary.is_some() {
            actions = actions.child(
                style::copy_button(
                    "copy-output",
                    self.copy_feedback.is_active(CopyTarget::Output),
                    "Copy output",
                    "Copied output",
                    cx,
                )
                .track_focus(&self.action_focus[1])
                .on_click(cx.listener(|this, _, _, cx| this.copy_output(cx))),
            );
        }
        if let BlockKind::Step(Step::Subtask(task)) = &self.data.kind {
            let id = task.id.clone();
            let controller = self.controller.clone();
            actions = actions.child(
                style::icon_button("inspect", IconName::Eye, "Inspect subtask")
                    .track_focus(&self.action_focus[2])
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |c, cx| c.select_pane(Some(id.clone()), cx))
                    }),
            );
        }
        if let Some(id) = self.inspectable_job(cx).cloned() {
            let controller = self.controller.clone();
            actions = actions.child(
                style::icon_button("show-in-jobs", IconName::ExternalLink, "Show in Jobs")
                    .track_focus(&self.action_focus[SHOW_IN_JOBS_ACTION])
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |c, cx| c.inspect_job(id.clone(), cx))
                    }),
            );
        }
        actions
    }

    fn disclosure_overlay(right: f32) -> Div {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(right))
            .flex()
            .items_center()
            // Let the overlay buttons handle the click without also pressing
            // or toggling the disclosure trigger underneath them.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }

    fn floating_actions(
        &self,
        job_controls_width: Option<f32>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let right = if matches!(&self.data.kind, BlockKind::Jobs { .. }) {
            LIVE_CONTROLS_RIGHT
        } else {
            job_controls_width.map_or(6., |width| LIVE_CONTROLS_RIGHT + width + 6.)
        };
        Self::disclosure_overlay(right)
            .id("floating-actions")
            .test_support()
            .child(
                self.actions(window, cx)
                    .px(px(3.))
                    .py(px(1.))
                    .rounded(px(6.))
                    .bg(palette(cx).surfaces.overlay)
                    .border_1()
                    .border_color(tones(cx).hairline),
            )
    }

    fn live_job_controls(&self, job: &CommandJobSnapshot) -> Div {
        Self::disclosure_overlay(LIVE_CONTROLS_RIGHT)
            .gap(px(JOB_ACTION_GAP))
            .when(JobsInspector::can_background(job), |controls| {
                controls.child(
                    jobs::background_button(job.id.clone(), self.controller.clone())
                        .size(px(JOB_ACTION_SIZE))
                        .track_focus(&self.job_action_focus[1]),
                )
            })
            .child(
                jobs::stop_button(job.id.clone(), self.controller.clone())
                    .size(px(JOB_ACTION_SIZE))
                    .track_focus(&self.job_action_focus[0]),
            )
    }

    fn disclosure_header(
        &self,
        label: String,
        contents: Div,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let controls = self.running_job(cx).map(|job| self.live_job_controls(job));
        let width = self.running_job(cx).map(live_controls_width);
        div()
            .relative()
            .flex()
            .items_center()
            .min_w_0()
            .child(self.disclosure(label, contents, window, cx))
            .child(self.floating_actions(width, window, cx))
            .children(controls)
    }

    fn rich_text(&self, document: bool, cx: &App) -> TextView {
        TextView::new(&self.text)
            .markdown_extensions(math::extensions())
            .style(style::markdown(cx, document))
            .code_block_actions(code_block_header)
            .scrollable(false)
            .font_family(if document {
                style::DOCUMENT_FONT
            } else {
                style::UI_FONT
            })
            .map(|view| {
                if document {
                    style::DOCUMENT.apply(view)
                } else {
                    style::READING.apply(view)
                }
            })
    }

    fn thought(
        &self,
        thought: &ThoughtGroup,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = palette(cx);
        let pencil = tones(cx).pencil;
        let count = thought.segments.len();
        let contents = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                style::label()
                    .flex_shrink_0()
                    .text_color(t.text.secondary)
                    .child(if thought.is_live {
                        "Thinking"
                    } else {
                        "Thought"
                    }),
            )
            .when(count > 1, |row| {
                row.child(style::tag(count.to_string(), t.text.muted, cx))
            })
            .child(style::pencil(cx).flex_1().min_w_0().truncate().when(
                !self.row.expanded,
                |preview| {
                    preview.child(
                        thought
                            .segments
                            .last()
                            .map(|s| s.headline.to_string())
                            .unwrap_or_default(),
                    )
                },
            ))
            .when_some(phase_label(self.row.phase), |row, phase| {
                row.child(style::step_badge(phase, t.feedback.warning))
            })
            .child(
                div()
                    .id("thought-tail")
                    .test_support()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .gap(px(8.))
                    .child(style::step_badge(
                        format!("{}w", thought.word_count),
                        t.text.muted,
                    ))
                    .when(thought.is_live, |tail| {
                        tail.child(style::step_status(StatusIcon::Running, pencil, cx))
                    }),
            );
        div()
            .id("thought")
            .test_support()
            .group("timeline-block")
            .flex()
            .flex_col()
            .child(div().px(px(1.)).child(self.disclosure_header(
                "Thought details".into(),
                contents,
                window,
                cx,
            )))
            .when(self.row.expanded, |panel| {
                panel.child(
                    div().pl(px(32.)).pr(px(12.)).py(px(5.)).child(
                        self.rich_text(false, cx)
                            .style(style::thought_markdown(cx))
                            .font_family(style::DOCUMENT_FONT)
                            .map(|view| style::PENCIL.apply(view))
                            .text_color(pencil),
                    ),
                )
            })
    }

    fn step(&self, step: &Step, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
        let tool_status = step.status();
        let background_status = self.background_job_status(cx);
        let status = background_status.map_or(tool_status, job_status_icon);
        let (icon, label, target) = match step {
            Step::Tool(tool) => (
                tool_icon(&tool.name),
                tool_label(&tool.name).into_owned(),
                tool.summary.clone(),
            ),
            Step::Web(web) => (
                if web
                    .single()
                    .is_some_and(|a| a.kind == zevria_presentation::blocks::WebActionKind::Search)
                {
                    Lucide::Search
                } else {
                    Lucide::Globe
                },
                web.label().into(),
                web.target(),
            ),
            Step::Subtask(task) => (
                Lucide::Bot,
                style::kind_label(task.kind).into(),
                task.title.clone(),
            ),
        };
        let mut summary = style::SECONDARY
            .apply(div())
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(t.text.primary);
        if let Step::Tool(tool) = step
            && matches!(
                tool.name.as_str(),
                "command"
                    | "command_status"
                    | "command_terminate"
                    | "edit"
                    | "write"
                    | "read"
                    | "delete"
                    | READ_IMAGE_TOOL_NAME
            )
        {
            summary = style::CODE
                .apply(summary)
                .font_family(style::MONO_FONT)
                .child(StyledText::new(target.clone()).with_highlights(
                    tool.highlights.iter().map(|(range, token)| {
                        (
                            range.clone(),
                            HighlightStyle {
                                color: Some(style::shell_ink(*token, &t)),
                                ..Default::default()
                            },
                        )
                    }),
                ));
        } else {
            summary = summary.child(target.clone());
        }
        if self.row.expanded
            && let Step::Tool(tool) = step
            && let ToolDetail::Command { input, .. } = &tool.detail
            && !input.is_empty()
            && is_redundant_input(&target, input)
        {
            // The stored header may fit on a wide pane but be clipped beside
            // an inspector. Only repeat the command if the rendered width drops it.
            let text_style = TextStyle {
                font_family: style::MONO_FONT.into(),
                color: t.text.primary,
                ..Default::default()
            };
            let width = window
                .text_system()
                .shape_line(
                    target.clone().into(),
                    px(style::CODE.size),
                    &[text_style.to_run(target.len())],
                    None,
                )
                .width;
            let owner = cx.weak_entity();
            let was_clipped = self.command_clipped;
            summary = summary.relative().child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), _, cx| {
                        let clipped = width > bounds.size.width + px(0.5);
                        if clipped != was_clipped {
                            cx.defer(move |cx| {
                                let _ = owner.update(cx, |this, cx| {
                                    if this.command_clipped != clipped {
                                        this.command_clipped = clipped;
                                        cx.emit(DetailResized);
                                        cx.notify();
                                    }
                                });
                            });
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        }
        let title = if target.is_empty() {
            format!("{label} details")
        } else {
            format!("{label}: {target}")
        };
        let mut header = div()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .child(style::step_icon(icon, t.text.muted)),
            )
            .child(
                style::label()
                    .max_w(px(140.))
                    .truncate()
                    .text_color(t.text.secondary)
                    .child(label),
            )
            .child(summary);
        if let Step::Tool(tool) = step {
            header = header.children(self.tool_meta(tool, cx));
        }
        let status_label = match tool_status {
            StatusIcon::Denied => Some("Denied"),
            StatusIcon::Interrupted => Some("Interrupted"),
            StatusIcon::Failed if !matches!(step, Step::Tool(t) if matches!(&t.detail, ToolDetail::Command { exit: Some(_), .. })) => {
                Some("Failed")
            }
            _ => None,
        };
        header = header
            .when_some(status_label, |header, label| {
                header.child(style::step_badge(
                    label,
                    style::glyph_color(status, t.text.muted, cx),
                ))
            })
            .when_some(self.running_job(cx), |header, job| {
                // Reserve space for the sibling overlay without moving the status icon.
                header.child(div().flex_shrink_0().w(px(live_controls_width(job))))
            })
            .child(
                div()
                    .id("step-status")
                    .test_support()
                    .flex()
                    .flex_shrink_0()
                    .child(style::step_status(status, t.text.muted, cx)),
            );
        let pos = self.row.position.unwrap_or(StepPosition::Only);
        div()
            .id("timeline-step")
            .test_support()
            .group("timeline-block")
            .flex()
            .flex_col()
            .min_w_0()
            .border_x_1()
            .border_b_1()
            .border_color(tones(cx).hairline)
            .when(pos.starts(), |card| card.border_t_1().rounded_t(px(10.)))
            .when(pos.ends(), |card| card.rounded_b(px(10.)))
            .bg(t.surfaces.panel)
            .child(self.disclosure_header(title, header, window, cx))
            .when(self.row.expanded, |card| {
                let details = match step {
                    Step::Tool(tool) => self.tool_details(tool, cx),
                    Step::Web(web) => web::details(web, cx).into_any_element(),
                    Step::Subtask(task) => div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(style::body().child(task.title.clone()))
                        .child(
                            style::label()
                                .text_color(t.text.muted)
                                .child(task.status.to_string()),
                        )
                        .when_some(task.workspace.clone(), |row, path| {
                            row.child(style::mono().child(path))
                        })
                        .into_any_element(),
                };
                card.child(
                    div()
                        .min_w_0()
                        .pl(px(32.))
                        .pr(px(12.))
                        .pt(px(4.))
                        .pb(px(10.))
                        .child(details),
                )
            })
    }

    fn background_job_status<'a>(&'a self, cx: &'a App) -> Option<&'a CommandJobStatus> {
        let tool = self.tool().filter(|tool| tool.background)?;
        self.tool_runtime_job(cx)
            .map(|job| &job.status)
            .or_else(|| {
                // Keep a settled tool outcome after eviction, but never treat an
                // attached snapshot as proof that a process is still running.
                tool.job
                    .as_ref()
                    .filter(|job| !job.running)
                    .map(|job| &job.status)
            })
    }

    fn tool_meta(&self, tool: &ToolPresentation, cx: &App) -> Vec<Div> {
        let t = palette(cx);
        let mut badges = vec![];
        if let ToolDetail::Image(Some(summary)) = &tool.detail {
            badges.push(style::step_badge(image_facts_label(summary), t.text.muted));
        }
        if let ToolDetail::WebSearch {
            running_label,
            completed,
            ..
        } = &tool.detail
        {
            let label = running_label.clone().or_else(|| {
                completed
                    .as_ref()
                    .map(|detail| zevria_presentation::blocks::web_search_counts(detail))
            });
            if let Some(label) = label.filter(|label| !label.is_empty()) {
                badges.push(
                    style::step_badge(label, t.text.muted)
                        .min_w_0()
                        .max_w(px(240.))
                        .truncate(),
                );
            }
        }
        if tool.added > 0 {
            badges.push(style::step_badge(
                format!("+{}", tool.added),
                t.feedback.success,
            ));
        }
        if tool.removed > 0 {
            badges.push(style::step_badge(
                format!("−{}", tool.removed),
                t.feedback.error,
            ));
        }
        if tool.background {
            if let Some(job) = self.running_job(cx) {
                let elapsed = format_job_duration(job.started_at.elapsed().unwrap_or_default());
                let ink = super::status::accent(&self.controller.read(cx).model.root, cx);
                badges.push(style::step_badge(format!("Running · {elapsed}"), ink));
                return badges;
            }
            badges.push(style::step_badge("Background", t.text.muted));
            if let Some(status) = self.background_job_status(cx)
                && let Some((label, ink)) = settled_job_badge(status, cx)
            {
                let times = self
                    .tool_runtime_job(cx)
                    .map(|job| (job.started_at, job.finished_at))
                    .or_else(|| {
                        tool.job
                            .as_ref()
                            .map(|job| (job.started_at, job.finished_at))
                    });
                let elapsed = times.and_then(|(start, end)| {
                    end.map(|end| {
                        format_job_duration(end.duration_since(start).unwrap_or_default())
                    })
                });
                let label = elapsed.map_or(label.clone(), |elapsed| format!("{label} · {elapsed}"));
                badges.push(style::step_badge(label, ink));
                return badges;
            }
        }
        if let ToolDetail::Command {
            duration,
            exit,
            lines,
            ..
        } = &tool.detail
        {
            if let Some(exit) = exit {
                badges.push(style::step_badge(exit.clone(), t.feedback.error));
            }
            let elapsed = self
                .running_job(cx)
                .filter(|_| tool.status == StatusIcon::Running)
                .map(|job| format_job_duration(job.started_at.elapsed().unwrap_or_default()));
            let metadata = lines
                .map(|n| format!("{n} {}", if n == 1 { "line" } else { "lines" }))
                .into_iter()
                .chain(duration.clone().or(elapsed))
                .collect::<Vec<_>>()
                .join(" · ");
            if !metadata.is_empty() {
                badges.push(style::step_badge(metadata, t.text.muted));
            }
        }
        badges
    }

    fn tool_details(&self, tool: &ToolPresentation, cx: &mut Context<Self>) -> AnyElement {
        let t = palette(cx);
        let mut content = div().flex().flex_col().min_w_0().gap(px(6.));
        match &tool.detail {
            ToolDetail::Command { input, .. } => {
                content = content.child(self.command_details(tool, input, cx));
            }
            ToolDetail::File => {}
            ToolDetail::Image(summary) => {
                if let Some(summary) = summary {
                    content = content
                        .children(self.preview.as_ref().map(|image| {
                            div()
                                .id("image-read-preview")
                                .test_support()
                                .child(image_preview(image.clone(), summary.dimensions, cx))
                        }))
                        .when_some(image_resized_label(summary), |panel, label| {
                            panel.child(style::label().text_color(t.text.secondary).child(label))
                        });
                }
            }
            ToolDetail::List {
                introduction,
                items,
            } => {
                content =
                    content
                        .when_some(introduction.clone(), |panel, label| {
                            panel.child(style::body().child(label))
                        })
                        .children(items.iter().map(|item| {
                            div()
                                .flex()
                                .items_start()
                                .gap(px(8.))
                                .child(div().pt(px(3.)).flex_shrink_0().child(
                                    item.status.map_or_else(
                                        || style::step_icon(Lucide::Scale, t.text.muted),
                                        |s| style::step_status(s, t.text.muted, cx),
                                    ),
                                ))
                                .child(
                                    style::SECONDARY
                                        .apply(div())
                                        .text_color(if item.muted {
                                            t.text.muted
                                        } else {
                                            t.text.primary
                                        })
                                        .child(item.text.clone()),
                                )
                        }));
            }
            ToolDetail::Questions(questions) => {
                content = content.children(questions.iter().map(|question| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(style::label().child(question.question.clone()))
                        .when(!question.options.is_empty(), |r| {
                            r.child(
                                style::SECONDARY
                                    .apply(div())
                                    .text_color(t.text.muted)
                                    .child(question.options.join(", ")),
                            )
                        })
                        .when_some(question.answer.clone(), |r, answer| {
                            r.child(style::body().child(format!("Answer: {answer}")))
                        })
                        .when_some(question.note.clone(), |r, note| {
                            r.child(
                                style::SECONDARY
                                    .apply(div())
                                    .text_color(t.text.secondary)
                                    .child(format!("Note: {note}")),
                            )
                        })
                }));
            }
            ToolDetail::WebSearch {
                activity,
                running_label,
                completed,
            } => {
                if !activity.is_empty() {
                    content = content.child(web::action_details(activity, cx));
                } else if let Some(label) = running_label {
                    content = content.child(style::step_badge(format!("{label}…"), t.text.muted));
                }
                if let Some(detail) = completed {
                    if !tool.body.is_empty() {
                        content = content.child(self.rich_text(true, cx));
                    }
                    let sources: Vec<_> = detail
                        .sources
                        .iter()
                        .map(|source| zevria_content::CitedSource {
                            url: source.url.clone(),
                            title: (!source.title.is_empty()).then(|| source.title.clone()),
                        })
                        .collect();
                    content =
                        content.children(web::source_chips(&sources, self.show_all_sources, cx));
                    let mut facts = vec![detail.depth.as_str().to_owned()];
                    if let Some(total) = detail.total_tokens {
                        facts.push(format!("{} tok", crate::ui::status::compact_number(total)));
                    }
                    facts.push(format!("{}/{}", detail.provider, detail.model));
                    content = content.child(
                        style::meta()
                            .text_color(t.text.muted)
                            .child(facts.join(" · ")),
                    );
                }
            }
            ToolDetail::Standard {
                markdown,
                show_output,
            } => {
                if *markdown && !tool.body.is_empty() {
                    content = content.child(self.rich_text(true, cx));
                }
                if *show_output
                    && let Some(text) = self
                        .data
                        .secondary
                        .as_deref()
                        .filter(|text| !text.is_empty())
                {
                    content = content.child(
                        style::SECONDARY
                            .apply(div())
                            .text_color(t.text.secondary)
                            .child(tools::bounded(&tools::plain_diagnostic(text))),
                    );
                }
            }
        }
        if !tool.body.is_empty()
            && !matches!(
                tool.detail,
                ToolDetail::Standard { markdown: true, .. }
                    | ToolDetail::WebSearch {
                        completed: Some(_),
                        ..
                    }
            )
        {
            content = content.child(
                style::SECONDARY
                    .apply(div())
                    .text_color(if tool.status == StatusIcon::Failed {
                        t.feedback.error
                    } else {
                        t.text.secondary
                    })
                    .child(tool.body.clone()),
            );
        }
        if let Some(diagnostic) = &tool.diagnostic
            && !matches!(tool.detail, ToolDetail::Command { .. })
            && !self.data.secondary.as_deref().is_some_and(|output| {
                matches!(
                    tool.detail,
                    ToolDetail::Standard {
                        show_output: true,
                        ..
                    }
                ) && tools::plain_diagnostic(output) == *diagnostic
            })
        {
            content = content.child(
                style::SECONDARY
                    .apply(div())
                    .text_color(t.feedback.warning)
                    .child(diagnostic.clone()),
            );
        }
        for change in tool.changes.iter() {
            if let FileChange::Omitted { reason, .. } = &change.change {
                content = content.child(
                    style::SECONDARY
                        .apply(div())
                        .text_color(t.text.muted)
                        .child(reason.clone()),
                );
            }
        }
        if let Some(diff) = &self.diff {
            content = content.child(
                div().id("native-diff").test_support().child(
                    Diff::new(diff)
                        .h(px(tools::diff_height(&tool.changes)))
                        .w_full(),
                ),
            );
        } else if let Some(error) = &self.diff_error {
            content = content.child(
                style::SECONDARY
                    .apply(div())
                    .text_color(t.feedback.warning)
                    .child(error.clone()),
            );
        } else if self.diff_task.is_some() {
            content = content.child(style::step_badge("Preparing diff…", t.text.muted));
        }
        if matches!(&tool.detail, ToolDetail::File | ToolDetail::Image(None))
            && tool.body.is_empty()
            && tool.diagnostic.is_none()
            && tool.changes.is_empty()
        {
            content = content.child(style::step_badge(
                if tool.status == StatusIcon::Running {
                    "In progress…"
                } else {
                    "No details"
                },
                t.text.muted,
            ));
        }
        content.into_any_element()
    }

    fn command_details(&self, tool: &ToolPresentation, input: &str, cx: &mut Context<Self>) -> Div {
        let t = palette(cx);
        let mut detail = div().min_w_0().flex().flex_col().gap(px(6.));
        let background_id = tool.background.then_some(tool.job_id.as_ref()).flatten();
        if let Some(id) = background_id {
            detail = detail.child(style::step_badge(id.to_string(), t.text.muted));
        }
        if !input.is_empty() && (self.command_clipped || !is_redundant_input(&tool.summary, input))
        {
            detail = detail.child(
                style::mono()
                    .id("command-input")
                    .test_support()
                    .text_color(t.text.secondary)
                    .child(format!("$ {}", tools::bounded(&sanitize_readable(input)))),
            );
        }
        if let Some(output) = self.data.secondary.as_deref() {
            let output = CommandOutput::parse(output);
            let lines = output.output_line_count().unwrap_or(0);
            let mut budget = if self.show_all_output { usize::MAX } else { 16 };
            let mut terminal = |text: &str| {
                let visible = if budget == usize::MAX {
                    Cow::Borrowed(text)
                } else {
                    Cow::Owned(text.lines().take(budget).collect::<Vec<_>>().join("\n"))
                };
                budget = budget.saturating_sub(visible.lines().count());
                tools::bounded(&sanitize_readable(&visible))
            };
            let (stdout, stderr) = match &output {
                CommandOutput::Raw(text) if tool.status == StatusIcon::Failed => {
                    (String::new(), terminal(text))
                }
                CommandOutput::Raw(text) => (terminal(text), String::new()),
                CommandOutput::Envelope(result) => (
                    result.stdout.map(&mut terminal).unwrap_or_default(),
                    result.stderr.map(&mut terminal).unwrap_or_default(),
                ),
            };
            if !stdout.is_empty() || !stderr.is_empty() {
                detail = detail.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .bg(t.surfaces.canvas)
                        .rounded(px(6.))
                        .px(px(10.))
                        .py(px(7.))
                        .when(!stdout.is_empty(), |panel| {
                            panel.child(
                                style::mono()
                                    .id("command-stdout")
                                    .test_support()
                                    .child(stdout),
                            )
                        })
                        .when(!stderr.is_empty(), |panel| {
                            panel.child(
                                style::mono()
                                    .id("command-stderr")
                                    .test_support()
                                    .text_color(t.feedback.warning)
                                    .child(stderr),
                            )
                        }),
                );
            }
            if let CommandOutput::Envelope(result) = &output {
                if let Some(error) = result.error {
                    detail = detail.child(
                        style::SECONDARY
                            .apply(div())
                            .text_color(t.feedback.error)
                            .child(sanitize_readable(error)),
                    );
                }
                let status = result.status.filter(|s| *s != "error");
                let outcome = result
                    .exit_code
                    .map(|code| format!("exit {code}"))
                    .or_else(|| status.map(str::to_owned))
                    .or_else(|| output.exit_badge());
                if lines == 0 && result.error.is_none() {
                    let metadata = std::iter::once("No output".to_owned())
                        .chain(outcome)
                        .chain(result.duration.map(str::to_owned))
                        .collect::<Vec<_>>()
                        .join(" · ");
                    detail = detail.child(
                        style::step_badge(metadata, t.text.muted)
                            .id("command-empty")
                            .test_support(),
                    );
                }
                if let Some(note) = result.note {
                    detail = detail.child(
                        style::SECONDARY
                            .apply(div())
                            .text_color(t.text.secondary)
                            .child(sanitize_readable(note)),
                    );
                }
                if let Some(id) = result.id.filter(|id| {
                    *id != tool.summary && background_id.is_none_or(|job| job.as_str() != *id)
                }) {
                    detail = detail.child(style::step_badge(id.to_owned(), t.text.muted));
                }
            }
            if lines > 16 {
                detail = detail.child(self.output_toggle(lines, cx));
            }
        } else if tool.status == StatusIcon::Running {
            detail = detail.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(style::step_badge("Waiting for output…", t.text.muted))
                    .when_some(self.waiting_command_job(cx), |waiting, job| {
                        let id = job.id.clone();
                        let controller = self.controller.clone();
                        waiting.child(
                            Button::new("command-open-jobs")
                                .ghost()
                                .xsmall()
                                .label("Show in Jobs")
                                .track_focus(&self.open_jobs_focus)
                                .on_click(move |_, _, cx| {
                                    controller.update(cx, |c, cx| c.inspect_job(id.clone(), cx));
                                }),
                        )
                    }),
            );
        } else if let Some(diagnostic) = &tool.diagnostic {
            detail = detail.child(
                style::SECONDARY
                    .apply(div())
                    .text_color(t.feedback.warning)
                    .child(diagnostic.clone()),
            );
        } else {
            detail = detail.child(style::step_badge("No output", t.text.muted));
        }
        detail
    }

    fn output_toggle(&self, lines: usize, cx: &mut Context<Self>) -> Div {
        div().child(
            Button::new("show-all-output")
                .ghost()
                .xsmall()
                .label(if self.show_all_output {
                    "Show less".into()
                } else {
                    format!("Show all {lines} lines")
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_all_output = !this.show_all_output;
                    cx.emit(DetailResized);
                    cx.notify();
                })),
        )
    }

    fn prompt(&self, preparing: bool, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
        let tone = tones(cx);
        let editing = self.is_active_edit_target(cx);
        div()
            .id("prompt-card")
            .test_support()
            .group("timeline-block")
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(16.))
            .py(px(12.))
            .rounded(px(12.))
            .border_1()
            .border_color(if editing { tone.focus } else { tone.hairline })
            .bg(t.surfaces.panel)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .child(
                        style::body()
                            .id("prompt-text")
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .child(self.data.body.to_string()),
                    )
                    .when(editing, |row| {
                        row.child(
                            style::tag("Editing", t.roles.you, cx)
                                .id("prompt-editing-badge")
                                .test_support()
                                .border_1()
                                .border_color(tone.hairline)
                                .flex_shrink_0(),
                        )
                    })
                    .child(self.actions(window, cx)),
            )
            .children(self.attachments.iter().map(|(label, dimensions, image)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(style::step_badge(label.clone(), t.text.muted))
                    .child(image_preview(image.clone(), *dimensions, cx))
            }))
            .when(preparing, |p| {
                p.child(
                    style::step_badge("Preparing…", t.text.muted)
                        .id("prompt-preparing")
                        .test_support(),
                )
            })
    }
}

fn settled_job_badge(status: &CommandJobStatus, cx: &App) -> Option<(String, Hsla)> {
    use CommandJobStatus::*;
    let t = palette(cx);
    Some(match status {
        Running => return None,
        Exited {
            success: true,
            code,
            ..
        } => (
            code.map_or_else(|| "exited".into(), |code| format!("exited {code}")),
            t.text.muted,
        ),
        Exited {
            signal: Some(signal),
            ..
        } => (format!("signal {signal}"), t.feedback.error),
        Exited {
            code: Some(code), ..
        } => (format!("exit {code}"), t.feedback.error),
        Exited { .. } | Failed { .. } => ("failed".into(), t.feedback.error),
        TimedOut { .. } => ("timed out".into(), t.feedback.error),
        Terminated { .. } => ("stopped".into(), t.feedback.warning),
    })
}

fn phase_label(phase: Phase) -> Option<&'static str> {
    match phase {
        Phase::Interrupted => Some("Interrupted"),
        Phase::Unsaved => Some("Not saved"),
        _ => None,
    }
}

fn tool_icon(name: &str) -> Lucide {
    match name {
        "command" | "command_status" | "command_terminate" => Lucide::SquareTerminal,
        "edit" => Lucide::Replace,
        "write" | "read" => Lucide::FileText,
        "delete" => Lucide::Delete,
        READ_IMAGE_TOOL_NAME => Lucide::Eye,
        "web_search" => Lucide::Search,
        "task" | "reconcile_reports" => Lucide::ListChecks,
        "question" => Lucide::Info,
        "launch_subtasks" => Lucide::Bot,
        "skill" | "skill_read" | "create_plan" | "read_plan" | "update_plan" | "submit_plan" => {
            Lucide::BookOpen
        }
        _ => Lucide::Wrench,
    }
}

pub(super) fn code_block_header(code: &CodeBlock, _: &mut Window, cx: &mut App) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            style::meta()
                .text_color(palette(cx).text.muted)
                .child(code.lang().unwrap_or_default()),
        )
        .child(div().flex_1())
        .child(style::KeyedCopyButton::new(
            "copy-code",
            code.code(),
            "Copy code",
            "Copied code",
        ))
}

impl Render for BlockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_subscriptions.is_empty() {
            for handle in self
                .action_focus
                .iter()
                .chain(&self.job_action_focus)
                .chain([&self.disclosure_focus, &self.open_jobs_focus])
            {
                self.focus_subscriptions
                    .push(cx.on_focus(handle, window, |_, _, cx| cx.notify()));
                self.focus_subscriptions
                    .push(cx.on_blur(handle, window, |_, _, cx| cx.notify()));
            }
            self.sync_notice_focus(window, cx);
        }
        let t = palette(cx);
        let data = self.data.clone();
        let contents = match &data.kind {
            BlockKind::Prompt { preparing, .. } => {
                self.prompt(*preparing, window, cx).into_any_element()
            }
            BlockKind::ThoughtGroup(thought) => {
                self.thought(thought, window, cx).into_any_element()
            }
            BlockKind::Step(step) => self.step(step, window, cx).into_any_element(),
            BlockKind::Prose { .. } => div()
                .id("answer")
                .test_support()
                .group("timeline-block")
                .flex()
                .flex_col()
                .gap(px(5.))
                .child(self.rich_text(false, cx))
                .children(web::sources_footer(self, cx))
                .when_some(phase_label(self.row.phase), |answer, phase| {
                    answer.child(style::step_badge(phase, t.feedback.warning))
                })
                .relative()
                .child(
                    div().absolute().top_0().right_0().child(
                        self.actions(window, cx)
                            .bg(t.surfaces.canvas)
                            .rounded(px(4.)),
                    ),
                )
                .into_any_element(),
            BlockKind::Document {
                title,
                version,
                handoff,
            } => div()
                .bg(t.surfaces.overlay)
                .rounded(px(12.))
                .border_1()
                .border_color(tones(cx).hairline)
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .group("timeline-block")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(style::step_icon(Lucide::BookOpen, t.workflow.plan))
                        .child(
                            style::HEADINGS[1]
                                .apply(div())
                                .flex_1()
                                .min_w_0()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title.clone()),
                        )
                        .child(style::tag(
                            version.clone(),
                            if *handoff {
                                t.text.secondary
                            } else {
                                t.workflow.plan
                            },
                            cx,
                        ))
                        .child(self.actions(window, cx)),
                )
                .child(self.rich_text(true, cx))
                .into_any_element(),
            BlockKind::Jobs {
                kind,
                finished,
                running,
            } => self
                .jobs_notice(*kind, finished, running, window, cx)
                .into_any_element(),
            BlockKind::Divider => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .py(px(6.))
                .child(div().flex_1().h(px(1.)).bg(tones(cx).hairline))
                .child(style::step_badge("Context compacted", t.text.muted))
                .child(div().flex_1().h(px(1.)).bg(tones(cx).hairline))
                .into_any_element(),
            BlockKind::Notice(level) => {
                let (icon, ink) = match level {
                    NoticeLevel::Error => (Lucide::CircleX, t.feedback.error),
                    NoticeLevel::Warning => (Lucide::TriangleAlert, t.feedback.warning),
                };
                div()
                    .group("timeline-block")
                    .flex()
                    .items_start()
                    .gap(px(8.))
                    .child(div().pt(px(3.)).child(style::step_icon(icon, ink)))
                    .child(
                        style::SECONDARY
                            .apply(div())
                            .flex_1()
                            .min_w_0()
                            .text_color(t.text.secondary)
                            .child(data.body.to_string()),
                    )
                    .child(self.actions(window, cx))
                    .into_any_element()
            }
            BlockKind::Ensemble { status } => div()
                .group("timeline-block")
                .flex()
                .items_center()
                .gap(px(8.))
                .children(status.map(|status| style::step_status(status, t.text.muted, cx)))
                .child(
                    style::body()
                        .flex_1()
                        .min_w_0()
                        .child(data.body.to_string()),
                )
                .child(self.actions(window, cx))
                .into_any_element(),
            BlockKind::Image(_) => div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(self.attachments.iter().map(|(label, dimensions, image)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(style::step_badge(label.clone(), t.text.muted))
                        .child(image_preview(image.clone(), *dimensions, cx))
                }))
                .into_any_element(),
            BlockKind::Live => {
                let controller = self.controller.read(cx);
                let text = if data.body.starts_with("Reconnecting") {
                    super::status::working_activity(
                        controller.model.visible(),
                        controller.cancelling,
                    )
                } else {
                    data.body.to_string()
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(style::step_status(StatusIcon::Running, t.text.muted, cx))
                    .child(style::label().text_color(t.text.secondary).child(text))
                    .into_any_element()
            }
        };
        div()
            .id(data.key.clone())
            .test_support()
            .track_focus(&self.block_focus)
            .tab_stop(false)
            .on_action(
                cx.listener(|this, _: &gpui_kit::component::input::Copy, window, cx| {
                    if this.has_disclosure_focus(window) {
                        this.copy_primary(cx);
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .min_w_0()
            .w_full()
            .child(contents)
    }
}
