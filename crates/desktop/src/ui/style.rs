//! Desktop design primitives. Geometry and typography are shared by the log and its next line.
use super::theme::{palette, tones};
use crate::actions::Shortcut;
use gpui_kit::base::text::TextViewStyle;
use gpui_kit::component::{
    Icon, IconName, Sizable,
    button::{Button, ButtonCustomVariant, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use zevria_presentation::{shell::ShellToken, status_icon::StatusIcon};
use zevria_theme::ZevriaTheme;

pub const SIDEBAR: f32 = 256.;
pub const TITLE: f32 = 40.;
pub const COLUMN: f32 = 800.;
pub const COLUMN_INSET: f32 = 16.;
pub const UI_FONT: &str = ".SystemUIFont";
#[cfg(target_os = "macos")]
pub const MONO_FONT: &str = "Menlo";
#[cfg(windows)]
pub const MONO_FONT: &str = "Consolas";
#[cfg(target_os = "macos")]
pub const DOCUMENT_FONT: &str = "Charter";
#[cfg(windows)]
pub const DOCUMENT_FONT: &str = "Georgia";

/// Transcript typography in physical pixels; Kit's 14 px rem base is separate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Type {
    pub size: f32,
    pub leading: f32,
}
impl Type {
    pub fn apply<E: Styled>(self, element: E) -> E {
        element
            .text_size(px(self.size))
            .line_height(px(self.leading))
    }
}
pub const READING: Type = Type {
    size: 14.,
    leading: 23.,
};
pub const DOCUMENT: Type = Type {
    size: 15.,
    leading: 24.,
};
pub const PENCIL: Type = Type {
    size: 13.,
    leading: 21.,
};
pub const CODE: Type = Type {
    size: 12.,
    leading: 18.,
};
pub const SECONDARY: Type = Type {
    size: 13.,
    leading: 20.,
};
pub const HEADINGS: [Type; 3] = [
    Type {
        size: 18.,
        leading: 24.,
    },
    Type {
        size: 16.,
        leading: 22.,
    },
    Type {
        size: 15.,
        leading: 21.,
    },
];
pub const LABEL: Type = Type {
    size: 12.,
    leading: 16.,
};
pub const META: Type = Type {
    size: 11.,
    leading: 14.,
};
pub fn kind_label(kind: zevria_foundation::SubtaskKind) -> &'static str {
    match kind {
        zevria_foundation::SubtaskKind::Explore => "Explore",
        zevria_foundation::SubtaskKind::Build => "Build",
    }
}

/// The transcript, composer, and question dock share exactly one reading axis.
pub struct Measure;
impl Measure {
    pub fn column() -> Div {
        div().w_full().max_w(px(COLUMN)).mx_auto().min_w_0()
    }
}
pub fn body() -> Div {
    READING.apply(div())
}
pub fn label() -> Div {
    LABEL.apply(div()).font_weight(FontWeight::MEDIUM)
}
pub fn meta() -> Div {
    META.apply(div()).font_features(tnum())
}
pub fn mono() -> Div {
    CODE.apply(div()).font_family(MONO_FONT)
}
pub fn pencil(cx: &App) -> Div {
    PENCIL
        .apply(div())
        .font_family(DOCUMENT_FONT)
        .text_color(tones(cx).pencil)
}
pub fn focus_ring(radius: f32, cx: &App) -> Div {
    div()
        .absolute()
        .top(px(-4.))
        .bottom(px(-4.))
        .left(px(-4.))
        .right(px(-4.))
        .border_2()
        .border_color(tones(cx).focus)
        .rounded(px(radius + 4.))
}
pub fn step_icon(icon: gpui_kit::assets::IconName, ink: Hsla) -> gpui_kit::component::Icon {
    gpui_kit::component::Icon::new(icon)
        .size(px(14.))
        .text_color(ink)
}

pub fn step_status(status: StatusIcon, ink: Hsla, cx: &App) -> gpui_kit::component::Icon {
    use gpui_kit::assets::IconName;
    let icon = match status {
        StatusIcon::Running => IconName::LoaderCircle,
        StatusIcon::Done | StatusIcon::Updated => IconName::Check,
        StatusIcon::Failed => IconName::CircleX,
        StatusIcon::Denied | StatusIcon::Interrupted => IconName::TriangleAlert,
        StatusIcon::Pending => IconName::Clock,
        _ => IconName::CircleMinus,
    };
    let icon = step_icon(icon, glyph_color(status, ink, cx));
    if status == StatusIcon::Running {
        icon.rotate(Radians(
            (frame(cx) % 8) as f32 * std::f32::consts::FRAC_PI_4,
        ))
    } else {
        icon
    }
}

