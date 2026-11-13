//! Plan orchestration.

use crate::session::SessionEngine;
use crate::session::kernel::records::{RecordChange, TurnAnchor};
use crate::session::turn::execution::{AcceptedKind, AcceptedTurn, Rejection, TurnCompletion};
use crate::session::turn::tools::{
    BatchShape, EngineCall, EngineTool, ToolResultBatch, assistant_tool_calls,
    tool_arguments_for_dispatch,
};
use std::collections::HashMap;
use std::path::PathBuf;
use zevria_foundation::policy::{SessionMode, TurnId};
use zevria_foundation::question::QuestionTerminalDisposition;
use zevria_foundation::tool_result::ToolCallOutcome;
use zevria_session_api::event::{SessionEvent, SessionEventSender};
use zevria_session_api::provider::ModelProvider;
use zevria_session_api::turn::TurnContext;
use zevria_transcript::SessionReplayError;
use zevria_transcript::transcript::TranscriptItem;
use zevria_workflow::ensemble::{
    EnsembleRecord, EnsembleRunId, ReconciliationNextStep, ReportReconciliation,
    ReportReconciliationCatalog, ValidatedReportReconciliation,
};
use zevria_workflow::plan::{
    MAX_PLAN_ARTIFACT_BYTES, PlanArtifact, PlanCandidate, PlanDecision, PlanDraft, PlanHandoff,
    PlanId, PlanPublicationProvenance, PlanRecord, PlanResolution, PlanVersion, PlanWorkflowState,
    derived_title, replay_plan_state,
};

pub(in crate::session) const PLAN_SLUG_MAX_WORDS: usize = 8;
/// Cap on a plan file name's characters, bounding wordless scripts where a
/// whole line counts as one word.
pub(in crate::session) const PLAN_SLUG_MAX_CHARS: usize = 48;

/// Join a source line's leading alphanumeric words into a slug, or `None`
/// when the line contains none.
pub(in crate::session) fn slug_words(source: &str) -> Option<String> {
    let words: Vec<String> = source
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(PLAN_SLUG_MAX_WORDS)
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return None;
    }
    let slug: String = words.join("-").chars().take(PLAN_SLUG_MAX_CHARS).collect();
    Some(slug.trim_end_matches('-').to_string())
}

/// Determine the durable workflow record paired with a direct prompt. Ready
/// artifacts are modal; Planning can be explicitly abandoned only by a Build
/// submission, and a new Plan turn after Idle/Resolved starts a fresh id.
pub(in crate::session) fn workflow_transition_for_mode(
    state: &PlanWorkflowState,
    mode: SessionMode,
) -> Result<Option<PlanRecord>, String> {
    match (state, mode) {
        (PlanWorkflowState::Ready { artifact }, _) => Err(format!(
            "Plan {} is awaiting a decision; revise or approve it before submitting more work",
            artifact.version
        )),
        (PlanWorkflowState::Planning { .. }, SessionMode::Plan)
        | (
            PlanWorkflowState::Resolved { .. } | PlanWorkflowState::Published { .. },
            SessionMode::Build,
        )
        | (PlanWorkflowState::Idle, SessionMode::Build) => Ok(None),
        (PlanWorkflowState::Planning { id, previous, .. }, SessionMode::Build) => {
            Ok(Some(PlanRecord::Resolved {
                id: *id,
                artifact: previous.clone(),
                resolution: PlanResolution::Abandoned,
            }))
        }
        (
            PlanWorkflowState::Idle
            | PlanWorkflowState::Resolved { .. }
            | PlanWorkflowState::Published { .. },
            SessionMode::Plan,
        ) => Ok(Some(PlanRecord::Started { id: PlanId::new() })),
    }
}

#[derive(Debug)]
pub(in crate::session) enum PendingPlanGateCall {
    Command,
    Reconciliation {
        allowed: bool,
        declaration: Option<ReportReconciliation>,
    },
    Question {
        allowed: bool,
    },
    SubmitPlan {
        allowed: bool,
    },
}

