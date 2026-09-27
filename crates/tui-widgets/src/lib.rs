//! Reusable terminal rendering, text, and viewport primitives.

use zevria_theme as theme;
pub mod chrome;
pub mod diff_render;
pub mod markdown;
pub mod overlay;
pub mod syntax;
pub mod text;
pub mod viewport;
pub mod workspace_header;

/// Draw the composition root's Connecting/Resuming frame with the same selected
/// Zevria canvas as the interactive TUI, without exposing internal tokens.
pub fn render_startup_frame(frame: &mut ratatui::Frame, status: &str) {
    use ratatui::{
        style::Style,
        widgets::{Block, BorderType, Paragraph},
    };

    let area = frame.area();
    let canvas = Style::new()
        .fg(theme::theme().text.primary)
        .bg(theme::theme().surfaces.canvas);
    chrome::paint_surface(frame.buffer_mut(), area, canvas);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Zevria ")
        .style(canvas)
        .border_style(Style::new().fg(theme::theme().surfaces.border_strong));
    frame.render_widget(Paragraph::new(status).style(canvas).block(block), area);
}
