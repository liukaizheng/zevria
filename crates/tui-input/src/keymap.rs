//! One binding catalogue. Printable text is resolved only in text-entry contexts.
use crate::action::Action;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyContext {
    Composer,
    Completion,
    Transcript,
    BlockNav,
    OverlayList,
    OverlayInspect,
    SkillsList,
    SkillsInspect,
    ModelList,
    ModelReasoning,
    ModelConfirm,
    PlanDecision,
    Question,
    TextEntry,
    Help,
}
impl KeyContext {
    pub fn title(self) -> &'static str {
        match self {
            Self::Composer => "Composer",
            Self::Completion => "Completion",
            Self::Transcript => "Normal",
            Self::BlockNav => "Selection",
            Self::OverlayList => "Sessions",
            Self::OverlayInspect => "Inspect",
            Self::SkillsList | Self::SkillsInspect => "Skills",
            Self::ModelList | Self::ModelReasoning | Self::ModelConfirm => "Models",
            Self::PlanDecision => "Plan decision",
            Self::Question => "Question",
            Self::TextEntry => "Text entry",
            Self::Help => "Help",
        }
    }
    pub fn has_help(self) -> bool {
        !matches!(self, Self::Composer | Self::Completion | Self::TextEntry)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub prefix: Option<char>,
}
const fn key(code: KeyCode) -> Key {
    Key {
        code,
        modifiers: KeyModifiers::NONE,
        prefix: None,
    }
}
const fn ctrl(code: KeyCode) -> Key {
    Key {
        code,
        modifiers: KeyModifiers::CONTROL,
        prefix: None,
    }
}
const fn chord(prefix: char, next: char) -> Key {
    Key {
        prefix: Some(prefix),
        ..key(KeyCode::Char(next))
    }
}

