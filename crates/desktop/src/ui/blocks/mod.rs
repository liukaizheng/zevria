//! Retained semantic log blocks. They never infer or mutate engine semantics.
mod tools;
use super::{
    math, style,
    theme::{palette, tones},
};
use crate::{
    controller::SessionController,
    disclosure::{Phase, TailKind},
};
use gpui_kit::base::{AccordionTrigger, text::TextView};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::{
    Icon, IconName, Sizable,
    button::{Button, ButtonVariants},
    diff::{Diff, DiffState},
    text::TextViewState,
};
use gpui_kit::{prelude::*, *};
use std::{ops::Range, sync::Arc};
use zevria_presentation::{
    blocks::*,
    conversation::HistoryEntry,
    shell::{self, ShellToken},
    status_icon::StatusIcon,
};

pub fn tool_label(name: &str) -> &str {
    match name {
        "command" | "command_status" | "command_terminate" => "Command",
        "edit" => "Edit",
        "write" => "Write",
        "delete" => "Delete",
        "read" | "skill_read" => "Read",
        "web_search" => "Search",
        "question" => "Question",
        "create_plan" | "update_plan" | "read_plan" | "submit_plan" => "Plan draft",
        "task" => "Tasks",
        "launch_subtasks" => "Subtasks",
        _ => name,
    }
}

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
    Neutral,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    Artifact,
    Handoff,
}
#[derive(Clone, Debug)]
pub enum LiveTail {
    Activity(String),
}
#[derive(Clone, Debug, Default)]
pub struct ToolMeta {
    pub added: usize,
    pub removed: usize,
    pub background: bool,
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
        highlights: Vec<(Range<usize>, ShellToken)>,
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
    pub expanded: bool,
    pub phase: Phase,
    pub source: Option<PresentationBlockId>,
    pub header: Option<NativeHeader>,
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
            expanded: false,
            phase: Phase::Finished,
            source: None,
            header: None,
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
        data.source = Some(block.id);
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
                                data.body = format!("```json\n{}\n```", bounded(&data.primary));
                                data.plain = false;
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
                        let highlights = if call.function.name.as_str() == "command" {
                            let display = shell::display(&summary);
                            summary = display.text;
                            display.spans
                        } else {
                            Vec::new()
                        };
                        data.kind = BlockKind::Tool {
                            status,
                            name: call.function.name.to_string(),
                            summary,
                            highlights,
                            meta: ToolMeta {
                                added,
                                removed,
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
                            highlights: Vec::new(),
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
                if let Some(outcome) = zevria_presentation::incomplete_response_outcome(&block.kind)
                {
                    data.kind = BlockKind::Notice {
                        level: NoticeLevel::Warning,
                    };
                    data.body = if outcome == zevria_content::ResponseAttemptOutcome::Interrupted {
                        "Stopped before the response finished"
                    } else {
                        "The response failed before it finished"
                    }
                    .into();
                    data.gutter = Gutter::Glyph(StatusIcon::Interrupted);
                }
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
            let interrupted = entry
                .blocks
                .iter()
                .any(|b| zevria_presentation::incomplete_response_outcome(&b.kind).is_some());
            for block in &mut blocks {
                block.header = entry.header;
                if interrupted {
                    block.phase = Phase::Interrupted;
                }
            }
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
                    zevria_presentation::jobs::display_notice_lines(notice).join("\n")
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
        HistoryEntry::Error(e) => {
            let mut notice = BlockData::new(
                key,
                BlockKind::Notice {
                    level: if e == zevria_presentation::TURN_CANCELLED {
                        NoticeLevel::Neutral
                    } else {
                        NoticeLevel::Error
                    },
                },
                Gutter::Glyph(if e == zevria_presentation::TURN_CANCELLED {
                    StatusIcon::Interrupted
                } else {
                    StatusIcon::Failed
                }),
                if e == zevria_presentation::TURN_CANCELLED {
                    "Stopped".into()
                } else {
                    e.clone()
                },
                true,
            );
            notice.primary = e.clone();
            vec![notice]
        }
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
impl BlockData {
    pub fn tail_kind(&self) -> Option<TailKind> {
        match self.kind {
            BlockKind::Tool { .. } | BlockKind::Subtask { .. } | BlockKind::Live(_) => None,
            BlockKind::Reasoning => Some(TailKind::Thinking),
            BlockKind::Prose {
                role: PresentationRole::Assistant,
                ..
            } => Some(TailKind::Answer),
            _ => Some(TailKind::Other),
        }
    }
}
#[derive(Clone)]
pub struct DisclosureChanged {
    pub key: String,
    pub expanded: bool,
}

fn output_preview(text: &str, all: bool) -> String {
    if all {
        text.into()
    } else {
        text.lines().take(16).collect::<Vec<_>>().join("\n")
    }
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
    compact: bool,
    show_all_output: bool,
    diff: Option<Entity<DiffState>>,
    diff_error: Option<String>,
    diff_task: Option<Task<()>>,
    _text: Subscription,
    _output: Subscription,
    _clock: Subscription,
    action_focus: [FocusHandle; 3],
    disclosure_focus: FocusHandle,
    focus_subscriptions: Vec<Subscription>,
    controller: Entity<SessionController>,
}
impl EventEmitter<DisclosureChanged> for BlockView {}
impl BlockView {
    pub fn key(&self) -> &str {
        &self.data.key
    }
    #[cfg(test)]
    pub(super) fn text_id(&self) -> EntityId {
        self.text.entity_id()
    }
    #[cfg(test)]
    pub(super) fn disclosure_focus(&self) -> FocusHandle {
        self.disclosure_focus.clone()
    }
    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.disclosure_focus.contains_focused(window, cx)
            || self
                .action_focus
                .iter()
                .any(|f| f.contains_focused(window, cx))
    }
    pub fn disclose(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if !self.data.collapsible {
            return;
        }
        self.data.expanded = expanded;
        cx.emit(DisclosureChanged {
            key: self.data.key.clone(),
            expanded,
        });
        if expanded {
            self.prepare_diff(cx);
        }
        cx.notify();
    }
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
                &plain_html(&output_preview(
                    data.secondary.as_deref().unwrap_or_default(),
                    false,
                )),
                cx,
            )
        });
        let watch = cx.observe(&text, |_, _, cx| cx.notify());
        let watch_output = cx.observe(&output, |_, _, cx| cx.notify());
        let clock = cx.observe(&controller, |this, _, cx| {
            if matches!(
                this.data.gutter,
                Gutter::Live | Gutter::Glyph(StatusIcon::Running)
            ) || (matches!(this.data.kind, BlockKind::Reasoning)
                && this.data.phase == Phase::Live)
            {
                cx.notify();
            }
        });
        Self {
            data,
            text,
            output,
            compact,
            show_all_output: false,
            diff: None,
            diff_error: None,
            diff_task: None,
            _text: watch,
            _output: watch_output,
            _clock: clock,
            action_focus: [cx.focus_handle(), cx.focus_handle(), cx.focus_handle()],
            disclosure_focus: cx.focus_handle(),
            focus_subscriptions: Vec::new(),
            controller,
        }
    }
    pub fn sync(&mut self, data: BlockData, compact: bool, cx: &mut Context<Self>) {
        if compact != self.compact {
            self.compact = compact;
            cx.notify();
        }
        let metadata_changed = data.expanded != self.data.expanded
            || data.phase != self.data.phase
            || data.key != self.data.key;
        if data.revision == self.data.revision
            && data.body == self.data.body
            && data.secondary == self.data.secondary
        {
            self.data = data;
            if metadata_changed {
                if self.data.expanded {
                    self.prepare_diff(cx);
                }
                cx.notify();
            }
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
        } else if data.body != self.data.body {
            self.text.update(cx, |text, cx| text.set_text(&body, cx));
        }
        self.output.update(cx, |text, cx| {
            text.set_text(
                &plain_html(&output_preview(
                    data.secondary.as_deref().unwrap_or_default(),
                    self.show_all_output,
                )),
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
        if self.data.expanded {
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
    fn disclosure(
        &self,
        name: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AccordionTrigger {
        let owner = cx.entity();
        AccordionTrigger::new("expand")
            .open(self.data.expanded)
            .track_focus(&self.disclosure_focus)
            .aria_label(name)
            .relative()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(tones(cx).hover))
            .active(|s| s.bg(tones(cx).pressed))
            .on_change(move |expanded, _, window, cx| {
                owner.update(cx, |this, cx| {
                    window.focus(&this.disclosure_focus, cx);
                    this.disclose(expanded, cx);
                })
            })
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.disclose(!this.data.expanded, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                Icon::new(if self.data.expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .small(),
            )
            .when(self.disclosure_focus.is_focused(window), |trigger| {
                trigger.child(style::focus_ring(6., cx))
            })
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
            .markdown_extensions(math::extensions())
            .style(style::markdown(cx, document))
            .code_block_actions(|code, _, cx| {
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
                    .child(
                        Clipboard::new("copy-code")
                            .value(code.code())
                            .tooltip("Copy code")
                            .accessibility_label("Copy code"),
                    )
            })
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
}
impl Render for BlockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_subscriptions.is_empty() {
            for handle in self
                .action_focus
                .iter()
                .chain(std::iter::once(&self.disclosure_focus))
            {
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
                .text_color(match speaker {
                    Speaker::You | Speaker::Task => t.roles.you,
                    Speaker::Zevria if matches!(self.data.kind, BlockKind::Reasoning) => {
                        tones(cx).pencil
                    }
                    Speaker::Zevria => t.roles.assistant,
                    Speaker::System => t.roles.system,
                    Speaker::Plan => t.workflow.plan,
                })
                .child(speaker.label()),
            Gutter::Glyph(_) | Gutter::Live => gutter,
        };
        if matches!(self.data.kind, BlockKind::Web) {
            gutter = gutter.child(Icon::new(IconName::Globe).small().text_color(t.roles.tools));
        }
        let mut content = div()
            .flex_1()
            .min_w_0()
            .max_w(px(style::CONTENT))
            .flex()
            .flex_col()
            .relative()
            .py(px(4.))
            .gap(px(8.));
        match &self.data.kind {
            BlockKind::Prose { role, .. } => {
                let prompt = *role == PresentationRole::User;
                if self.data.phase == Phase::Unsaved {
                    content = content.child(
                        style::meta()
                            .text_color(t.feedback.warning)
                            .child("Stopped, not saved to this session"),
                    );
                }
                content = content.max_w(px(style::PROSE)).child(
                    div()
                        .relative()
                        .group("log-block")
                        .when(prompt, |d| d.font_weight(FontWeight::MEDIUM))
                        .child(
                            self.rich_text(false, cx)
                                .when(*role == PresentationRole::System, |view| {
                                    style::SECONDARY.apply(view)
                                }),
                        )
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
                let preview = crate::disclosure::preview(&self.data.body);
                let owner = cx.entity();
                let mut header = AccordionTrigger::new("thinking-disclosure")
                    .open(self.data.expanded)
                    .track_focus(&self.disclosure_focus)
                    .aria_label("Thinking")
                    .aria_description(preview.clone())
                    .relative()
                    .w_full()
                    .min_h(px(28.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|h| h.bg(tones(cx).hover))
                    .active(|h| h.bg(tones(cx).pressed))
                    .on_change(move |expanded, _, window, cx| {
                        owner.update(cx, |this, cx| {
                            window.focus(&this.disclosure_focus, cx);
                            this.disclose(expanded, cx);
                        })
                    })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.disclose(!this.data.expanded, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        Icon::new(if self.data.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .text_color(tones(cx).pencil),
                    )
                    .child(
                        style::label()
                            .text_color(tones(cx).pencil)
                            .child("Thinking"),
                    );
                if self.data.phase == Phase::Live {
                    header = header.child(style::status_glyph(StatusIcon::Running, ink, cx));
                }
                if self.data.phase == Phase::Unsaved {
                    header = header.child(
                        style::meta()
                            .text_color(t.feedback.warning)
                            .child("Stopped, not saved to this session"),
                    );
                }
                if !self.data.expanded {
                    header = header
                        .child(
                            style::pencil()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .italic()
                                .text_color(tones(cx).pencil)
                                .child(preview),
                        )
                        .child(
                            style::meta()
                                .flex_shrink_0()
                                .text_color(t.text.muted)
                                .child(format!(
                                    "{} words",
                                    self.data.body.split_whitespace().count()
                                )),
                        );
                }
                if self.disclosure_focus.is_focused(window) {
                    header = header.child(style::focus_ring(6., cx));
                }
                content = content.max_w(px(style::PROSE)).child(
                    div()
                        .group("log-block")
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(header)
                        .child(self.actions(window, cx)),
                );
                if self.data.expanded {
                    content = content.child(
                        div().pl(px(24.)).child(
                            self.rich_text(false, cx)
                                .style(style::thought_markdown(cx))
                                .code_block_highlighter(|_| Vec::new())
                                .font_family(style::DOCUMENT_FONT)
                                .map(|view| style::PENCIL.apply(view))
                                .italic()
                                .text_color(tones(cx).pencil),
                        ),
                    );
                }
            }
            BlockKind::Tool {
                status,
                name,
                summary,
                highlights,
                meta,
            } => {
                let path = matches!(name.as_str(), "edit" | "write" | "read" | "delete");
                let displayed = if path {
                    super::sidebar::middle_path(summary, 64)
                } else {
                    summary.clone()
                };
                let header_highlights: Vec<_> = if path {
                    displayed
                        .rfind('/')
                        .map(|end| {
                            (
                                0..end + 1,
                                HighlightStyle {
                                    color: Some(t.text.muted),
                                    ..Default::default()
                                },
                            )
                        })
                        .into_iter()
                        .collect()
                } else {
                    highlights
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
                        .collect()
                };
                let mut row = div()
                    .min_h(px(28.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .group("log-block")
                    .child(
                        style::label()
                            .text_color(t.text.secondary)
                            .child(tool_label(name).to_owned()),
                    )
                    .child(
                        style::mono()
                            .flex_1()
                            .min_w_0()
                            .text_color(t.text.primary)
                            .truncate()
                            .child(StyledText::new(displayed).with_highlights(header_highlights)),
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
                    row = row.child(style::tag("background", t.feedback.info, cx));
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
                    if self.data.expanded {
                        "Hide details"
                    } else {
                        "Show details"
                    },
                    window,
                    cx,
                ));
                content = content.child(row);
                if self.data.expanded {
                    let mut well = div()
                        .w_full()
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(t.surfaces.panel)
                        .flex()
                        .flex_col()
                        .gap(px(12.));
                    well = well
                        .child(style::label().text_color(t.text.muted).child("Input"))
                        .child(
                            self.rich_text(false, cx)
                                .when(name == "command", |view| {
                                    view.code_block_highlighter(move |block| {
                                        if block.lang().as_deref() != Some("sh") {
                                            return Vec::new();
                                        }
                                        shell::tokens(&block.code())
                                            .into_iter()
                                            .map(|(range, token)| {
                                                (
                                                    range,
                                                    HighlightStyle {
                                                        color: Some(style::shell_ink(token, &t)),
                                                        ..Default::default()
                                                    },
                                                )
                                            })
                                            .collect()
                                    })
                                })
                                .font_family(style::MONO_FONT)
                                .map(|view| style::CODE.apply(view)),
                        );
                    if !self.data.changes.is_empty() {
                        well = well.child(
                            style::meta()
                                .text_color(t.text.secondary)
                                .child(tools::summary(&self.data.changes)),
                        );
                    }
                    if self.data.secondary.is_some() {
                        well = well
                            .child(style::label().text_color(t.text.muted).child("Output"))
                            .child(
                                TextView::new(&self.output)
                                    .style(style::markdown(cx, false))
                                    .scrollable(false)
                                    .font_family(style::MONO_FONT)
                                    .map(|view| style::CODE.apply(view)),
                            );
                    }
                    if let Some(output) = &self.data.secondary {
                        let lines = output.lines().count();
                        if lines > 16 {
                            well = well.child(
                                Button::new("show-all-output")
                                    .ghost()
                                    .small()
                                    .label(if self.show_all_output {
                                        "Show less".into()
                                    } else {
                                        format!("Show all {lines} lines")
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_all_output = !this.show_all_output;
                                        let body = plain_html(&output_preview(
                                            this.data.secondary.as_deref().unwrap_or_default(),
                                            this.show_all_output,
                                        ));
                                        this.output
                                            .update(cx, |output, cx| output.set_text(&body, cx));
                                        cx.notify();
                                    })),
                            );
                        }
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
                            style::mono()
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
                                .map(|view| style::SECONDARY.apply(view))
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
                                    .map(|view| style::HEADINGS[1].apply(view))
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
                            .when(!self.data.expanded, |v| v.max_lines(8)),
                    )
                    .child(
                        Button::new("expand")
                            .ghost()
                            .small()
                            .label(if self.data.expanded {
                                "Show less"
                            } else {
                                "Show full plan"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.disclose(!this.data.expanded, cx);
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
                let mut metadata = div().flex().items_center().gap(px(12.));
                for segment in label.trim_matches(['[', ']']).split('·') {
                    metadata = metadata.child(
                        style::meta()
                            .text_color(t.text.muted)
                            .child(segment.trim().to_owned()),
                    );
                }
                content = content.child(metadata);
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
                        .map(|view| style::SECONDARY.apply(view))
                        .text_color(match level {
                            NoticeLevel::Error => t.feedback.error,
                            NoticeLevel::Warning => t.feedback.warning,
                            NoticeLevel::Neutral => t.text.secondary,
                        }),
                );
            }
            BlockKind::Live(_) => {
                let controller = self.controller.read(cx);
                content = content
                    .text_color(t.text.secondary)
                    .map(|view| style::SECONDARY.apply(view))
                    .child(super::status::working_activity(
                        controller.model.visible(),
                        controller.cancelling,
                    ));
            }
            BlockKind::Web | BlockKind::Ensemble => {
                content = content.child(
                    self.rich_text(false, cx)
                        .map(|view| style::SECONDARY.apply(view)),
                );
            }
        }
        if matches!(
            self.data.kind,
            BlockKind::Image { .. }
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
        let threaded = !matches!(
            self.data.kind,
            BlockKind::Prose {
                role: PresentationRole::User,
                ..
            } | BlockKind::Divider
        ) && !matches!(self.data.header, Some(NativeHeader::Prompt(_)));
        if threaded {
            content = content.child(style::thread_segment(
                matches!(self.data.kind, BlockKind::Reasoning),
                cx,
            ));
            let node = match self.data.gutter {
                Gutter::Glyph(status) => Some(status),
                Gutter::Live => Some(StatusIcon::Running),
                _ => None,
            };
            if let Some(status) = node {
                content = content.child(
                    if matches!(
                        self.data.kind,
                        BlockKind::Notice {
                            level: NoticeLevel::Neutral
                        }
                    ) {
                        style::thread_marker("◼", t.text.secondary, cx)
                    } else {
                        style::thread_node(status, ink, cx)
                    },
                );
            }
        }
        let show_gutter = !self.compact || matches!(self.data.gutter, Gutter::Label(_));
        measure
            .row()
            .id(self.data.key.clone())
            .test_support()
            .when(self.data.turn_boundary, |r| r.pt(px(32.)))
            .when(show_gutter, |row| row.child(gutter))
            .child(
                content
                    .when(self.compact, |c| c.ml(px(16.)))
                    .when(self.data.turn_boundary, |c| {
                        c.border_t_1().border_color(tones(cx).hairline).pt(px(16.))
                    }),
            )
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
    fn command_header_is_stripped_and_highlighted_but_input_and_copy_stay_exact() {
        use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
        let command = "rtk proxy gh api x";
        let args = serde_json::json!({"command": command});
        let block = PresentationBlock {
            id: PresentationBlockId(1),
            revision: 0,
            role: Some(PresentationRole::Assistant),
            prompt_group: None,
            prompt: None,
            visibility: BlockVisibility::Always,
            kind: PresentationBlockKind::Tool(PresentedTool::Native {
                call: Box::new(ToolCall::new(
                    CallId::from_wire("command"),
                    ToolFunction::new(ToolName::new("command").unwrap(), args.clone()),
                )),
                state: Box::new(NativeToolState::new(NativeToolCallStatus::Finished, &args)),
            }),
        };
        let data = BlockData::block("epoch", &block);
        assert_eq!(data.primary, block.primary_copy());
        assert_eq!(data.primary, args.to_string());
        assert_eq!(data.body, format!("```sh\n{command}\n```"));
        let BlockKind::Tool {
            summary,
            highlights,
            ..
        } = data.kind
        else {
            panic!("tool")
        };
        assert_eq!(summary, "gh api x");
        assert_eq!(highlights, [(0..2, ShellToken::Program)]);
    }

    #[::core::prelude::v1::test]
    fn job_notices_map_to_system_prose_and_copy_verbatim_text() {
        use zevria_foundation::*;
        let notice = CommandJobNotice::new(
            CommandJobNoticeKind::Update,
            vec![CommandJobSnapshot {
                id: CommandJobId::new("job-view"),
                call_id: None,
                command: "rtk proxy cargo test".into(),
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
            "Command job update: full output from rtk proxy cargo test".into(),
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
        assert!(data[0].body.ends_with("cargo test"));
        assert!(!data[0].body.contains("rtk"));
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
