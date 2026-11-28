//! Cursor and Visual selections in the rendered body of one transcript block.
use ratatui::{
    style::{Color, Modifier},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct TextPos {
    pub line: usize,
    pub grapheme: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum VisualMode {
    #[default]
    None,
    Char {
        anchor: TextPos,
    },
    Line {
        anchor_line: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wait {
    FirstLine,
    Yank,
    Object(bool),
    Find { forward: bool, till: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Find {
    needle: char,
    forward: bool,
    till: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct VimTextSelection {
    lines: Vec<String>,
    pub cursor: TextPos,
    preferred_col: usize,
    pub visual: VisualMode,
    pending: Option<Wait>,
    find: Option<Find>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum TextAction {
    Continue,
    Exit,
    Ask(String),
    Yank(String),
}

fn class(grapheme: &str, long: bool) -> u8 {
    if grapheme.chars().all(char::is_whitespace) {
        0
    } else if long
        || grapheme
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        1
    } else {
        2
    }
}

impl VimTextSelection {
    pub(crate) fn new(mut lines: Vec<String>, first: usize) -> Self {
        if lines.is_empty() {
            lines.push(String::new());
        }
        let cursor = TextPos {
            line: first.min(lines.len() - 1),
            grapheme: 0,
        };
        Self {
            lines,
            cursor,
            preferred_col: 0,
            visual: VisualMode::None,
            pending: None,
            find: None,
        }
    }

    pub(crate) fn mode_label(&self) -> &'static str {
        match self.visual {
            VisualMode::None => "TEXT",
            VisualMode::Char { .. } => "VISUAL",
            VisualMode::Line { .. } => "V-LINE",
        }
    }

    fn set_column(&mut self, column: usize) {
        self.cursor.grapheme = column.min(
            self.lines[self.cursor.line]
                .graphemes(true)
                .count()
                .saturating_sub(1),
        );
        self.preferred_col = self.cursor.grapheme;
    }

    fn jump(&mut self, line: usize) {
        self.cursor.line = line.min(self.lines.len() - 1);
        self.cursor.grapheme = self.preferred_col.min(
            self.lines[self.cursor.line]
                .graphemes(true)
                .count()
                .saturating_sub(1),
        );
    }

    fn ends(&self, anchor: TextPos) -> (TextPos, TextPos) {
        (self.cursor.min(anchor), self.cursor.max(anchor))
    }

    pub(crate) fn selected(&self, pos: TextPos) -> bool {
        match self.visual {
            VisualMode::None => false,
            VisualMode::Char { anchor } => {
                let (a, b) = self.ends(anchor);
                (a..=b).contains(&pos)
            }
            VisualMode::Line { anchor_line } => (anchor_line.min(self.cursor.line)
                ..=anchor_line.max(self.cursor.line))
                .contains(&pos.line),
        }
    }

    pub(crate) fn excerpt(&self) -> String {
        match self.visual {
            VisualMode::None => self.lines[self.cursor.line].clone(),
            VisualMode::Line { anchor_line } => self.lines
                [anchor_line.min(self.cursor.line)..=anchor_line.max(self.cursor.line)]
                .join("\n"),
            VisualMode::Char { anchor } => {
                let (start, end) = self.ends(anchor);
                (start.line..=end.line)
                    .map(|line| {
                        let left = if line == start.line {
                            start.grapheme
                        } else {
                            0
                        };
                        let right = if line == end.line {
                            end.grapheme.saturating_add(1)
                        } else {
                            usize::MAX
                        };
                        self.lines[line]
                            .graphemes(true)
                            .skip(left)
                            .take(right.saturating_sub(left))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    fn word(&mut self, key: char) {
        let long = key.is_ascii_uppercase();
        let key = key.to_ascii_lowercase();
        let mut positions = Vec::new();
        let mut origin = 0;
        for (line, text) in self.lines.iter().enumerate() {
            let start = positions.len();
            positions.extend(
                text.graphemes(true)
                    .enumerate()
                    .map(|(grapheme, text)| (TextPos { line, grapheme }, class(text, long))),
            );
            if self.cursor.line == line {
                origin = start + self.cursor.grapheme;
            }
            positions.push((
                TextPos {
                    line,
                    grapheme: positions.len() - start,
                },
                0,
            ));
        }
        let mut i = origin;
        match key {
            'w' => {
                let kind = positions[i].1;
                i += 1;
                if kind != 0 {
                    while positions.get(i).is_some_and(|p| p.1 == kind) {
                        i += 1;
                    }
                }
                while positions.get(i).is_some_and(|p| p.1 == 0) {
                    i += 1;
                }
            }
            'e' => {
                i += 1;
                while positions.get(i).is_some_and(|p| p.1 == 0) {
                    i += 1;
                }
                if let Some((_, kind)) = positions.get(i) {
                    while positions.get(i + 1).is_some_and(|p| p.1 == *kind) {
                        i += 1;
                    }
                }
            }
            'b' => {
                i = i.saturating_sub(1);
                while i > 0 && positions[i].1 == 0 {
                    i -= 1;
                }
                while i > 0 && positions[i - 1].1 == positions[i].1 {
                    i -= 1;
                }
            }
            _ => unreachable!(),
        }
        if key == 'e' && i == positions.len() {
            return;
        }
        let pos = positions
            .get(i)
            .unwrap_or_else(|| positions.last().expect("nonempty text positions"))
            .0;
        self.cursor.line = pos.line;
        self.set_column(pos.grapheme);
    }

    fn object(&mut self, inner: bool, long: bool) {
        if !matches!(self.visual, VisualMode::Char { .. }) {
            return;
        }
        let body = self.lines[self.cursor.line]
            .graphemes(true)
            .map(|part| class(part, long))
            .collect::<Vec<_>>();
        let pos = self.cursor.grapheme;
        let Some(&kind) = body.get(pos) else { return };
        let mut left = pos;
        let mut right = pos;
        while left > 0 && body[left - 1] == kind {
            left -= 1;
        }
        while body.get(right + 1) == Some(&kind) {
            right += 1;
        }
        if !inner {
            if kind == 0 {
                if let Some(next) = body.get(right + 1) {
                    while body.get(right + 1) == Some(next) {
                        right += 1;
                    }
                }
            } else if body.get(right + 1) == Some(&0) {
                while body.get(right + 1) == Some(&0) {
                    right += 1;
                }
            } else {
                while left > 0 && body[left - 1] == 0 {
                    left -= 1;
                }
            }
        }
        self.visual = VisualMode::Char {
            anchor: TextPos {
                line: self.cursor.line,
                grapheme: left,
            },
        };
        self.set_column(right);
    }

    fn find(&mut self, find: Find, repeated: bool) {
        let start = self.cursor.grapheme;
        let line = self.lines[self.cursor.line]
            .graphemes(true)
            .collect::<Vec<_>>();
        let matched = if find.forward {
            (start.saturating_add(1 + usize::from(repeated && find.till))..line.len())
                .find(|&i| line[i].starts_with(find.needle))
        } else {
            (0..start.saturating_sub(usize::from(repeated && find.till)))
                .rev()
                .find(|&i| line[i].starts_with(find.needle))
        };
        if let Some(i) = matched {
            let i = if !find.till {
                i
            } else if find.forward {
                i.saturating_sub(1)
            } else {
                i.saturating_add(1)
            };
            self.set_column(i);
        }
    }

    pub(crate) fn awaiting_find(&self) -> bool {
        matches!(self.pending, Some(Wait::Find { .. }))
    }

    /// Escape clears pending input and Visual before leaving the block.
    pub(crate) fn escape(&mut self) -> TextAction {
        if self.pending.take().is_some() {
            return TextAction::Continue;
        }
        if self.visual == VisualMode::None {
            TextAction::Exit
        } else {
            self.visual = VisualMode::None;
            TextAction::Continue
        }
    }

    pub(crate) fn key(&mut self, key: char) -> TextAction {
        match self.pending.take() {
            Some(Wait::FirstLine) if key == 'g' => {
                self.jump(0);
                self.set_column(0);
                return TextAction::Continue;
            }
            Some(Wait::Yank) if key == 'y' => return TextAction::Yank(self.excerpt()),
            Some(Wait::Object(inner)) if matches!(key, 'w' | 'W') => {
                self.object(inner, key == 'W');
                return TextAction::Continue;
            }
            Some(Wait::Find { forward, till }) => {
                let find = Find {
                    needle: key,
                    forward,
                    till,
                };
                self.find(find, false);
                self.find = Some(find);
                return TextAction::Continue;
            }
            _ => {}
        }
        match key {
            'h' => self.set_column(self.cursor.grapheme.saturating_sub(1)),
            'l' => self.set_column(self.cursor.grapheme.saturating_add(1)),
            'j' => self.jump(self.cursor.line.saturating_add(1)),
            'k' => self.jump(self.cursor.line.saturating_sub(1)),
            '0' => self.set_column(0),
            '^' => self.set_column(
                self.lines[self.cursor.line]
                    .graphemes(true)
                    .take_while(|p| class(p, true) == 0)
                    .count(),
            ),
            '$' => self.set_column(usize::MAX),
            'g' => self.pending = Some(Wait::FirstLine),
            'G' => {
                self.jump(self.lines.len() - 1);
                self.set_column(0);
            }
            'w' | 'b' | 'e' | 'W' | 'B' | 'E' => self.word(key),
            'f' | 't' | 'F' | 'T' => {
                self.pending = Some(Wait::Find {
                    forward: key.is_ascii_lowercase(),
                    till: matches!(key, 't' | 'T'),
                })
            }
            ';' | ',' => {
                if let Some(mut find) = self.find {
                    if key == ',' {
                        find.forward = !find.forward;
                    }
                    self.find(find, true);
                }
            }
            'i' | 'a' => {
                if matches!(self.visual, VisualMode::Char { .. }) {
                    self.pending = Some(Wait::Object(key == 'i'));
                }
            }
            'v' => {
                self.visual = match self.visual {
                    VisualMode::None => VisualMode::Char {
                        anchor: self.cursor,
                    },
                    VisualMode::Char { .. } => VisualMode::None,
                    VisualMode::Line { anchor_line } => VisualMode::Char {
                        anchor: TextPos {
                            line: anchor_line,
                            grapheme: 0,
                        },
                    },
                }
            }
            'V' => {
                self.visual = match self.visual {
                    VisualMode::None => VisualMode::Line {
                        anchor_line: self.cursor.line,
                    },
                    VisualMode::Char { anchor } => VisualMode::Line {
                        anchor_line: anchor.line,
                    },
                    VisualMode::Line { .. } => VisualMode::None,
                }
            }
            'o' => match self.visual {
                VisualMode::Char { anchor } => {
                    self.visual = VisualMode::Char {
                        anchor: self.cursor,
                    };
                    self.cursor = anchor;
                    self.preferred_col = anchor.grapheme;
                }
                VisualMode::Line { anchor_line } => {
                    self.visual = VisualMode::Line {
                        anchor_line: self.cursor.line,
                    };
                    self.jump(anchor_line);
                }
                VisualMode::None => {}
            },
            's' => return TextAction::Ask(self.excerpt()),
            'y' if self.visual == VisualMode::None => self.pending = Some(Wait::Yank),
            'y' => {
                let text = self.excerpt();
                self.visual = VisualMode::None;
                return TextAction::Yank(text);
            }
            _ => {}
        }
        TextAction::Continue
    }
}

pub(crate) fn gutter(line: &Line<'_>) -> usize {
    // A card draws its left border as its own synthetic span. Code/diff
    // gutters and real indentation are left intact.
    line.spans
        .first()
        .map_or(0, |span| match span.content.as_ref() {
            "│ " | "│" => span.content.len(),
            _ => 0,
        })
}

pub(crate) fn visible_text(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let text = line.to_string();
            text[gutter(line)..].to_owned()
        })
        .collect()
}

pub(crate) fn highlight_vim_selection(
    lines: &[Line<'static>],
    selection: &VimTextSelection,
    selection_background: Color,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let first = gutter(line);
            let text = line.to_string();
            let ranges: Vec<_> = text[first..]
                .grapheme_indices(true)
                .map(|(byte, part)| (first + byte, first + byte + part.len()))
                .collect();
            let mut spans = Vec::new();
            let mut offset = 0;
            let mut index = 0;
            for span in &line.spans {
                let end = offset + span.content.len();
                while offset < end {
                    while ranges.get(index).is_some_and(|&(_, right)| right <= offset) {
                        index += 1;
                    }
                    let pos = TextPos {
                        line: row,
                        grapheme: index,
                    };
                    let (right, style) = match ranges.get(index) {
                        Some(&(left, right)) if offset >= left => {
                            let style = if pos == selection.cursor {
                                span.style.add_modifier(Modifier::REVERSED)
                            } else if selection.selected(pos) {
                                span.style.bg(selection_background)
                            } else {
                                span.style
                            };
                            (right.min(end), style)
                        }
                        _ => (end.min(first.max(offset + 1)), span.style),
                    };
                    let base = end - span.content.len();
                    spans.push(Span::styled(
                        span.content[offset - base..right - base].to_owned(),
                        style,
                    ));
                    offset = right;
                }
            }
            if ranges.is_empty() && selection.cursor.line == row {
                spans.push(Span::styled(
                    " ",
                    line.style.add_modifier(Modifier::REVERSED),
                ));
            }
            Line::from(spans)
                .style(line.style)
                .alignment(line.alignment.unwrap_or_default())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_uses_grapheme_coordinates_and_has_two_escape_levels() {
        let mut text = VimTextSelection::new(vec!["a👩‍💻雪".into(), "x".into(), "abcdef".into()], 0);
        text.key('l');
        text.key('v');
        text.key('l');
        assert_eq!(text.excerpt(), "👩‍💻雪");
        text.key('o');
        assert_eq!(text.cursor.grapheme, 1);
        assert_eq!(text.escape(), TextAction::Continue);
        text.key('j');
        assert_eq!(text.cursor.grapheme, 0);
        text.key('j');
        assert_eq!(text.cursor.grapheme, 1);
        assert_eq!(text.escape(), TextAction::Exit);
    }

    #[test]
    fn word_objects_and_find_repeat_follow_the_displayed_text() {
        let mut text = VimTextSelection::new(vec!["open src/main.rs then turn".into()], 0);
        text.key('w');
        text.key('v');
        text.key('i');
        text.key('W');
        assert_eq!(text.key('y'), TextAction::Yank("src/main.rs".into()));
        text.key('0');
        text.key('t');
        text.key('t');
        let first = text.cursor;
        text.key(';');
        assert!(text.cursor > first);
        text.key(',');
        assert!(
            text.cursor
                < TextPos {
                    line: 0,
                    grapheme: 21
                }
        );
        text.key('y');
        assert_eq!(
            text.key('y'),
            TextAction::Yank("open src/main.rs then turn".into())
        );
    }
}