pub struct Binding {
    pub action: Action,
    pub keys: &'static [Key],
    /// Action description, not a second spelling of the key.
    pub label: &'static str,
    /// Smaller values win in narrow footers; zero means help-only.
    pub primary: u8,
    pub group: &'static str,
}
impl Binding {
    pub fn key_label(&self) -> String {
        let mut labels = Vec::new();
        for key in self.keys {
            let label = key_label(key.code, key.modifiers);
            let label = key
                .prefix
                .map_or_else(|| label.clone(), |prefix| format!("{prefix}{label}"));
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        labels.join("/")
    }
    pub fn hint_key_label(&self) -> String {
        let key = self.keys[0];
        let label = key_label(key.code, key.modifiers);
        key.prefix
            .map_or_else(|| label.clone(), |prefix| format!("{prefix}{label}"))
    }
    pub fn matches_key(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.keys
            .iter()
            .any(|key| key.prefix.is_none() && key.code == code && key.modifiers == modifiers)
    }
}

/// Canonical terminal key spelling, also used by form footers.
pub fn key_label(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut modifiers = modifiers;
    if code == KeyCode::BackTab {
        modifiers |= KeyModifiers::SHIFT;
    }
    let name = match code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(ch) if modifiers.contains(KeyModifiers::CONTROL) => {
            ch.to_ascii_uppercase().to_string()
        }
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    let mut label = String::new();
    for (modifier, text) in [
        (KeyModifiers::CONTROL, "Ctrl+"),
        (KeyModifiers::ALT, "Alt+"),
        (KeyModifiers::SHIFT, "Shift+"),
    ] {
        if modifiers.contains(modifier) {
            label.push_str(text);
        }
    }
    label.push_str(&name);
    label
}

use KeyCode::*;
macro_rules! b {
    ($action:ident, $label:literal, $primary:literal, $group:literal, $($key:expr),+ $(,)?) => {
        Binding { action: Action::$action, label: $label, primary: $primary, group: $group, keys: &[$($key),+] }
    };
}
const GLOBAL: &[Binding] = &[b!(Cancel, "cancel", 4, "Session", ctrl(Char('c')))];
const PANES: &[Binding] = &[
    b!(ReturnRoot, "root", 3, "Panes", ctrl(Char('o'))),
    b!(
        LatestPane,
        "latest pane",
        0,
        "Panes",
        key(Tab),
        ctrl(Char('i'))
    ),
];
const PAGES: &[Binding] = &[
    b!(
        PageUp,
        "page up",
        0,
        "Navigation",
        key(PageUp),
        ctrl(Char('b'))
    ),
    b!(
        PageDown,
        "page down",
        0,
        "Navigation",
        key(PageDown),
        ctrl(Char('f'))
    ),
    b!(HalfPageUp, "half page up", 0, "Navigation", ctrl(Char('u'))),
    b!(
        HalfPageDown,
        "half page down",
        0,
        "Navigation",
        ctrl(Char('d'))
    ),
    b!(Home, "start", 0, "Navigation", key(Home)),
    b!(End, "end", 0, "Navigation", key(End)),
];
const PHYSICAL_PAGES: &[Binding] = &[
    b!(PageUp, "page up", 0, "Navigation", key(PageUp)),
    b!(PageDown, "page down", 0, "Navigation", key(PageDown)),
    b!(Home, "start", 0, "Navigation", key(Home)),
    b!(End, "end", 0, "Navigation", key(End)),
];
const USER_JUMPS: &[Binding] = &[
    b!(HalfPageUp, "previous user", 0, "Selection", ctrl(Char('u'))),
    b!(HalfPageDown, "next user", 0, "Selection", ctrl(Char('d'))),
];
const LIST: &[Binding] = &[
    b!(Down, "next", 2, "Navigation", key(Char('j')), key(Down)),
    b!(Up, "previous", 0, "Navigation", key(Char('k')), key(Up)),
    b!(Home, "start", 0, "Navigation", key(Char('g'))),
    b!(End, "end", 0, "Navigation", key(Char('G'))),
    b!(Close, "close", 3, "Actions", key(Esc), key(Char('q'))),
];
const CONFIRM: &[Binding] = &[b!(Confirm, "confirm", 1, "Actions", key(Enter))];
const FILTER: &[Binding] = &[b!(Filter, "filter", 4, "Actions", key(Char('/')))];
const INSPECT: &[Binding] = &[b!(
    Back,
    "back",
    2,
    "Actions",
    key(Char('b')),
    key(Left),
    key(Backspace)
)];
const SKILLS: &[Binding] = &[
    b!(Toggle, "toggle", 3, "Skills", key(Char(' '))),
    b!(Reload, "reload", 0, "Skills", key(Char('r'))),
];
const EDITOR: &[Binding] = &[
    b!(Submit, "send", 1, "Editing", ctrl(Enter)),
    b!(Newline, "newline", 0, "Editing", key(Enter)),
    b!(Close, "normal", 2, "Editing", key(Esc)),
    b!(Backspace, "delete left", 0, "Editing", key(Backspace)),
    b!(Left, "left", 0, "Editing", key(Left)),
    b!(Right, "right", 0, "Editing", key(Right)),
    b!(WordLeft, "word left", 0, "Editing", ctrl(Left)),
    b!(WordRight, "word right", 0, "Editing", ctrl(Right)),
    b!(Up, "up", 0, "Editing", key(Up)),
    b!(Down, "down", 0, "Editing", key(Down)),
    b!(Home, "line start", 0, "Editing", key(Home)),
    b!(End, "line end", 0, "Editing", key(End)),
    b!(PageUp, "page up", 0, "Editing", key(PageUp)),
    b!(PageDown, "page down", 0, "Editing", key(PageDown)),
    b!(Paste, "paste", 0, "Editing", ctrl(Char('v'))),
    b!(Undo, "undo", 3, "Editing", ctrl(Char('z'))),
    b!(Redo, "redo", 4, "Editing", ctrl(Char('y'))),
    b!(DeleteToEnd, "delete to end", 0, "Editing", ctrl(Char('k'))),
    b!(
        DeleteLine,
        "delete line",
        0,
        "Editing",
        Key {
            modifiers: KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            ..key(Char('k'))
        },
        Key {
            modifiers: KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            ..key(Char('K'))
        }
    ),
];
const MODE: &[Binding] = &[b!(
    ToggleMode,
    "mode",
    3,
    "Session",
    key(BackTab),
    Key {
        modifiers: KeyModifiers::SHIFT,
        ..key(BackTab)
    }
)];
const COMPLETION: &[Binding] = &[
    b!(AcceptCompletion, "accept/run", 1, "Completion", key(Enter)),
    b!(
        Complete,
        "complete",
        1,
        "Completion",
        key(Tab),
        ctrl(Char('i'))
    ),
];
const NORMAL: &[Binding] = &[
    b!(
        PreviousTurn,
        "previous turn start",
        0,
        "Navigation",
        key(Char('['))
    ),
    b!(NextTurn, "next turn start", 0, "Navigation", key(Char(']'))),
    b!(Insert, "input", 1, "Actions", key(Char('i'))),
    b!(Select, "select", 2, "Actions", key(Char('v'))),
    b!(PlanReview, "plan choices", 3, "Actions", key(Char('p'))),
    b!(RecoverDraft, "recover draft", 1, "Actions", key(Char('r'))),
    b!(
        ConfirmWorker,
        "confirm worker",
        1,
        "Actions",
        key(Char('c')),
        ctrl(Char('y'))
    ),
    b!(Diagnostics, "diagnostics", 0, "Actions", key(Char('d'))),
    b!(Confirm, "retry handoff", 1, "Actions", key(Enter)),
];
const TRANSCRIPT: &[Binding] = &[
    b!(
        Down,
        "scroll down",
        4,
        "Navigation",
        key(Char('j')),
        key(Down)
    ),
    b!(Up, "scroll up", 0, "Navigation", key(Char('k')), key(Up)),
    b!(Home, "start", 0, "Navigation", chord('g', 'g')),
    b!(End, "bottom", 0, "Navigation", key(Char('G'))),
    b!(Close, "normal / select", 0, "Actions", key(Esc)),
    b!(FoldTurns, "fold turns", 0, "Folds", chord('z', 'm')),
    b!(FoldOlder, "fold older turns", 0, "Folds", chord('z', 'M')),
    b!(UnfoldAll, "unfold all", 0, "Folds", chord('z', 'R')),
];
const SELECTION: &[Binding] = &[
    b!(Copy, "copy/params", 1, "Selection", key(Char('y'))),
    b!(CopyOutput, "output", 0, "Selection", chord('y', 'y')),
    b!(CopyList, "list", 0, "Selection", key(Char('Y'))),
    b!(Edit, "edit", 3, "Selection", ctrl(Char('e'))),
    b!(
        Confirm,
        "blocks / inspect child",
        2,
        "Selection",
        key(Enter)
    ),
    b!(FoldToggle, "toggle fold", 0, "Folds", chord('z', 'a')),
    b!(FoldClose, "close fold", 0, "Folds", chord('z', 'c')),
    b!(FoldOpen, "open fold", 0, "Folds", chord('z', 'o')),
];
const PLAN: &[Binding] = &[
    b!(Copy, "copy", 2, "Plan", key(Char('y')), ctrl(Char('y'))),
    b!(PlanCurrent, "implement", 0, "Plan", key(Char('1'))),
    b!(PlanFresh, "implement fresh", 0, "Plan", key(Char('2'))),
    b!(
        PlanRevise,
        "revise / return",
        0,
        "Plan",
        key(Char('3')),
        key(Char('n'))
    ),
];
const QUESTION: &[Binding] = &[
    b!(Confirm, "answer", 1, "Question", key(Enter)),
    b!(Close, "dismiss", 2, "Question", key(Esc), key(Char('q'))),
    b!(Home, "first choice", 0, "Question", key(Char('g'))),
    b!(End, "last choice", 0, "Question", key(Char('G'))),
    b!(Down, "next", 3, "Question", key(Down), key(Char('j'))),
    b!(Up, "previous", 0, "Question", key(Up), key(Char('k'))),
    b!(Toggle, "toggle", 3, "Question", key(Char(' '))),
    b!(Back, "previous question", 0, "Question", key(Left)),
];
const TEXT: &[Binding] = &[
    b!(Confirm, "accept", 1, "Text entry", key(Enter)),
    b!(Close, "back", 2, "Text entry", key(Esc)),
    b!(Backspace, "delete left", 0, "Text entry", key(Backspace)),
    b!(Delete, "delete right", 0, "Text entry", key(Delete)),
    b!(Left, "left", 0, "Text entry", key(Left)),
    b!(Right, "right", 0, "Text entry", key(Right)),
    b!(WordLeft, "word left", 0, "Text entry", ctrl(Left)),
    b!(WordRight, "word right", 0, "Text entry", ctrl(Right)),
    b!(Home, "start", 0, "Text entry", key(Home)),
    b!(End, "end", 0, "Text entry", key(End)),
];
const HELP: &[Binding] = &[b!(Help, "help", 5, "Help", key(Char('?')))];
const HELP_CLOSE: &[Binding] = &[b!(Help, "close", 5, "Help", key(Char('?')))];

pub fn bindings(context: KeyContext) -> impl Iterator<Item = &'static Binding> {
    use KeyContext::*;
    let groups: &[&[Binding]] = match context {
        Composer => &[EDITOR, MODE, PANES, GLOBAL],
        Completion => &[COMPLETION, EDITOR, MODE, &PANES[..1], GLOBAL],
        Transcript => &[NORMAL, TRANSCRIPT, PAGES, MODE, PANES, GLOBAL, HELP],
        BlockNav => &[
            SELECTION,
            TRANSCRIPT,
            PHYSICAL_PAGES,
            USER_JUMPS,
            PANES,
            GLOBAL,
            HELP,
        ],
        OverlayList => &[CONFIRM, LIST, PAGES, GLOBAL, HELP],
        OverlayInspect => &[LIST, PAGES, GLOBAL, HELP],
        SkillsList => &[CONFIRM, SKILLS, FILTER, LIST, PAGES, GLOBAL, HELP],
        SkillsInspect => &[INSPECT, SKILLS, FILTER, LIST, PAGES, GLOBAL, HELP],
        ModelList => &[CONFIRM, FILTER, LIST, PAGES, GLOBAL, HELP],
        ModelReasoning => &[CONFIRM, INSPECT, LIST, PAGES, GLOBAL, HELP],
        ModelConfirm => &[CONFIRM, LIST, PAGES, GLOBAL, HELP],
        PlanDecision => &[CONFIRM, PLAN, LIST, PHYSICAL_PAGES, GLOBAL, HELP],
        Question => &[QUESTION, PAGES, GLOBAL, HELP],
        TextEntry => &[TEXT, GLOBAL],
        Help => &[LIST, PAGES, GLOBAL, HELP_CLOSE],
    };
    // Preserve first binding precedence (completion before editor, for example).
    groups
        .iter()
        .flat_map(|group| group.iter())
        .collect::<Vec<_>>()
        .into_iter()
}

