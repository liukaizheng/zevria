//! Desktop display phases and retained streaming-renderer identity.
use zevria_presentation::blocks::NativeHeader;

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