pub fn step_badge(text: impl Into<SharedString>, ink: Hsla) -> Div {
    meta()
        .whitespace_nowrap()
        .flex_shrink_0()
        .text_color(ink)
        .child(text.into())
}

pub(super) fn key_hint(shortcut: Shortcut, label: &'static str, cx: &App) -> Div {
    meta()
        .flex()
        .items_center()
        .gap(px(4.))
        .text_color(palette(cx).text.muted)
        .child(gpui_kit::component::kbd::Kbd::new(shortcut.keystroke()))
        .child(label)
}
pub fn document() -> Div {
    DOCUMENT.apply(div()).font_family(DOCUMENT_FONT)
}
pub fn tnum() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}
pub fn kbd_hint(text: impl Into<SharedString>, cx: &App) -> Div {
    meta().text_color(palette(cx).text.muted).child(text.into())
}
pub fn group_label(text: impl Into<SharedString>, cx: &App) -> Div {
    label()
        .text_color(palette(cx).text.muted)
        .px(px(8.))
        .pt(px(16.))
        .pb(px(8.))
        .child(text.into())
}
pub fn tag(text: impl Into<SharedString>, ink: Hsla, cx: &App) -> Div {
    label()
        .text_color(ink)
        .bg(palette(cx).surfaces.panel)
        .px(px(6.))
        .py(px(2.))
        .rounded(px(4.))
        .child(text.into())
}
/// Icon controls always carry both a tooltip and an accessible name.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: gpui_kit::component::IconName,
    name: impl Into<SharedString>,
) -> Button {
    let name = name.into();
    Button::new(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .tooltip(name.clone())
        .accessibility_label(name)
}

/// Duration that a copy button shows its confirmation checkmark.
pub const COPY_FEEDBACK_DURATION: Duration = Duration::from_secs(2);

/// Ephemeral confirmation state for mutually exclusive copy targets.
pub struct CopyFeedback<T = ()> {
    active: Option<T>,
    generation: u64,
    reset: Option<Task<()>>,
}

impl<T> Default for CopyFeedback<T> {
    fn default() -> Self {
        Self {
            active: None,
            generation: 0,
            reset: None,
        }
    }
}

impl<T: Copy + PartialEq + 'static> CopyFeedback<T> {
    pub fn is_active(&self, target: T) -> bool {
        self.active == Some(target)
    }

    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.active = None;
        self.reset = None;
    }

    pub fn trigger<V: 'static>(
        &mut self,
        target: T,
        cx: &mut Context<V>,
        select: impl Fn(&mut V) -> &mut Self + 'static,
    ) {
        self.clear();
        self.active = Some(target);
        let generation = self.generation;
        self.reset = Some(cx.spawn(async move |owner, cx| {
            cx.background_executor().timer(COPY_FEEDBACK_DURATION).await;
            let _ = owner.update(cx, |this, cx| {
                let feedback = select(this);
                // A stale reset must not clear a newer copy or context.
                if feedback.generation == generation {
                    feedback.clear();
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

/// Icon button that keeps the same bounds when showing a success-tinted checkmark.
pub fn copy_button(
    id: impl Into<ElementId>,
    copied: bool,
    idle_label: impl Into<SharedString>,
    copied_label: impl Into<SharedString>,
    cx: &App,
) -> Button {
    let (icon, name) = if copied {
        (
            Icon::new(IconName::Check).text_color(palette(cx).feedback.success),
            copied_label.into(),
        )
    } else {
        (Icon::new(IconName::Copy), idle_label.into())
    };
    Button::new(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .tooltip(name.clone())
        .accessibility_label(name)
}

/// Copy button with state scoped to its enclosing element (e.g. a Markdown code block).
#[derive(IntoElement)]
pub struct KeyedCopyButton {
    id: &'static str,
    text: SharedString,
    idle_label: SharedString,
    copied_label: SharedString,
}

impl KeyedCopyButton {
    pub fn new(
        id: &'static str,
        text: impl Into<SharedString>,
        idle_label: impl Into<SharedString>,
        copied_label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id,
            text: text.into(),
            idle_label: idle_label.into(),
            copied_label: copied_label.into(),
        }
    }
}

impl RenderOnce for KeyedCopyButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // RenderOnce runs within the enclosing element's ID scope. Use a different
        // key from the button, which stores its focus handle under self.id.
        let feedback = window.use_keyed_state(
            SharedString::from(format!("{}-feedback", self.id)),
            cx,
            |_, _| CopyFeedback::default(),
        );
        let copied = feedback.read(cx).is_active(());
        let text = self.text;
        copy_button(self.id, copied, self.idle_label, self.copied_label, cx).on_click(
            move |_, _, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                feedback.update(cx, |this, cx| this.trigger((), cx, |this| this));
            },
        )
    }
}

pub(super) fn inspector_panel(
    id: &'static str,
    width: f32,
    overlay: bool,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(width))
        .h_full()
        .flex_shrink_0()
        .min_h_0()
        .when(overlay, |panel| {
            // Keep the status-bar chips accessible underneath the compact overlay.
            panel
                .absolute()
                .top_0()
                .right_0()
                .bottom(px(super::status::HEIGHT))
                .h_auto()
                .shadow_lg()
                .occlude()
        })
        .flex()
        .flex_col()
        .bg(palette(cx).surfaces.overlay)
        .rounded(px(14.))
        .border_l_1()
        .border_color(tones(cx).hairline)
}

pub(super) fn inspector_header(
    title: &'static str,
    summary: String,
    close: Button,
    cx: &App,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .flex_shrink_0()
        .px(px(20.))
        .py(px(14.))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(18.))
                        .line_height(px(24.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(meta().text_color(palette(cx).text.muted).child(summary)),
        )
        .child(close)
}

pub fn mode_button(mode: zevria_foundation::SessionMode, cx: &App) -> ButtonCustomVariant {
    let t = palette(cx);
    let tones = tones(cx);
    let (ink, interaction) = if mode == zevria_foundation::SessionMode::Build {
        (t.workflow.build, tones.build)
    } else {
        (t.workflow.plan, tones.plan)
    };
    ButtonCustomVariant::new(cx)
        .color(ink)
        .foreground(t.surfaces.canvas)
        .hover(interaction.hover)
        .active(interaction.active)
}
pub fn sheet(cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(16.))
        .p(px(24.))
        .rounded(px(14.))
        .bg(palette(cx).surfaces.overlay)
        .border_1()
        .border_color(tones(cx).hairline)
        .shadow_lg()
}
pub fn scrim(cx: &App) -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(tones(cx).scrim)
        .occlude()
}
pub fn glyph_color(status: StatusIcon, ink: Hsla, cx: &App) -> Hsla {
    let t = palette(cx);
    match status {
        StatusIcon::Running => ink,
        StatusIcon::Done => t.feedback.success,
        StatusIcon::Failed => t.feedback.error,
        StatusIcon::Denied | StatusIcon::Interrupted => t.feedback.warning,
        StatusIcon::Updated => t.feedback.info,
        _ => t.text.muted,
    }
}
pub fn frame(cx: &App) -> usize {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    if cx.reduce_motion() {
        0
    } else {
        (START.get_or_init(Instant::now).elapsed().as_millis() / 250) as usize
    }
}
pub fn status_glyph(status: StatusIcon, ink: Hsla, cx: &App) -> Div {
    label()
        .text_size(px(14.))
        .text_color(glyph_color(status, ink, cx))
        .child(status.glyph(frame(cx)))
}

pub fn markdown(cx: &App, is_document: bool) -> TextViewStyle {
    let t = palette(cx);
    let tone = tones(cx);
    TextViewStyle::default()
        .with_dark(super::theme::mode_is_dark(cx))
        .with_foreground(t.text.primary)
        .with_muted_foreground(t.text.secondary)
        .with_link(t.content.link)
        .with_selection(tone.selection)
        .with_code_background(t.surfaces.panel)
        .with_border(tone.hairline)
        .with_paragraph_gap(rems(12. / 14.))
        .with_heading(move |level| {
            HEADINGS[match level {
                1 => 0,
                2 => 1,
                _ => 2,
            }]
            .apply(StyleRefinement::default())
            .font_family(if is_document { DOCUMENT_FONT } else { UI_FONT })
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.content.heading)
        })
        .with_code_block(
            CODE.apply(StyleRefinement::default())
                .bg(t.surfaces.panel)
                .rounded(px(8.))
                .p(px(12.))
                .font_family(MONO_FONT),
        )
        .with_inline_code_style(
            CODE.apply(StyleRefinement::default())
                .font_family(MONO_FONT)
                .text_color(t.content.code)
                .bg(t.surfaces.overlay)
                .rounded(px(4.))
                .px(px(4.)),
        )
        .with_blockquote_style(
            StyleRefinement::default()
                .border_l_2()
                .border_color(tone.hairline)
                .text_color(t.text.secondary),
        )
        .with_table(
            StyleRefinement::default()
                .border_1()
                .border_color(tone.hairline),
        )
        .with_table_head(
            StyleRefinement::default()
                .bg(t.surfaces.panel)
                .text_color(t.text.secondary),
        )
        .with_table_cell(StyleRefinement::default().border_color(tone.hairline))
}

