//! The only desktop color boundary: shared tokens plus once-computed OKLab tones.
use gpui_kit::component::{Colorize, Theme, ThemeMode};
use gpui_kit::{App, Hsla, px, rgb};
use zevria_theme::{Rgb, ZevriaTheme};

#[derive(Clone, Copy, Debug)]
pub struct Interaction {
    pub hover: Hsla,
    pub active: Hsla,
}
#[derive(Clone, Copy, Debug)]
pub struct Tones {
    pub hairline: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub build_tint: Hsla,
    pub plan_tint: Hsla,
    pub list_active: Hsla,
    pub selection: Hsla,
    pub scrim: Hsla,
    pub drop_target: Hsla,
    pub build: Interaction,
    pub danger: Interaction,
    pub success: Interaction,
    pub warning: Interaction,
    pub info: Interaction,
    pub link_hover: Hsla,
}
impl Tones {
    fn new(t: ZevriaTheme<Hsla>) -> Self {
        let interaction = |ink: Hsla| Interaction {
            hover: ink.mix_oklab(t.text.primary, 0.85),
            active: ink.mix_oklab(t.surfaces.canvas, 0.88),
        };
        Self {
            hairline: t.surfaces.border.mix_oklab(t.surfaces.canvas, 0.55),
            hover: t.surfaces.overlay.mix_oklab(t.surfaces.border, 0.65),
            pressed: t.surfaces.overlay.mix_oklab(t.surfaces.border, 0.45),
            build_tint: t.surfaces.panel.mix_oklab(t.workflow.build, 0.86),
            plan_tint: t.surfaces.panel.mix_oklab(t.workflow.plan, 0.86),
            list_active: t.surfaces.selection_background.alpha(0.14),
            selection: t.surfaces.selection_background.alpha(0.40),
            scrim: t.surfaces.canvas.alpha(0.72),
            drop_target: t.workflow.plan.alpha(0.12),
            build: interaction(t.workflow.build),
            danger: interaction(t.feedback.error),
            success: interaction(t.feedback.success),
            warning: interaction(t.feedback.warning),
            info: interaction(t.feedback.info),
            link_hover: t.content.link.mix_oklab(t.text.primary, 0.8),
        }
    }
}
#[derive(Clone, Copy)]
pub struct Palette {
    pub tokens: ZevriaTheme<Hsla>,
    pub tones: Tones,
}
impl gpui_kit::Global for Palette {}
pub fn color(Rgb(r, g, b): Rgb) -> Hsla {
    rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
}
pub fn prepare(config: &zevria_host::Config) -> anyhow::Result<ZevriaTheme> {
    if let Some(selector) = config.theme() {
        let definition = zevria_host::theme_store::ThemeStore::global()?.load(&selector.name)?;
        zevria_theme::install_theme(&definition)?;
        return Ok(*zevria_theme::theme());
    }
    Ok(zevria_theme::ZEVRIA_GRAPHITE)
}

