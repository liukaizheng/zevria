//! Paste recovery for terminals without bracketed paste.
//!
//! The Windows console input API never reports `Event::Paste`: a paste arrives
//! as a stream of synthetic key records, and each pasted line feed is reported
//! as Ctrl+Enter, which the composer binds to Submit. Those records arrive
//! back to back, far faster than anyone types, so an input burst containing a
//! line break is folded into one `Event::Paste` at the terminal boundary.

use std::collections::VecDeque;
use std::io;
#[cfg(windows)]
use std::time::Duration;

use futures_util::StreamExt as _;
use ratatui::crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};

/// Longest silence still treated as part of one synthetic paste.
#[cfg(windows)]
const PASTE_GAP: Duration = Duration::from_millis(8);
/// Bound on one burst so a huge paste still renders progressively.
#[cfg(windows)]
const MAX_BURST: usize = 64 * 1024;

/// The text a key contributes to a paste, or `None` when the key ends a run.
fn paste_text(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        KeyCode::Char(character) => {
            let chorded = key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
            // AltGr is reported as Ctrl+Alt and still produces plain text.
            (chorded.is_empty() || chorded == KeyModifiers::CONTROL | KeyModifiers::ALT)
                .then_some(character)
        }
        _ => None,
    }
}

fn is_text_press(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind != KeyEventKind::Release && paste_text(key).is_some())
}

/// Fold each run of text keys that contains a line break into one paste.
/// Runs without a line break, and single keystrokes, are left untouched.
pub(crate) fn coalesce(events: Vec<Event>) -> Vec<Event> {
    let mut output = Vec::with_capacity(events.len());
    let mut run: Vec<Event> = Vec::new();
    let flush = |run: &mut Vec<Event>, output: &mut Vec<Event>| {
        let mut text = String::new();
        let mut presses = 0usize;
        for event in run.iter() {
            if let Event::Key(key) = event
                && key.kind != KeyEventKind::Release
                && let Some(character) = paste_text(key)
            {
                presses += 1;
                text.push(character);
            }
        }
        if presses >= 2 && text.contains('\n') {
            output.push(Event::Paste(text));
            run.clear();
        } else {
            output.append(run);
        }
    };
    for event in events {
        let in_run = match &event {
            Event::Key(key) if key.kind == KeyEventKind::Release => !run.is_empty(),
            Event::Key(key) => paste_text(key).is_some(),
            _ => false,
        };
        if in_run {
            run.push(event);
        } else {
            flush(&mut run, &mut output);
            output.push(event);
        }
    }
    flush(&mut run, &mut output);
    output
}

/// Terminal event source that reassembles key-record pastes.
///
/// Cancel-safe: every received event is stored on `self` before the next
/// await, so a dropped `next()` future loses nothing.
pub(crate) struct InputReader {
    stream: EventStream,
    #[cfg_attr(not(windows), allow(dead_code))]
    burst: Vec<Event>,
    ready: VecDeque<Event>,
}

impl InputReader {
    pub(crate) fn new() -> Self {
        Self {
            stream: EventStream::new(),
            burst: Vec::new(),
            ready: VecDeque::new(),
        }
    }

    pub(crate) async fn next(&mut self) -> Option<io::Result<Event>> {
        if let Some(event) = self.ready.pop_front() {
            return Some(Ok(event));
        }
        #[cfg(windows)]
        {
            self.next_burst().await
        }
        #[cfg(not(windows))]
        {
            self.stream.next().await
        }
    }

    #[cfg(windows)]
    async fn next_burst(&mut self) -> Option<io::Result<Event>> {
        if self.burst.is_empty() {
            match self.stream.next().await? {
                Ok(event) if is_text_press(&event) => self.burst.push(event),
                other => return Some(other),
            }
        }
        let mut failure = None;
        while self.burst.len() < MAX_BURST {
            match tokio::time::timeout(PASTE_GAP, self.stream.next()).await {
                Ok(Some(Ok(event))) => self.burst.push(event),
                Ok(Some(Err(error))) => {
                    failure = Some(error);
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        self.ready.extend(coalesce(std::mem::take(&mut self.burst)));
        match self.ready.pop_front() {
            Some(event) => {
                if let Some(error) = failure {
                    // Surface the stream error after the collected input.
                    tracing::warn!(%error, "terminal input error during paste burst");
                }
                Some(Ok(event))
            }
            None => failure.map(Err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))
    }

    fn ch(character: char) -> Event {
        press(KeyCode::Char(character), KeyModifiers::NONE)
    }

    #[test]
    fn line_feed_burst_becomes_one_paste() {
        let events = vec![
            ch('a'),
            press(KeyCode::Char('B'), KeyModifiers::SHIFT),
            press(KeyCode::Enter, KeyModifiers::CONTROL),
            ch('c'),
            press(KeyCode::Enter, KeyModifiers::NONE),
            press(KeyCode::Tab, KeyModifiers::NONE),
            ch('d'),
        ];
        assert_eq!(coalesce(events), vec![Event::Paste("aB\nc\n\td".into())]);
    }

    #[test]
    fn single_keystrokes_are_untouched() {
        for event in [
            press(KeyCode::Enter, KeyModifiers::CONTROL),
            press(KeyCode::Enter, KeyModifiers::NONE),
            ch('x'),
        ] {
            assert_eq!(coalesce(vec![event.clone()]), vec![event]);
        }
    }

    #[test]
    fn runs_without_line_breaks_stay_keys() {
        let events = vec![ch('a'), ch('b')];
        assert_eq!(coalesce(events.clone()), events);
    }

    #[test]
    fn releases_are_skipped_and_other_events_keep_order() {
        let events = vec![
            press(KeyCode::Up, KeyModifiers::NONE),
            ch('a'),
            release(KeyCode::Char('a')),
            press(KeyCode::Enter, KeyModifiers::CONTROL),
            release(KeyCode::Enter),
            ch('b'),
            release(KeyCode::Char('b')),
            press(KeyCode::Esc, KeyModifiers::NONE),
            press(KeyCode::Char('v'), KeyModifiers::CONTROL),
        ];
        assert_eq!(
            coalesce(events),
            vec![
                press(KeyCode::Up, KeyModifiers::NONE),
                Event::Paste("a\nb".into()),
                press(KeyCode::Esc, KeyModifiers::NONE),
                press(KeyCode::Char('v'), KeyModifiers::CONTROL),
            ]
        );
    }

    #[test]
    fn text_press_detection() {
        assert!(is_text_press(&ch('a')));
        assert!(is_text_press(&press(KeyCode::Enter, KeyModifiers::CONTROL)));
        assert!(!is_text_press(&press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_text_press(&release(KeyCode::Char('a'))));
    }
}
