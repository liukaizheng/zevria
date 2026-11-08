//! Desktop-only disclosure policy and tail identity. No renderer or provider state.
use std::collections::{HashMap, HashSet};
use zevria_presentation::blocks::NativeHeader;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Expanded,
    Collapsed,
}
impl Choice {
    pub fn expanded(self) -> bool {
        self == Self::Expanded
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    Live,
    /// Supplied during the current turn, but no longer the streaming tip.
    Working,
    #[default]
    Finished,
    Restored,
    Interrupted,
    Unsaved,
}

pub fn default_expanded(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Live | Phase::Working | Phase::Interrupted | Phase::Unsaved
    )
}

#[derive(Default)]
pub struct DisclosureStore {
    choices: HashMap<String, Choice>,
    phases: HashMap<String, Phase>,
    pending_folds: HashSet<String>,
}
impl DisclosureStore {
    pub fn observe(&mut self, key: &str, phase: Phase) {
        if phase == Phase::Finished
            && self
                .phases
                .get(key)
                .is_some_and(|p| matches!(p, Phase::Live | Phase::Working))
            && !self.choices.contains_key(key)
        {
            self.pending_folds.insert(key.to_owned());
        }
        self.phases.insert(key.to_owned(), phase);
    }
    pub fn choose(&mut self, key: &str, expanded: bool) {
        self.choices.insert(
            key.to_owned(),
            if expanded {
                Choice::Expanded
            } else {
                Choice::Collapsed
            },
        );
        self.pending_folds.remove(key);
    }
    pub fn expanded(&self, key: &str, phase: Phase) -> bool {
        self.choices.get(key).map_or_else(
            || default_expanded(phase) || self.pending_folds.contains(key),
            |choice| choice.expanded(),
        )
    }
    /// Returns exactly the rows whose default disclosure changed.
    pub fn follow_tail(&mut self, following: bool) -> Vec<String> {
        if following {
            self.pending_folds.drain().collect()
        } else {
            Vec::new()
        }
    }
    pub fn rekey(&mut self, old: &str, new: &str) {
        if let Some(choice) = self.choices.remove(old) {
            self.choices.insert(new.to_owned(), choice);
        }
        if let Some(phase) = self.phases.remove(old) {
            self.phases.insert(new.to_owned(), phase);
        }
        if self.pending_folds.remove(old) {
            self.pending_folds.insert(new.to_owned());
        }
    }
    pub fn retain(&mut self, keys: &HashSet<String>) {
        self.choices.retain(|key, _| keys.contains(key));
        self.phases.retain(|key, _| keys.contains(key));
        self.pending_folds.retain(|key| keys.contains(key));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TailKind {
    Thinking,
    Answer,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TailIdentity {
    pub header: Option<NativeHeader>,
    pub kind: TailKind,
    /// Ordinal among non-tool blocks, not visual row position.
    pub ordinal: usize,
}
impl TailIdentity {
    pub fn key(self, epoch: &str) -> String {
        format!(
            "{epoch}-tail-{:?}-{:?}-{}",
            self.header, self.kind, self.ordinal
        )
    }
}
/// Only transfer retained parsing/disclosure to a compatible canonical block.
pub fn can_commit(tail: TailIdentity, committed: TailIdentity, before: &str, after: &str) -> bool {
    tail == committed && (after.starts_with(before) || before.starts_with(after))
}

/// First supplied non-empty line, without Markdown presentation markers.
pub fn preview(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let line = line.trim_start_matches(['#', '>', '-', '+', '*', ' ']);
    let mut result = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' | '*' | '_' | '~' | '[' | ']' => {
                if ch == ']' && chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    for ch in chars.by_ref() {
                        if ch == '(' {
                            depth += 1;
                        }
                        if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            _ => result.push(ch),
        }
    }
    result.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_presentation::blocks::DisplayTurn;
    #[test]
    fn preview_uses_only_provider_text() {
        assert_eq!(
            preview("\n\n### **Trace** the [source](file.rs) with `rg`\nnext line"),
            "Trace the source with rg"
        );
        assert_eq!(preview(" \n "), "");
    }
    #[test]
    fn lifecycle_policy_and_deferred_fold_respect_explicit_choices() {
        for phase in [
            Phase::Live,
            Phase::Working,
            Phase::Interrupted,
            Phase::Unsaved,
        ] {
            assert!(default_expanded(phase));
        }
        for phase in [Phase::Finished, Phase::Restored] {
            assert!(!default_expanded(phase));
        }
        let mut store = DisclosureStore::default();
        store.observe("a", Phase::Live);
        store.observe("a", Phase::Finished);
        assert!(store.expanded("a", Phase::Finished));
        assert!(store.follow_tail(false).is_empty());
        assert_eq!(store.follow_tail(true), ["a"]);
        assert!(!store.expanded("a", Phase::Finished));
        store.choose("a", true);
        store.observe("a", Phase::Working);
        store.observe("a", Phase::Finished);
        store.follow_tail(true);
        assert!(store.expanded("a", Phase::Finished));
        store.choose("a", false);
        assert!(!store.expanded("a", Phase::Live));
    }
    #[test]
    fn tail_commit_transfer_and_next_call_do_not_share_choices() {
        let a = TailIdentity {
            header: Some(NativeHeader::Assistant {
                turn: DisplayTurn(1),
                call: 1,
            }),
            kind: TailKind::Thinking,
            ordinal: 0,
        };
        let b = TailIdentity {
            header: Some(NativeHeader::Assistant {
                turn: DisplayTurn(2),
                call: 1,
            }),
            ..a
        };
        assert_ne!(a.key("epoch"), b.key("epoch"));
        assert!(can_commit(a, a, "Read", "Read source"));
        assert!(!can_commit(a, b, "Read", "Read source"));
        assert!(!can_commit(a, a, "Read", "Different thought"));
        let mut store = DisclosureStore::default();
        store.observe(&a.key("epoch"), Phase::Live);
        store.choose(&a.key("epoch"), false);
        store.rekey(&a.key("epoch"), "committed");
        assert!(!store.expanded("committed", Phase::Live));
        assert!(store.expanded(&b.key("epoch"), Phase::Live));
        store.retain(&HashSet::new());
        assert!(store.expanded("committed", Phase::Live));
    }
}