/// Reasoning is an indented graphite note, never answer-colored Markdown.
pub fn thought_markdown(cx: &App) -> TextViewStyle {
    let t = palette(cx);
    let tone = tones(cx);
    markdown(cx, true)
        .with_foreground(tone.pencil)
        .with_muted_foreground(tone.pencil)
        .with_heading(move |_| {
            PENCIL
                .apply(StyleRefinement::default())
                .font_family(DOCUMENT_FONT)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tone.pencil)
        })
        .with_inline_code_style(
            CODE.apply(StyleRefinement::default())
                .font_family(MONO_FONT)
                .text_color(tone.pencil)
                .bg(t.surfaces.panel)
                .rounded(px(4.))
                .px(px(4.)),
        )
        .with_code_block(
            CODE.apply(StyleRefinement::default())
                .font_family(MONO_FONT)
                .text_color(tone.pencil)
                .bg(t.surfaces.panel)
                .rounded(px(8.))
                .p(px(12.)),
        )
        .with_blockquote_style(
            StyleRefinement::default()
                .border_l_2()
                .border_color(tone.hairline)
                .text_color(tone.pencil),
        )
        .with_table_head(
            StyleRefinement::default()
                .bg(t.surfaces.panel)
                .text_color(tone.pencil),
        )
}