/// Reconstruct every durable Ensemble Plan submission stage from the tail
/// after the latest matching ReportsReady record. Assistant arguments establish
/// typed declarations; Plan records retain working copies, and correlated result metadata proves which calls
/// Zevria accepted and records terminal question semantics without rereading
/// child logs or parsing worker prose.
pub(in crate::session) fn plan_submission_gate_after_reports(
    items: &[TranscriptItem],
    run_id: &EnsembleRunId,
) -> Result<Option<PlanSubmissionGate>, zevria_workflow::ReportReconciliationError> {
    let Some((reports_index, catalog)) =
        items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, item)| match item {
                TranscriptItem::Ensemble(EnsembleRecord::ReportsReady {
                    run_id: candidate,
                    agents,
                    ..
                }) if candidate == run_id => {
                    Some((index, ReportReconciliationCatalog::from_summaries(agents)))
                }
                _ => None,
            })
    else {
        return Ok(None);
    };
    let mut gate = PlanSubmissionGate::ensemble(catalog?);
    let mut pending = HashMap::<String, PendingPlanGateCall>::new();
    let mut submit_accepted = false;

    for item in &items[reports_index.saturating_add(1)..] {
        if let Some(message) = item.message() {
            let calls = assistant_tool_calls(message);
            let calls = calls.iter().map(EngineCall::new).collect::<Vec<_>>();
            let shape = BatchShape::new(&calls);
            let ensemble = gate
                .ensemble
                .as_ref()
                .expect("recovered Ensemble Plan gate has ensemble state");
            for call in calls {
                let pending_call = match call.tool {
                    EngineTool::Command => Some(PendingPlanGateCall::Command),
                    EngineTool::ReconcileReports => {
                        let arguments = tool_arguments_for_dispatch(&call.function);
                        Some(PendingPlanGateCall::Reconciliation {
                            allowed: ensemble.inspection_completed,
                            declaration: serde_json::from_str(&arguments).ok(),
                        })
                    }
                    EngineTool::Question => Some(PendingPlanGateCall::Question {
                        allowed: shape.standalone_question
                            && ensemble.reconciliation.is_some()
                            && ensemble.requires_question()
                            && ensemble.question_disposition.is_none(),
                    }),
                    EngineTool::SubmitPlan => Some(PendingPlanGateCall::SubmitPlan {
                        allowed: !shape.contains_reconciliation
                            && !shape.contains_question
                            && ensemble.can_submit()
                            && !submit_accepted
                            // Recognize only the stored-draft signature. Never
                            // recover legacy one-shot content from arguments,
                            // or mistake its retained revision for its candidate.
                            && call.function.arguments_value().as_object().is_some_and(|arguments| arguments.is_empty()),
                    }),
                    _ => None,
                };
                if let Some(pending_call) = pending_call {
                    pending.insert(call.id.to_string(), pending_call);
                }
            }
        }

        let TranscriptItem::ToolResults { metadata, .. } = item else {
            continue;
        };
        for result in metadata {
            let Some(pending_call) = pending.remove(&result.id) else {
                continue;
            };
            let tool = EngineTool::resolve(&result.tool_name);
            match pending_call {
                PendingPlanGateCall::Command
                    if tool == EngineTool::Command
                        && !matches!(result.outcome, ToolCallOutcome::Denied) =>
                {
                    gate.ensemble
                        .as_mut()
                        .expect("recovered gate state")
                        .inspection_completed = true;
                }
                PendingPlanGateCall::Reconciliation {
                    allowed: true,
                    declaration: Some(declaration),
                } if tool == EngineTool::ReconcileReports && result.outcome.is_success() => {
                    let catalog = &gate
                        .ensemble
                        .as_ref()
                        .expect("recovered gate state")
                        .catalog;
                    if let Ok(reconciliation) = declaration.validate(catalog) {
                        let ensemble = gate.ensemble.as_mut().expect("recovered gate state");
                        ensemble.reconciliation = Some(reconciliation);
                        ensemble.question_disposition = None;
                    }
                }
                PendingPlanGateCall::Question { allowed: true }
                    if tool == EngineTool::Question && result.question_disposition().is_some() =>
                {
                    gate.ensemble
                        .as_mut()
                        .expect("recovered gate state")
                        .question_disposition = result.question_disposition();
                }
                PendingPlanGateCall::SubmitPlan { allowed: true }
                    if tool == EngineTool::SubmitPlan && result.outcome.is_success() =>
                {
                    submit_accepted = true;
                }
                PendingPlanGateCall::Reconciliation { .. }
                | PendingPlanGateCall::Question { .. }
                | PendingPlanGateCall::SubmitPlan { .. }
                | PendingPlanGateCall::Command => {}
            }
        }
    }
    if submit_accepted {
        // DraftSaved follows ToolResults within the same atomic commit, so
        // derive only after replaying the entire tail. The accepted-call lock
        // prevents later authoring calls from changing this working copy.
        if let Ok(state) = replay_plan_state(items.iter().filter_map(|item| match item {
            TranscriptItem::Plan(record) => Some(record),
            _ => None,
        })) {
            let markdown = state
                .working_copy()
                .map(|copy| copy.markdown().to_owned())
                .or_else(|| match &state {
                    PlanWorkflowState::Ready { artifact }
                    | PlanWorkflowState::Published { artifact } => Some(artifact.markdown.clone()),
                    _ => None,
                });
            gate.candidate = markdown.and_then(|markdown| {
                PlanCandidate::from_markdown(markdown, MAX_PLAN_ARTIFACT_BYTES).ok()
            });
        }
    }
    Ok(Some(gate))
}

