//! Caret-local completion, independent of leading-command classification.
//!
//! File results and request/dismissal identity are UI state, never draft content.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Skill,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionQuery {
    pub kind: CompletionKind,
    /// Decoded text before the caret, not the whole replacement token.
    pub prefix: String,
    pub replacement: Range<usize>,
}

impl CompletionQuery {
    pub fn replacement(&self) -> Range<usize> {
        self.replacement.clone()
    }
}

/// Preserve the leading `/` and `$` palette's prefix-only replacement contract.
pub fn command_query(input: &str, cursor: usize) -> Option<CompletionQuery> {
    let kind = match input.as_bytes().first()? {
        b'/' => CompletionKind::Command,
        b'$' => CompletionKind::Skill,
        _ => return None,
    };
    let cursor = clamp_cursor(input, cursor).max(1);
    let prefix = input.get(1..cursor)?;
    if prefix.chars().any(char::is_whitespace) {
        return None;
    }
    Some(CompletionQuery {
        kind,
        prefix: prefix.into(),
        replacement: 0..cursor,
    })
}

/// Parse whole reference tokens while searching for the one containing the caret.
/// Quoted references may contain whitespace. A backslash escapes the next
/// character; the canonical encoder only needs to escape quotes and backslashes.
/// Registered image markers are hard boundaries, not searchable file text.
pub fn file_query(
    input: &str,
    cursor: usize,
    images: impl IntoIterator<Item = Range<usize>>,
) -> Option<CompletionQuery> {
    let cursor = clamp_cursor(input, cursor);
    let mut chars = input.char_indices().peekable();
    let mut graphemes = input.grapheme_indices(true).peekable();
    let mut boundary = true;
    while let Some((start, ch)) = chars.next() {
        if start >= cursor {
            break;
        }
        while graphemes.peek().is_some_and(|(offset, _)| *offset < start) {
            let (_, grapheme) = graphemes.next().expect("peeked grapheme");
            boundary = grapheme.chars().any(char::is_whitespace);
        }
        if ch != '@' || !boundary {
            continue;
        }
        let mut quoted = chars.peek().is_some_and(|(_, ch)| *ch == '"');
        if quoted {
            chars.next();
        }
        let mut prefix = String::new();
        let mut end = input.len();
        while let Some(&(offset, ch)) = chars.peek() {
            if !quoted && ch.is_whitespace() {
                end = offset;
                break;
            }
            chars.next();
            if ch == '"' && quoted {
                quoted = false;
                continue;
            }
            if ch == '\\' {
                if let Some(&(escaped_offset, escaped)) = chars.peek() {
                    chars.next();
                    if escaped_offset < cursor {
                        prefix.push(escaped);
                    }
                } else if offset < cursor {
                    prefix.push(ch);
                }
            } else if offset < cursor {
                prefix.push(ch);
            }
        }
        if cursor <= end {
            let replacement = start..end;
            // Whitespace with combining marks can make a character boundary
            // differ from an editor boundary. Never split a grapheme to accept.
            if clamp_cursor(input, start) != start
                || clamp_cursor(input, end) != end
                || images
                    .into_iter()
                    .any(|image| replacement.start < image.end && image.start < replacement.end)
            {
                return None;
            }
            return Some(CompletionQuery {
                kind: CompletionKind::File,
                prefix,
                replacement,
            });
        }
    }
    None
}

/// Reversible ordinary text, deliberately not an attachment marker.
pub fn file_reference(path: &str) -> String {
    if path
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\'))
    {
        format!("@\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("@{path}")
    }
}

/// Clamp an arbitrary byte offset to the preceding grapheme boundary.
pub fn clamp_cursor(input: &str, cursor: usize) -> usize {
    let cursor = cursor.min(input.len());
    if cursor == input.len() {
        return cursor;
    }
    input
        .grapheme_indices(true)
        .map(|(byte, _)| byte)
        .take_while(|byte| *byte <= cursor)
        .last()
        .unwrap_or(0)
}
