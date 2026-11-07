//! Retained native block entities. They never infer engine semantics.
mod tools;
use super::theme::palette;
use crate::controller::SessionController;
use gpui_kit::component::{
    button::Button,
    diff::{Diff, DiffState},
    text::{TextView, TextViewState},
};
use gpui_kit::{prelude::*, *};
use std::sync::Arc;
use zevria_presentation::{blocks::*, conversation::HistoryEntry};

#[derive(Clone)]
pub struct BlockData {
    pub key: String,
    pub revision: u64,
    pub title: String,
    pub body: String,
    pub plain: bool,
    pub collapsible: bool,
    pub primary: String,
    pub secondary: Option<String>,
    pub subtask: Option<zevria_foundation::SubtaskId>,
    pub image: Option<Arc<Image>>,
    pub changes: Vec<zevria_foundation::FileChangeOutput>,
}
fn bounded(text: &str) -> String {
    let mut value = text.chars().take(24_000).collect::<String>();
    if value.len() < text.len() {
        value.push_str("\n… Display truncated; Copy contains the full value.");
    }
    value
}
impl BlockData {
    pub fn text(key: String, title: String, body: String, plain: bool) -> Self {
        Self {
            key,
            revision: 0,
            title,
            primary: body.clone(),
            body,
            plain,
            collapsible: false,
            secondary: None,
            subtask: None,
            image: None,
            changes: Vec::new(),
        }
    }
    fn block(epoch: &str, block: &PresentationBlock) -> Self {
        let mut data = Self::text(
            format!("{epoch}-{}", block.id.0),
            String::new(),
            block.primary_copy(),
            false,
        );
        data.revision = block.revision;
        data.secondary = block.secondary_copy();
        data.title = match block.role {
            Some(PresentationRole::User) => "You",
            Some(PresentationRole::Assistant) => "Zevria",
            Some(PresentationRole::System) => "System",
            None => "",
        }
        .into();
        match &block.kind {
            PresentationBlockKind::Text { flavor, .. } => data.plain = *flavor == TextFlavor::Plain,
            PresentationBlockKind::Reasoning { .. } => {
                data.title = "Reasoning".into();
                data.collapsible = true;
            }
            PresentationBlockKind::Tool(tool) => {
                data.collapsible = true;
                match tool {
                    PresentedTool::Native { call, state } => {
                        data.title = format!(
                            "{} {}",
                            native_tool_status(state).icon().glyph(0),
                            call.function.name
                        );
                        if let Some(list) = block.readable_list_copy() {
                            data.body = list;
                        } else if call.function.name.as_str() == "command" {
                            data.body = format!(
                                "```sh\n{}\n```",
                                tool_argument(state, "command")
                                    .unwrap_or_else(|| data.primary.clone())
                            );
                        } else if let Some((body, plain)) =
                            tools::details(call.function.name.as_str(), state)
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
                            if let Some(draft) = metadata
                                .detail
                                .as_ref()
                                .and_then(|detail| detail.plan_draft())
                            {
                                data.body = format!(
                                    "{}\nRevision {} · draft {} · {} bytes\n\n{}",
                                    draft.title,
                                    draft.target_revision,
                                    draft.sequence.map_or_else(
                                        || "submitted".into(),
                                        |sequence| sequence.to_string()
                                    ),
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
                    }
                    PresentedTool::Acp(tool) => data.title = tool.title.clone(),
                }
            }
            PresentationBlockKind::Subtask { descriptor, .. } => {
                data.title = format!(
                    "{} {} · {}",
                    subtask_icon(descriptor.status).glyph(0),
                    descriptor.kind,
                    descriptor.title
                );
                data.subtask = Some(descriptor.id.clone());
                data.body.clear();
            }
            PresentationBlockKind::Image { image, ordinal, .. } => {
                data.title = image.label(*ordinal);
                data.body.clear();
                let format = match image.mime_type() {
                    "image/png" => Some(ImageFormat::Png),
                    "image/jpeg" => Some(ImageFormat::Jpeg),
                    "image/webp" => Some(ImageFormat::Webp),
                    "image/gif" => Some(ImageFormat::Gif),
                    _ => None,
                };
                data.image = format.map(|format| {
                    Arc::new(Image::from_bytes(format, image.encoded_bytes().to_vec()))
                });
            }
            PresentationBlockKind::WebActivity(_) => {
                data.title = "Web activity".into();
                data.plain = true;
            }
            PresentationBlockKind::Error(_) => data.title = "Error".into(),
            _ => {}
        }
        if matches!(&block.kind, PresentationBlockKind::Tool(_)) {
            data.body = bounded(&data.body);
        }
        data
    }
}
pub fn entry(epoch: &str, index: usize, history: &HistoryEntry, revision: u64) -> Vec<BlockData> {
    let key = format!("{epoch}-row-{index}");
    let mut blocks = match history {
        HistoryEntry::Conversation(entry) => entry
            .blocks
            .iter()
            .filter(|b| b.visible(false))
            .map(|b| BlockData::block(epoch, b))
            .collect(),
        HistoryEntry::PlanArtifact(artifact) => vec![BlockData::text(
            key,
            format!("Plan · {}", artifact.version),
            artifact.markdown.clone(),
            false,
        )],
        HistoryEntry::PlanHandoff(handoff, _) => vec![BlockData::text(
            key,
            "Approved Plan handoff".into(),
            handoff.artifact.markdown.clone(),
            false,
        )],
        HistoryEntry::Ensemble(ensemble) => vec![BlockData::text(
            key,
            "Ensemble".into(),
            format!(
                "{}\n\n{}",
                ensemble.command(),
                ensemble
                    .workers
                    .iter()
                    .map(|w| format!("{} · {:?}", w.descriptor.id, w.status))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            true,
        )],
        HistoryEntry::Error(error) => {
            vec![BlockData::text(key, "Notice".into(), error.clone(), true)]
        }
        HistoryEntry::CompactionDivider => vec![BlockData::text(
            key,
            "Context compacted".into(),
            String::new(),
            true,
        )],
    };
    if !matches!(history, HistoryEntry::Conversation(_)) {
        for block in &mut blocks {
            block.revision = revision;
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
    diff: Option<Entity<DiffState>>,
    diff_error: Option<String>,
    diff_task: Option<Task<()>>,
    _text: Subscription,
    _output: Subscription,
    controller: Entity<SessionController>,
}
impl BlockView {
    pub fn new(
        data: BlockData,
        controller: Entity<SessionController>,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = if data.plain {
            plain_html(&data.body)
        } else {
            data.body.clone()
        };
        let text = cx.new(|cx| {
            if data.plain {
                TextViewState::html(&body, cx)
            } else {
                TextViewState::markdown(&body, cx)
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
        Self {
            data,
            text,
            output,
            expanded: false,
            diff: None,
            diff_error: None,
            diff_task: None,
            _text: watch,
            _output: watch_output,
            controller,
        }
    }
    pub fn sync(&mut self, data: BlockData, cx: &mut Context<Self>) {
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
        self.data = data;
        self.diff = None;
        self.diff_error = None;
        self.diff_task = None;
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
}
impl Render for BlockView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = palette(cx);
        let primary = self.data.primary.clone();
        let secondary = self.data.secondary.clone();
        let mut card = div()
            .id(self.data.key.clone())
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md();
        if self.data.collapsible {
            card = card
                .bg(t.surfaces.panel)
                .border_1()
                .border_color(t.surfaces.border);
        }
        let mut header = div()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(t.text.secondary)
            .child(self.data.title.clone());
        if self.data.collapsible {
            header = header.child(
                Button::new("expand")
                    .label(if self.expanded { "Collapse" } else { "Expand" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        if this.expanded {
                            this.prepare_diff(cx);
                        }
                        cx.notify();
                    })),
            );
        }
        header = header
            .child(div().flex_1())
            .child(Button::new("copy").label("Copy").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(primary.clone()))
            }));
        if let Some(output) = secondary {
            header = header.child(Button::new("copy-output").label("Copy output").on_click(
                move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(output.clone())),
            ));
        }
        if let Some(id) = self.data.subtask.clone() {
            let controller = self.controller.clone();
            header = header.child(Button::new("inspect").label("Inspect").on_click(
                move |_, _, cx| {
                    controller.update(cx, |this, cx| {
                        this.model.selected_child = Some(id.clone());
                        cx.notify();
                    })
                },
            ));
        }
        card = card.child(header);
        let reasoning = self.data.title == "Reasoning";
        if !reasoning || self.expanded {
            card = card.child(TextView::new(&self.text).scrollable(false));
        }
        if let Some(image) = &self.data.image {
            card = card.child(img(image.clone()).max_h(px(240.)).max_w(px(480.)));
        }
        if self.expanded {
            if self.data.secondary.is_some() {
                card = card.child(TextView::new(&self.output).scrollable(false));
            }
            if let Some(diff) = &self.diff {
                card = card.child(Diff::new(diff).h(px(280.)).w_full());
            }
            if let Some(error) = &self.diff_error {
                card = card.child(div().text_color(t.feedback.warning).child(error.clone()));
            }
        }
        card
    }
}
