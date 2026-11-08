//! Startup-only semantic color graph with the unchanged Zevria Dark fallback.
//!
//! The reference palette was authored in OKLCH, then chroma-reduced to the
//! sRGB gamut while holding perceptual lightness and hue. The terminal only
//! receives the resulting 24-bit RGB values: there is no runtime conversion,
//! terminal-default color, or ANSI fallback.

pub use definition::{HexRgb, HexRgb as Rgb, ReferencePalette, ThemeDefinition};
pub use generation::generate_theme;
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
pub const fn rgb(red: u8, green: u8, blue: u8) -> Rgb {
    Rgb(red, green, blue)
}

pub const fn rgb_channels(color: Rgb) -> (u8, u8, u8) {
    (color.0, color.1, color.2)
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

// Zevria Graphite: warm, low-chroma planes and two cool workflow inks.
// Hex outputs are authoritative; comments record the authored OKLCH sources.
const GRAPHITE_CANVAS: HexRgb = HexRgb(0x20, 0x1e, 0x1b); // oklch(23.5% 0.006 75) -> #201E1B
const GRAPHITE_PANEL: HexRgb = HexRgb(0x19, 0x17, 0x14); // oklch(20.5% 0.006 75) -> #191714
const GRAPHITE_OVERLAY: HexRgb = HexRgb(0x2a, 0x27, 0x24); // oklch(27.5% 0.007 75) -> #2A2724
const GRAPHITE_BORDER: HexRgb = HexRgb(0x4a, 0x47, 0x43); // oklch(40.0% 0.008 75) -> #4A4743
const GRAPHITE_BORDER_STRONG: HexRgb = HexRgb(0x8a, 0x85, 0x80); // oklch(62.0% 0.010 75) -> #8A8580
const GRAPHITE_SELECTION: HexRgb = HexRgb(0x80, 0xb1, 0xda); // oklch(74.0% 0.080 245) -> #80B1DA
const GRAPHITE_TEXT_PRIMARY: HexRgb = HexRgb(0xec, 0xe9, 0xe2); // oklch(93.5% 0.010 85) -> #ECE9E2
const GRAPHITE_TEXT_SECONDARY: HexRgb = HexRgb(0xc7, 0xc3, 0xbd); // oklch(82.0% 0.010 80) -> #C7C3BD
const GRAPHITE_TEXT_MUTED: HexRgb = HexRgb(0xa2, 0x9e, 0x98); // oklch(70.0% 0.010 75) -> #A29E98
const GRAPHITE_BUILD: HexRgb = HexRgb(0x63, 0xdf, 0xb1); // oklch(82.0% 0.130 166) -> #63DFB1
const GRAPHITE_PLAN: HexRgb = HexRgb(0x64, 0xa9, 0xf3); // oklch(72.0% 0.130 252) -> #64A9F3
const GRAPHITE_REVIEW: HexRgb = HexRgb(0xf2, 0xbd, 0x68); // oklch(83.0% 0.120 78) -> #F2BD68
const GRAPHITE_EXPLORE: HexRgb = HexRgb(0xd2, 0xbf, 0xf8); // oklch(84.0% 0.080 300) -> #D2BFF8
const GRAPHITE_YOU: HexRgb = HexRgb(0x8e, 0xbe, 0xa8); // oklch(76.0% 0.060 165) -> #8EBEA8
const GRAPHITE_ASSISTANT: HexRgb = HexRgb(0xb8, 0xa8, 0xd8); // oklch(76.0% 0.070 300) -> #B8A8D8
const GRAPHITE_SYSTEM: HexRgb = HexRgb(0xc9, 0xad, 0x7e); // oklch(76.0% 0.070 80) -> #C9AD7E
const GRAPHITE_TOOLS: HexRgb = HexRgb(0x6c, 0xc7, 0xd7); // oklch(78.0% 0.090 210) -> #6CC7D7
const GRAPHITE_SUCCESS: HexRgb = HexRgb(0x93, 0xc4, 0x49); // oklch(76.0% 0.160 128) -> #93C449
const GRAPHITE_WARNING: HexRgb = HexRgb(0xff, 0xd3, 0x8f); // oklch(89.0% 0.110 78) -> #FFD38F
const GRAPHITE_ERROR: HexRgb = HexRgb(0xef, 0x6e, 0x88); // oklch(70.0% 0.160 10) -> #EF6E88
const GRAPHITE_INFO: HexRgb = HexRgb(0x55, 0xbe, 0xf0); // oklch(76.0% 0.120 232) -> #55BEF0
const GRAPHITE_DIFF_ADDITION_BACKGROUND: HexRgb = HexRgb(0x21, 0x33, 0x1d); // oklch(30.0% 0.045 140) -> #21331D
const GRAPHITE_DIFF_DELETION_BACKGROUND: HexRgb = HexRgb(0x44, 0x23, 0x24); // oklch(30.0% 0.050 18) -> #442324
const GRAPHITE_SYNTAX_KEYWORD: HexRgb = HexRgb(0xc8, 0xa5, 0xf0); // oklch(78.0% 0.110 305) -> #C8A5F0
const GRAPHITE_SYNTAX_FUNCTION: HexRgb = HexRgb(0x7a, 0xca, 0xe9); // oklch(80.0% 0.090 225) -> #7ACAE9
const GRAPHITE_SYNTAX_STRING: HexRgb = HexRgb(0x95, 0xcf, 0x96); // oklch(80.0% 0.100 145) -> #95CF96
const GRAPHITE_SYNTAX_CONSTANT: HexRgb = HexRgb(0xe7, 0xb3, 0x69); // oklch(80.0% 0.110 75) -> #E7B369

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZevriaTheme<C = Rgb> {
    pub surfaces: SurfaceTokens<C>,
    pub text: TextTokens<C>,
    pub workflow: WorkflowTokens<C>,
    pub roles: RoleTokens<C>,
    pub feedback: FeedbackTokens<C>,
    pub content: ContentTokens<C>,
    pub syntax: SyntaxTokens<C>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceTokens<C = Rgb> {
    pub canvas: C,
    pub panel: C,
    pub overlay: C,
    pub border: C,
    pub border_strong: C,
    pub selection_background: C,
    pub selection_foreground: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextTokens<C = Rgb> {
    pub primary: C,
    pub secondary: C,
    pub muted: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkflowTokens<C = Rgb> {
    pub build: C,
    pub plan: C,
    pub review: C,
    pub explore: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoleTokens<C = Rgb> {
    pub you: C,
    pub assistant: C,
    pub system: C,
    pub tools: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedbackTokens<C = Rgb> {
    pub success: C,
    pub warning: C,
    pub error: C,
    pub info: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentTokens<C = Rgb> {
    pub heading: C,
    pub strong: C,
    pub code: C,
    pub link: C,
    pub diff_addition: C,
    pub diff_addition_background: C,
    pub diff_deletion: C,
    pub diff_deletion_background: C,
    pub diff_hunk: C,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxTokens<C = Rgb> {
    pub keyword: C,
    pub function: C,
    pub r#type: C,
    pub string: C,
    pub constant: C,
    pub comment: C,
    pub invalid: C,
}

impl<C> ZevriaTheme<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> ZevriaTheme<D> {
        ZevriaTheme {
            surfaces: self.surfaces.map(&mut f),
            text: self.text.map(&mut f),
            workflow: self.workflow.map(&mut f),
            roles: self.roles.map(&mut f),
            feedback: self.feedback.map(&mut f),
            content: self.content.map(&mut f),
            syntax: self.syntax.map(&mut f),
        }
    }
}

impl<C> SurfaceTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> SurfaceTokens<D> {
        SurfaceTokens {
            canvas: f(self.canvas),
            panel: f(self.panel),
            overlay: f(self.overlay),
            border: f(self.border),
            border_strong: f(self.border_strong),
            selection_background: f(self.selection_background),
            selection_foreground: f(self.selection_foreground),
        }
    }
}

impl<C> TextTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> TextTokens<D> {
        TextTokens {
            primary: f(self.primary),
            secondary: f(self.secondary),
            muted: f(self.muted),
        }
    }
}

impl<C> WorkflowTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> WorkflowTokens<D> {
        WorkflowTokens {
            build: f(self.build),
            plan: f(self.plan),
            review: f(self.review),
            explore: f(self.explore),
        }
    }
}

impl<C> RoleTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> RoleTokens<D> {
        RoleTokens {
            you: f(self.you),
            assistant: f(self.assistant),
            system: f(self.system),
            tools: f(self.tools),
        }
    }
}

impl<C> FeedbackTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> FeedbackTokens<D> {
        FeedbackTokens {
            success: f(self.success),
            warning: f(self.warning),
            error: f(self.error),
            info: f(self.info),
        }
    }
}

impl<C> ContentTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> ContentTokens<D> {
        ContentTokens {
            heading: f(self.heading),
            strong: f(self.strong),
            code: f(self.code),
            link: f(self.link),
            diff_addition: f(self.diff_addition),
            diff_addition_background: f(self.diff_addition_background),
            diff_deletion: f(self.diff_deletion),
            diff_deletion_background: f(self.diff_deletion_background),
            diff_hunk: f(self.diff_hunk),
        }
    }
}

impl<C> SyntaxTokens<C> {
    pub fn map<D>(self, mut f: impl FnMut(C) -> D) -> SyntaxTokens<D> {
        SyntaxTokens {
            keyword: f(self.keyword),
            function: f(self.function),
            r#type: f(self.r#type),
            string: f(self.string),
            constant: f(self.constant),
            comment: f(self.comment),
            invalid: f(self.invalid),
        }
    }
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

/// Built-in desktop default. This does not initialize the process-wide theme:
/// the TUI's default remains [`ZEVRIA_DARK`], and a selected theme wins in both.
pub const ZEVRIA_GRAPHITE: ZevriaTheme = ReferencePalette {
    canvas: GRAPHITE_CANVAS,
    panel: GRAPHITE_PANEL,
    overlay: GRAPHITE_OVERLAY,
    border: GRAPHITE_BORDER,
    border_strong: GRAPHITE_BORDER_STRONG,
    selection: GRAPHITE_SELECTION,
    text_primary: GRAPHITE_TEXT_PRIMARY,
    text_secondary: GRAPHITE_TEXT_SECONDARY,
    text_muted: GRAPHITE_TEXT_MUTED,
    build: GRAPHITE_BUILD,
    plan: GRAPHITE_PLAN,
    review: GRAPHITE_REVIEW,
    explore: GRAPHITE_EXPLORE,
    you: GRAPHITE_YOU,
    assistant: GRAPHITE_ASSISTANT,
    system: GRAPHITE_SYSTEM,
    tools: GRAPHITE_TOOLS,
    success: GRAPHITE_SUCCESS,
    warning: GRAPHITE_WARNING,
    error: GRAPHITE_ERROR,
    info: GRAPHITE_INFO,
    diff_addition_background: GRAPHITE_DIFF_ADDITION_BACKGROUND,
    diff_deletion_background: GRAPHITE_DIFF_DELETION_BACKGROUND,
    syntax_keyword: GRAPHITE_SYNTAX_KEYWORD,
    syntax_function: GRAPHITE_SYNTAX_FUNCTION,
    syntax_string: GRAPHITE_SYNTAX_STRING,
    syntax_constant: GRAPHITE_SYNTAX_CONSTANT,
}
.tokens();

/// Near-neutral paper sibling of Graphite. The reference schema and semantic
/// aliases are identical; accent lightness is deliberately spread for CVD separation.
pub const ZEVRIA_GRAPHITE_LIGHT: ZevriaTheme = ReferencePalette {
    canvas: HexRgb(0xfb, 0xfa, 0xf8),
    panel: HexRgb(0xf2, 0xf0, 0xec),
    overlay: HexRgb(0xff, 0xff, 0xff),
    border: HexRgb(0xd3, 0xd0, 0xcd),
    border_strong: HexRgb(0x83, 0x80, 0x7b),
    selection: HexRgb(0x22, 0x66, 0xa4),
    text_primary: HexRgb(0x22, 0x1f, 0x1b),
    text_secondary: HexRgb(0x45, 0x42, 0x3e),
    text_muted: HexRgb(0x66, 0x63, 0x5e),
    build: HexRgb(0x07, 0x55, 0x3e),
    plan: HexRgb(0x02, 0x67, 0xc7),
    review: HexRgb(0x90, 0x60, 0x1f),
    explore: HexRgb(0x5f, 0x27, 0x9e),
    you: HexRgb(0x24, 0x5e, 0x4a),
    assistant: HexRgb(0x69, 0x47, 0x87),
    system: HexRgb(0x78, 0x56, 0x29),
    tools: HexRgb(0x14, 0x60, 0x71),
    success: HexRgb(0x00, 0x48, 0x0e),
    warning: HexRgb(0x86, 0x65, 0x32),
    error: HexRgb(0x9b, 0x12, 0x48),
    info: HexRgb(0x09, 0x6a, 0xcb),
    diff_addition_background: HexRgb(0xee, 0xf6, 0xee),
    diff_deletion_background: HexRgb(0xff, 0xf0, 0xf2),
    syntax_keyword: HexRgb(0x70, 0x34, 0x90),
    syntax_function: HexRgb(0x15, 0x5b, 0x78),
    syntax_string: HexRgb(0x24, 0x5b, 0x2a),
    syntax_constant: HexRgb(0x77, 0x50, 0x1c),
}
.tokens();

#[cfg(test)]
mod tests {
    use super::*;

    const BUILT_INS: [(&str, ZevriaTheme); 3] = [
        ("Zevria Dark", ZEVRIA_DARK),
        ("Zevria Graphite", ZEVRIA_GRAPHITE),
        ("Zevria Graphite Light", ZEVRIA_GRAPHITE_LIGHT),
    ];

    #[test]
    fn every_reference_reaches_the_terminal_as_truecolor() {
        for (name, t) in BUILT_INS {
            let result = std::panic::catch_unwind(|| {
                for (name, color) in all_tokens(t) {
                    assert!(
                        matches!(color, Rgb(_, _, _)),
                        "{name} must be a 24-bit RGB token, got {color:?}"
                    );
                }
            });
            assert!(result.is_ok(), "{name} failed its built-in contract");
        }
    }

    #[test]
    fn root_canvas_matches_ghostty_default() {
        assert_eq!(ZEVRIA_DARK.surfaces.canvas, Rgb(0x28, 0x2c, 0x34));
    }

    #[test]
    fn normal_and_semantic_foregrounds_meet_wcag_aa_on_every_surface() {
        for (name, t) in BUILT_INS {
            let result = std::panic::catch_unwind(|| {
                let surfaces = [
                    ("canvas", t.surfaces.canvas),
                    ("panel", t.surfaces.panel),
                    ("overlay", t.surfaces.overlay),
                ];
                let text = [
                    ("primary", t.text.primary),
                    ("secondary", t.text.secondary),
                    ("muted", t.text.muted),
                ];
                for (foreground_name, foreground) in text {
                    for &(surface_name, background) in &surfaces {
                        assert_contrast(foreground_name, foreground, surface_name, background, 4.5);
                    }
                }

                for (name, foreground) in semantic_foregrounds(t) {
                    for &(surface_name, background) in &surfaces {
                        assert_contrast(name, foreground, surface_name, background, 4.5);
                    }
                }
                assert_contrast(
                    "selection foreground",
                    t.surfaces.selection_foreground,
                    "selection background",
                    t.surfaces.selection_background,
                    4.5,
                );
            });
            assert!(result.is_ok(), "{name} failed its built-in contract");
        }
    }

    #[test]
    fn semantic_aliases_and_selection_polarity_remain_stable() {
        for (name, t) in BUILT_INS {
            let result = std::panic::catch_unwind(|| {
                assert_eq!(t.content.heading, t.workflow.explore);
                assert_eq!(t.content.strong, t.workflow.plan);
                assert_eq!(t.syntax.r#type, t.workflow.plan);
                assert_eq!(t.content.code, t.syntax.string);
                assert_eq!(t.content.link, t.feedback.info);
                assert_eq!(t.content.diff_hunk, t.feedback.info);
                assert_eq!(t.content.diff_addition, t.feedback.success);
                assert_eq!(t.content.diff_deletion, t.feedback.error);
                assert_eq!(t.syntax.invalid, t.feedback.error);
                assert_eq!(t.syntax.comment, t.text.muted);
                assert_eq!(t.surfaces.selection_foreground, t.surfaces.canvas);
            });
            assert!(result.is_ok(), "{name} failed its built-in contract");
        }
    }

    #[test]
    fn diff_row_foregrounds_meet_wcag_aa_on_diff_backgrounds() {
        for (name, t) in BUILT_INS {
            let result = std::panic::catch_unwind(|| {
                let backgrounds = [
                    (
                        "diff addition background",
                        t.content.diff_addition_background,
                    ),
                    (
                        "diff deletion background",
                        t.content.diff_deletion_background,
                    ),
                ];
                let foregrounds = [
                    ("primary", t.text.primary),
                    ("secondary", t.text.secondary),
                    ("muted", t.text.muted),
                    ("diff addition", t.content.diff_addition),
                    ("diff deletion", t.content.diff_deletion),
                    ("keyword", t.syntax.keyword),
                    ("function", t.syntax.function),
                    ("type", t.syntax.r#type),
                    ("string", t.syntax.string),
                    ("constant", t.syntax.constant),
                    ("comment", t.syntax.comment),
                    ("invalid", t.syntax.invalid),
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
            });
            assert!(result.is_ok(), "{name} failed its built-in contract");
        }
    }

    #[test]
    fn strong_boundaries_and_focus_indicators_meet_three_to_one() {
        for (name, t) in BUILT_INS {
            let result = std::panic::catch_unwind(|| {
                for (surface_name, surface) in [
                    ("canvas", t.surfaces.canvas),
                    ("panel", t.surfaces.panel),
                    ("overlay", t.surfaces.overlay),
                ] {
                    assert_contrast(
                        "strong border",
                        t.surfaces.border_strong,
                        surface_name,
                        surface,
                        3.0,
                    );
                    assert_contrast(
                        "selection background",
                        t.surfaces.selection_background,
                        surface_name,
                        surface,
                        3.0,
                    );
                }

                for (name, focus) in [
                    ("Build focus", t.workflow.build),
                    ("Plan focus", t.workflow.plan),
                    ("Review focus", t.workflow.review),
                    ("Explore focus", t.workflow.explore),
                ] {
                    assert_contrast(name, focus, "panel", t.surfaces.panel, 3.0);
                }
            });
            assert!(result.is_ok(), "{name} failed its built-in contract");
        }
    }

    #[test]
    fn workflow_and_feedback_accents_remain_separated_under_full_cvd() {
        for (name, t) in BUILT_INS {
            let result = std::panic::catch_unwind(|| {
                let workflows = [
                    ("Build", t.workflow.build),
                    ("Plan", t.workflow.plan),
                    ("Review", t.workflow.review),
                    ("Explore", t.workflow.explore),
                ];
                let feedback = [
                    ("success", t.feedback.success),
                    ("warning", t.feedback.warning),
                    ("error", t.feedback.error),
                    ("info", t.feedback.info),
                ];
                for (simulation, matrix) in validation::SIMULATIONS {
                    assert_pairwise_separation(simulation, &workflows, matrix, 0.08);
                    assert_pairwise_separation(simulation, &feedback, matrix, 0.075);
                }
            });
            assert!(result.is_ok(), "{name} failed its built-in contract");
        }
    }

    fn all_tokens(t: ZevriaTheme) -> Vec<(&'static str, Rgb)> {
        vec![
            ("surface.canvas", t.surfaces.canvas),
            ("surface.panel", t.surfaces.panel),
            ("surface.overlay", t.surfaces.overlay),
            ("surface.border", t.surfaces.border),
            ("surface.border_strong", t.surfaces.border_strong),
            (
                "surface.selection_background",
                t.surfaces.selection_background,
            ),
            (
                "surface.selection_foreground",
                t.surfaces.selection_foreground,
            ),
            ("text.primary", t.text.primary),
            ("text.secondary", t.text.secondary),
            ("text.muted", t.text.muted),
            ("workflow.build", t.workflow.build),
            ("workflow.plan", t.workflow.plan),
            ("workflow.review", t.workflow.review),
            ("workflow.explore", t.workflow.explore),
            ("roles.you", t.roles.you),
            ("roles.assistant", t.roles.assistant),
            ("roles.system", t.roles.system),
            ("roles.tools", t.roles.tools),
            ("feedback.success", t.feedback.success),
            ("feedback.warning", t.feedback.warning),
            ("feedback.error", t.feedback.error),
            ("feedback.info", t.feedback.info),
            ("content.heading", t.content.heading),
            ("content.strong", t.content.strong),
            ("content.code", t.content.code),
            ("content.link", t.content.link),
            ("content.diff_addition", t.content.diff_addition),
            (
                "content.diff_addition_background",
                t.content.diff_addition_background,
            ),
            ("content.diff_deletion", t.content.diff_deletion),
            (
                "content.diff_deletion_background",
                t.content.diff_deletion_background,
            ),
            ("content.diff_hunk", t.content.diff_hunk),
            ("syntax.keyword", t.syntax.keyword),
            ("syntax.function", t.syntax.function),
            ("syntax.type", t.syntax.r#type),
            ("syntax.string", t.syntax.string),
            ("syntax.constant", t.syntax.constant),
            ("syntax.comment", t.syntax.comment),
            ("syntax.invalid", t.syntax.invalid),
        ]
    }

    fn semantic_foregrounds(t: ZevriaTheme) -> Vec<(&'static str, Rgb)> {
        vec![
            ("Build", t.workflow.build),
            ("Plan", t.workflow.plan),
            ("Review", t.workflow.review),
            ("Explore", t.workflow.explore),
            ("You", t.roles.you),
            ("Assistant", t.roles.assistant),
            ("System", t.roles.system),
            ("tools", t.roles.tools),
            ("success", t.feedback.success),
            ("warning", t.feedback.warning),
            ("error", t.feedback.error),
            ("info", t.feedback.info),
            ("heading", t.content.heading),
            ("strong", t.content.strong),
            ("code", t.content.code),
            ("link", t.content.link),
            ("diff addition", t.content.diff_addition),
            ("diff deletion", t.content.diff_deletion),
            ("diff hunk", t.content.diff_hunk),
            ("keyword", t.syntax.keyword),
            ("function", t.syntax.function),
            ("type", t.syntax.r#type),
            ("string", t.syntax.string),
            ("constant", t.syntax.constant),
            ("comment", t.syntax.comment),
            ("invalid", t.syntax.invalid),
        ]
    }

    fn assert_contrast(
        foreground_name: &str,
        foreground: Rgb,
        background_name: &str,
        background: Rgb,
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
        colors: &[(&str, Rgb)],
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
