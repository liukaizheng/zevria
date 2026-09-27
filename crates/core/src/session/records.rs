//! Records orchestration.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnAnchor {
    Append,
    ReplaceFrom(usize),
}

impl TurnAnchor {
    pub(super) const fn replacement_index(self) -> Option<usize> {
        match self {
            Self::Append => None,
            Self::ReplaceFrom(index) => Some(index),
        }
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) fn reset_after_history_replacement(&mut self) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        self.provider.reset();
        self.context.invalidate_prepared_count();
        self.reestimate_context_usage();
        self.context.recompute_arming(self.conversation.items());
        Ok(())
    }

    pub(super) fn record_required(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.ensure_replay_valid()?;
        let estimated = item.clone();
        let replay = self.conversation.commit_anchored(None, vec![item], None)?;
        self.install_replay(replay.into())?;
        self.append_context_estimates(std::slice::from_ref(&estimated));
        Ok(())
    }

    pub(super) fn record_required_items(
        &mut self,
        items: Vec<TranscriptItem>,
    ) -> anyhow::Result<()> {
        self.ensure_replay_valid()?;
        if items.is_empty() {
            return Ok(());
        }
        if items.len() == 1 {
            self.record_required(items.into_iter().next().expect("one item"))
        } else {
            let estimated = items.clone();
            let replay = self.conversation.commit_anchored(None, items, None)?;
            self.install_replay(replay.into())?;
            self.append_context_estimates(&estimated);
            Ok(())
        }
    }

    pub(super) fn record_completed(&mut self, item: TranscriptItem) -> anyhow::Result<()> {
        self.ensure_replay_valid()?;
        let estimated = item.clone();
        let model_visible =
            item.model_request_item().is_some() && !matches!(item, TranscriptItem::Directive(_));
        let result = if item.display_attempt_id().is_some() {
            self.conversation.push_completed_linked(item)
        } else {
            self.conversation.push_completed(item)
        };
        self.refresh_replay()?;
        self.append_context_estimates(std::slice::from_ref(&estimated));
        if model_visible {
            self.context.arm_all();
        }
        result
    }

    pub(super) fn record_completed_items(
        &mut self,
        items: Vec<TranscriptItem>,
    ) -> anyhow::Result<()> {
        self.ensure_replay_valid()?;
        let model_visible = items.iter().any(|item| {
            item.model_request_item().is_some() && !matches!(item, TranscriptItem::Directive(_))
        });
        let estimated = items.clone();
        let result = self.conversation.push_completed_batch(items);
        self.refresh_replay()?;
        self.append_context_estimates(&estimated);
        if model_visible {
            self.context.arm_all();
        }
        result
    }

    /// Commit one transcript-owned proposal and install its validated replay
    /// only after durability. Invalid prospective evidence cannot poison the
    /// healthy branch; raw committed evidence still uses the terminal latch.
    pub(super) fn commit_anchored_records(
        &mut self,
        anchor: TurnAnchor,
        records: Vec<TranscriptItem>,
        mode: SessionMode,
    ) -> anyhow::Result<PlanWorkflowState> {
        self.ensure_replay_valid()?;
        let previous_state = self.plan_state()?.clone();
        let persist_mode = self.capabilities.mode_management
            || self.instruction_replay()?.persisted_mode().is_some();
        let estimated = (anchor == TurnAnchor::Append && !persist_mode).then(|| records.clone());
        let replay = self.conversation.commit_anchored(
            anchor.replacement_index(),
            records,
            persist_mode.then_some(mode),
        )?;
        self.install_replay(replay.into())?;
        if matches!(anchor, TurnAnchor::ReplaceFrom(_)) {
            self.provider.reset();
            self.context.recompute_arming(self.conversation.items());
        }
        if persist_mode || matches!(anchor, TurnAnchor::ReplaceFrom(_)) {
            self.invalidate_mode_accounting();
        } else if let Some(estimated) = estimated {
            self.append_context_estimates(&estimated);
        }
        Ok(previous_state)
    }

    pub(super) fn instruction_replay(
        &self,
    ) -> Result<&zevria_transcript::InstructionReplayState, SessionReplayError> {
        match &self.replay {
            SessionReplayState::Valid { instructions, .. } => Ok(instructions),
            SessionReplayState::Failed(error) => Err(error.clone()),
        }
    }

    /// Reduce the current conversation and install both replay domains. Mutation
    /// paths keep this state current; runtime startup and idle dispatch only check
    /// the latch. Direct command APIs also refresh here. Failure is terminal and
    /// never retains a stale payload.
    pub(super) fn refresh_replay(&mut self) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        match validate_session_replay(self.conversation.items()) {
            Ok(replay) => self.install_replay(replay),
            Err(error) => {
                self.replay = SessionReplayState::Failed(error.clone());
                self.exit_turn();
                self.provider.cancel();
                Err(error)
            }
        }
    }

    pub(super) fn install_replay(
        &mut self,
        replay: SessionReplayState,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        #[cfg(feature = "test-support")]
        super::pipeline_probe::record(|counts| counts.replay_installations += 1);
        self.replay = replay;
        if self.skills.management.is_some() {
            self.refresh_skill_policies()?;
        }
        self.publish_skill_query_context()
    }

    pub(super) fn publish_skill_query_context(&self) -> Result<(), SessionReplayError> {
        if let EnginePhase::Turn { skill_queries, .. } = &self.phase {
            *skill_queries
                .lock()
                .expect("skill query projection poisoned") = self.management_skill_context()?;
        }
        self.ensure_replay_valid()
    }

    /// Persistence degradation is recoverable; replay failure is never ignored.
    pub(super) async fn record_recoverable_items(
        &mut self,
        items: Vec<TranscriptItem>,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        if let Err(error) = self.record_completed_items(items) {
            self.persistence_failed(&error, events).await?;
        }
        Ok(())
    }

    pub(super) async fn gate_persistence(
        &mut self,
        events: &SessionEventSender,
    ) -> Result<(), Rejection> {
        self.ensure_replay_valid()?;
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

    /// Failed required writes leave no accepted work; committed replay failures
    /// must retain their fatal identity rather than becoming a local rejection.
    pub(super) async fn acceptance_failed(
        &mut self,
        operation: &str,
        error: anyhow::Error,
        events: &SessionEventSender,
    ) -> Rejection {
        if let Err(error) = self.ensure_replay_valid() {
            return Rejection::Fatal(error);
        }
        if let Err(error) = self.persistence_failed(&error, events).await {
            return Rejection::Fatal(error);
        }
        Rejection::from(error.context(format!("failed to persist the {operation}")))
    }

    pub(super) async fn persistence_failed(
        &mut self,
        error: &anyhow::Error,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
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