fn luminance(color: Hsla) -> f32 {
    let c = color.to_rgb();
    let linear = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
}
pub fn mode(tokens: ZevriaTheme) -> ThemeMode {
    if luminance(color(tokens.surfaces.canvas)) > luminance(color(tokens.text.primary)) {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    }
}
pub fn apply(tokens: ZevriaTheme, cx: &mut App) {
    let t = tokens.map(color);
    let tones = Tones::new(t);
    Theme::change(mode(tokens), None, cx);
    Theme::update(cx, |theme| {
        let c = &mut theme.colors;
        c.background = t.surfaces.canvas;
        c.foreground = t.text.primary;
        c.border = tones.hairline;
        c.button = t.surfaces.panel;
        c.button_foreground = t.text.primary;
        c.button_hover = tones.hover;
        c.button_active = tones.pressed;
        c.input = t.surfaces.border;
        c.primary = t.workflow.build;
        c.primary_foreground = t.surfaces.canvas;
        c.accent = tones.hover;
        c.accent_foreground = t.text.primary;
        c.secondary = t.surfaces.panel;
        c.secondary_foreground = t.text.primary;
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
        c.selection = tones.selection;
        c.sidebar = t.surfaces.panel;
        c.sidebar_foreground = t.text.primary;
        c.scrollbar = t.surfaces.canvas;
        c.scrollbar_thumb = t.surfaces.border;
        c.scrollbar_thumb_hover = t.surfaces.border_strong;
        // Assign every Kit surface/interaction slot. Theme::change alone leaves
        // neutral defaults (including saturated accent chips) behind.
        macro_rules! slots {
            ($value:expr; $($slot:ident),+ $(,)?) => { $(c.$slot = $value;)+ };
        }
        slots!(t.surfaces.canvas; list, table, list_even, table_even, status_bar, tab_active);
        slots!(t.surfaces.panel; tab, tab_bar, tab_bar_segmented, list_head, table_head,
            table_foot, description_list_label, accordion, group_box, skeleton, title_bar,
            button_secondary);
        slots!(t.text.primary; sidebar_accent_foreground, group_box_foreground,
            tab_active_foreground, caret, button_secondary_foreground);
        slots!(t.text.secondary; tab_foreground, table_head_foreground, table_foot_foreground,
            description_list_label_foreground);
        slots!(tones.hairline; sidebar_border, title_bar_border, status_bar_border,
            window_border, table_row_border, chart_grid);
        slots!(t.surfaces.selection_background; list_active_border, table_active_border);
        slots!(tones.hover; list_hover, table_hover, sidebar_accent, secondary_hover,
            button_secondary_hover);
        slots!(tones.pressed; secondary_active, button_secondary_active);
        slots!(tones.list_active; list_active, table_active);
        c.drag_border = t.workflow.plan;
        c.drop_target = tones.drop_target;
        c.overlay = tones.scrim;
        slots!(t.workflow.build; button_primary, sidebar_primary, progress_bar, slider_bar);
        slots!(tones.build.hover; primary_hover, button_primary_hover);
        slots!(tones.build.active; primary_active, button_primary_active);
        slots!(t.surfaces.canvas; button_primary_foreground, sidebar_primary_foreground,
            button_danger_foreground, success_foreground, button_success_foreground,
            warning_foreground, button_warning_foreground, info_foreground, button_info_foreground);
        c.switch = t.surfaces.border;
        slots!(t.text.primary; switch_thumb, slider_thumb);
        c.link_hover = tones.link_hover;
        c.link_active = t.content.link;
        c.button_danger = t.feedback.error;
        slots!(tones.danger.hover; danger_hover, button_danger_hover);
        slots!(tones.danger.active; danger_active, button_danger_active);
        c.button_success = t.feedback.success;
        slots!(tones.success.hover; success_hover, button_success_hover);
        slots!(tones.success.active; success_active, button_success_active);
        c.button_warning = t.feedback.warning;
        slots!(tones.warning.hover; warning_hover, button_warning_hover);
        slots!(tones.warning.active; warning_active, button_warning_active);
        c.button_info = t.feedback.info;
        slots!(tones.info.hover; info_hover, button_info_hover);
        slots!(tones.info.active; info_active, button_info_active);
        slots!(t.feedback.error; red, red_light, chart_bearish);
        slots!(t.feedback.success; green, green_light, chart_bullish);
        slots!(t.workflow.plan; blue, blue_light);
        slots!(t.workflow.review; yellow, yellow_light);
        slots!(t.roles.assistant; magenta, magenta_light);
        slots!(t.roles.tools; cyan, cyan_light);
        c.chart_1 = t.workflow.build;
        c.chart_2 = t.workflow.plan;
        c.chart_3 = t.workflow.review;
        c.chart_4 = t.workflow.explore;
        c.chart_5 = t.roles.tools;
        theme.font_family = ".SystemUIFont".into();
        theme.font_size = px(14.);
        theme.mono_font_family = "Menlo".into();
        theme.mono_font_size = px(13.);
        theme.radius = px(6.);
        theme.radius_lg = px(10.);
        theme.shadow = true;
        theme.focus_ring = true;
        let highlight = std::sync::Arc::make_mut(&mut theme.highlight_theme);
        highlight.name = "Zevria".into();
        highlight.style.editor_background = Some(t.surfaces.panel);
        highlight.style.editor_foreground = Some(t.text.primary);
        highlight.style.editor_line_number = Some(t.text.muted);
        highlight.style.editor_gutter_background = Some(t.surfaces.panel);
        highlight.style.editor_active_line = Some(tones.hover);
        highlight.style.editor_active_line_number = Some(t.text.secondary);
        // Kit 0.9's syntect-only StatusColors is an empty compatibility stub.
        // Keep the portable mapping ready for implementations that support it;
        // Diff actually consumes colors.success/danger, assigned above. Do not
        // enable tree-sitter (and its production dependencies) just for this.
        let mut status = serde_json::Map::new();
        for (name, ink) in [
            ("error", t.feedback.error),
            ("warning", t.feedback.warning),
            ("info", t.feedback.info),
            ("success", t.feedback.success),
            ("hint", t.text.muted),
        ] {
            status.insert(name.into(), serde_json::json!(ink));
            status.insert(format!("{name}.border"), serde_json::json!(ink));
            status.insert(
                format!("{name}.background"),
                serde_json::json!(t.surfaces.canvas.blend(ink.alpha(0.12))),
            );
        }
        status.insert("created".into(), serde_json::json!(t.feedback.success));
        status.insert("deleted".into(), serde_json::json!(t.feedback.error));
        status.insert("modified".into(), serde_json::json!(t.feedback.warning));
        highlight.style.status =
            serde_json::from_value(status.into()).expect("mapped highlight status");
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
        syntax.title = Some(syntax_style(t.text.primary));
        syntax.emphasis = Some(syntax_style(t.text.primary));
        syntax.emphasis_strong = Some(syntax_style(t.text.primary));
        syntax.link_text = Some(syntax_style(t.content.link));
        syntax.link_uri = Some(syntax_style(t.content.link));
    });
    cx.set_global(Palette { tokens: t, tones });
}
pub fn palette(cx: &App) -> ZevriaTheme<Hsla> {
    cx.global::<Palette>().tokens
}
pub fn tones(cx: &App) -> Tones {
    cx.global::<Palette>().tones
}
pub fn mode_is_dark(cx: &App) -> bool {
    Theme::global(cx).mode == ThemeMode::Dark
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

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui_kit::test]
    fn visible_kit_slots_have_no_default_leaks(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            apply(zevria_theme::ZEVRIA_GRAPHITE, cx);
            let t = palette(cx);
            let s = tones(cx);
            let theme = Theme::global(cx);
            let c = theme.colors;
            for (actual, expected) in [
                (c.button_primary, t.workflow.build),
                (c.button_primary_hover, s.build.hover),
                (c.list_hover, s.hover),
                (c.list_active, s.list_active),
                (c.sidebar_accent, s.hover),
                (c.title_bar, t.surfaces.panel),
                (c.switch, t.surfaces.border),
                (c.overlay, s.scrim),
                (c.table_head, t.surfaces.panel),
                (c.accent, s.hover),
                (c.selection, s.selection),
                // The native Diff renderer uses these, not the syntect stub.
                (c.success, t.feedback.success),
                (c.danger, t.feedback.error),
                (c.warning, t.feedback.warning),
            ] {
                assert_eq!(actual, expected);
            }
            assert_eq!(theme.font_size, px(14.));
            // GPUI's fake test text system only offers .SystemUIFont; Kit
            // legitimately resolves Menlo to that fallback in unit tests.
            assert_eq!(theme.mono_font_size, px(13.));
            assert_eq!(theme.mode, ThemeMode::Dark);
        });
    }
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
    #[test]
    fn text_tiers_meet_aa_on_derived_tones() {
        let t = zevria_theme::ZEVRIA_GRAPHITE.map(color);
        let s = Tones::new(t);
        for background in [
            t.surfaces.canvas,
            t.surfaces.panel,
            t.surfaces.overlay,
            s.hover,
            s.pressed,
            s.build_tint,
            s.plan_tint,
        ] {
            assert!(contrast(t.text.primary, background) >= 4.5);
            assert!(contrast(t.text.secondary, background) >= 4.5);
        }
        assert!(contrast(t.text.muted, s.hover) >= 4.5);
        // Active rows on Lift deliberately promote metadata to secondary.
        for surface in [t.surfaces.canvas, t.surfaces.panel, t.surfaces.overlay] {
            assert!(contrast(t.text.secondary, surface.blend(s.list_active)) >= 4.5);
        }
        for ink in [t.workflow.build, t.workflow.plan, t.feedback.error] {
            assert!(contrast(t.surfaces.canvas, ink) >= 4.5);
        }
    }
    #[test]
    fn generated_light_palette_selects_light_mode() {
        let definition = zevria_theme::generate_theme("#F5EEDF".parse().unwrap()).unwrap();
        assert_eq!(mode(definition.palette.tokens()), ThemeMode::Light);
        assert_eq!(mode(zevria_theme::ZEVRIA_GRAPHITE), ThemeMode::Dark);
    }
}
