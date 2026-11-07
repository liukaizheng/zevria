//! Exact shared sRGB tokens mapped to GPUI; no palette re-derivation.
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, rgb};
use zevria_theme::{Rgb, ZevriaTheme};
#[derive(Clone, Copy)]
pub struct Palette(pub ZevriaTheme<Hsla>);
impl gpui_kit::Global for Palette {}
pub fn color(Rgb(r, g, b): Rgb) -> Hsla {
    rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
}
pub fn prepare(config: &zevria_host::Config) -> anyhow::Result<ZevriaTheme> {
    if let Some(selector) = config.theme() {
        let definition = zevria_host::theme_store::ThemeStore::global()?.load(&selector.name)?;
        zevria_theme::install_theme(&definition)?;
    }
    Ok(*zevria_theme::theme())
}
pub fn apply(tokens: ZevriaTheme, cx: &mut App) {
    let t = tokens.map(color);
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| {
        let c = &mut theme.colors;
        c.background = t.surfaces.canvas;
        c.foreground = t.text.primary;
        c.border = t.surfaces.border;
        c.button = t.surfaces.panel;
        c.button_foreground = t.text.primary;
        c.button_hover = t.surfaces.overlay;
        c.button_active = t.surfaces.overlay;
        c.input = t.surfaces.border_strong;
        c.primary = t.workflow.build;
        c.primary_foreground = t.surfaces.canvas;
        c.accent = t.surfaces.selection_background;
        c.accent_foreground = t.surfaces.selection_foreground;
        c.secondary = t.surfaces.panel;
        c.secondary_foreground = t.text.secondary;
        c.muted = t.surfaces.panel;
        c.muted_foreground = t.text.muted;
        c.popover = t.surfaces.overlay;
        c.popover_foreground = t.text.primary;
        c.danger = t.feedback.error;
        c.danger_foreground = t.surfaces.canvas;
        c.success = t.feedback.success;
        c.warning = t.feedback.warning;
        c.info = t.feedback.info;
        c.link = t.content.link;
        c.ring = t.surfaces.border_strong;
        c.selection = t.surfaces.selection_background;
        c.sidebar = t.surfaces.panel;
        c.sidebar_foreground = t.text.primary;
        c.scrollbar = t.surfaces.canvas;
        c.scrollbar_thumb = t.surfaces.border;
        c.scrollbar_thumb_hover = t.surfaces.border_strong;
        let highlight = std::sync::Arc::make_mut(&mut theme.highlight_theme);
        highlight.name = "Zevria".into();
        highlight.style.editor_background = Some(t.surfaces.panel);
        highlight.style.editor_foreground = Some(t.text.primary);
        highlight.style.editor_line_number = Some(t.text.muted);
        highlight.style.editor_gutter_background = Some(t.surfaces.panel);
        let syntax = &mut highlight.style.syntax;
        syntax.keyword = Some(syntax_style(t.syntax.keyword));
        syntax.operator = Some(syntax_style(t.syntax.keyword));
        syntax.preproc = Some(syntax_style(t.syntax.keyword));
        syntax.tag = Some(syntax_style(t.syntax.keyword));
        syntax.tag_doctype = Some(syntax_style(t.syntax.keyword));
        syntax.function = Some(syntax_style(t.syntax.function));
        syntax.constructor = Some(syntax_style(t.syntax.function));
        syntax.string = Some(syntax_style(t.syntax.string));
        syntax.string_escape = Some(syntax_style(t.syntax.string));
        syntax.string_regex = Some(syntax_style(t.syntax.string));
        syntax.string_special = Some(syntax_style(t.syntax.string));
        syntax.string_special_symbol = Some(syntax_style(t.syntax.string));
        syntax.text_code_span = Some(syntax_style(t.syntax.string));
        syntax.constant = Some(syntax_style(t.syntax.constant));
        syntax.number = Some(syntax_style(t.syntax.constant));
        syntax.boolean = Some(syntax_style(t.syntax.constant));
        syntax.enum_ = Some(syntax_style(t.syntax.constant));
        syntax.variant = Some(syntax_style(t.syntax.constant));
        syntax.comment = Some(syntax_style(t.syntax.comment));
        syntax.comment_doc = Some(syntax_style(t.syntax.comment));
        syntax.hint = Some(syntax_style(t.syntax.comment));
        syntax.predictive = Some(syntax_style(t.syntax.comment));
        syntax.type_ = Some(syntax_style(t.syntax.r#type));
        syntax.primary = Some(syntax_style(t.text.primary));
        syntax.variable = Some(syntax_style(t.text.primary));
        syntax.variable_special = Some(syntax_style(t.text.primary));
        syntax.property = Some(syntax_style(t.text.primary));
        syntax.attribute = Some(syntax_style(t.text.primary));
        syntax.label = Some(syntax_style(t.text.primary));
        syntax.embedded = Some(syntax_style(t.text.primary));
        syntax.punctuation = Some(syntax_style(t.text.primary));
        syntax.punctuation_bracket = Some(syntax_style(t.text.primary));
        syntax.punctuation_delimiter = Some(syntax_style(t.text.primary));
        syntax.punctuation_list_marker = Some(syntax_style(t.text.primary));
        syntax.punctuation_special = Some(syntax_style(t.text.primary));
        syntax.text_literal = Some(syntax_style(t.text.primary));
        syntax.title = Some(syntax_style(t.content.heading));
        syntax.emphasis = Some(syntax_style(t.content.heading));
        syntax.emphasis_strong = Some(syntax_style(t.content.heading));
        syntax.link_text = Some(syntax_style(t.content.link));
        syntax.link_uri = Some(syntax_style(t.content.link));
    });
    cx.set_global(Palette(t));
}
pub fn palette(cx: &App) -> ZevriaTheme<Hsla> {
    cx.global::<Palette>().0
}
pub fn install(config: &zevria_host::Config, cx: &mut App) -> anyhow::Result<()> {
    apply(prepare(config)?, cx);
    Ok(())
}

fn syntax_style(color: Hsla) -> gpui_kit::component::highlighter::ThemeStyle {
    // Kit exposes the portable theme document as the construction boundary.
    serde_json::from_value(serde_json::json!({"color": color}))
        .expect("opaque mapped sRGB is a valid syntax style")
}
