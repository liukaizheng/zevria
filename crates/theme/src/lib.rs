//! Startup-only semantic color graph with the unchanged Zevria Dark fallback.
//!
//! The reference palette was authored in OKLCH, then chroma-reduced to the
//! sRGB gamut while holding perceptual lightness and hue. The terminal only
//! receives the resulting 24-bit RGB values: there is no runtime conversion,
//! terminal-default color, or ANSI fallback.

pub use definition::{HexRgb, ReferencePalette, ThemeDefinition};
pub use generation::generate_theme;
use ratatui::style::Color;
use std::sync::OnceLock;
pub use validation::{ValidationError, ValidationFailure, validate_theme};

pub mod definition;
pub mod generation;
pub mod validation;

static THEME: OnceLock<ZevriaTheme> = OnceLock::new();

/// First use freezes the built-in fallback as well, preventing stale caches.
pub fn theme() -> &'static ZevriaTheme {
    THEME.get_or_init(|| ZEVRIA_DARK)
}

/// Color conversion boundary for adapters such as syntax highlighters. Components
/// select semantic roles; only this crate constructs or destructures colors.
pub const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

pub fn rgb_channels(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(red, green, blue) => (red, green, blue),
        _ => unreachable!("theme colors are always 24-bit RGB"),
    }
}

/// Must run before the first frame or syntax highlight. No hot reload.
pub fn install_theme(definition: &definition::ThemeDefinition) -> anyhow::Result<()> {
    validation::validate_theme(definition)?;
    THEME.set(definition.palette.tokens()).map_err(|_| {
        anyhow::anyhow!(
            "theme already initialized; installation must occur before rendering or highlighting"
        )
    })
}

// Each comment records the authored OKLCH source followed by its fixed,
// gamut-mapped sRGB output. The canvas is anchored to Ghostty's #282C34.
const REF_CANVAS: HexRgb = HexRgb(0x28, 0x2c, 0x34); // oklch(29.252% 0.0157 264.3) -> #282C34
const REF_PANEL: HexRgb = HexRgb(0x30, 0x34, 0x3d); // oklch(32.482% 0.0169 266.4) -> #30343D
const REF_OVERLAY: HexRgb = HexRgb(0x33, 0x38, 0x42); // oklch(34.0% 0.019 264.3) -> #333842
const REF_BORDER: HexRgb = HexRgb(0x59, 0x61, 0x6f); // oklch(49.1% 0.025 262) -> #59616F
const REF_BORDER_STRONG: HexRgb = HexRgb(0x8c, 0x95, 0xa8); // oklch(66.9% 0.030 265) -> #8C95A8
const REF_SELECTION: HexRgb = HexRgb(0x73, 0x9b, 0xc4); // oklch(67.6% 0.075 249.6) -> #739BC4
const REF_TEXT_PRIMARY: HexRgb = HexRgb(0xe5, 0xec, 0xf5); // oklch(94.0% 0.014 255) -> #E5ECF5
const REF_TEXT_SECONDARY: HexRgb = HexRgb(0xbc, 0xc5, 0xd1); // oklch(82.0% 0.020 256) -> #BCC5D1
const REF_TEXT_MUTED: HexRgb = HexRgb(0x9e, 0xa9, 0xb7); // oklch(73.0% 0.024 254) -> #9EA9B7

// Workflow accents occupy deliberately separated OKLCH hue regions.
const REF_BUILD: HexRgb = HexRgb(0x52, 0xd8, 0xb9); // oklch(80.0% 0.125 175) -> #52D8B9
const REF_PLAN: HexRgb = HexRgb(0x5d, 0xa6, 0xef); // oklch(71.0% 0.130 250) -> #5DA6EF
const REF_REVIEW: HexRgb = HexRgb(0xf4, 0xc2, 0x6a); // oklch(84.0% 0.120 80) -> #F4C26A
const REF_EXPLORE: HexRgb = HexRgb(0xde, 0xce, 0xff); // oklch(88.0% 0.070 300) -> #DECEFF

