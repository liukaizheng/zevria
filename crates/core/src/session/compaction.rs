//! Compaction orchestration.

use super::*;

pub(super) fn snapshot_model_input(
    input: Vec<ModelRequestItem<'_>>,
) -> anyhow::Result<Vec<OwnedModelRequestItem>> {
    input
        .into_iter()
        .map(ModelRequestItem::to_owned_item)
        .collect()
}

pub(super) fn snapshot_conversation_input(
    input: Vec<ModelRequestItem<'_>>,
) -> anyhow::Result<Vec<OwnedModelRequestItem>> {
    snapshot_model_input(
        input
            .into_iter()
            .filter(|item| {
                !matches!(
                    item,
                    ModelRequestItem::DeveloperInstruction(_)
                        | ModelRequestItem::RequestInstruction(_)
                )
            })
            .collect(),
    )
}

pub(super) fn append_task_snapshot_context(
    history: &mut Vec<OwnedModelRequestItem>,
    snapshot: Option<&TaskList>,
) {
    if let Some(snapshot) = snapshot {
        history.push(OwnedModelRequestItem::message(Message::user(
            snapshot.checkpoint_context(),
        )));
    }
}

pub(super) fn automatic_compaction_armed_for(items: &[TranscriptItem]) -> bool {
    items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| matches!(item, TranscriptItem::Compaction(_)).then_some(index))
        .is_none_or(|index| {
            // A checkpoint followed only by the prompt it was prepared for
            // is still the post-compaction request. Arm again once a provider
            // response or another model-visible item extends that request.
            items[index.saturating_add(1)..]
                .iter()
                .filter(|item| {
                    item.model_request_item().is_some()
                        && !matches!(
                            item,
                            TranscriptItem::Directive(_) | TranscriptItem::RequestDirective(_)
                        )
                })
                .nth(1)
                .is_some()
        })
}

