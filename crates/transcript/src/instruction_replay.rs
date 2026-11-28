use crate::skill::*;
use crate::transcript::{SessionMetadataState, TranscriptItem};
use crate::{DirectivePayload, DirectiveState};
use anyhow::Context as _;
use rig_core::message::{AssistantContent, Message, UserContent};
use std::{collections::BTreeSet, fmt, sync::Arc};

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
    skills: Arc<ActiveSkills>,
    directives: Arc<DirectiveState>,
    requests: Arc<crate::request_replay::RequestReplayState>,
    pending: Arc<BTreeSet<String>>,
    tool_initial: bool,
    metadata: Arc<SessionMetadataState>,
    next_record_index: usize,
    fork_origin: bool,
    fork_boundary: bool,
    fork_prompt_count: usize,
    fork_quote: Option<Arc<str>>,
    turn_count: usize,
    safe_len: usize,
    safe_turn_count: usize,
}

impl InstructionReplayState {
    pub fn replay(items: &[TranscriptItem]) -> anyhow::Result<Self> {
        Self::replay_with(items, false)
    }

    pub(crate) fn replay_with(items: &[TranscriptItem], decoded: bool) -> anyhow::Result<Self> {
        #[cfg(feature = "test-support")]
        crate::replay_probe::record(|counts| {
            counts.full_replays += 1;
            counts.full_records += items.len();
        });
        Self::default().apply_records(items, decoded)
    }

    pub fn skills(&self) -> &ActiveSkills {
        &self.skills
    }

    pub fn directives(&self) -> &DirectiveState {
        &self.directives
    }

    /// Header facts from validated replay, without a second history scan.
    pub fn persisted_mode(&self) -> Option<crate::SessionMode> {
        self.metadata.mode
    }

    pub fn session_models(&self) -> Option<&crate::models::SessionModels> {
        self.metadata.models.as_ref()
    }

