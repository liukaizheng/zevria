//! Desktop display phases, retained streaming identity, and per-pane user intent.
use std::collections::{HashMap, HashSet};
use zevria_presentation::blocks::NativeHeader;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisclosureOverride {
    Expanded,
    Collapsed,
}

#[derive(Default, Debug)]
pub struct DisclosureState {
    overrides: HashMap<String, DisclosureOverride>,
}
impl DisclosureState {
    pub fn is_expanded(&self, key: &str, default: bool) -> bool {
        self.overrides
            .get(key)
            .map_or(default, |value| *value == DisclosureOverride::Expanded)
    }

    /// A click pins an intent even when it happens to match today's default.
    pub fn set(&mut self, key: String, expanded: bool) {
        self.overrides.insert(
            key,
            if expanded {
                DisclosureOverride::Expanded
            } else {
                DisclosureOverride::Collapsed
            },
        );
    }

    pub fn toggle(&mut self, key: String, currently_expanded: bool) {
        self.set(key, !currently_expanded);
    }

    pub fn expand_keys(&mut self, keys: impl IntoIterator<Item = String>) {
        for key in keys {
            self.set(key, true);
        }
    }

    pub fn collapse_keys(&mut self, keys: impl IntoIterator<Item = String>) {
        for key in keys {
            self.set(key, false);
        }
    }

    pub fn collapse_all(&mut self, live_keys: impl IntoIterator<Item = String>) {
        self.overrides
            .retain(|_, value| *value == DisclosureOverride::Collapsed);
        self.collapse_keys(live_keys);
    }

    pub fn has_any_expanded<'a>(&self, items: impl IntoIterator<Item = (&'a str, bool)>) -> bool {
        items
            .into_iter()
            .any(|(key, default)| self.is_expanded(key, default))
    }

    pub fn rekey(&mut self, old: &str, new: &str) {
        if let Some(value) = self.overrides.remove(old) {
            self.overrides.insert(new.to_owned(), value);
        }
    }

    pub fn retain_keys(&mut self, valid: &HashSet<&str>) {
        self.overrides.retain(|key, _| valid.contains(key.as_str()));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
/// Only transfer a retained renderer to a compatible canonical block.
pub fn can_commit(tail: TailIdentity, committed: TailIdentity, before: &str, after: &str) -> bool {
    tail == committed && (after.starts_with(before) || before.starts_with(after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_presentation::blocks::DisplayTurn;

    #[test]
    fn live_defaults_are_not_recorded_but_clicks_pin_across_commit() {
        let mut state = DisclosureState::default();
        assert!(state.is_expanded("thought", true));
        assert!(!state.is_expanded("thought", false));
        state.toggle("thought".into(), true);
        assert!(!state.is_expanded("thought", true));
        state.set("thought".into(), true);
        state.rekey("thought", "group");
        assert!(state.is_expanded("group", false));
        assert!(!state.is_expanded("thought", false));
    }

    #[test]
    fn bulk_actions_collapse_live_and_do_not_pin_future_items() {
        let mut state = DisclosureState::default();
        state.expand_keys(["one".into(), "two".into()]);
        assert!(state.has_any_expanded([("one", false), ("two", false)]));
        state.collapse_all(["two".into()]);
        assert!(!state.has_any_expanded([("one", false), ("two", true)]));
        assert!(state.is_expanded("future", true));
        state.retain_keys(&HashSet::new());
        assert!(state.is_expanded("two", true));
    }

    #[test]
    fn tail_commit_transfer_matches_only_the_same_call_and_content() {
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
    }
}
