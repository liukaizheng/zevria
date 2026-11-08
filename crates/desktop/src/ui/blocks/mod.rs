//! Retained semantic log blocks. They never infer or mutate engine semantics.
mod tools;
use super::{
    style,
    theme::{palette, tones},
};
use crate::controller::SessionController;
use gpui_kit::base::text::TextView;
use gpui_kit::component::{
    Icon, IconName, Sizable,
    button::{Button, ButtonVariants},
    diff::{Diff, DiffState},
    text::TextViewState,
};
use gpui_kit::{prelude::*, *};
use std::sync::Arc;
use zevria_presentation::{blocks::*, conversation::HistoryEntry, status_icon::StatusIcon};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    You,
    Task,
    Zevria,
    System,
    Plan,
}
impl Speaker {
    fn label(self) -> &'static str {
        match self {
            Self::You => "You",
            Self::Task => "Task",
            Self::Zevria => "Zevria",
            Self::System => "System",
            Self::Plan => "Plan",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gutter {
    None,
    Label(Speaker),
    Glyph(StatusIcon),
    Live,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Error,
    Warning,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    Artifact,
    Handoff,
}
#[derive(Clone, Debug)]
pub enum LiveTail {
    Waiting {
        elapsed: String,
    },
    Compacting {
        elapsed: String,
    },
    Retrying {
        attempt: usize,
        remaining: String,
        error: String,
    },
}
#[derive(Clone, Debug, Default)]
pub struct ToolMeta {
    pub added: usize,
    pub removed: usize,
    pub background: bool,
    /// The native Diff header already repeats this file-change-only summary.
    pub diff_summary: bool,
}
#[derive(Clone, Debug)]
pub enum BlockKind {
    Prose {
        role: PresentationRole,
        plain: bool,
    },
    Reasoning,
    Tool {
        status: StatusIcon,
        name: String,
        summary: String,
        meta: ToolMeta,
    },
    Subtask {
        status: StatusIcon,
        kind: zevria_foundation::SubtaskKind,
        title: String,
    },
    Image {
        label: String,
    },
    Web,
    Notice {
        level: NoticeLevel,
    },
    Divider,
    Document {
        kind: DocumentKind,
        title: String,
        version: String,
    },
    Ensemble,
    Live(LiveTail),
}
#[derive(Clone)]
pub struct BlockData {
    pub key: String,
    pub revision: u64,
    pub kind: BlockKind,
    pub gutter: Gutter,
    pub body: String,
    pub plain: bool,
    pub collapsible: bool,
    pub primary: String,
    pub secondary: Option<String>,
    pub subtask: Option<zevria_foundation::SubtaskId>,
    pub image: Option<Arc<Image>>,
    pub changes: Vec<zevria_foundation::FileChangeOutput>,
    pub turn_boundary: bool,
}
fn bounded(text: &str) -> String {
    let mut value = text.chars().take(24_000).collect::<String>();
    if value.len() < text.len() {
        value.push_str("\nShowing the first 24,000 characters. Copy includes everything.");
    }
    value
}
impl BlockData {
    pub fn new(key: String, kind: BlockKind, gutter: Gutter, body: String, plain: bool) -> Self {
        Self {
            key,
            revision: 0,
            kind,
            gutter,
            primary: body.clone(),
            body,
            plain,
            collapsible: false,
            secondary: None,
            subtask: None,
            image: None,
            changes: Vec::new(),
            turn_boundary: false,
        }
    }
    fn block(epoch: &str, block: &PresentationBlock) -> Self {
        let role = block.role.unwrap_or(PresentationRole::System);
        let mut data = Self::new(
            format!("{epoch}-{}", block.id.0),
            BlockKind::Prose { role, plain: false },
            Gutter::None,
            block.primary_copy(),
            false,
        );
        data.revision = block.revision;
        data.secondary = block.secondary_copy();
        match &block.kind {
            PresentationBlockKind::Text { flavor, .. } => {
                data.plain = *flavor == TextFlavor::Plain;
                data.kind = BlockKind::Prose {
                    role,
                    plain: data.plain,
                };
            }
            PresentationBlockKind::Reasoning { .. } => {
                data.kind = BlockKind::Reasoning;
                data.collapsible = true;
            }
            PresentationBlockKind::Tool(tool) => {
                data.collapsible = true;
                match tool {
                    PresentedTool::Native { call, state } => {
                        let status = native_header_status(call, state);
                        let mut summary =
                            tools::tool_summary(&call.function.name, state.arguments.as_ref());
                        if let Some(list) = block.readable_list_copy() {
                            data.body = list;
                            data.plain = true;
                        } else if call.function.name.as_str() == "command" {
                            data.body = format!(
                                "```sh\n{}\n```",
                                tool_argument(state, "command")
                                    .unwrap_or_else(|| data.primary.clone())
                            );
                        } else if let Some((body, plain)) =
                            tools::details(&call.function.name, state)
                        {
                            data.body = body;
                            data.plain = plain;
                        } else {
                            data.body = format!("```json\n{}\n```", bounded(&data.primary));
                        }
                        if let Some(metadata) = &state.metadata {
                            data.changes = metadata.file_changes().to_vec();
                            if !data.changes.is_empty() {
                                data.body = tools::summary(&data.changes);
                                data.plain = true;
                            }
                            if let Some(draft) =
                                metadata.detail.as_ref().and_then(|d| d.plan_draft())
                            {
                                summary = draft.title.clone();
                                data.body = format!(
                                    "{}\nRevision {}\nDraft {}\n{} bytes\n\n{}",
                                    draft.title,
                                    draft.target_revision,
                                    draft
                                        .sequence
                                        .map_or_else(|| "submitted".into(), |s| s.to_string()),
                                    draft.bytes,
                                    data.body
                                );
                                if let Some(change) = &draft.change {
                                    data.changes.push(zevria_foundation::FileChangeOutput {
                                        path: draft
                                            .projection
                                            .clone()
                                            .unwrap_or_else(|| "Plan working copy".into()),
                                        change: change.clone(),
                                    });
                                }
                            }
                            if let Some(diagnostic) = &metadata.diagnostic {
                                data.body.push_str(&format!("\n{diagnostic}"));
                            }
                        }
                        let (added, removed) = tools::counts(&data.changes);
                        data.kind = BlockKind::Tool {
                            status,
                            name: call.function.name.to_string(),
                            summary,
                            meta: ToolMeta {
                                added,
                                removed,
                                diff_summary: state.metadata.as_ref().is_some_and(|m| {
                                    !m.file_changes().is_empty()
                                        && m.diagnostic.is_none()
                                        && m.detail.as_ref().and_then(|d| d.plan_draft()).is_none()
                                }),
                                background: state.command_job.is_some()
                                    || state
                                        .arguments
                                        .as_ref()
                                        .and_then(|a| a.get("background"))
                                        .and_then(|a| a.as_bool())
                                        == Some(true),
                            },
                        };
                        data.gutter = Gutter::Glyph(status);
                    }
                    PresentedTool::Acp(tool) => {
                        let status = tool.presented_status().icon();
                        data.kind = BlockKind::Tool {
                            status,
                            name: tool.title.clone(),
                            summary: String::new(),
                            meta: ToolMeta::default(),
                        };
                        data.gutter = Gutter::Glyph(status);
                    }
                }
                data.body = bounded(&data.body);
            }
            PresentationBlockKind::Subtask { descriptor, .. } => {
                let status = subtask_icon(descriptor.status);
                data.kind = BlockKind::Subtask {
                    status,
                    kind: descriptor.kind,
                    title: descriptor.title.clone(),
                };
                data.gutter = Gutter::Glyph(status);
                data.subtask = Some(descriptor.id.clone());
                data.body.clear();
            }
            PresentationBlockKind::Image { image, ordinal, .. } => {
                data.kind = BlockKind::Image {
                    label: image.label(*ordinal),
                };
                data.body.clear();
                let format = match image.mime_type() {
                    "image/png" => Some(ImageFormat::Png),
                    "image/jpeg" => Some(ImageFormat::Jpeg),
                    "image/webp" => Some(ImageFormat::Webp),
                    "image/gif" => Some(ImageFormat::Gif),
                    _ => None,
                };
                data.image =
                    format.map(|f| Arc::new(Image::from_bytes(f, image.encoded_bytes().to_vec())));
            }
            PresentationBlockKind::WebActivity(_) => {
                data.kind = BlockKind::Web;
                data.plain = true;
            }
            PresentationBlockKind::Error(_) => {
                data.kind = BlockKind::Notice {
                    level: NoticeLevel::Error,
                };
                data.gutter = Gutter::Glyph(StatusIcon::Failed);
                data.plain = true;
            }
            PresentationBlockKind::Diagnostic(d) => {
                data.kind = BlockKind::Notice {
                    level: if d.tone == DiagnosticTone::Error {
                        NoticeLevel::Error
                    } else {
                        NoticeLevel::Warning
                    },
                };
                data.gutter = Gutter::Glyph(StatusIcon::Denied);
                data.plain = true;
            }
            PresentationBlockKind::Plan(_) => {
                data.kind = BlockKind::Ensemble;
                data.plain = true;
            }
            PresentationBlockKind::Placeholder(_) => {
                data.plain = true;
            }
        }
        data
    }
}
pub fn entry(
    epoch: &str,
    index: usize,
    history: &HistoryEntry,
    revision: u64,
    continues_turn: bool,
    child: bool,
) -> Vec<BlockData> {
    let key = format!("{epoch}-row-{index}");
    let mut blocks: Vec<_> = match history {
        HistoryEntry::Conversation(entry) => {
            let mut blocks = entry
                .blocks
                .iter()
                .filter(|b| b.visible(false))
                .map(|b| BlockData::block(epoch, b))
                .collect::<Vec<_>>();
            if let Some(first) = blocks.first_mut() {
                if matches!(entry.header, Some(NativeHeader::Prompt(_))) {
                    first.gutter = Gutter::Label(if child { Speaker::Task } else { Speaker::You });
                    first.turn_boundary = index > 0;
                } else {
                    match first.kind {
                        BlockKind::Prose {
                            role: PresentationRole::User,
                            ..
                        } => {
                            first.gutter =
                                Gutter::Label(if child { Speaker::Task } else { Speaker::You });
                            first.turn_boundary = index > 0;
                        }
                        BlockKind::Prose {
                            role: PresentationRole::Assistant,
                            ..
                        }
                        | BlockKind::Reasoning
                            if !continues_turn =>
                        {
                            first.gutter = Gutter::Label(Speaker::Zevria)
                        }
                        BlockKind::Prose {
                            role: PresentationRole::System,
                            ..
                        } => first.gutter = Gutter::Label(Speaker::System),
                        _ => {}
                    }
                }
            }
            blocks
        }
        HistoryEntry::PlanArtifact(a) => vec![BlockData::new(
            key,
            BlockKind::Document {
                kind: DocumentKind::Artifact,
                title: a.title.clone(),
                version: format!("Revision {}", a.version.revision),
            },
            Gutter::Label(Speaker::Plan),
            a.markdown.clone(),
            false,
        )],
        HistoryEntry::PlanHandoff(h, _) => vec![BlockData::new(
            key,
            BlockKind::Document {
                kind: DocumentKind::Handoff,
                title: h.artifact.title.clone(),
                version: "Approved".into(),
            },
            Gutter::Label(Speaker::Plan),
            h.artifact.markdown.clone(),
            false,
        )],
        HistoryEntry::JobNotice(notice, header) => {
            let mut block = BlockData::new(
                key,
                BlockKind::Prose {
                    role: PresentationRole::System,
                    plain: true,
                },
                Gutter::Label(Speaker::System),
                format!(
                    "Background jobs finished\n{}",
                    notice.summary_lines().join("\n")
                ),
                true,
            );
            block.primary = notice.text().to_owned();
            block.turn_boundary = header.is_some() && index > 0;
            vec![block]
        }
        HistoryEntry::Ensemble(e) => vec![BlockData::new(
            key,
            BlockKind::Ensemble,
            Gutter::None,
            format!(
                "{}\n\n{}",
                e.command(),
                e.workers
                    .iter()
                    .map(|w| format!(
                        "{} {}",
                        StatusIcon::agent(w.status).glyph(0),
                        w.descriptor.id
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            true,
        )],
        HistoryEntry::Error(e) => vec![BlockData::new(
            key,
            BlockKind::Notice {
                level: NoticeLevel::Error,
            },
            Gutter::Glyph(StatusIcon::Failed),
            e.clone(),
            true,
        )],
        HistoryEntry::CompactionDivider => vec![BlockData::new(
            key,
            BlockKind::Divider,
            Gutter::None,
            String::new(),
            true,
        )],
    };
    for block in &mut blocks {
        // Gutter changes invalidate retained blocks even when their source revision is unchanged.
        block.revision = block.revision.wrapping_add(revision);
        if matches!(block.kind, BlockKind::Document { .. }) {
            block.collapsible = true;
        }
    }
    blocks
}
fn plain_html(text: &str) -> String {
    format!(
        "<p>{}</p>",
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\n', "<br>")
    )
}
pub struct BlockView {
    data: BlockData,
    text: Entity<TextViewState>,
    output: Entity<TextViewState>,
    expanded: bool,
    compact: bool,
    diff: Option<Entity<DiffState>>,
    diff_error: Option<String>,
    diff_task: Option<Task<()>>,
    _text: Subscription,
    _output: Subscription,
    _clock: Subscription,
    action_focus: [FocusHandle; 3],
    focus_subscriptions: Vec<Subscription>,
    controller: Entity<SessionController>,
}
impl BlockView {
    pub fn new(
        data: BlockData,
        controller: Entity<SessionController>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| {
            if data.plain {
                TextViewState::html(&plain_html(&data.body), cx)
            } else {
                TextViewState::markdown(&data.body, cx)
            }
        });
        let output = cx.new(|cx| {
            TextViewState::html(
                &plain_html(&bounded(data.secondary.as_deref().unwrap_or_default())),
                cx,
            )
        });
        let watch = cx.observe(&text, |_, _, cx| cx.notify());
        let watch_output = cx.observe(&output, |_, _, cx| cx.notify());
        let clock = cx.observe(&controller, |this, _, cx| {
            if matches!(
                this.data.gutter,
                Gutter::Live | Gutter::Glyph(StatusIcon::Running)
            ) {
                cx.notify();
            }
        });
        Self {
            data,
            text,
            output,
            expanded: false,
            compact,
            diff: None,
            diff_error: None,
            diff_task: None,
            _text: watch,
            _output: watch_output,
            _clock: clock,
            action_focus: [cx.focus_handle(), cx.focus_handle(), cx.focus_handle()],
            focus_subscriptions: Vec::new(),
            controller,
        }
    }
    pub fn sync(&mut self, data: BlockData, compact: bool, cx: &mut Context<Self>) {
        if compact != self.compact {
            self.compact = compact;
            cx.notify();
        }
        if data.revision == self.data.revision {
            return;
        }
        let body = if data.plain {
            plain_html(&data.body)
        } else {
            data.body.clone()
        };
        if data.plain != self.data.plain {
            self.text = cx.new(|cx| {
                if data.plain {
                    TextViewState::html(&body, cx)
                } else {
                    TextViewState::markdown(&body, cx)
                }
            });
            self._text = cx.observe(&self.text, |_, _, cx| cx.notify());
        } else {
            self.text.update(cx, |text, cx| text.set_text(&body, cx));
        }
        self.output.update(cx, |text, cx| {
            text.set_text(
                &plain_html(&bounded(data.secondary.as_deref().unwrap_or_default())),
                cx,
            )
        });
        let changes_changed = data.changes != self.data.changes;
        self.data = data;
        if changes_changed {
            self.diff = None;
            self.diff_error = None;
            self.diff_task = None;
        }
        if self.expanded {
            self.prepare_diff(cx);
        }
        cx.notify();
    }
    fn prepare_diff(&mut self, cx: &mut Context<Self>) {
        if self.diff.is_some() || self.diff_task.is_some() || self.data.changes.is_empty() {
            return;
        }
        let changes = self.data.changes.clone();
        let job = cx
            .background_executor()
            .spawn(async move { tools::parse(changes) });
        self.diff_task = Some(cx.spawn(async move |owner, cx| {
            let result = job.await;
            let _ = owner.update(cx, |this, cx| {
                match result {
                    Ok(files) => this.diff = Some(cx.new(|cx| DiffState::new(files, cx))),
                    Err(error) => this.diff_error = Some(error),
                }
                cx.notify();
            });
        }));
    }
    fn disclosure(&self, name: &'static str, cx: &mut Context<Self>) -> Button {
        style::icon_button(
            "expand",
            if self.expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            },
            name,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.expanded = !this.expanded;
            if this.expanded {
                this.prepare_diff(cx);
            }
            cx.notify();
        }))
    }
    fn actions(&self, window: &Window, _cx: &mut Context<Self>) -> Div {
        let primary = self.data.primary.clone();
        let focused = self.action_focus.iter().any(|f| f.is_focused(window));
        let mut actions = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .opacity(if focused { 1. } else { 0. })
            .group_hover("log-block", |s| s.opacity(1.))
            .child(
                style::icon_button(
                    "copy",
                    IconName::Copy,
                    if matches!(self.data.kind, BlockKind::Tool { .. }) {
                        "Copy input"
                    } else {
                        "Copy text"
                    },
                )
                .track_focus(&self.action_focus[0])
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(primary.clone()))
                }),
            );
        if let Some(output) = self.data.secondary.clone() {
            actions = actions.child(
                style::icon_button("copy-output", IconName::Copy, "Copy output")
                    .track_focus(&self.action_focus[1])
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(output.clone()))
                    }),
            );
        }
        if let Some(id) = self.data.subtask.clone() {
            let controller = self.controller.clone();
            actions = actions.child(
                style::icon_button("inspect", IconName::Eye, "Inspect subtask")
                    .track_focus(&self.action_focus[2])
                    .on_click(move |_, _, cx| {
                        controller.update(cx, |this, cx| {
                            this.model.selected_child = Some(id.clone());
                            cx.notify();
                        })
                    }),
            );
        }
        actions
    }
    fn rich_text(&self, document: bool, cx: &App) -> TextView {
        TextView::new(&self.text)
            .style(style::markdown(cx, document))
            .scrollable(false)
            .font_family(if document {
                style::DOCUMENT_FONT
            } else {
                style::UI_FONT
            })
            .text_size(px(16.))
            .line_height(px(if document { 27. } else { 26. }))
    }
}
impl Render for BlockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_subscriptions.is_empty() {
            for handle in &self.action_focus {
                self.focus_subscriptions
                    .push(cx.on_focus(handle, window, |_, _, cx| cx.notify()));
                self.focus_subscriptions
                    .push(cx.on_blur(handle, window, |_, _, cx| cx.notify()));
            }
        }
        let t = palette(cx);
        let ink = super::status::accent(&self.controller.read(cx).model.root, cx);
        let measure = style::Measure {
            compact: self.compact,
        };
        let mut gutter = measure.gutter().pt(px(5.));
        gutter = match self.data.gutter {
            Gutter::None => gutter,
            Gutter::Label(speaker) => gutter
                .text_color(if speaker == Speaker::Plan {
                    t.workflow.plan
                } else {
                    t.text.muted
                })
                .child(speaker.label()),
            Gutter::Glyph(status) => gutter.child(style::status_glyph(status, ink, cx)),
            Gutter::Live => gutter.child(style::status_glyph(StatusIcon::Running, ink, cx)),
        };
        if matches!(self.data.kind, BlockKind::Web) {
            gutter = gutter.child(Icon::new(IconName::Globe).small().text_color(t.text.muted));
        }
        if matches!(
            self.data.kind,
            BlockKind::Notice {
                level: NoticeLevel::Warning
            } | BlockKind::Live(LiveTail::Retrying { .. })
        ) {
            gutter = measure
                .gutter()
                .pt(px(5.))
                .text_size(px(14.))
                .text_color(t.feedback.warning)
                .child("!");
        }
        let mut content = div()
            .flex_1()
            .min_w_0()
            .max_w(px(style::CONTENT))
            .flex()
            .flex_col()
            .gap(px(8.));
        match &self.data.kind {
            BlockKind::Prose { role, .. } => {
                let prompt = *role == PresentationRole::User;
                content = content.max_w(px(style::PROSE)).child(
                    div()
                        .relative()
                        .group("log-block")
                        .when(prompt, |d| d.font_weight(FontWeight::MEDIUM))
                        .child(self.rich_text(false, cx))
                        .child(
                            div()
                                .absolute()
                                .right_0()
                                .top_0()
                                .child(self.actions(window, cx)),
                        ),
                );
            }
            BlockKind::Reasoning => {
                content = content.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(self.disclosure(
                            if self.expanded {
                                "Hide details"
                            } else {
                                "Show details"
                            },
                            cx,
                        ))
                        .child(
                            div()
                                .text_size(px(14.))
                                .line_height(px(20.))
                                .italic()
                                .text_color(t.text.muted)
                                .child("Reasoning"),
                        ),
                );
                if self.expanded {
                    content = content.child(
                        self.rich_text(false, cx)
                            .text_size(px(14.))
                            .line_height(px(22.))
                            .text_color(t.text.secondary)
                            .max_w(px(style::PROSE)),
                    );
                }
            }
            BlockKind::Tool {
                status,
                name,
                summary,
                meta,
            } => {
                let mut row = div()
                    .min_h(px(28.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .group("log-block")
                    .child(
                        style::label()
                            .text_color(t.text.secondary)
                            .child(name.clone()),
                    )
                    .child(
                        style::mono()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(summary.clone()),
                    );
                if meta.added > 0 {
                    row = row.child(
                        style::meta()
                            .text_color(t.feedback.success)
                            .child(format!("+{}", meta.added)),
                    );
                }
                if meta.removed > 0 {
                    row = row.child(
                        style::meta()
                            .text_color(t.feedback.error)
                            .child(format!("−{}", meta.removed)),
                    );
                }
                if meta.background {
                    row = row.child(style::tag("background", t.text.secondary, cx));
                }
                match status {
                    StatusIcon::Denied => {
                        row = row.child(
                            style::label()
                                .text_color(t.feedback.warning)
                                .child("denied"),
                        )
                    }
                    StatusIcon::Interrupted => {
                        row = row.child(
                            style::label()
                                .text_color(t.feedback.warning)
                                .child("interrupted"),
                        )
                    }
                    _ => {}
                }
                row = row.child(self.actions(window, cx)).child(self.disclosure(
                    if self.expanded {
                        "Hide details"
                    } else {
                        "Show details"
                    },
                    cx,
                ));
                content = content.child(row);
                if self.expanded {
                    let mut well = div()
                        .w_full()
                        .p(px(12.))
                        .rounded(px(10.))
                        .bg(t.surfaces.panel)
                        .flex()
                        .flex_col()
                        .gap(px(12.));
                    // Remove the redundant path/count summary once the native
                    // Diff header is visible. Diagnostics and plan-draft detail
                    // remain visible, and all copy payloads stay exact.
                    if self.diff.is_none() || !meta.diff_summary {
                        well = well.child(
                            self.rich_text(false, cx)
                                .text_size(px(14.))
                                .line_height(px(22.)),
                        );
                    }
                    if self.data.secondary.is_some() {
                        well = well.child(
                            TextView::new(&self.output)
                                .style(style::markdown(cx, false))
                                .scrollable(false)
                                .font_family(style::MONO_FONT)
                                .text_size(px(13.))
                                .line_height(px(20.)),
                        );
                    }
                    if let Some(diff) = &self.diff {
                        well = well.child(
                            Diff::new(diff)
                                .h(px(tools::diff_height(&self.data.changes)))
                                .w_full(),
                        );
                    }
                    if let Some(error) = &self.diff_error {
                        well = well.child(
                            div()
                                .text_size(px(12.))
                                .text_color(t.feedback.warning)
                                .child(error.clone()),
                        );
                    }
                    content = content.child(well);
                }
            }
            BlockKind::Subtask { kind, title, .. } => {
                let controller = self.controller.clone();
                let id = self.data.subtask.clone();
                content = content.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            style::label()
                                .text_color(match kind {
                                    zevria_foundation::SubtaskKind::Build => t.workflow.build,
                                    zevria_foundation::SubtaskKind::Explore => t.workflow.explore,
                                })
                                .child(style::kind_label(*kind)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_size(px(14.))
                                .child(title.clone()),
                        )
                        .child(
                            Button::new("inspect")
                                .ghost()
                                .small()
                                .label("Inspect")
                                .on_click(move |_, _, cx| {
                                    controller.update(cx, |this, cx| {
                                        this.model.selected_child = id.clone();
                                        cx.notify();
                                    })
                                }),
                        ),
                );
            }
            BlockKind::Document {
                kind,
                title,
                version,
            } => {
                content = content
                    .bg(t.surfaces.overlay)
                    .border_1()
                    .border_color(tones(cx).hairline)
                    .rounded(px(10.))
                    .p(px(16.))
                    .group("log-block")
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(16.))
                                    .line_height(px(22.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title.clone()),
                            )
                            .child(self.actions(window, cx)),
                    )
                    .child(style::tag(
                        version.clone(),
                        if *kind == DocumentKind::Handoff {
                            t.text.secondary
                        } else {
                            t.workflow.plan
                        },
                        cx,
                    ))
                    .child(
                        self.rich_text(true, cx)
                            .when(!self.expanded, |v| v.max_lines(8)),
                    )
                    .child(
                        Button::new("expand")
                            .ghost()
                            .small()
                            .label(if self.expanded {
                                "Show less"
                            } else {
                                "Show full plan"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.expanded = !this.expanded;
                                cx.notify();
                            })),
                    );
            }
            BlockKind::Image { label } => {
                if let Some(image) = &self.data.image {
                    content = content.child(
                        img(image.clone())
                            .w_full()
                            .max_h(px(240.))
                            .max_w(px(480.))
                            .object_fit(ObjectFit::Contain)
                            .rounded(px(6.))
                            .border_1()
                            .border_color(tones(cx).hairline),
                    );
                }
                content =
                    content.child(style::meta().text_color(t.text.muted).child(label.clone()));
            }
            BlockKind::Divider => {
                content = content.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .py(px(16.))
                        .child(div().flex_1().h(px(1.)).bg(tones(cx).hairline))
                        .child(
                            style::label()
                                .text_color(t.text.muted)
                                .child("Context compacted"),
                        )
                        .child(div().flex_1().h(px(1.)).bg(tones(cx).hairline)),
                );
            }
            BlockKind::Notice { level } => {
                content = content.child(
                    self.rich_text(false, cx)
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(if *level == NoticeLevel::Error {
                            t.feedback.error
                        } else {
                            t.feedback.warning
                        }),
                );
            }
            BlockKind::Live(tail) => {
                content = content
                    .text_color(t.text.secondary)
                    .text_size(px(14.))
                    .line_height(px(20.));
                match tail {
                    LiveTail::Waiting { elapsed } | LiveTail::Compacting { elapsed } => {
                        content = content.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .child(if matches!(tail, LiveTail::Waiting { .. }) {
                                    "Waiting for the model"
                                } else {
                                    "Compacting context"
                                })
                                .child(
                                    style::meta()
                                        .text_color(t.text.muted)
                                        .child(elapsed.clone()),
                                ),
                        );
                    }
                    LiveTail::Retrying {
                        attempt,
                        remaining,
                        error,
                    } => {
                        content = content
                            .child(format!("Reconnecting, attempt {attempt}"))
                            .child(
                                style::meta()
                                    .text_color(t.text.muted)
                                    .child(format!("Next try in {remaining}")),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(t.text.muted)
                                    .child(error.clone()),
                            );
                    }
                }
            }
            BlockKind::Web | BlockKind::Ensemble => {
                content = content.child(
                    self.rich_text(false, cx)
                        .text_size(px(14.))
                        .line_height(px(20.)),
                );
            }
        }
        if matches!(
            self.data.kind,
            BlockKind::Reasoning
                | BlockKind::Image { .. }
                | BlockKind::Web
                | BlockKind::Notice { .. }
                | BlockKind::Ensemble
        ) {
            content = content.relative().group("log-block").child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .child(self.actions(window, cx)),
            );
        }
        let compact_inline = self.compact
            && (matches!(self.data.gutter, Gutter::Glyph(_) | Gutter::Live)
                || matches!(self.data.kind, BlockKind::Web));
        let show_gutter = !self.compact
            || self.data.gutter != Gutter::None
            || matches!(self.data.kind, BlockKind::Web);
        measure
            .row()
            .id(self.data.key.clone())
            .test_support()
            .when(compact_inline, |r| r.flex_row().gap(px(8.)))
            .when(self.data.turn_boundary, |r| r.pt(px(32.)))
            .when(show_gutter, |row| row.child(gutter))
            .child(content.when(self.data.turn_boundary, |c| {
                c.border_t_1().border_color(tones(cx).hairline).pt(px(16.))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn tool_previews_are_bounded_without_changing_copy_or_summary() {
        use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
        let args = serde_json::json!({"file_path":"src/lib.rs", "content":"x".repeat(32_000)});
        let call = ToolCall::new(
            CallId::from_wire("large"),
            ToolFunction::new(ToolName::new("write").unwrap(), args.clone()),
        );
        let block = PresentationBlock {
            id: PresentationBlockId(1),
            revision: 0,
            role: Some(PresentationRole::Assistant),
            prompt_group: None,
            prompt: None,
            visibility: BlockVisibility::Always,
            kind: PresentationBlockKind::Tool(PresentedTool::Native {
                call: Box::new(call),
                state: Box::new(NativeToolState::new(NativeToolCallStatus::Finished, &args)),
            }),
        };
        let data = BlockData::block("epoch", &block);
        assert_eq!(data.primary, block.primary_copy());
        assert!(data.primary.len() > 32_000);
        assert!(
            data.body
                .ends_with("Showing the first 24,000 characters. Copy includes everything.")
        );
        assert!(data.body.chars().count() < 24_100);
        assert!(matches!(data.kind,BlockKind::Tool { summary,.. } if summary == "src/lib.rs"));
    }
    #[::core::prelude::v1::test]
    fn job_notices_map_to_system_prose_and_copy_verbatim_text() {
        use zevria_foundation::*;
        let notice = CommandJobNotice::new(
            CommandJobNoticeKind::Update,
            vec![CommandJobSnapshot {
                id: CommandJobId::new("job-view"),
                call_id: None,
                command: "test".into(),
                mode: CommandJobMode::Background,
                status: CommandJobStatus::Exited {
                    code: Some(1),
                    success: false,
                    signal: None,
                },
                started_at: std::time::SystemTime::UNIX_EPOCH,
                finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
                stdout_bytes: 0,
                stderr_bytes: 0,
            }],
            vec![],
            "Command job update: full output".into(),
        );
        let history = HistoryEntry::JobNotice(notice.clone(), None);
        let data = entry("epoch", 1, &history, 0, false, false);
        assert!(matches!(
            data[0].kind,
            BlockKind::Prose {
                role: PresentationRole::System,
                ..
            }
        ));
        assert_eq!(data[0].gutter, Gutter::Label(Speaker::System));
        assert_eq!(data[0].primary, notice.text());
        assert!(!data[0].body.contains("full output"));
    }
    #[::core::prelude::v1::test]
    fn child_prompts_use_task_gutters_and_prompt_boundaries() {
        let history = HistoryEntry::from_message(
            rig_core::message::Message::user("Task text"),
            zevria_presentation::conversation::ToolCallStatus::Finished,
        )
        .unwrap();
        let data = entry("epoch", 1, &history, 0, false, true);
        assert_eq!(data[0].gutter, Gutter::Label(Speaker::Task));
        assert!(data[0].turn_boundary);
        assert!(!entry("epoch", 0, &history, 0, false, false)[0].turn_boundary);
    }
}