#[derive(Debug, Clone)]
pub(in crate::session) struct PlanSubmissionGate {
    pub(in crate::session) candidate: Option<PlanCandidate>,
    pub(in crate::session) ensemble: Option<EnsemblePlanSubmissionGate>,
}

#[derive(Debug, Clone)]
pub(in crate::session) struct EnsemblePlanSubmissionGate {
    pub(in crate::session) catalog: ReportReconciliationCatalog,
    pub(in crate::session) inspection_completed: bool,
    pub(in crate::session) reconciliation: Option<ValidatedReportReconciliation>,
    pub(in crate::session) question_disposition: Option<QuestionTerminalDisposition>,
}

impl PlanSubmissionGate {
    pub(in crate::session) fn inert(candidate: Option<PlanCandidate>) -> Self {
        Self {
            candidate,
            ensemble: None,
        }
    }

    pub(in crate::session) fn ensemble(catalog: ReportReconciliationCatalog) -> Self {
        Self {
            candidate: None,
            ensemble: Some(EnsemblePlanSubmissionGate {
                catalog,
                inspection_completed: false,
                reconciliation: None,
                question_disposition: None,
            }),
        }
    }

    pub(in crate::session) fn candidate_accepted(&self) -> bool {
        self.candidate.is_some()
    }

    pub(in crate::session) fn reconciliation_catalog(
        &self,
    ) -> Option<&ReportReconciliationCatalog> {
        self.ensemble.as_ref().map(|ensemble| &ensemble.catalog)
    }

    pub(in crate::session) fn apply_batch(&mut self, batch: &ToolResultBatch) {
        let Some(ensemble) = &mut self.ensemble else {
            if self.candidate.is_none() {
                self.candidate.clone_from(&batch.candidate);
            }
            return;
        };
        ensemble.inspection_completed |= batch.inspection_attempted;
        if let Some(reconciliation) = &batch.reconciliation {
            ensemble.reconciliation = Some(reconciliation.clone());
            ensemble.question_disposition = None;
        }
        if let Some(disposition) = batch.question_disposition {
            ensemble.question_disposition = Some(disposition);
        }
        if self.candidate.is_none() {
            self.candidate.clone_from(&batch.candidate);
        }
    }
}

impl EnsemblePlanSubmissionGate {
    pub(in crate::session) fn requires_question(&self) -> bool {
        self.reconciliation.as_ref().is_some_and(|reconciliation| {
            reconciliation.next_step == ReconciliationNextStep::Question
        })
    }

    pub(in crate::session) fn can_submit(&self) -> bool {
        self.inspection_completed
            && self.reconciliation.is_some()
            && (!self.requires_question() || self.question_disposition.is_some())
    }
}

