//! Compaction orchestration.

use crate::session::SessionEngine;
use crate::session::kernel::records::TurnAnchor;
use crate::session::kernel::request_plan::HistoryView;
use crate::session::turn::execution::{
    AcceptedKind, AcceptedTurn, Failure, Rejection, TurnCompletion,
};
use futures_util::FutureExt as _;
use rig_core::message::Message;
use std::sync::Arc;
use zevria_foundation::policy::{SessionMode, TurnPolicy};
use zevria_foundation::task::TaskList;
use zevria_model::ToolDeclarations;
use zevria_model::compaction::{
    CompactionBackend, CompactionCheckpoint, CompactionTrigger, select_recent_user_messages,
};
use zevria_model::request::{CompactResult, ModelRequest, ModelRequestItem, OwnedModelRequestItem};
use zevria_session_api::event::{ProgressReporter, SessionEvent, SessionEventSender};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::turn::TurnContext;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::{TranscriptItem, latest_successful_task_snapshot};
use zevria_workflow::plan::replay_plan_state;

pub(in crate::session) fn snapshot_model_input(
    input: Vec<ModelRequestItem<'_>>,
) -> anyhow::Result<Vec<OwnedModelRequestItem>> {
    input
        .into_iter()
        .map(ModelRequestItem::to_owned_item)
        .collect()
}

pub(in crate::session) fn snapshot_conversation_input(
    input: &[OwnedModelRequestItem],
) -> (Vec<OwnedModelRequestItem>, Vec<usize>) {
    input
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            !matches!(
                item,
                OwnedModelRequestItem::DeveloperInstruction(_)
                    | OwnedModelRequestItem::RequestInstruction(_)
                    | OwnedModelRequestItem::AdditionalTools(_)
            )
        })
        .map(|(position, item)| (item.clone(), position))
        .unzip()
}

pub(in crate::session) fn append_task_snapshot_context(
    history: &mut Vec<OwnedModelRequestItem>,
    snapshot: Option<&TaskList>,
) {
    if let Some(snapshot) = snapshot {
        history.push(OwnedModelRequestItem::message(Message::user(
            snapshot.checkpoint_context(),
        )));
    }
}

pub(in crate::session) fn append_plan_context(
    history: &mut Vec<OwnedModelRequestItem>,
    context: Option<&str>,
) {
    if let Some(context) = context {
        history.push(OwnedModelRequestItem::message(Message::user(context)));
    }
}

pub(in crate::session) fn automatic_compaction_armed_for(items: &[TranscriptItem]) -> bool {
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
                            TranscriptItem::Directive(_)
                                | TranscriptItem::RequestDirective(_)
                                | TranscriptItem::ToolDeclarations(_)
                        )
                })
                .nth(1)
                .is_some()
        })
}

pub(in crate::session) struct CompactionRequest {
    pub(in crate::session) instructions: Arc<str>,
    pub(in crate::session) tools: Arc<ToolDeclarations>,
    pub(in crate::session) input: Vec<OwnedModelRequestItem>,
}

pub(in crate::session) struct CompactionSource {
    pub(in crate::session) request: CompactionRequest,
    pub(in crate::session) history: Vec<OwnedModelRequestItem>,
    pub(in crate::session) positions: Vec<usize>,
    pub(in crate::session) retained_user_candidates: Vec<String>,
    pub(in crate::session) task_snapshot: Option<TaskList>,
    pub(in crate::session) plan_context: Option<String>,
}

impl CompactionSource {
    pub(in crate::session) fn cut(&self, k: usize) -> usize {
        self.positions
            .get(k)
            .copied()
            .unwrap_or(self.request.input.len())
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn compaction_source(
        &self,
        items: &[TranscriptItem],
        policy: &TurnPolicy,
    ) -> anyhow::Result<CompactionSource> {
        let input = snapshot_model_input(zevria_transcript::transcript::model_input(items))?;
        let (history, positions) = snapshot_conversation_input(&input);
        Ok(CompactionSource {
            request: CompactionRequest {
                instructions: self.rendered_instructions(policy),
                tools: self.request_tools(HistoryView::Prospective(items), policy),
                input,
            },
            history,
            positions,
            retained_user_candidates: zevria_transcript::transcript::retained_user_candidates(
                items,
            ),
            task_snapshot: latest_successful_task_snapshot(items),
            plan_context: replay_plan_state(items.iter().filter_map(|item| match item {
                TranscriptItem::Plan(record) => Some(record),
                _ => None,
            }))?
            .checkpoint_context(),
        })
    }

    pub(in crate::session) async fn automatic_checkpoint_for_prompt(
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
                let source = self.compaction_source(prefix, policy)?;
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

    pub(in crate::session) async fn compact_command(
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

    pub(in crate::session) async fn prepare_compaction(
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

        let remote = {
            let input = source
                .request
                .input
                .iter()
                .map(OwnedModelRequestItem::as_borrowed)
                .collect::<Vec<_>>();
            let request = ModelRequest {
                input_identity: None,
                instructions: &source.request.instructions,
                tools: &source.request.tools,
                forbid_tool_calls: false,
                input,
                model_role: policy.model_role,
                allowed_tool_names: policy.allowed_tool_names.as_deref(),
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
                    .summarize_prefix(source, &context, policy, events, turn)
                    .boxed()
                    .await?;
                let replacement = zevria_model::compaction::summary_tail_history(
                    &source.history,
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
        append_plan_context(&mut replacement_history, source.plan_context.as_deref());

        if turn.is_cancelled() {
            self.provider.reset();
            anyhow::bail!("turn cancelled");
        }
        CompactionCheckpoint::new(trigger, backend, replacement_history, retained)
    }

    pub(in crate::session) async fn announce_compaction_completed(
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

    pub(in crate::session) async fn compact_context(
        &mut self,
        trigger: CompactionTrigger,
        policy: &TurnPolicy,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> anyhow::Result<()> {
        if !self.conversation.has_compaction_prompt() {
            anyhow::bail!("cannot compact a conversation before its first user prompt");
        }
        let source = self.compaction_source(self.conversation.items(), policy)?;
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
        self.commit(TranscriptItem::Compaction(checkpoint))?;
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
pub(in crate::session) enum CheckpointPlacement {
    InstalledBeforeCommit,
    FoldedIntoCommit(CompactionCheckpoint),
}
impl CheckpointPlacement {
    pub(in crate::session) fn folded(&self) -> Option<&CompactionCheckpoint> {
        match self {
            Self::InstalledBeforeCommit => None,
            Self::FoldedIntoCommit(checkpoint) => Some(checkpoint),
        }
    }
}
