//! Desktop design primitives. Geometry and typography are shared by the log and its next line.
use super::theme::{palette, tones};
use gpui_kit::base::text::TextViewStyle;
use gpui_kit::component::{
    Sizable,
    button::{Button, ButtonCustomVariant, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use std::{sync::Arc, time::Instant};
use zevria_presentation::{shell::ShellToken, status_icon::StatusIcon};
use zevria_theme::ZevriaTheme;

pub const SIDEBAR: f32 = 256.;
pub const TITLE: f32 = 40.;
pub const GUTTER: f32 = 96.;
pub const GAP: f32 = 24.;
pub const CONTENT: f32 = 720.;
pub const PROSE: f32 = 600.;
pub const UI_FONT: &str = ".SystemUIFont";
pub const MONO_FONT: &str = "Menlo";
pub const DOCUMENT_FONT: &str = "Charter";

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
    leading: 22.,
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

#[derive(Clone, Copy, Default)]
pub struct Measure {
    pub compact: bool,
}
impl Measure {
    pub fn column(self) -> Div {
        div()
            .w_full()
            .max_w(px(if self.compact {
                CONTENT
            } else {
                GUTTER + GAP + CONTENT
            }))
            .min_w_0()
    }
    pub fn row(self) -> Div {
        self.column()
            .flex()
            .gap(px(GAP))
            .when(self.compact, |row| row.flex_col().gap(px(4.)))
    }
    pub fn gutter(self) -> Div {
        label()
            .flex_shrink_0()
            .when(!self.compact, |g| g.w(px(GUTTER)).text_right())
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
pub fn pencil() -> Div {
    PENCIL.apply(div()).font_family(DOCUMENT_FONT)
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
/// The line covers the entire block, including its vertical padding.
pub fn thread_segment(dashed: bool, cx: &App) -> Div {
    div()
        .absolute()
        .left(px(-16.))
        .top_0()
        .bottom_0()
        .border_l_1()
        .border_color(tones(cx).thread)
        .when(dashed, |line| line.border_dashed())
}
pub fn thread_node(status: StatusIcon, ink: Hsla, cx: &App) -> Div {
    thread_marker(status.glyph(frame(cx)), glyph_color(status, ink, cx), cx)
}
pub fn thread_marker(glyph: impl Into<SharedString>, ink: Hsla, cx: &App) -> Div {
    div()
        .absolute()
        .left(px(-24.))
        .top(px(8.))
        .w(px(16.))
        .flex()
        .justify_center()
        .bg(palette(cx).surfaces.canvas)
        .child(
            label()
                .text_size(px(14.))
                .text_color(ink)
                .child(glyph.into()),
        )
}
pub fn key_hint(key: &'static str, label: &'static str, cx: &App) -> Div {
    meta()
        .flex()
        .items_center()
        .gap(px(4.))
        .text_color(palette(cx).text.muted)
        .child(gpui_kit::component::kbd::Kbd::new(
            Keystroke::parse(key).expect("static desktop shortcut"),
        ))
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
                .italic()
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
        CODE, HEADINGS, PENCIL, focus_ring, glyph_color, markdown, palette, shell_ink,
        thought_markdown, tones,
    };
    use gpui_kit::{FontStyle, FontWeight, Styled, TestAppContext, px};
    use zevria_presentation::{shell::ShellToken, status_icon::StatusIcon};

    #[gpui_kit::test]
    fn markdown_voices_use_the_type_scale_and_semantic_inks(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::super::theme::apply(zevria_theme::ZEVRIA_GRAPHITE, cx);
            let t = palette(cx);
            let pencil = tones(cx).pencil;
            let answer = markdown(cx, false);
            let thought = thought_markdown(cx);
            assert_eq!(thought.foreground(), pencil);
            assert_eq!(thought.muted_foreground(), pencil);
            assert_eq!(thought.link(), t.content.link);
            for level in 1..=6 {
                let heading = answer.heading(level);
                assert_eq!(heading.text.color, Some(t.content.heading));
                assert_eq!(
                    heading.text.font_size,
                    Some(px(HEADINGS[(level as usize - 1).min(2)].size).into())
                );
                let note = thought.heading(level);
                assert_eq!(note.text.color, Some(pencil));
                assert_eq!(note.text.font_size, Some(px(PENCIL.size).into()));
                assert_eq!(note.text.font_weight, Some(FontWeight::SEMIBOLD));
                assert_eq!(note.text.font_style, Some(FontStyle::Italic));
            }
            for code in [thought.code_block(), thought.inline_code_style()] {
                assert_eq!(code.text.color, Some(pencil));
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