// Speaker/action accents share lightness while retaining distinct hue cues.
const REF_YOU: HexRgb = HexRgb(0x7e, 0xbd, 0xab); // oklch(75.0% 0.070 175) -> #7EBDAB
const REF_ASSISTANT: HexRgb = HexRgb(0xb9, 0xa2, 0xd5); // oklch(75.0% 0.076 305) -> #B9A2D5
const REF_SYSTEM: HexRgb = HexRgb(0xc7, 0xa9, 0x77); // oklch(75.0% 0.075 80) -> #C7A977
const REF_TOOLS: HexRgb = HexRgb(0x5f, 0xc9, 0xdb); // oklch(78.0% 0.100 210) -> #5FC9DB

// Feedback colors retain luminance and hue separation under full-severity CVD.
const REF_SUCCESS: HexRgb = HexRgb(0x9c, 0xd8, 0x79); // oklch(82.0% 0.140 135) -> #9CD879
const REF_WARNING: HexRgb = HexRgb(0xff, 0xde, 0xba); // oklch(92.0% 0.060 70) -> #FFDEBA
const REF_ERROR: HexRgb = HexRgb(0xf9, 0x7a, 0x88); // oklch(73.0% 0.155 15) -> #F97A88
const REF_INFO: HexRgb = HexRgb(0x49, 0xc8, 0xf3); // oklch(78.0% 0.125 225) -> #49C8F3

// Diff backgrounds retain the success/error hues at canvas-scale lightness.
const REF_DIFF_ADDITION_BACKGROUND: HexRgb = HexRgb(0x2b, 0x3b, 0x23); // oklch(33.0% 0.045 135) -> #2B3B23
const REF_DIFF_DELETION_BACKGROUND: HexRgb = HexRgb(0x4a, 0x2c, 0x2e); // oklch(33.0% 0.045 15) -> #4A2C2E