#[derive(Debug, Default)]
pub struct ChordState {
    context: Option<KeyContext>,
    prefix: Option<char>,
}
impl ChordState {
    pub fn clear(&mut self) {
        self.prefix = None;
        self.context = None;
    }
    pub fn pending(&self, prefix: char) -> bool {
        self.prefix == Some(prefix)
    }
    pub fn resolve(&mut self, context: KeyContext, mut event: KeyEvent) -> Option<Action> {
        if event.kind != KeyEventKind::Press {
            return None;
        }
        if let Char(ch) = event.code {
            if event.modifiers == KeyModifiers::SHIFT {
                event.modifiers = KeyModifiers::NONE;
            }
            if event.modifiers == KeyModifiers::CONTROL {
                event.code = Char(ch.to_ascii_lowercase());
            }
        }
        let prefix = if self.context == Some(context) {
            self.prefix.take()
        } else {
            self.prefix = None;
            None
        };
        self.context = Some(context);
        if let Some(prefix) = prefix
            && let Some(binding) = bindings(context).find(|b| {
                b.keys.iter().any(|key| {
                    key.prefix == Some(prefix)
                        && key.code == event.code
                        && key.modifiers == event.modifiers
                })
            })
        {
            return Some(binding.action);
        }
        if event.modifiers.is_empty()
            && let Char(ch @ ('g' | 'z')) = event.code
            && bindings(context).any(|b| b.keys.iter().any(|key| key.prefix == Some(ch)))
        {
            self.prefix = Some(ch);
            return None;
        }
        let action = bindings(context)
            .find(|b| b.matches_key(event.code, event.modifiers))
            .map(|b| b.action)
            .filter(|action| {
                !(prefix == Some('z') && matches!(action, Action::RecoverDraft | Action::Confirm))
            })
            .or_else(|| {
                if matches!(
                    context,
                    KeyContext::Composer | KeyContext::Completion | KeyContext::TextEntry
                ) && !event.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) && let Char(ch) = event.code
                {
                    return Some(Action::Type(ch));
                }
                None
            });
        // A first y is immediately useful; the second is an output-copy intent.
        // The selected entity still owns the timed, same-row admission check.
        if context == KeyContext::BlockNav && action == Some(Action::Copy) {
            self.prefix = Some('y');
        }
        action
    }
}