pub(super) struct CompactionSource {
    pub(super) input: Vec<OwnedModelRequestItem>,
    /// Prepared current maintenance guidance; never copied into the checkpoint.
    pub(super) instructions: zevria_instructions::InstructionSet,
    pub(super) retained_user_candidates: Vec<String>,
    pub(super) task_snapshot: Option<TaskList>,
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) async fn automatic_checkpoint_for_prompt(
        &mut self,
        anchor: TurnAnchor,
        policy: &TurnPolicy,
        due: bool,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<Option<CheckpointPlacement>, Rejection> {
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        if !due {
            return Ok(None);
        }
        let result = match anchor {
            TurnAnchor::Append if self.conversation.has_compaction_prompt() => self
                .compact_context(CompactionTrigger::AutomaticPreTurn, policy, events, turn)
                .await
                .map(|()| Some(CheckpointPlacement::InstalledBeforeCommit)),
            TurnAnchor::Append => Ok(None),
            TurnAnchor::ReplaceFrom(index) => {
                let prefix = &self.conversation.items()[..index];
                if !prefix
                    .iter()
                    .any(zevria_transcript::transcript::is_compaction_prompt_item)
                {
                    return Ok(None);
                }
                let source = CompactionSource {
                    input: snapshot_conversation_input(
                        zevria_transcript::transcript::model_input(prefix),
                    )?,
                    instructions: self.prepare_maintenance_guidance(),
                    retained_user_candidates:
                        zevria_transcript::transcript::retained_user_candidates(prefix),
                    task_snapshot: latest_successful_task_snapshot(prefix),
                };
                self.prepare_compaction(
                    CompactionTrigger::AutomaticPreTurn,
                    policy,
                    events,
                    turn,
                    &source,
                )
                .await
                .map(|checkpoint| Some(CheckpointPlacement::FoldedIntoCommit(checkpoint)))
            }
        };
        self.ensure_replay_valid()?;
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        match result {
            Ok(checkpoint) => Ok(checkpoint),
            Err(error) => {
                self.persistence_failed(&error, events).await?;
                Err(error.into())
            }
        }
    }

    pub(super) async fn compact_command(
        &mut self,
        mode: SessionMode,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        if let Err(rejection) = self.gate_persistence(events).await {
            return self.finish_rejected(turn, events, rejection).await;
        }
        if !self.conversation.has_compaction_prompt() {
            return self
                .finish_rejected(
                    turn,
                    events,
                    Rejection::Rejected(
                        "cannot compact a conversation before its first user prompt".into(),
                    ),
                )
                .await;
        }
        if turn.is_cancelled() {
            return self
                .finish_rejected(turn, events, Rejection::Cancelled)
                .await;
        }
        // Manual compaction's acceptance is its explicit CompactionStarted
        // acknowledgement; it does not fabricate a prompt row.
        let accepted = AcceptedTurn::new(AcceptedKind::ManualCompaction { mode });
        let policy = self.policies.policy(mode).clone();
        let outcome = self
            .compact_context(CompactionTrigger::Manual, &policy, events, turn)
            .await
            .map(|()| TurnCompletion)
            .map_err(|error| Failure::from(error).during_work(turn));
        self.finish_accepted(&accepted, turn, events, outcome).await
    }

    pub(super) async fn prepare_compaction(
        &mut self,
        trigger: CompactionTrigger,
        policy: &TurnPolicy,
        events: &SessionEventSender,
        turn: &TurnContext,
        source: &CompactionSource,
    ) -> anyhow::Result<CompactionCheckpoint> {
        let _ = events
            .send(SessionEvent::CompactionStarted {
                turn_id: turn.id,
                trigger,
            })
            .await;

        let context = self.context_policy(policy.model_role).clone();
        let retained = select_recent_user_messages(
            &source.retained_user_candidates,
            context.retained_user_tokens,
        );

        let instructions = source.instructions.render();
        let remote = {
            let input = source
                .input
                .iter()
                .map(OwnedModelRequestItem::as_borrowed)
                .collect::<Vec<_>>();
            zevria_model::maintenance::validate_maintenance_input(&input)?;
            let request = ModelRequest {
                instructions: &instructions,
                input,
                model_role: policy.model_role,
                allowed_tool_names: Some(&[]),
            };
            let compact = self.provider.compact(request);
            tokio::pin!(compact);
            tokio::select! {
                biased;
                () = turn.cancellation().cancelled() => None,
                result = &mut compact => Some(result),
            }
        };
        let remote = match remote {
            Some(result) => result?,
            None => {
                self.provider.cancel();
                anyhow::bail!("turn cancelled");
            }
        };

        let (backend, mut replacement_history) = match remote {
            CompactResult::Replacement(history) => {
                if history.is_empty() {
                    anyhow::bail!("provider compaction returned empty replacement history");
                }
                (CompactionBackend::OpenaiResponsesCompact, history)
            }
            CompactResult::Unsupported => {
                // Type erasure bounds the spawned engine's nested Send checks
                // without raising the compiler's recursion limit.
                let (summary, prefix_end) = self
                    .summarize_prefix(
                        &source.input,
                        &source.instructions,
                        &context,
                        policy,
                        events,
                        turn,
                    )
                    .boxed()
                    .await?;
                let replacement = zevria_model::compaction::summary_tail_history(
                    &source.input,
                    prefix_end,
                    &summary,
                )?;
                self.preflight_summary_input(
                    &context,
                    &replacement
                        .iter()
                        .map(OwnedModelRequestItem::as_borrowed)
                        .collect::<Vec<_>>(),
                )?;
                (CompactionBackend::LocalSummary, replacement)
            }
        };

        append_task_snapshot_context(&mut replacement_history, source.task_snapshot.as_ref());

        if turn.is_cancelled() {
            self.provider.reset();
            anyhow::bail!("turn cancelled");
        }
        CompactionCheckpoint::new(trigger, backend, replacement_history, retained)
    }

    pub(super) async fn announce_compaction_completed(
        &mut self,
        trigger: CompactionTrigger,
        backend: CompactionBackend,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        self.ensure_replay_valid()?;
        let _ = events
            .send(SessionEvent::CompactionCompleted {
                turn_id: turn.id,
                trigger,
                backend,
            })
            .await;
        Ok(())
    }

    pub(super) async fn compact_context(
        &mut self,
        trigger: CompactionTrigger,
        policy: &TurnPolicy,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> anyhow::Result<()> {
        if !self.conversation.has_compaction_prompt() {
            anyhow::bail!("cannot compact a conversation before its first user prompt");
        }
        let source = CompactionSource {
            input: snapshot_conversation_input(self.conversation.model_input())?,
            instructions: self.prepare_maintenance_guidance(),
            retained_user_candidates: self.conversation.retained_user_candidates(),
            task_snapshot: latest_successful_task_snapshot(self.conversation.items()),
        };
        let checkpoint = self
            .prepare_compaction(trigger, policy, events, turn, &source)
            .await?;
        if turn.is_cancelled() {
            self.provider.reset();
            anyhow::bail!("turn cancelled");
        }
        let backend = checkpoint.backend;
        // Write first: a failed append leaves both the active projection and
        // in-memory transcript unchanged.
        self.record_required(TranscriptItem::Compaction(checkpoint))?;
        self.reset_after_history_replacement()?;
        if trigger == CompactionTrigger::Manual && !turn.is_cancelled() {
            let progress = ProgressReporter::for_turn(
                events.clone(),
                turn.clone(),
                policy.model_role,
                self.context_policy(policy.model_role),
            );
            if let Err(error) = self
                .assess_current_request(policy, &progress, false, false)
                .await
            {
                tracing::warn!(
                    role = policy.model_role.name(),
                    error = %error,
                    "failed to refresh projected context after manual compaction"
                );
            }
        }
        self.announce_compaction_completed(trigger, backend, events, turn)
            .await?;
        Ok(())
    }
}

/// Append checkpoints survive a later rejected prompt. Edited checkpoints are
/// installed only with the atomic tail rewrite and never truncate early.
pub(super) enum CheckpointPlacement {
    InstalledBeforeCommit,
    FoldedIntoCommit(CompactionCheckpoint),
}
impl CheckpointPlacement {
    pub(super) fn folded(&self) -> Option<&CompactionCheckpoint> {
        match self {
            Self::InstalledBeforeCommit => None,
            Self::FoldedIntoCommit(checkpoint) => Some(checkpoint),
        }
    }
}
