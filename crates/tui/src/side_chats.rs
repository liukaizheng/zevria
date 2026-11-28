//! Terminal presentation for the Side Chat thread picker.
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Clear, List, ListItem},
};
use zevria_foundation::{SideChatDescriptor, SideChatStatus};

use crate::{
    app::UiAction,
    hints::{Eligibility, hint_line},
    input::{Action, ChordState, KeyContext},
    text::truncate_display_width,
    viewport::render_scrollbar,
};
use ratatui::crossterm::event::KeyEvent;
use ratatui::text::Line;
use zevria_tui_widgets::overlay::{ListNav, ListOutcome, modal};

const CONTEXT: KeyContext = KeyContext::SideChatsList;

#[derive(Default)]
pub(crate) struct SideChatsOverlay {
    rows: Vec<SideChatDescriptor>,
    nav: ListNav,
    keys: ChordState,
}

impl SideChatsOverlay {
    pub fn new(rows: Vec<SideChatDescriptor>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    pub fn replace_rows(&mut self, rows: Vec<SideChatDescriptor>) {
        self.nav.selected = self
            .nav
            .selected
            .checked_sub(1)
            .and_then(|index| self.rows.get(index))
            .and_then(|old| rows.iter().position(|row| row.id == old.id))
            .map_or(0, |index| index + 1);
        self.rows = rows;
    }

    pub fn invalidate_geometry(&mut self) {
        self.nav.invalidate_geometry();
    }

    pub fn hint_eligibility(&self) -> Eligibility {
        Eligibility {
            labels: vec![(
                Action::Confirm,
                if self.nav.selected == 0 {
                    "new chat"
                } else {
                    "open"
                },
            )],
            disabled: if self.nav.selected == 0 {
                vec![Action::SideChatReferenceInsert]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> (bool, Option<UiAction>) {
        let Some(action) = self.keys.resolve(CONTEXT, key) else {
            return (false, None);
        };
        let selected = self
            .nav
            .selected
            .checked_sub(1)
            .and_then(|index| self.rows.get(index));
        match action {
            Action::SideChatOverlay => (true, None),
            Action::NewSideChat => (true, Some(UiAction::NewSideChat)),
            Action::NewSideChatFromClipboard => (true, Some(UiAction::ReadSideChatExcerpt)),
            Action::SideChatReferenceInsert => selected.map_or((false, None), |row| {
                (true, Some(UiAction::InsertSideReference { id: row.id }))
            }),
            _ => {
                let Some(action) = action.list_action() else {
                    return (false, None);
                };
                match self.nav.handle(action, self.rows.len() + 1) {
                    ListOutcome::Close => (true, None),
                    ListOutcome::Confirm => {
                        let action = self
                            .nav
                            .selected
                            .checked_sub(1)
                            .and_then(|index| self.rows.get(index))
                            .map_or(UiAction::NewSideChat, |row| UiAction::OpenSidePane {
                                id: row.id,
                            });
                        (true, Some(action))
                    }
                    ListOutcome::Handled => (false, None),
                }
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, body: Rect) {
        if body.width < 4 || body.height < 4 {
            return;
        }
        let width = body.width.min(72);
        let height = body.height.min((self.rows.len() + 3).clamp(4, 14) as u16);
        let area = Rect::new(
            body.x + (body.width - width) / 2,
            body.y + (body.height - height) / 2,
            width,
            height,
        );
        let footer = hint_line(
            CONTEXT,
            &self.hint_eligibility(),
            usize::from(width.saturating_sub(2)),
        );
        let inner = modal(frame, area, " Side Chats ", footer);
        let text_width = usize::from(inner.width);
        self.nav
            .reconcile(self.rows.len() + 1, usize::from(inner.height));
        let range = self.nav.viewport.visible_range();
        let items: Vec<_> = (range.start()..range.end())
            .map(|index| {
                let text = if index == 0 {
                    "+ New side chat".to_owned()
                } else {
                    let row = &self.rows[index - 1];
                    let status = match row.status {
                        SideChatStatus::Running => "◐",
                        SideChatStatus::Failed => "!",
                        SideChatStatus::Idle => "○",
                    };
                    let title = sanitize_single_line(&row.title);
                    format!(
                        "{status}  {}  {title}  ({} turns)",
                        row.short_id, row.turn_count
                    )
                };
                let mut line = Line::from(truncate_display_width(&text, text_width));
                self.nav.style_line(index, &mut line);
                ListItem::new(line)
            })
            .collect();
        frame.render_widget(List::new(items), inner);
        self.nav.paint_selected(frame, inner);
        render_scrollbar(frame, area, &self.nav.viewport);
    }
}

pub(crate) fn render_completions(
    frame: &mut Frame,
    body: Rect,
    prompt: Rect,
    matches: &[zevria_presentation::side_chats::SideChatCompletionItem],
    selected: usize,
) {
    let available = prompt.y.saturating_sub(body.y);
    if matches.is_empty() || available < 3 || prompt.width < 4 {
        return;
    }
    let style = Style::new().bg(crate::theme::theme().surfaces.canvas);
    let height = (matches.len() as u16 + 2).min(available);
    let area = Rect::new(prompt.x, prompt.y - height, prompt.width.min(72), height);
    frame.render_widget(Clear, area);
    let visible = usize::from(height - 2);
    let selected = selected.min(matches.len() - 1);
    let start = selected.saturating_add(1).saturating_sub(visible);
    let text_width = usize::from(area.width.saturating_sub(2));
    let items = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, item)| {
            let line = format!(" {}  {}", item.insertion, sanitize_single_line(&item.label));
            ListItem::new(truncate_display_width(&line, text_width)).style(if index == selected {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            })
        });
    frame.render_widget(
        List::new(items)
            .style(style)
            .block(Block::bordered().title(" Side Chat references / Tab insert ")),
        area,
    );
}

fn sanitize_single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