pub(in crate::session) fn ensemble_plan_terminal_error(
    gate: &PlanSubmissionGate,
) -> Option<String> {
    let ensemble = gate.ensemble.as_ref()?;
    if !ensemble.inspection_completed {
        return Some(
            "Ensemble Plan synthesis ended without a terminal command inspection attempt"
                .to_string(),
        );
    }
    if ensemble.reconciliation.is_none() {
        return Some(
            "Ensemble Plan synthesis ended without a durable accepted reconcile_reports declaration"
                .to_string(),
        );
    }
    if ensemble.requires_question() && ensemble.question_disposition.is_none() {
        return Some(
            "Ensemble Plan synthesis ended before the required root question reached a terminal disposition"
                .to_string(),
        );
    }
    gate.candidate
        .is_none()
        .then(|| "Ensemble Plan synthesis completed without calling submit_plan".to_string())
}

impl<P: ModelProvider> SessionEngine<P> {
    /// A write-only convenience path, stable from the thread's first saved title.
    pub(in crate::session) fn plan_projection_path(
        &self,
        id: PlanId,
        fallback_title: &str,
    ) -> Option<PathBuf> {
        let root = self.capabilities.plans_dir.as_ref()?;
        let first_title = self
            .conversation
            .items()
            .iter()
            .find_map(|item| match item {
                TranscriptItem::Plan(PlanRecord::DraftSaved { draft }) if draft.target.id == id => {
                    derived_title(&draft.markdown).ok()
                }
                TranscriptItem::Plan(
                    PlanRecord::Ready { artifact: earlier }
                    | PlanRecord::Published {
                        artifact: earlier, ..
                    },
                ) if earlier.version.id == id => Some(earlier.title.as_str()),
                _ => None,
            })
            .unwrap_or(fallback_title);
        Some(root.join(self.conversation.session_id()).join(format!(
            "{}-{}.md",
            id,
            slug_words(first_title).unwrap_or_else(|| "plan".to_string())
        )))
    }

    fn project_plan_text(
        &self,
        id: PlanId,
        title: &str,
        markdown: &str,
        exact: bool,
    ) -> Result<(), (PathBuf, String)> {
        let Some(path) = self.plan_projection_path(id, title) else {
            return Ok(());
        };
        let mut markdown = markdown.to_owned();
        if !exact && !markdown.ends_with('\n') {
            markdown.push('\n');
        }
        std::fs::create_dir_all(path.parent().expect("plan directory"))
            .map_err(anyhow::Error::from)
            .and_then(|()| zevria_foundation::atomic_file::replace(&path, markdown.as_bytes()))
            .map_err(|error| (path, error.to_string()))
    }

    /// Restoration never repairs projections or imports manual edits.
    pub(in crate::session) fn project_plan(
        &self,
        artifact: &PlanArtifact,
    ) -> Result<(), (PathBuf, String)> {
        let direct = self.conversation.items().iter().any(|item| {
            matches!(item,
                TranscriptItem::Plan(PlanRecord::Published {
                    artifact: published,
                    provenance: PlanPublicationProvenance::ConfirmedWorker { .. },
                }) if published.version == artifact.version
            )
        });
        self.project_plan_text(
            artifact.version.id,
            &artifact.title,
            &artifact.markdown,
            direct,
        )
    }

    pub(in crate::session) async fn project_plan_draft(
        &mut self,
        draft: &PlanDraft,
        events: &SessionEventSender,
    ) {
        let title = derived_title(&draft.markdown).expect("persisted draft title");
        match self.project_plan_text(draft.target.id, title, &draft.markdown, false) {
            Ok(()) => {
                self.plan_projection_warnings.remove(&draft.target.id);
            }
            Err((path, error)) if self.plan_projection_warnings.insert(draft.target.id) => {
                let _ = events
                    .send(SessionEvent::PlanProjectionWarning {
                        version: draft.target,
                        draft: Some(draft.sequence),
                        path,
                        error,
                    })
                    .await;
            }
            Err(_) => {}
        }
    }

