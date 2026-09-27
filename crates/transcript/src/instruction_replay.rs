use crate::skill::*;
use crate::transcript::{SessionHeader, TranscriptItem};
use crate::{DirectivePayload, DirectiveState};
use anyhow::Context as _;
use rig_core::message::{AssistantContent, Message, UserContent};
use std::{collections::BTreeSet, fmt};

/// Pin-only validation. Deliberately independent of directive/header validity.
pub fn replay_active_skills(items: &[TranscriptItem]) -> anyhow::Result<ActiveSkills> {
    #[cfg(feature = "test-support")]
    crate::replay_probe::record(|counts| {
        counts.pin_replays += 1;
        counts.pin_records += items.len();
    });
    let mut active = ActiveSkills::default();
    for (index, item) in items.iter().enumerate() {
        apply_skill_record(&mut active, index, item)?;
    }
    Ok(active)
}

/// Shared lifecycle checks, including diagnostic locations, for both reducers.
fn apply_skill_record(
    active: &mut ActiveSkills,
    index: usize,
    item: &TranscriptItem,
) -> anyhow::Result<()> {
    (|| -> anyhow::Result<()> {
        match item {
            TranscriptItem::SkillInvocation(invocation) => {
                anyhow::ensure!(
                    invocation.name() == invocation.application().name(),
                    "skill invocation application name mismatch"
                );
                active.apply(invocation.application())?;
            }
            TranscriptItem::ToolResults {
                message,
                metadata,
                skill_applications,
            } => apply_tool_applications(active, message, metadata, skill_applications)?,
            TranscriptItem::Compaction(checkpoint) => checkpoint.validate()?,
            _ => {}
        }
        Ok(())
    })()
    .context(SkillLifecycleLocation(index))
}

#[derive(Debug)]
pub(crate) struct SkillLifecycleLocation(pub(crate) usize);
impl fmt::Display for SkillLifecycleLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid skill lifecycle at record {}", self.0 + 1)
    }
}
impl std::error::Error for SkillLifecycleLocation {}

/// A validated instruction prefix, including structural context needed to
/// continue replay. Effective pins/directives alone cannot validate a suffix:
/// unresolved calls and exact metadata positions must survive the split too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstructionReplayState {
    skills: ActiveSkills,
    directives: DirectiveState,
    requests: crate::request_replay::RequestReplayState,
    pending: BTreeSet<String>,
    header: SessionHeader,
    next_record_index: usize,
}

impl InstructionReplayState {
    pub fn replay(items: &[TranscriptItem]) -> anyhow::Result<Self> {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| {
            counts.full_replays += 1;
            counts.full_records += items.len();
        });
        Self::default().apply_records(items)
    }

    pub fn skills(&self) -> &ActiveSkills {
        &self.skills
    }

    pub fn directives(&self) -> &DirectiveState {
        &self.directives
    }

    /// Header facts from validated replay, without a second history scan.
    pub fn persisted_mode(&self) -> Option<crate::SessionMode> {
        self.header.selected()
    }

    /// Historical capability, not just the currently active request contract.
    pub fn has_request_boundaries(&self) -> bool {
        self.requests.has_boundaries()
    }

    /// Consume the staging state so a partially failed reduction cannot escape.
    /// Validate a complete suffix batch, including pin/body causality.
    pub fn apply_suffix(self, items: &[TranscriptItem]) -> anyhow::Result<Self> {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| {
            counts.suffix_applications += 1;
            counts.suffix_records += items.len();
        });
        self.apply_records(items)
    }

    fn apply_records(mut self, items: &[TranscriptItem]) -> anyhow::Result<Self> {
        for item in items {
            self.apply_record(item)?;
            self.next_record_index += 1;
        }
        self.requests.validate_complete()?;
        self.directives.snapshot().validate()?;
        Ok(self)
    }

    fn apply_record(&mut self, item: &TranscriptItem) -> anyhow::Result<()> {
        self.header.apply(self.next_record_index, item)?;
        self.requests.apply(item, self.pending.is_empty())?;
        apply_skill_record(&mut self.skills, self.next_record_index, item)?;
        match item {
            TranscriptItem::Directive(directive) => {
                directive.validate()?;
                anyhow::ensure!(
                    self.pending.is_empty(),
                    "a directive cannot split a tool call/result batch"
                );
                match &directive.payload {
                    DirectivePayload::SkillBody { name, digest, body } => {
                        anyhow::ensure!(
                            self.skills.get(name).is_some_and(|activation| {
                                activation.digest() == *digest && activation.body() == body
                            }),
                            "skill body directive has no matching full pinned activation"
                        );
                    }
                    DirectivePayload::SkillRevocation { name, .. } => anyhow::ensure!(
                        self.skills.contains(name),
                        "skill revocation has no recorded activation"
                    ),
                }
                self.directives.apply(directive)?;
            }
            TranscriptItem::Compaction(_) => anyhow::ensure!(
                self.pending.is_empty(),
                "a checkpoint cannot split a tool call/result batch"
            ),
            _ => {}
        }
        if let Some(message) = item.message() {
            match message {
                Message::System { .. } => {
                    anyhow::bail!("raw system messages must be typed directives")
                }
                Message::Assistant { content, .. } => {
                    for block in content {
                        if let AssistantContent::ToolCall(call) = block {
                            anyhow::ensure!(
                                self.pending.insert(call.id.to_string()),
                                "duplicate unresolved tool call id {}",
                                call.id
                            );
                        }
                    }
                }
                Message::User { content } => {
                    for block in content {
                        if let UserContent::ToolResult(result) = block {
                            self.pending.remove(result.call.as_str());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Project directives from the same causally validated instruction reduction.
pub fn replay_directives(items: &[TranscriptItem]) -> anyhow::Result<DirectiveState> {
    Ok(InstructionReplayState::replay(items)?.directives)
}

/// Fold ordered skill directives identically for live and loaded histories,
/// including directives preceding a compaction checkpoint.
pub fn effective_directives(items: &[TranscriptItem]) -> Vec<&crate::DirectiveContent> {
    zevria_instructions::directive::effective_directives(items.iter().filter_map(
        |item| match item {
            TranscriptItem::Directive(directive) => Some(directive),
            _ => None,
        },
    ))
}