pub fn shell_ink(token: ShellToken, t: &ZevriaTheme<Hsla>) -> Hsla {
    match token {
        ShellToken::Program => t.syntax.function,
        ShellToken::Flag | ShellToken::Number | ShellToken::Variable => t.syntax.constant,
        ShellToken::String => t.syntax.string,
        ShellToken::Operator | ShellToken::Keyword => t.syntax.keyword,
        ShellToken::Comment | ShellToken::Routing => t.syntax.comment,
        ShellToken::Assignment => t.text.secondary,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CODE, DOCUMENT_FONT, HEADINGS, PENCIL, focus_ring, glyph_color, markdown, palette, pencil,
        shell_ink, thought_markdown, tones,
    };
    use gpui_kit::{FontWeight, Styled, TestAppContext, px};
    use zevria_presentation::{shell::ShellToken, status_icon::StatusIcon};

    #[gpui_kit::test]
    fn markdown_voices_use_the_type_scale_and_semantic_inks(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::super::theme::apply(zevria_theme::ZEVRIA_GRAPHITE, cx);
            let t = palette(cx);
            let pencil_ink = tones(cx).pencil;
            let mut preview = pencil(cx);
            let preview = &preview.style().text;
            assert_eq!(preview.color, Some(pencil_ink));
            assert_eq!(preview.font_family, Some(DOCUMENT_FONT.into()));
            assert_eq!(preview.font_size, Some(px(PENCIL.size).into()));
            assert_eq!(preview.line_height, Some(px(PENCIL.leading).into()));
            assert_eq!(preview.font_style, None);

            let answer = markdown(cx, false);
            let thought = thought_markdown(cx);
            assert_eq!(answer.foreground(), t.text.primary);
            assert_eq!(thought.foreground(), pencil_ink);
            assert_eq!(thought.muted_foreground(), pencil_ink);
            assert_eq!(thought.link(), t.content.link);
            for level in 1..=6 {
                let heading = answer.heading(level);
                assert_eq!(heading.text.color, Some(t.content.heading));
                assert_eq!(
                    heading.text.font_size,
                    Some(px(HEADINGS[(level as usize - 1).min(2)].size).into())
                );
                let note = thought.heading(level);
                assert_eq!(note.text.color, Some(pencil_ink));
                assert_eq!(note.text.font_size, Some(px(PENCIL.size).into()));
                assert_eq!(note.text.line_height, Some(px(PENCIL.leading).into()));
                assert_eq!(note.text.font_weight, Some(FontWeight::SEMIBOLD));
                assert_eq!(note.text.font_style, None);
            }
            for code in [thought.code_block(), thought.inline_code_style()] {
                assert_eq!(code.text.color, Some(pencil_ink));
                assert_eq!(code.text.font_size, Some(px(CODE.size).into()));
                assert_eq!(code.background, Some(t.surfaces.panel.into()));
            }
            assert_eq!(
                glyph_color(StatusIcon::Done, t.text.primary, cx),
                t.feedback.success
            );
            assert_eq!(shell_ink(ShellToken::Program, &t), t.syntax.function);
            assert_eq!(shell_ink(ShellToken::Routing, &t), t.syntax.comment);
            let mut ring = focus_ring(6., cx);
            assert_eq!(ring.style().corner_radii.top_left, Some(px(10.).into()));
        });
    }
}