impl Action {
    pub fn list_action(self) -> Option<zevria_tui_widgets::overlay::ListAction> {
        use zevria_tui_widgets::overlay::ListAction as L;
        Some(match self {
            Self::Up => L::Up,
            Self::Down => L::Down,
            Self::PageUp => L::PageUp,
            Self::PageDown => L::PageDown,
            Self::HalfPageUp => L::HalfPageUp,
            Self::HalfPageDown => L::HalfPageDown,
            Self::Home => L::Home,
            Self::End => L::End,
            Self::Close | Self::Cancel => L::Close,
            Self::Confirm => L::Confirm,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chords_are_context_scoped_and_intervening_keys_consume_them() {
        let mut map = ChordState::default();
        let event = |ch| KeyEvent::new(Char(ch), KeyModifiers::NONE);
        assert_eq!(map.resolve(KeyContext::Transcript, event('g')), None);
        assert_eq!(
            map.resolve(KeyContext::Transcript, event('g')),
            Some(Action::Home)
        );
        for (ch, action) in [
            ('a', Action::FoldToggle),
            ('c', Action::FoldClose),
            ('o', Action::FoldOpen),
            ('m', Action::FoldTurns),
            ('M', Action::FoldOlder),
            ('R', Action::UnfoldAll),
        ] {
            assert_eq!(map.resolve(KeyContext::BlockNav, event('z')), None);
            assert_eq!(map.resolve(KeyContext::BlockNav, event(ch)), Some(action));
        }
        assert_eq!(
            map.resolve(KeyContext::BlockNav, event('y')),
            Some(Action::Copy)
        );
        assert_eq!(
            map.resolve(KeyContext::BlockNav, event('y')),
            Some(Action::CopyOutput)
        );
        map.resolve(KeyContext::Transcript, event('g'));
        assert_eq!(
            map.resolve(KeyContext::Composer, event('g')),
            Some(Action::Type('g'))
        );
        map.resolve(KeyContext::Transcript, event('z'));
        map.resolve(KeyContext::Transcript, event('j'));
        assert_eq!(map.resolve(KeyContext::Transcript, event('R')), None);
    }
    #[test]
    fn overlays_share_keys_but_text_entry_keeps_printable_characters() {
        for context in [
            KeyContext::OverlayList,
            KeyContext::SkillsList,
            KeyContext::SkillsInspect,
            KeyContext::ModelList,
            KeyContext::ModelReasoning,
            KeyContext::ModelConfirm,
            KeyContext::Help,
        ] {
            let mut map = ChordState::default();
            for (ch, action) in [
                ('j', Action::Down),
                ('k', Action::Up),
                ('q', Action::Close),
                ('g', Action::Home),
                ('G', Action::End),
                ('?', Action::Help),
            ] {
                assert_eq!(
                    map.resolve(context, KeyEvent::new(Char(ch), KeyModifiers::NONE)),
                    Some(action)
                );
            }
        }
        for ch in ['j', 'q', '?'] {
            assert_eq!(
                ChordState::default().resolve(
                    KeyContext::TextEntry,
                    KeyEvent::new(Char(ch), KeyModifiers::NONE)
                ),
                Some(Action::Type(ch))
            );
        }
    }
    #[test]
    fn every_unshadowed_catalogue_binding_resolves_in_its_context() {
        for context in [
            KeyContext::Composer,
            KeyContext::Completion,
            KeyContext::Transcript,
            KeyContext::BlockNav,
            KeyContext::OverlayList,
            KeyContext::OverlayInspect,
            KeyContext::SkillsList,
            KeyContext::SkillsInspect,
            KeyContext::ModelList,
            KeyContext::ModelReasoning,
            KeyContext::ModelConfirm,
            KeyContext::PlanDecision,
            KeyContext::Question,
            KeyContext::TextEntry,
            KeyContext::Help,
        ] {
            let mut seen = Vec::new();
            for binding in bindings(context) {
                for key in binding.keys {
                    let signature = (key.prefix, key.code, key.modifiers);
                    if seen.contains(&signature) {
                        continue;
                    }
                    seen.push(signature);
                    let mut resolver = ChordState::default();
                    if let Some(prefix) = key.prefix {
                        resolver.resolve(context, KeyEvent::new(Char(prefix), KeyModifiers::NONE));
                    }
                    assert_eq!(
                        resolver.resolve(context, KeyEvent::new(key.code, key.modifiers)),
                        Some(binding.action),
                        "{context:?}: {}",
                        binding.key_label()
                    );
                }
            }
        }
    }

    #[test]
    fn turn_navigation_is_normal_only_and_brackets_remain_text() {
        for (ch, action) in [('[', Action::PreviousTurn), (']', Action::NextTurn)] {
            let event = KeyEvent::new(Char(ch), KeyModifiers::NONE);
            assert_eq!(
                ChordState::default().resolve(KeyContext::Transcript, event),
                Some(action)
            );
            for context in [
                KeyContext::Composer,
                KeyContext::Completion,
                KeyContext::TextEntry,
            ] {
                assert_eq!(
                    ChordState::default().resolve(context, event),
                    Some(Action::Type(ch))
                );
            }
            for context in [
                KeyContext::BlockNav,
                KeyContext::OverlayList,
                KeyContext::OverlayInspect,
                KeyContext::SkillsList,
                KeyContext::SkillsInspect,
                KeyContext::ModelList,
                KeyContext::ModelReasoning,
                KeyContext::ModelConfirm,
                KeyContext::PlanDecision,
                KeyContext::Question,
                KeyContext::Help,
            ] {
                assert_eq!(
                    ChordState::default().resolve(context, event),
                    None,
                    "{context:?}"
                );
            }
            let binding = bindings(KeyContext::Transcript)
                .find(|binding| binding.action == action)
                .unwrap();
            assert_eq!(binding.group, "Navigation");
            assert_eq!(binding.primary, 0, "help only, not the compact footer");
        }
        for (ch, action) in [('u', Action::HalfPageUp), ('d', Action::HalfPageDown)] {
            assert_eq!(
                ChordState::default().resolve(
                    KeyContext::BlockNav,
                    KeyEvent::new(Char(ch), KeyModifiers::CONTROL)
                ),
                Some(action)
            );
        }
    }

    #[test]
    fn labels_have_one_spelling() {
        assert_eq!(key_label(Char('c'), KeyModifiers::CONTROL), "Ctrl+C");
        assert_eq!(key_label(Enter, KeyModifiers::CONTROL), "Ctrl+Enter");
        assert_eq!(key_label(BackTab, KeyModifiers::NONE), "Shift+Tab");
        assert_eq!(key_label(PageUp, KeyModifiers::NONE), "PgUp");
    }
}