    pub(crate) fn ensure_metadata_boundary(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.fork_boundary,
            "session metadata cannot follow a Side Chat fork boundary"
        );
        self.requests.validate_complete()?;
        anyhow::ensure!(
            self.pending.is_empty(),
            "session metadata cannot split a tool call/result batch"
        );
        Ok(())
    }

    pub(crate) fn append_metadata(&mut self, item: &TranscriptItem) {
        debug_assert!(self.ensure_metadata_boundary().is_ok());
        Arc::make_mut(&mut self.metadata).apply(item);
        self.next_record_index += 1;
        self.advance_cursor();
    }

    pub fn fork_cursor(&self) -> (usize, usize) {
        (self.safe_len, self.safe_turn_count)
    }

    pub fn side_prompt_count(&self) -> usize {
        self.fork_prompt_count
    }

    fn advance_cursor(&mut self) {
        if self.pending.is_empty() && self.requests.is_complete() {
            self.safe_len = self.next_record_index;
            self.safe_turn_count = self.turn_count;
        }
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
        self.apply_records(items, false)
    }

    fn apply_records(mut self, items: &[TranscriptItem], decoded: bool) -> anyhow::Result<Self> {
        for item in items {
            self.apply_record(item, decoded)?;
            self.next_record_index += 1;
            self.turn_count += usize::from(crate::is_prompt_item(item));
            self.advance_cursor();
        }
        self.requests.validate_complete()?;
        anyhow::ensure!(
            !self.fork_origin || self.fork_boundary,
            "Side Chat origin has no fork boundary"
        );
        Ok(self)
    }

    fn apply_record(&mut self, item: &TranscriptItem, decoded: bool) -> anyhow::Result<()> {
        if self.fork_boundary {
            let allowed = match item {
                TranscriptItem::RequestPrompt { .. }
                | TranscriptItem::IncompleteResponse(_)
                | TranscriptItem::Error { .. }
                | TranscriptItem::ProviderMessage(_)
                | TranscriptItem::Message(Message::Assistant(_)) => true,
                TranscriptItem::RequestDirective(directive) => {
                    directive.request.behavior == zevria_foundation::RequestBehavior::SideChat
                }
                TranscriptItem::ToolResults {
                    skill_applications, ..
                } => skill_applications.is_empty(),
                TranscriptItem::ToolDeclarations(record) => {
                    record.placement == crate::ToolDeclarationPlacement::Initial
                }
                _ => false,
            };
            anyhow::ensure!(
                allowed,
                "record is forbidden after a Side Chat fork boundary"
            );
        }
        match item {
            TranscriptItem::SideChatOrigin(fork) => {
                anyhow::ensure!(
                    self.next_record_index == 0,
                    "Side Chat origin must be the first record"
                );
                if !decoded {
                    fork.validate()?;
                }
                self.fork_origin = true;
                self.fork_quote = fork.origin.quoted_excerpt.as_deref().map(Arc::from);
            }
            TranscriptItem::SideChatForkBoundary => {
                self.ensure_metadata_boundary()?;
                anyhow::ensure!(
                    self.fork_origin && !self.fork_boundary,
                    "unexpected Side Chat fork boundary"
                );
                self.fork_boundary = true;
            }
            TranscriptItem::RequestPrompt {
                request,
                references,
                ..
            } => {
                anyhow::ensure!(
                    (request.behavior == zevria_foundation::RequestBehavior::SideChat)
                        == self.fork_boundary,
                    "Side Chat intent must occur after a fork boundary"
                );
                let excerpt = references
                    .as_ref()
                    .and_then(|reference| reference.quoted_excerpt());
                if self.fork_boundary {
                    anyhow::ensure!(
                        references
                            .as_ref()
                            .is_none_or(|reference| reference.attachments().is_empty()),
                        "Side Chat references belong to the main conversation"
                    );
                    let expected = (self.fork_prompt_count == 0)
                        .then_some(self.fork_quote.as_deref())
                        .flatten();
                    anyhow::ensure!(
                        excerpt == expected,
                        "Side Chat selection must belong to the first question and match its origin"
                    );
                    self.fork_prompt_count += 1;
                } else {
                    anyhow::ensure!(
                        excerpt.is_none(),
                        "a main prompt cannot carry a Side Chat selection"
                    );
                }
            }
            _ => {}
        }
        if crate::is_leading_metadata(item) {
            self.ensure_metadata_boundary()?;
            Arc::make_mut(&mut self.metadata).apply(item);
            return Ok(());
        }
        Arc::make_mut(&mut self.requests).apply(item, self.pending.is_empty(), decoded)?;
        match item {
            TranscriptItem::ToolResults {
                skill_applications, ..
            } if skill_applications.is_empty() => {
                // Still check result/metadata correlation; there is no pin to mutate.
                apply_skill_record(&mut ActiveSkills::default(), self.next_record_index, item)?;
            }
            TranscriptItem::SkillInvocation(_) | TranscriptItem::ToolResults { .. } => {
                apply_skill_record(
                    Arc::make_mut(&mut self.skills),
                    self.next_record_index,
                    item,
                )?;
            }
            TranscriptItem::Compaction(checkpoint) if !decoded => checkpoint.validate()?,
            _ => {}
        }
        match item {
            TranscriptItem::ToolDeclarations(record) => {
                if !decoded {
                    record.validate()?;
                }
                anyhow::ensure!(
                    self.pending.is_empty(),
                    "tool declarations cannot split a tool call/result batch"
                );
                match record.placement {
                    crate::ToolDeclarationPlacement::Initial => {
                        anyhow::ensure!(
                            !self.tool_initial,
                            "a tool window may have only one Initial declaration"
                        );
                        self.tool_initial = true;
                    }
                    crate::ToolDeclarationPlacement::Additional => anyhow::ensure!(
                        self.tool_initial,
                        "Additional tool declarations require an Initial in this window"
                    ),
                }
            }
            TranscriptItem::Directive(directive) => {
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
                    DirectivePayload::McpInstructions { .. }
                    | DirectivePayload::McpInstructionsRevocation { .. } => {}
                }
                let directives = Arc::make_mut(&mut self.directives);
                if decoded {
                    directives.apply_validated(directive);
                } else {
                    directives.apply(directive)?;
                }
            }
            TranscriptItem::CommandJobNotice(_) => anyhow::ensure!(
                self.pending.is_empty(),
                "a command job notice cannot split a tool call/result batch"
            ),
            TranscriptItem::Compaction(_) => {
                anyhow::ensure!(
                    self.pending.is_empty(),
                    "a checkpoint cannot split a tool call/result batch"
                );
                self.tool_initial = false;
            }
            _ => {}
        }
        if let Some(message) = item.message() {
            match message {
                Message::System { .. } => {
                    anyhow::bail!("raw system messages must be typed directives")
                }
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let AssistantContent::ToolCall(call) = block {
                            anyhow::ensure!(
                                Arc::make_mut(&mut self.pending).insert(call.id.to_string()),
                                "duplicate unresolved tool call id {}",
                                call.id
                            );
                        }
                    }
                }
                Message::User { content } => {
                    for block in content {
                        if let UserContent::ToolResult(result) = block {
                            Arc::make_mut(&mut self.pending).remove(result.call.wire().as_ref());
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
    Ok(Arc::unwrap_or_clone(
        InstructionReplayState::replay(items)?.directives,
    ))
}

/// Fold ordered skill directives identically for live and loaded histories,
/// including directives preceding a compaction checkpoint.
pub fn effective_directives(items: &[TranscriptItem]) -> Vec<&crate::DirectiveContent> {
    zevria_instructions::directive::effective_directives(items.iter().filter_map(
        |item| match item {
            TranscriptItem::Directive(directive) => Some(directive.as_ref()),
            _ => None,
        },
    ))
}
