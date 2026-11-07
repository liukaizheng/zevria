//! Terminal-only adapter for the frontend-neutral semantic palette.
use ratatui::style::Color;
use std::sync::OnceLock;
pub use zevria_theme::{
    HexRgb, ReferencePalette, Rgb, ThemeDefinition, generate_theme, install_theme, validate_theme,
};
pub type ZevriaTheme = zevria_theme::ZevriaTheme<Color>;
static THEME: OnceLock<ZevriaTheme> = OnceLock::new();
pub fn theme() -> &'static ZevriaTheme {
    THEME.get_or_init(|| zevria_theme::theme().map(|c| rgb(c.0, c.1, c.2)))
}
pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}
pub fn rgb_channels(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => unreachable!("theme tokens are RGB"),
    }
}
/// Compile-time fallback, byte-for-byte identical to the shared palette.
pub const ZEVRIA_DARK: ZevriaTheme = convert(zevria_theme::ZEVRIA_DARK);
const fn convert(t: zevria_theme::ZevriaTheme) -> ZevriaTheme {
    ZevriaTheme {
        surfaces: zevria_theme::SurfaceTokens {
            canvas: rgb(
                t.surfaces.canvas.0,
                t.surfaces.canvas.1,
                t.surfaces.canvas.2,
            ),
            panel: rgb(t.surfaces.panel.0, t.surfaces.panel.1, t.surfaces.panel.2),
            overlay: rgb(
                t.surfaces.overlay.0,
                t.surfaces.overlay.1,
                t.surfaces.overlay.2,
            ),
            border: rgb(
                t.surfaces.border.0,
                t.surfaces.border.1,
                t.surfaces.border.2,
            ),
            border_strong: rgb(
                t.surfaces.border_strong.0,
                t.surfaces.border_strong.1,
                t.surfaces.border_strong.2,
            ),
            selection_background: rgb(
                t.surfaces.selection_background.0,
                t.surfaces.selection_background.1,
                t.surfaces.selection_background.2,
            ),
            selection_foreground: rgb(
                t.surfaces.selection_foreground.0,
                t.surfaces.selection_foreground.1,
                t.surfaces.selection_foreground.2,
            ),
        },
        text: zevria_theme::TextTokens {
            primary: rgb(t.text.primary.0, t.text.primary.1, t.text.primary.2),
            secondary: rgb(t.text.secondary.0, t.text.secondary.1, t.text.secondary.2),
            muted: rgb(t.text.muted.0, t.text.muted.1, t.text.muted.2),
        },
        workflow: zevria_theme::WorkflowTokens {
            build: rgb(t.workflow.build.0, t.workflow.build.1, t.workflow.build.2),
            plan: rgb(t.workflow.plan.0, t.workflow.plan.1, t.workflow.plan.2),
            review: rgb(
                t.workflow.review.0,
                t.workflow.review.1,
                t.workflow.review.2,
            ),
            explore: rgb(
                t.workflow.explore.0,
                t.workflow.explore.1,
                t.workflow.explore.2,
            ),
        },
        roles: zevria_theme::RoleTokens {
            you: rgb(t.roles.you.0, t.roles.you.1, t.roles.you.2),
            assistant: rgb(
                t.roles.assistant.0,
                t.roles.assistant.1,
                t.roles.assistant.2,
            ),
            system: rgb(t.roles.system.0, t.roles.system.1, t.roles.system.2),
            tools: rgb(t.roles.tools.0, t.roles.tools.1, t.roles.tools.2),
        },
        feedback: zevria_theme::FeedbackTokens {
            success: rgb(
                t.feedback.success.0,
                t.feedback.success.1,
                t.feedback.success.2,
            ),
            warning: rgb(
                t.feedback.warning.0,
                t.feedback.warning.1,
                t.feedback.warning.2,
            ),
            error: rgb(t.feedback.error.0, t.feedback.error.1, t.feedback.error.2),
            info: rgb(t.feedback.info.0, t.feedback.info.1, t.feedback.info.2),
        },
        content: zevria_theme::ContentTokens {
            heading: rgb(
                t.content.heading.0,
                t.content.heading.1,
                t.content.heading.2,
            ),
            strong: rgb(t.content.strong.0, t.content.strong.1, t.content.strong.2),
            code: rgb(t.content.code.0, t.content.code.1, t.content.code.2),
            link: rgb(t.content.link.0, t.content.link.1, t.content.link.2),
            diff_addition: rgb(
                t.content.diff_addition.0,
                t.content.diff_addition.1,
                t.content.diff_addition.2,
            ),
            diff_addition_background: rgb(
                t.content.diff_addition_background.0,
                t.content.diff_addition_background.1,
                t.content.diff_addition_background.2,
            ),
            diff_deletion: rgb(
                t.content.diff_deletion.0,
                t.content.diff_deletion.1,
                t.content.diff_deletion.2,
            ),
            diff_deletion_background: rgb(
                t.content.diff_deletion_background.0,
                t.content.diff_deletion_background.1,
                t.content.diff_deletion_background.2,
            ),
            diff_hunk: rgb(
                t.content.diff_hunk.0,
                t.content.diff_hunk.1,
                t.content.diff_hunk.2,
            ),
        },
        syntax: zevria_theme::SyntaxTokens {
            keyword: rgb(t.syntax.keyword.0, t.syntax.keyword.1, t.syntax.keyword.2),
            function: rgb(
                t.syntax.function.0,
                t.syntax.function.1,
                t.syntax.function.2,
            ),
            r#type: rgb(t.syntax.r#type.0, t.syntax.r#type.1, t.syntax.r#type.2),
            string: rgb(t.syntax.string.0, t.syntax.string.1, t.syntax.string.2),
            constant: rgb(
                t.syntax.constant.0,
                t.syntax.constant.1,
                t.syntax.constant.2,
            ),
            comment: rgb(t.syntax.comment.0, t.syntax.comment.1, t.syntax.comment.2),
            invalid: rgb(t.syntax.invalid.0, t.syntax.invalid.1, t.syntax.invalid.2),
        },
    }
}
