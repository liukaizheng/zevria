//! The two transcript admission paths and their ordered post-commit effects.

use crate::session::SessionEngine;
use crate::session::kernel::state::EnginePhase;
use crate::session::kernel::timing::LifecycleTiming;
use crate::session::turn::execution::Rejection;
use std::fmt;
use zevria_foundation::policy::{ModelRole, SessionMode};
use zevria_session_api::event::{SessionEvent, SessionEventSender};
use zevria_session_api::provider::ModelProvider;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::session) enum TurnAnchor {
    Append,
    ReplaceFrom(usize),
}

impl TurnAnchor {
    pub(in crate::session) const fn replacement_index(self) -> Option<usize> {
        match self {
            Self::Append => None,
            Self::ReplaceFrom(index) => Some(index),
        }
    }
}

pub(in crate::session) struct CompletedItems {
    items: Vec<TranscriptItem>,
    batch: bool,
}
impl From<Vec<TranscriptItem>> for CompletedItems {
    fn from(items: Vec<TranscriptItem>) -> Self {
        Self { items, batch: true }
    }
}
impl From<TranscriptItem> for CompletedItems {
    fn from(item: TranscriptItem) -> Self {
        Self {
            items: vec![item],
            batch: false,
        }
    }
}

pub(in crate::session) struct RecordChange {
    anchor: TurnAnchor,
    items: Vec<TranscriptItem>,
    mode: Option<SessionMode>,
}

impl RecordChange {
    pub(in crate::session) fn anchored(
        anchor: TurnAnchor,
        items: Vec<TranscriptItem>,
        mode: SessionMode,
    ) -> Self {
        Self {
            anchor,
            items,
            mode: Some(mode),
        }
    }
}

impl From<Vec<TranscriptItem>> for RecordChange {
    fn from(items: Vec<TranscriptItem>) -> Self {
        Self {
            anchor: TurnAnchor::Append,
            items,
            mode: None,
        }
    }
}
impl From<TranscriptItem> for RecordChange {
    fn from(item: TranscriptItem) -> Self {
        vec![item].into()
    }
}

/// The exact installed range. No detached records or replay payloads escape a commit.
#[derive(Debug)]
pub(in crate::session) struct Committed {
    pub(in crate::session) range: Range<usize>,
    pub(in crate::session) history_replaced: bool,
    reestimate: bool,
}

#[derive(Debug)]
pub(in crate::session) enum RecordError {
    Replay(SessionReplayError),
    Degraded(anyhow::Error),
    Rejected(anyhow::Error),
}
impl RecordError {
    pub(in crate::session) fn context(self, message: &str) -> Self {
        match self {
            Self::Replay(error) => Self::Replay(error),
            Self::Degraded(error) => Self::Degraded(error.context(message.to_owned())),
            Self::Rejected(error) => Self::Rejected(error.context(message.to_owned())),
        }
    }
}
impl From<SessionReplayError> for RecordError {
    fn from(error: SessionReplayError) -> Self {
        Self::Replay(error)
    }
}
impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Replay(error) => error.fmt(f),
            Self::Degraded(error) | Self::Rejected(error) => write!(f, "{error:#}"),
        }
    }
}
impl std::error::Error for RecordError {}

