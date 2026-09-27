//! Input focus, selection, and time-bounded key chords.

use std::time::{Duration, Instant};

use super::conversation::Selection;

const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(500);
const DOUBLE_YANK_WINDOW: Duration = Duration::from_millis(500);

/// A caller-supplied entry target and its persistent viewport intent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SelectionEntry {
    pub(crate) selection: Selection,
    pub(crate) reveal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectionScope {
    Message,
    Block,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActiveSelection {
    pub(crate) selection: Selection,
    pub(crate) scope: SelectionScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectionState {
    selection: Selection,
    scope: SelectionScope,
    reveal: bool,
    last_yank: Option<(Selection, Instant)>,
}

impl SelectionState {
    fn new(entry: SelectionEntry) -> Self {
        Self {
            selection: entry.selection,
            scope: SelectionScope::Message,
            reveal: entry.reveal,
            last_yank: None,
        }
    }

    pub(crate) const fn selection(&self) -> Selection {
        self.selection
    }

    pub(crate) const fn scope(&self) -> SelectionScope {
        self.scope
    }

    pub(crate) fn enter_block_scope(&mut self) {
        self.scope = SelectionScope::Block;
        self.reveal = false;
        self.clear_yank();
    }

    pub(crate) fn enter_message_scope(&mut self) {
        self.scope = SelectionScope::Message;
        self.reveal = false;
        self.clear_yank();
    }

    pub(crate) const fn reveal(&self) -> bool {
        self.reveal
    }

    pub(crate) fn request_reveal(&mut self) {
        self.reveal = true;
    }

    pub(crate) fn set_selection(&mut self, selection: Selection) {
        if self.selection != selection {
            self.request_reveal();
        }
        self.selection = selection;
        self.last_yank = None;
    }

    pub(crate) fn clear_yank(&mut self) {
        self.last_yank = None;
    }

    /// Return true for the second `y` on the same tool row within the chord
    /// window; otherwise arm the row for a later second press.
    pub(crate) fn press_yank(&mut self, now: Instant) -> bool {
        let selection = self.selection;
        let is_double = self.last_yank.take().is_some_and(|(previous, pressed)| {
            previous == selection && now.duration_since(pressed) <= DOUBLE_YANK_WINDOW
        });
        if !is_double {
            self.last_yank = Some((selection, now));
        }
        is_double
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum FocusState {
    #[default]
    Normal,
    Insert,
    Selecting(SelectionState),
}

#[derive(Debug, Default)]
struct ChordState {
    last_esc: Option<Instant>,
    keymap: crate::input::ChordState,
}

#[derive(Debug, Default)]
pub(crate) struct InteractionState {
    focus: FocusState,
    chords: ChordState,
}

impl InteractionState {
    pub(crate) const fn is_normal(&self) -> bool {
        matches!(self.focus, FocusState::Normal)
    }

    pub(crate) const fn is_insert(&self) -> bool {
        matches!(self.focus, FocusState::Insert)
    }

    pub(crate) const fn is_selecting(&self) -> bool {
        matches!(self.focus, FocusState::Selecting(_))
    }

    pub(crate) const fn selection(&self) -> Option<Selection> {
        match &self.focus {
            FocusState::Selecting(selection) => Some(selection.selection()),
            FocusState::Normal | FocusState::Insert => None,
        }
    }

    pub(crate) const fn selection_scope(&self) -> Option<SelectionScope> {
        match &self.focus {
            FocusState::Selecting(selection) => Some(selection.scope()),
            FocusState::Normal | FocusState::Insert => None,
        }
    }

    pub(crate) const fn active_selection(&self) -> Option<ActiveSelection> {
        match &self.focus {
            FocusState::Selecting(selection) => Some(ActiveSelection {
                selection: selection.selection(),
                scope: selection.scope(),
            }),
            FocusState::Normal | FocusState::Insert => None,
        }
    }

    pub(crate) const fn selection_reveal(&self) -> bool {
        match &self.focus {
            FocusState::Selecting(selection) => selection.reveal(),
            FocusState::Normal | FocusState::Insert => false,
        }
    }

    pub(crate) fn selection_state_mut(&mut self) -> Option<&mut SelectionState> {
        match &mut self.focus {
            FocusState::Selecting(selection) => Some(selection),
            FocusState::Normal | FocusState::Insert => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_focus(&mut self, focus: FocusState) {
        self.focus = focus;
        self.chords.last_esc = None;
        self.chords.keymap.clear();
    }

    pub(crate) fn enter_normal(&mut self) {
        self.focus = FocusState::Normal;
    }

    pub(crate) fn enter_insert(&mut self) {
        self.focus = FocusState::Insert;
        self.chords.last_esc = None;
    }

    #[cfg(test)]
    pub(crate) fn enter_selection(&mut self, selection: Selection) {
        self.enter_selection_entry(SelectionEntry {
            selection,
            reveal: true,
        });
    }

    /// Enter Message scope with the caller's viewport intent and no pending chords.
    pub(crate) fn enter_selection_entry(&mut self, entry: SelectionEntry) {
        self.focus = FocusState::Selecting(SelectionState::new(entry));
        self.clear_chords();
    }

    pub(crate) fn clear_selection(&mut self) {
        if self.is_selecting() {
            self.focus = FocusState::Normal;
        }
    }

    pub(crate) fn set_selection(&mut self, selection: Selection) -> bool {
        let Some(state) = self.selection_state_mut() else {
            return false;
        };
        state.set_selection(selection);
        true
    }

    /// Esc exits selection immediately. Otherwise it enters Normal focus and
    /// a second press within the configured window selects the supplied target,
    /// if available, retaining its reveal intent until explicit navigation.
    pub(crate) fn escape(&mut self, now: Instant, target: Option<SelectionEntry>) {
        if self.is_selecting() {
            self.focus = FocusState::Normal;
            self.chords.last_esc = None;
            return;
        }
        self.focus = FocusState::Normal;
        let is_double = self
            .chords
            .last_esc
            .take()
            .is_some_and(|previous| now.duration_since(previous) <= DOUBLE_ESC_WINDOW);
        if is_double {
            if let Some(entry) = target {
                self.enter_selection_entry(entry);
            }
        } else {
            self.chords.last_esc = Some(now);
        }
    }

    pub(crate) fn resolve(
        &mut self,
        context: crate::input::KeyContext,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<crate::input::Action> {
        self.chords.keymap.resolve(context, key)
    }

    #[cfg(test)]
    pub(crate) fn pending_g(&self) -> bool {
        self.chords.keymap.pending('g')
    }

    #[cfg(test)]
    pub(crate) fn pending_z(&self) -> bool {
        self.chords.keymap.pending('z')
    }

    pub(crate) fn interrupt_escape(&mut self) {
        self.chords.last_esc = None;
    }

    pub(crate) fn clear_chords(&mut self) {
        self.chords.last_esc = None;
        self.chords.keymap.clear();
        if let Some(selection) = self.selection_state_mut() {
            selection.clear_yank();
        }
    }

    pub(crate) fn clear_yank(&mut self) {
        if let Some(selection) = self.selection_state_mut() {
            selection.clear_yank();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.focus = FocusState::Normal;
        self.chords = ChordState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(index: usize) -> Selection {
        Selection {
            history_index: index,
            content_index: 0,
        }
    }

    #[test]
    fn double_escape_is_deterministic() {
        let base = Instant::now();
        let mut state = InteractionState::default();
        let target = Some(SelectionEntry {
            selection: selection(2),
            reveal: false,
        });
        state.escape(base, target);
        assert!(state.is_normal());
        state.escape(base + Duration::from_millis(400), target);
        assert_eq!(state.selection(), Some(selection(2)));
    }

    #[test]
    fn escape_respects_timing_target_availability_and_selection_exit() {
        let base = Instant::now();
        let target = Some(SelectionEntry {
            selection: selection(1),
            reveal: false,
        });
        for (millis, enters) in [(500, true), (501, false)] {
            let mut state = InteractionState::default();
            state.enter_insert();
            state.escape(base, target);
            assert!(state.is_normal());
            state.escape(base + Duration::from_millis(millis), target);
            assert_eq!(state.is_selecting(), enters);
            assert!(!state.selection_reveal());
        }
        let mut state = InteractionState::default();
        state.escape(base, target);
        state.escape(base + Duration::from_millis(100), None);
        assert!(state.is_normal(), "no target must not fall back");
        state.escape(base + Duration::from_millis(200), target);
        assert!(
            state.is_normal(),
            "a missing target consumed the previous pair"
        );
        state.escape(base + Duration::from_millis(300), target);
        assert!(state.is_selecting());
        state.escape(base + Duration::from_millis(400), target);
        assert!(state.is_normal());
        state.escape(base + Duration::from_millis(450), target);
        assert!(state.is_normal(), "selection exit does not arm a new chord");
    }

    #[test]
    fn selection_entry_preserves_reveal_intent_and_clears_pending_chords() {
        for reveal in [false, true] {
            for double_escape in [false, true] {
                let base = Instant::now();
                let entry = SelectionEntry {
                    selection: selection(2),
                    reveal,
                };
                let mut state = InteractionState::default();
                state.enter_selection(selection(1));
                let selected = state.selection_state_mut().unwrap();
                selected.enter_block_scope();
                selected.press_yank(base);
                if double_escape {
                    state.enter_normal();
                }
                state.chords.last_esc = Some(base);
                state.resolve(
                    crate::input::KeyContext::Transcript,
                    ratatui::crossterm::event::KeyEvent::new(
                        ratatui::crossterm::event::KeyCode::Char('g'),
                        ratatui::crossterm::event::KeyModifiers::NONE,
                    ),
                );
                state.resolve(
                    crate::input::KeyContext::Transcript,
                    ratatui::crossterm::event::KeyEvent::new(
                        ratatui::crossterm::event::KeyCode::Char('z'),
                        ratatui::crossterm::event::KeyModifiers::NONE,
                    ),
                );
                if double_escape {
                    state.escape(base + Duration::from_millis(100), Some(entry));
                } else {
                    state.enter_selection_entry(entry);
                }
                assert_eq!(state.selection(), Some(entry.selection));
                assert_eq!(state.selection_scope(), Some(SelectionScope::Message));
                assert_eq!(state.selection_reveal(), reveal);
                assert!(state.chords.last_esc.is_none());
                assert!(!state.pending_g());
                assert!(!state.pending_z());
                assert!(state.selection_state_mut().unwrap().last_yank.is_none());
            }
        }
    }

    #[test]
    fn reveal_intent_survives_same_selection_revalidation_and_yanking() {
        let base = Instant::now();
        let mut state = InteractionState::default();
        let target = Some(SelectionEntry {
            selection: selection(1),
            reveal: false,
        });
        state.escape(base, target);
        state.escape(base + Duration::from_millis(100), target);
        state.set_selection(selection(1));
        assert!(!state.selection_reveal());
        state.selection_state_mut().unwrap().press_yank(base);
        state.clear_chords();
        assert!(!state.selection_reveal());
        state.selection_state_mut().unwrap().request_reveal();
        assert!(state.selection_reveal(), "explicit navigation may clamp");
        state.enter_normal();
        assert!(!state.selection_reveal());
        state.escape(base, target);
        state.escape(base + Duration::from_millis(100), target);
        state.set_selection(selection(2));
        assert!(state.selection_reveal(), "changed coordinates reveal");
        state.enter_selection(selection(1));
        assert!(
            state.selection_reveal(),
            "programmatic selection preserves its default"
        );
        state.reset();
        assert!(!state.selection_reveal());
    }

    #[test]
    fn app_rejects_semantically_invalid_cached_targets_without_repair() {
        let mut app = crate::app::App::new();
        app.push_error("first".into());
        app.push_error("last".into());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        // Deliberately bypass normal geometry invalidation to exercise the
        // final semantic guard against stale index-based cache coordinates.
        *app.conversation.entry_mut(1).unwrap() = crate::app::HistoryEntry::CompactionDivider;
        let before = (app.view.scroll(), app.view.follow());
        let now = Instant::now();
        app.handle_normal_action(crate::input::Action::Select);
        assert!(app.interaction.is_normal());
        assert_eq!(app.interaction.selection(), None);
        assert_eq!((app.view.scroll(), app.view.follow()), before);
        app.handle_escape(now);
        app.handle_escape(now + Duration::from_millis(100));
        assert!(app.interaction.is_normal());
        assert_eq!(app.interaction.selection(), None);
        assert_eq!((app.view.scroll(), app.view.follow()), before);
    }

    #[test]
    fn z_prefix_is_consumed_and_cleared_by_focus_resets() {
        let mut state = InteractionState::default();
        state.resolve(
            crate::input::KeyContext::Transcript,
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('z'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert!(state.pending_z());
        state.resolve(
            crate::input::KeyContext::Transcript,
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('x'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        );
        assert!(!state.pending_z());
        for reset in 0..4 {
            state.resolve(
                crate::input::KeyContext::Transcript,
                ratatui::crossterm::event::KeyEvent::new(
                    ratatui::crossterm::event::KeyCode::Char('z'),
                    ratatui::crossterm::event::KeyModifiers::NONE,
                ),
            );
            match reset {
                0 => state.set_focus(FocusState::Insert),
                1 => state.enter_selection(selection(0)),
                2 => state.clear_chords(),
                _ => state.reset(),
            }
            assert!(!state.pending_z());
        }
    }

    #[test]
    fn selection_scopes_reset_reveal_and_yank_without_moving_the_cursor() {
        let now = Instant::now();
        let mut state = InteractionState::default();
        state.enter_selection(selection(2));
        assert_eq!(state.selection_scope(), Some(SelectionScope::Message));
        assert_eq!(
            state.active_selection(),
            Some(ActiveSelection {
                selection: selection(2),
                scope: SelectionScope::Message,
            })
        );
        let message_focus = state.focus.clone();
        let selected = state.selection_state_mut().unwrap();
        selected.press_yank(now);
        selected.enter_block_scope();
        assert!(!selected.reveal());
        assert!(selected.last_yank.is_none());
        assert_eq!(selected.selection(), selection(2));
        assert_ne!(state.focus, message_focus);
        assert_eq!(state.selection_scope(), Some(SelectionScope::Block));
        let selected = state.selection_state_mut().unwrap();
        selected.request_reveal();
        selected.press_yank(now);
        selected.enter_message_scope();
        assert!(!selected.reveal());
        assert!(selected.last_yank.is_none());
        assert_ne!(
            state.focus, message_focus,
            "reveal state is part of focus-state equality"
        );
        assert_eq!(state.selection_scope(), Some(SelectionScope::Message));
    }

    #[test]
    fn focus_resets_do_not_carry_block_scope_into_a_new_selection() {
        for reset in 0..3 {
            let mut state = InteractionState::default();
            state.enter_selection(selection(0));
            state.selection_state_mut().unwrap().enter_block_scope();
            match reset {
                0 => state.set_focus(FocusState::Normal),
                1 => state.reset(),
                _ => state.enter_selection(selection(1)),
            }
            if reset != 2 {
                assert_eq!(state.selection_scope(), None);
                state.enter_selection(selection(1));
            }
            assert_eq!(state.selection_scope(), Some(SelectionScope::Message));
        }
    }

    #[test]
    fn selection_owns_double_yank_timing() {
        let base = Instant::now();
        let mut state = InteractionState::default();
        state.enter_selection(selection(1));
        let selected = state.selection_state_mut().unwrap();
        assert!(!selected.press_yank(base));
        assert!(selected.press_yank(base + Duration::from_millis(100)));
    }
}
