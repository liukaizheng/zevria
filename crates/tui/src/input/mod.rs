//! One input owner, shared by routing, layering, navigation and cursor policy.

mod action;
mod keymap;
pub(crate) use action::Action;
pub(crate) use keymap::{ChordState, KeyContext};
mod capabilities;
pub(crate) use capabilities::{Capabilities, DisabledReason};

use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

static NEXT_PANE: AtomicU64 = AtomicU64::new(1);

/// Frontend identity, unrelated to vector positions or durable protocol IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct PaneId(u64);

impl Default for PaneId {
    fn default() -> Self {
        Self(NEXT_PANE.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum SurfaceKind {
    Transcript,
    Selection,
    Composer,
    Completion,
    PlanReview,
    Skills,
    Sessions,
    Models,
    Question,
    Help,
}

impl SurfaceKind {
    pub(crate) const fn layer(self) -> u8 {
        match self {
            Self::Transcript | Self::Selection | Self::Composer => 0,
            Self::Completion | Self::PlanReview => 1,
            Self::Skills => 2,
            Self::Sessions => 3,
            Self::Models => 4,
            Self::Question => 5,
            Self::Help => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct SurfaceId {
    pub pane: PaneId,
    pub kind: SurfaceKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScrollTarget {
    Transcript,
    Editor,
    List,
    Form,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Surface {
    pub id: SurfaceId,
    pub layer: u8,
    pub captures: bool,
    pub pane_navigation: bool,
    pub scroll: ScrollTarget,
    pub cursor: bool,
}

impl Surface {
    pub(crate) fn new(pane: PaneId, kind: SurfaceKind) -> Self {
        use SurfaceKind::*;
        let (captures, pane_navigation, scroll, cursor) = match kind {
            Transcript | Selection => (false, true, ScrollTarget::Transcript, false),
            Composer => (false, true, ScrollTarget::Editor, true),
            Completion => (false, true, ScrollTarget::List, true),
            PlanReview => (false, true, ScrollTarget::Transcript, false),
            Skills => (true, false, ScrollTarget::List, false),
            Sessions => (true, false, ScrollTarget::List, false),
            Models => (true, false, ScrollTarget::List, false),
            Question => (true, false, ScrollTarget::Form, true),
            Help => (true, false, ScrollTarget::List, false),
        };
        Self {
            id: SurfaceId { pane, kind },
            layer: kind.layer(),
            captures,
            pane_navigation,
            scroll,
            cursor,
        }
    }

    pub(crate) fn context(self) -> KeyContext {
        match self.id.kind {
            SurfaceKind::Composer => KeyContext::Composer,
            SurfaceKind::Completion => KeyContext::Completion,
            SurfaceKind::Transcript => KeyContext::Transcript,
            SurfaceKind::Selection => KeyContext::BlockNav,
            SurfaceKind::PlanReview => KeyContext::PlanDecision,
            SurfaceKind::Question => KeyContext::Question,
            SurfaceKind::Skills => KeyContext::SkillsList,
            SurfaceKind::Models => KeyContext::ModelList,
            SurfaceKind::Sessions => KeyContext::OverlayList,
            SurfaceKind::Help => KeyContext::Help,
        }
    }

    pub(crate) fn action(self, key: KeyEvent) -> Option<Action> {
        ChordState::default().resolve(self.context(), key)
    }
}

/// Ascending paint order is also the precedence used for input resolution.
pub(crate) fn resolve(layers: impl IntoIterator<Item = Surface>) -> Surface {
    layers
        .into_iter()
        .max_by_key(|surface| surface.layer)
        .expect("a pane always owns a surface")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DraftRecoveryHint {
    None,
    Saved,
    Available,
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;

#[derive(Clone, Debug)]
pub(crate) enum UserInput {
    Key(KeyEvent),
    Paste(String),
}

pub(crate) enum InputEvent {
    User(UserInput),
    Resize,
    Focus { gained: bool },
    Ignored,
}

/// Key kind/modifiers are normalized once at the terminal boundary. Repeats are
/// intentionally ignored like releases: activation and destructive controls
/// must not repeat based on terminal keyboard-protocol support.
pub(crate) fn normalize(event: Event) -> InputEvent {
    match event {
        Event::Key(mut key) if key.kind == KeyEventKind::Press => {
            if key
                .modifiers
                .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
            {
                return InputEvent::Ignored;
            }
            if let KeyCode::Char(character) = key.code {
                if key.modifiers == KeyModifiers::SHIFT {
                    key.modifiers = NONE;
                } else if key.modifiers == CTRL {
                    key.code = KeyCode::Char(character.to_ascii_lowercase());
                }
            }
            InputEvent::User(UserInput::Key(key))
        }
        Event::Paste(text) => InputEvent::User(UserInput::Paste(text)),
        Event::Resize(..) => InputEvent::Resize,
        Event::FocusGained => InputEvent::Focus { gained: true },
        Event::FocusLost => InputEvent::Focus { gained: false },
        _ => InputEvent::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layering_and_capture_share_the_same_owner() {
        let pane = PaneId::default();
        let owner = resolve(
            [
                SurfaceKind::Composer,
                SurfaceKind::Models,
                SurfaceKind::Sessions,
                SurfaceKind::Question,
            ]
            .map(|kind| Surface::new(pane, kind)),
        );
        assert_eq!(owner.id.kind, SurfaceKind::Question);
        assert!(owner.captures);
        assert!(!owner.pane_navigation);
        assert_eq!(owner.action(KeyEvent::new(KeyCode::Tab, NONE)), None);
    }

    #[test]
    fn completion_separates_enter_activation_and_exclusively_owns_both_tab_encodings() {
        let surface = Surface::new(PaneId::default(), SurfaceKind::Completion);
        for (code, modifiers) in [(KeyCode::Tab, NONE), (KeyCode::Char('i'), CTRL)] {
            assert_eq!(
                surface.action(KeyEvent::new(code, modifiers)),
                Some(Action::Complete)
            );
        }
        assert_eq!(
            surface.action(KeyEvent::new(KeyCode::Enter, NONE)),
            Some(Action::AcceptCompletion)
        );
        assert_eq!(
            surface.action(KeyEvent::new(KeyCode::Enter, CTRL)),
            Some(Action::Submit)
        );
        let composer = Surface::new(PaneId::default(), SurfaceKind::Composer);
        assert_eq!(
            composer.action(KeyEvent::new(KeyCode::Enter, NONE)),
            Some(Action::Newline)
        );
        for modifiers in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            CTRL | KeyModifiers::SHIFT,
        ] {
            assert_eq!(
                surface.action(KeyEvent::new(KeyCode::Enter, modifiers)),
                None
            );
        }
        assert_eq!(
            surface.action(KeyEvent::new(KeyCode::Tab, KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn release_and_repeat_never_activate() {
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            for modifiers in [NONE, CTRL] {
                assert!(matches!(
                    normalize(Event::Key(KeyEvent::new_with_kind(
                        KeyCode::Enter,
                        modifiers,
                        kind
                    ))),
                    InputEvent::Ignored
                ));
            }
        }
    }
}