/// Only storage failures are recoverable. Invalid completed evidence is fatal,
/// even when its persistence failed as well; no anyhow downcast is involved.
pub(in crate::session) fn recoverable(
    result: Result<Committed, RecordError>,
) -> Result<Option<anyhow::Error>, SessionReplayError> {
    match result {
        Ok(_) => Ok(None),
        Err(RecordError::Replay(error)) => Err(error),
        Err(RecordError::Degraded(error) | RecordError::Rejected(error)) => Ok(Some(error)),
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn reset_after_history_replacement(
        &mut self,
    ) -> Result<(), SessionReplayError> {
        self.provider.reset();
        let profiles = ModelRole::ALL.map(|role| self.context_policy(role).profile.clone());
        self.context
            .on_history_replaced(&self.conversation, profiles);
        Ok(())
    }

    /// Required work installs nothing until its validated proposal is durable.
    pub(in crate::session) fn commit(
        &mut self,
        change: impl Into<RecordChange>,
    ) -> Result<Committed, RecordError> {
        self.conversation.replay()?;
        let change = change.into();
        let _timing = LifecycleTiming::new("commit", change.items.len());
        let persist_mode = change.mode.filter(|_| {
            self.capabilities.mode_management
                || self
                    .conversation
                    .replay()
                    .is_ok_and(|r| r.instructions.persisted_mode().is_some())
        });
        let start = self.conversation.items().len();
        let history_replaced = matches!(change.anchor, TurnAnchor::ReplaceFrom(_));
        let reestimate = history_replaced || persist_mode.is_some();
        let empty = change.items.is_empty() && persist_mode.is_none() && !history_replaced;
        if empty {
            return Ok(Committed {
                range: start..start,
                history_replaced: false,
                reestimate: false,
            });
        }
        self.conversation
            .commit_anchored(
                change.anchor.replacement_index(),
                change.items,
                persist_mode,
            )
            .map_err(|error| self.record_error(error))?;
        let committed = Committed {
            range: if reestimate { 0 } else { start }..self.conversation.items().len(),
            history_replaced,
            reestimate,
        };
        self.after_commit(&committed, false)?;
        Ok(committed)
    }

    /// Completed external work remains installed even when persistence degrades.
    pub(in crate::session) fn record_completed(
        &mut self,
        items: impl Into<CompletedItems>,
    ) -> Result<Committed, RecordError> {
        self.conversation.replay()?;
        let CompletedItems { items, batch } = items.into();
        let _timing = LifecycleTiming::new("record_completed", items.len());
        let start = self.conversation.items().len();
        let result = if batch {
            self.conversation.push_completed_batch(items)
        } else {
            self.conversation
                .push_completed(items.into_iter().next().expect("one item"))
        };
        let committed = Committed {
            range: start..self.conversation.items().len(),
            history_replaced: false,
            reestimate: false,
        };
        if !committed.range.is_empty() {
            self.after_commit(&committed, true)?;
        }
        result.map_err(|error| self.record_error(error))?;
        Ok(committed)
    }

    fn record_error(&self, error: anyhow::Error) -> RecordError {
        if self.conversation.persistence_error().is_some() {
            RecordError::Degraded(error)
        } else {
            RecordError::Rejected(error)
        }
    }

    fn after_commit(&mut self, committed: &Committed, completed: bool) -> Result<(), RecordError> {
        // Replay failure takes precedence over storage failure. Never refresh
        // policy, account, or start any external work from invalid evidence.
        self.replay_changed()?;
        if committed.history_replaced {
            self.provider.reset();
            self.context.recompute_arming(self.conversation.items());
        }
        if committed.reestimate {
            self.invalidate_mode_accounting();
        } else {
            self.append_committed_estimates(committed.range.clone());
        }
        if completed
            && self.conversation.items()[committed.range.clone()]
                .iter()
                .any(|item| {
                    item.model_request_item().is_some()
                        && !matches!(
                            item,
                            TranscriptItem::Directive(_)
                                | TranscriptItem::RequestDirective(_)
                                | TranscriptItem::ToolDeclarations(_)
                        )
                })
        {
            self.context.arm_all();
        }
        Ok(())
    }

    pub(in crate::session) fn instruction_replay(
        &self,
    ) -> Result<&zevria_transcript::InstructionReplayState, SessionReplayError> {
        Ok(&self.conversation.replay()?.instructions)
    }

    pub(in crate::session) fn replay_changed(&mut self) -> Result<(), SessionReplayError> {
        if let Err(error) = self.conversation.replay() {
            self.exit_turn();
            self.provider.cancel();
            return Err(error);
        }
        #[cfg(feature = "test-support")]
        crate::session::pipeline_probe::record(|counts| counts.replay_installations += 1);
        if self.skills.management.is_some() {
            self.refresh_skill_policies()?;
        }
        self.publish_skill_query_context()
    }

    pub(in crate::session) fn publish_skill_query_context(&self) -> Result<(), SessionReplayError> {
        if let EnginePhase::Turn { skill_queries, .. } = &self.phase {
            *skill_queries
                .lock()
                .expect("skill query projection poisoned") = self.management_skill_context()?;
        }
        Ok(())
    }

    pub(in crate::session) async fn gate_persistence(
        &mut self,
        events: &SessionEventSender,
    ) -> Result<(), Rejection> {
        match self.conversation.ensure_durable() {
            Ok(restored) => {
                if restored {
                    let _ = events
                        .send(SessionEvent::PersistenceChanged {
                            path: self.conversation.path().to_path_buf(),
                            error: None,
                        })
                        .await;
                }
                Ok(())
            }
            Err(error) => {
                self.persistence_failed(&error, events).await?;
                Err(Rejection::Rejected(format!(
                    "session transcript persistence is degraded; new work was rejected: {error:#}"
                )))
            }
        }
    }

    pub(in crate::session) async fn acceptance_failed(
        &mut self,
        operation: &str,
        error: RecordError,
        events: &SessionEventSender,
    ) -> Rejection {
        match error {
            RecordError::Replay(error) => Rejection::Fatal(error),
            RecordError::Degraded(error) | RecordError::Rejected(error) => {
                if let Err(error) = self.persistence_failed(&error, events).await {
                    return Rejection::Fatal(error);
                }
                Rejection::Rejected(format!("failed to persist the {operation}: {error:#}"))
            }
        }
    }

    pub(in crate::session) async fn persistence_failed(
        &mut self,
        error: &impl fmt::Display,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        let Some(error) = self.conversation.persistence_error().map(ToOwned::to_owned) else {
            tracing::warn!("a transcript operation failed without degrading the writer: {error:#}");
            return Ok(());
        };
        let _ = events
            .send(SessionEvent::PersistenceChanged {
                path: self.conversation.path().to_path_buf(),
                error: Some(error),
            })
            .await;
        Ok(())
    }
}