    // Awaited lifecycle helpers keep the engine's exclusive borrow so their
    // futures require providers to be Send, not additionally Sync.
    pub(in crate::session) async fn emit_plan_state_if_changed(
        &mut self,
        previous: PlanWorkflowState,
        events: &SessionEventSender,
    ) -> Result<(), SessionReplayError> {
        if &previous != self.plan_state()? {
            let _ = events
                .send(SessionEvent::PlanStateChanged {
                    state: self.plan_state()?.clone(),
                })
                .await;
        }
        Ok(())
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    pub(in crate::session) fn next_plan_version(&self) -> anyhow::Result<PlanVersion> {
        let PlanWorkflowState::Planning { id, previous, .. } = self.plan_state()? else {
            anyhow::bail!("Plan publication completed outside an active Planning workflow");
        };
        Ok(PlanVersion {
            id: *id,
            revision: previous
                .as_ref()
                .map_or(1, |artifact| artifact.version.revision.saturating_add(1)),
        })
    }

    pub(in crate::session) fn plan_ready_record(
        &self,
        candidate: PlanCandidate,
        turn_id: TurnId,
    ) -> anyhow::Result<(PlanRecord, PlanArtifact)> {
        let artifact = candidate.into_artifact(self.next_plan_version()?, turn_id);
        Ok((
            PlanRecord::Ready {
                artifact: artifact.clone(),
            },
            artifact,
        ))
    }
}

impl<P: ModelProvider> SessionEngine<P> {
    async fn accept_plan_decision(
        &mut self,
        expected: PlanVersion,
        decision: PlanDecision,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<AcceptedTurn, Rejection> {
        self.gate_persistence(events).await?;
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        // Retrying a failed fresh-session launch reuses its durable resolution.
        if let PlanWorkflowState::Resolved {
            artifact,
            resolution: PlanResolution::ImplementedFresh,
        } = self.plan_state()?
            && decision == PlanDecision::ImplementFresh
            && artifact.version == expected
        {
            let handoff =
                PlanHandoff::new(artifact.clone(), self.conversation.session_id().to_string());
            if let Err(error) = self.commit(RecordChange::anchored(
                TurnAnchor::Append,
                Vec::new(),
                SessionMode::Build,
            )) {
                return Err(self
                    .acceptance_failed("fresh-session Plan retry", error, events)
                    .await);
            }
            self.emit_selected_mode(events).await;
            let _ = events
                .send(SessionEvent::FreshPlanHandoffRequested { handoff })
                .await;
            return Ok(AcceptedTurn::new(AcceptedKind::PlanDecision));
        }
        let artifact = match self.plan_state()? {
            PlanWorkflowState::Ready { artifact } | PlanWorkflowState::Published { artifact } => {
                artifact.clone()
            }
            PlanWorkflowState::Planning {
                previous: Some(artifact),
                ..
            } if decision != PlanDecision::Revise => artifact.clone(),
            _ => {
                return Err(Rejection::Rejected(
                    "there is no submitted Plan artifact available for that decision".into(),
                ));
            }
        };
        if artifact.version != expected {
            return Err(Rejection::Rejected(format!(
                "stale Plan decision for {expected}; the submitted artifact is {}",
                artifact.version
            )));
        }
        let previous = self.plan_state()?.clone();
        let handoff =
            PlanHandoff::new(artifact.clone(), self.conversation.session_id().to_string());
        let (mut records, operation) = match decision {
            PlanDecision::Revise => (
                vec![TranscriptItem::Plan(PlanRecord::RevisionRequested {
                    artifact,
                })],
                "Plan revision decision",
            ),
            PlanDecision::ImplementCurrent => (
                vec![
                    TranscriptItem::Plan(PlanRecord::Resolved {
                        id: artifact.version.id,
                        artifact: Some(artifact),
                        resolution: PlanResolution::ImplementedCurrent,
                    }),
                    TranscriptItem::Plan(PlanRecord::Handoff {
                        handoff: handoff.clone(),
                    }),
                ],
                "current-session Plan handoff",
            ),
            PlanDecision::ImplementFresh => (
                vec![TranscriptItem::Plan(PlanRecord::Resolved {
                    id: artifact.version.id,
                    artifact: Some(artifact),
                    resolution: PlanResolution::ImplementedFresh,
                })],
                "fresh-session Plan decision",
            ),
        };
        if decision == PlanDecision::ImplementCurrent {
            records.extend(self.standard_request_boundary());
            records.extend(self.instruction_updates(
                self.conversation.items(),
                self.policies.policy(SessionMode::Build),
                self.active_skills()?,
            )?);
        }
        let mode = match decision {
            PlanDecision::Revise => SessionMode::Plan,
            PlanDecision::ImplementCurrent | PlanDecision::ImplementFresh => SessionMode::Build,
        };
        if let Err(error) = self.commit(RecordChange::anchored(TurnAnchor::Append, records, mode)) {
            return Err(self.acceptance_failed(operation, error, events).await);
        }
        // Clear the Ready presentation lock before publishing the accepted
        // implementation selection; frontends must never present Build while
        // their last authoritative snapshot is still Ready.
        self.emit_plan_state_if_changed(previous, events).await?;
        self.emit_selected_mode(events).await;
        match decision {
            PlanDecision::Revise => Ok(AcceptedTurn::new(AcceptedKind::PlanDecision)),
            PlanDecision::ImplementFresh => {
                let _ = events
                    .send(SessionEvent::FreshPlanHandoffRequested { handoff })
                    .await;
                Ok(AcceptedTurn::new(AcceptedKind::PlanDecision))
            }
            PlanDecision::ImplementCurrent => {
                let _ = events
                    .send(SessionEvent::PlanHandoffStarted {
                        turn_id: turn.id,
                        handoff,
                    })
                    .await;
                Ok(AcceptedTurn::new(AcceptedKind::Handoff))
            }
        }
    }

    pub(in crate::session) async fn resolve_plan(
        &mut self,
        expected: PlanVersion,
        decision: PlanDecision,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        match self
            .accept_plan_decision(expected, decision, events, turn)
            .await
        {
            Err(rejection) => self.finish_rejected(turn, events, rejection).await,
            Ok(accepted) => {
                let outcome = if matches!(accepted.kind, AcceptedKind::Handoff) {
                    self.run_turn(&accepted, events, turn).await
                } else {
                    Ok(TurnCompletion)
                };
                self.finish_accepted(&accepted, turn, events, outcome).await
            }
        }
    }

    async fn accept_handoff(
        &mut self,
        handoff: PlanHandoff,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<AcceptedTurn, Rejection> {
        self.gate_persistence(events).await?;
        if self
            .conversation
            .items()
            .iter()
            .any(|item| !zevria_transcript::transcript::is_leading_metadata(item))
        {
            return Err(Rejection::Rejected(
                "StartFromPlan is only valid for an empty fresh session".into(),
            ));
        }
        if !handoff.is_canonical() {
            return Err(Rejection::Rejected(
                "StartFromPlan rejected a handoff whose prompt does not match its typed artifact"
                    .into(),
            ));
        }
        if turn.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        let mut records = vec![TranscriptItem::Plan(PlanRecord::Handoff {
            handoff: handoff.clone(),
        })];
        records.extend(self.standard_request_boundary());
        records.extend(self.instruction_updates(
            self.conversation.items(),
            self.policies.policy(SessionMode::Build),
            self.active_skills()?,
        )?);
        if let Err(error) = self.commit(RecordChange::anchored(
            TurnAnchor::Append,
            records,
            SessionMode::Build,
        )) {
            return Err(self
                .acceptance_failed("fresh-session Plan handoff", error, events)
                .await);
        }
        self.emit_selected_mode(events).await;
        let _ = events
            .send(SessionEvent::PlanHandoffStarted {
                turn_id: turn.id,
                handoff,
            })
            .await;
        Ok(AcceptedTurn::new(AcceptedKind::Handoff))
    }

    pub(in crate::session) async fn start_from_plan(
        &mut self,
        handoff: PlanHandoff,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> Result<(), SessionReplayError> {
        match self.accept_handoff(handoff, events, turn).await {
            Err(rejection) => self.finish_rejected(turn, events, rejection).await,
            Ok(accepted) => {
                let outcome = self.run_turn(&accepted, events, turn).await;
                self.finish_accepted(&accepted, turn, events, outcome).await
            }
        }
    }
}