// Syntax accents are independently gamut-mapped from the Zevria code ramp.
const REF_SYNTAX_KEYWORD: HexRgb = HexRgb(0xc7, 0xa6, 0xee); // oklch(78.0% 0.106 305) -> #C7A6EE
const REF_SYNTAX_FUNCTION: HexRgb = HexRgb(0x6e, 0xc9, 0xe5); // oklch(79.0% 0.095 220) -> #6EC9E5
const REF_SYNTAX_STRING: HexRgb = HexRgb(0x92, 0xcc, 0x93); // oklch(79.0% 0.100 145) -> #92CC93
const REF_SYNTAX_CONSTANT: HexRgb = HexRgb(0xe1, 0xb6, 0x6c); // oklch(80.0% 0.105 80) -> #E1B66C

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZevriaTheme {
    pub surfaces: SurfaceTokens,
    pub text: TextTokens,
    pub workflow: WorkflowTokens,
    pub roles: RoleTokens,
    pub feedback: FeedbackTokens,
    pub content: ContentTokens,
    pub syntax: SyntaxTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceTokens {
    pub canvas: Color,
    pub panel: Color,
    pub overlay: Color,
    pub border: Color,
    pub border_strong: Color,
    pub selection_background: Color,
    pub selection_foreground: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextTokens {
    pub primary: Color,
    pub secondary: Color,
    pub muted: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkflowTokens {
    pub build: Color,
    pub plan: Color,
    pub review: Color,
    pub explore: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoleTokens {
    pub you: Color,
    pub assistant: Color,
    pub system: Color,
    pub tools: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackTokens {
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentTokens {
    pub heading: Color,
    pub strong: Color,
    pub code: Color,
    pub link: Color,
    pub diff_addition: Color,
    pub diff_addition_background: Color,
    pub diff_deletion: Color,
    pub diff_deletion_background: Color,
    pub diff_hunk: Color,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxTokens {
    pub keyword: Color,
    pub function: Color,
    pub r#type: Color,
    pub string: Color,
    pub constant: Color,
    pub comment: Color,
    pub invalid: Color,
}

/// The exact built-in fallback. Components consume `theme()` semantic roles
/// rather than terminal defaults or raw reference colors.
pub const ZEVRIA_DARK: ZevriaTheme = ReferencePalette {
    canvas: REF_CANVAS,
    panel: REF_PANEL,
    overlay: REF_OVERLAY,
    border: REF_BORDER,
    border_strong: REF_BORDER_STRONG,
    selection: REF_SELECTION,
    text_primary: REF_TEXT_PRIMARY,
    text_secondary: REF_TEXT_SECONDARY,
    text_muted: REF_TEXT_MUTED,
    build: REF_BUILD,
    plan: REF_PLAN,
    review: REF_REVIEW,
    explore: REF_EXPLORE,
    you: REF_YOU,
    assistant: REF_ASSISTANT,
    system: REF_SYSTEM,
    tools: REF_TOOLS,
    success: REF_SUCCESS,
    warning: REF_WARNING,
    error: REF_ERROR,
    info: REF_INFO,
    diff_addition_background: REF_DIFF_ADDITION_BACKGROUND,
    diff_deletion_background: REF_DIFF_DELETION_BACKGROUND,
    syntax_keyword: REF_SYNTAX_KEYWORD,
    syntax_function: REF_SYNTAX_FUNCTION,
    syntax_string: REF_SYNTAX_STRING,
    syntax_constant: REF_SYNTAX_CONSTANT,
}
.tokens();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reference_reaches_the_terminal_as_truecolor() {
        for (name, color) in all_tokens() {
            assert!(
                matches!(color, Color::Rgb(_, _, _)),
                "{name} must be a 24-bit RGB token, got {color:?}"
            );
        }
    }

    #[test]
    fn root_canvas_matches_ghostty_default() {
        assert_eq!(ZEVRIA_DARK.surfaces.canvas, Color::Rgb(0x28, 0x2c, 0x34));
    }

    #[test]
    fn normal_and_semantic_foregrounds_meet_wcag_aa_on_every_surface() {
        let surfaces = [
            ("canvas", ZEVRIA_DARK.surfaces.canvas),
            ("panel", ZEVRIA_DARK.surfaces.panel),
            ("overlay", ZEVRIA_DARK.surfaces.overlay),
        ];
        let text = [
            ("primary", ZEVRIA_DARK.text.primary),
            ("secondary", ZEVRIA_DARK.text.secondary),
            ("muted", ZEVRIA_DARK.text.muted),
        ];
        for (foreground_name, foreground) in text {
            for &(surface_name, background) in &surfaces {
                assert_contrast(foreground_name, foreground, surface_name, background, 4.5);
            }
        }

        for (name, foreground) in semantic_foregrounds() {
            for &(surface_name, background) in &surfaces {
                assert_contrast(name, foreground, surface_name, background, 4.5);
            }
        }
        assert_contrast(
            "selection foreground",
            ZEVRIA_DARK.surfaces.selection_foreground,
            "selection background",
            ZEVRIA_DARK.surfaces.selection_background,
            4.5,
        );
    }

    #[test]
    fn semantic_aliases_and_selection_polarity_remain_stable() {
        assert_eq!(ZEVRIA_DARK.content.heading, ZEVRIA_DARK.workflow.explore);
        assert_eq!(ZEVRIA_DARK.content.strong, ZEVRIA_DARK.workflow.plan);
        assert_eq!(ZEVRIA_DARK.syntax.r#type, ZEVRIA_DARK.workflow.plan);
        assert_eq!(ZEVRIA_DARK.content.code, ZEVRIA_DARK.syntax.string);
        assert_eq!(ZEVRIA_DARK.content.link, ZEVRIA_DARK.feedback.info);
        assert_eq!(ZEVRIA_DARK.content.diff_hunk, ZEVRIA_DARK.feedback.info);
        assert_eq!(
            ZEVRIA_DARK.content.diff_addition,
            ZEVRIA_DARK.feedback.success
        );
        assert_eq!(
            ZEVRIA_DARK.content.diff_deletion,
            ZEVRIA_DARK.feedback.error
        );
        assert_eq!(ZEVRIA_DARK.syntax.invalid, ZEVRIA_DARK.feedback.error);
        assert_eq!(ZEVRIA_DARK.syntax.comment, ZEVRIA_DARK.text.muted);
        assert_eq!(
            ZEVRIA_DARK.surfaces.selection_foreground,
            ZEVRIA_DARK.surfaces.canvas
        );
    }

    #[test]
    fn diff_row_foregrounds_meet_wcag_aa_on_diff_backgrounds() {
        let backgrounds = [
            (
                "diff addition background",
                ZEVRIA_DARK.content.diff_addition_background,
            ),
            (
                "diff deletion background",
                ZEVRIA_DARK.content.diff_deletion_background,
            ),
        ];
        let foregrounds = [
            ("primary", ZEVRIA_DARK.text.primary),
            ("secondary", ZEVRIA_DARK.text.secondary),
            ("muted", ZEVRIA_DARK.text.muted),
            ("diff addition", ZEVRIA_DARK.content.diff_addition),
            ("diff deletion", ZEVRIA_DARK.content.diff_deletion),
            ("keyword", ZEVRIA_DARK.syntax.keyword),
            ("function", ZEVRIA_DARK.syntax.function),
            ("type", ZEVRIA_DARK.syntax.r#type),
            ("string", ZEVRIA_DARK.syntax.string),
            ("constant", ZEVRIA_DARK.syntax.constant),
            ("comment", ZEVRIA_DARK.syntax.comment),
            ("invalid", ZEVRIA_DARK.syntax.invalid),
        ];
        for (background_name, background) in backgrounds {
            for (foreground_name, foreground) in foregrounds {
                assert_contrast(
                    foreground_name,
                    foreground,
                    background_name,
                    background,
                    4.5,
                );
            }
        }
    }

    #[test]
    fn strong_boundaries_and_focus_indicators_meet_three_to_one() {
        for (surface_name, surface) in [
            ("canvas", ZEVRIA_DARK.surfaces.canvas),
            ("panel", ZEVRIA_DARK.surfaces.panel),
            ("overlay", ZEVRIA_DARK.surfaces.overlay),
        ] {
            assert_contrast(
                "strong border",
                ZEVRIA_DARK.surfaces.border_strong,
                surface_name,
                surface,
                3.0,
            );
            assert_contrast(
                "selection background",
                ZEVRIA_DARK.surfaces.selection_background,
                surface_name,
                surface,
                3.0,
            );
        }

        for (name, focus) in [
            ("Build focus", ZEVRIA_DARK.workflow.build),
            ("Plan focus", ZEVRIA_DARK.workflow.plan),
            ("Review focus", ZEVRIA_DARK.workflow.review),
            ("Explore focus", ZEVRIA_DARK.workflow.explore),
        ] {
            assert_contrast(name, focus, "panel", ZEVRIA_DARK.surfaces.panel, 3.0);
        }
    }

    #[test]
    fn workflow_and_feedback_accents_remain_separated_under_full_cvd() {
        let workflows = [
            ("Build", ZEVRIA_DARK.workflow.build),
            ("Plan", ZEVRIA_DARK.workflow.plan),
            ("Review", ZEVRIA_DARK.workflow.review),
            ("Explore", ZEVRIA_DARK.workflow.explore),
        ];
        let feedback = [
            ("success", ZEVRIA_DARK.feedback.success),
            ("warning", ZEVRIA_DARK.feedback.warning),
            ("error", ZEVRIA_DARK.feedback.error),
            ("info", ZEVRIA_DARK.feedback.info),
        ];
        for (simulation, matrix) in validation::SIMULATIONS {
            assert_pairwise_separation(simulation, &workflows, matrix, 0.08);
            assert_pairwise_separation(simulation, &feedback, matrix, 0.075);
        }
    }

    fn all_tokens() -> Vec<(&'static str, Color)> {
        vec![
            ("surface.canvas", ZEVRIA_DARK.surfaces.canvas),
            ("surface.panel", ZEVRIA_DARK.surfaces.panel),
            ("surface.overlay", ZEVRIA_DARK.surfaces.overlay),
            ("surface.border", ZEVRIA_DARK.surfaces.border),
            ("surface.border_strong", ZEVRIA_DARK.surfaces.border_strong),
            (
                "surface.selection_background",
                ZEVRIA_DARK.surfaces.selection_background,
            ),
            (
                "surface.selection_foreground",
                ZEVRIA_DARK.surfaces.selection_foreground,
            ),
            ("text.primary", ZEVRIA_DARK.text.primary),
            ("text.secondary", ZEVRIA_DARK.text.secondary),
            ("text.muted", ZEVRIA_DARK.text.muted),
            ("workflow.build", ZEVRIA_DARK.workflow.build),
            ("workflow.plan", ZEVRIA_DARK.workflow.plan),
            ("workflow.review", ZEVRIA_DARK.workflow.review),
            ("workflow.explore", ZEVRIA_DARK.workflow.explore),
            ("roles.you", ZEVRIA_DARK.roles.you),
            ("roles.assistant", ZEVRIA_DARK.roles.assistant),
            ("roles.system", ZEVRIA_DARK.roles.system),
            ("roles.tools", ZEVRIA_DARK.roles.tools),
            ("feedback.success", ZEVRIA_DARK.feedback.success),
            ("feedback.warning", ZEVRIA_DARK.feedback.warning),
            ("feedback.error", ZEVRIA_DARK.feedback.error),
            ("feedback.info", ZEVRIA_DARK.feedback.info),
            ("content.heading", ZEVRIA_DARK.content.heading),
            ("content.strong", ZEVRIA_DARK.content.strong),
            ("content.code", ZEVRIA_DARK.content.code),
            ("content.link", ZEVRIA_DARK.content.link),
            ("content.diff_addition", ZEVRIA_DARK.content.diff_addition),
            (
                "content.diff_addition_background",
                ZEVRIA_DARK.content.diff_addition_background,
            ),
            ("content.diff_deletion", ZEVRIA_DARK.content.diff_deletion),
            (
                "content.diff_deletion_background",
                ZEVRIA_DARK.content.diff_deletion_background,
            ),
            ("content.diff_hunk", ZEVRIA_DARK.content.diff_hunk),
            ("syntax.keyword", ZEVRIA_DARK.syntax.keyword),
            ("syntax.function", ZEVRIA_DARK.syntax.function),
            ("syntax.type", ZEVRIA_DARK.syntax.r#type),
            ("syntax.string", ZEVRIA_DARK.syntax.string),
            ("syntax.constant", ZEVRIA_DARK.syntax.constant),
            ("syntax.comment", ZEVRIA_DARK.syntax.comment),
            ("syntax.invalid", ZEVRIA_DARK.syntax.invalid),
        ]
    }

    fn semantic_foregrounds() -> Vec<(&'static str, Color)> {
        vec![
            ("Build", ZEVRIA_DARK.workflow.build),
            ("Plan", ZEVRIA_DARK.workflow.plan),
            ("Review", ZEVRIA_DARK.workflow.review),
            ("Explore", ZEVRIA_DARK.workflow.explore),
            ("You", ZEVRIA_DARK.roles.you),
            ("Assistant", ZEVRIA_DARK.roles.assistant),
            ("System", ZEVRIA_DARK.roles.system),
            ("tools", ZEVRIA_DARK.roles.tools),
            ("success", ZEVRIA_DARK.feedback.success),
            ("warning", ZEVRIA_DARK.feedback.warning),
            ("error", ZEVRIA_DARK.feedback.error),
            ("info", ZEVRIA_DARK.feedback.info),
            ("heading", ZEVRIA_DARK.content.heading),
            ("strong", ZEVRIA_DARK.content.strong),
            ("code", ZEVRIA_DARK.content.code),
            ("link", ZEVRIA_DARK.content.link),
            ("diff addition", ZEVRIA_DARK.content.diff_addition),
            ("diff deletion", ZEVRIA_DARK.content.diff_deletion),
            ("diff hunk", ZEVRIA_DARK.content.diff_hunk),
            ("keyword", ZEVRIA_DARK.syntax.keyword),
            ("function", ZEVRIA_DARK.syntax.function),
            ("type", ZEVRIA_DARK.syntax.r#type),
            ("string", ZEVRIA_DARK.syntax.string),
            ("constant", ZEVRIA_DARK.syntax.constant),
            ("comment", ZEVRIA_DARK.syntax.comment),
            ("invalid", ZEVRIA_DARK.syntax.invalid),
        ]
    }

    fn assert_contrast(
        foreground_name: &str,
        foreground: Color,
        background_name: &str,
        background: Color,
        minimum: f64,
    ) {
        let ratio = validation::contrast(foreground, background);
        assert!(
            ratio >= minimum,
            "{foreground_name} on {background_name}: {ratio:.3}:1 is below {minimum:.1}:1"
        );
    }

    fn assert_pairwise_separation(
        simulation: &str,
        colors: &[(&str, Color)],
        matrix: [[f64; 3]; 3],
        minimum: f64,
    ) {
        for (index, (left_name, left)) in colors.iter().enumerate() {
            for (right_name, right) in &colors[index + 1..] {
                let distance = validation::distance(
                    validation::simulate(*left, matrix),
                    validation::simulate(*right, matrix),
                );
                assert!(
                    distance >= minimum,
                    "{left_name}/{right_name} under {simulation}: OKLab {distance:.6} < {minimum:.3}"
                );
            }
        }
    }
}
