//! Strict root review lifecycle validation. Proof-only histories are not consent.
use crate::transcript::TranscriptItem;
use crate::*;
use std::collections::{HashMap, HashSet};

/// Root-authoritative display restoration. Sidecar audit messages never select
/// a baseline. Callers validate the transcript before exposing live controls.
pub fn project_worker_reviews(
    items: &[TranscriptItem],
) -> Result<HashMap<EnsembleRunId, Vec<WorkerReviewState>>, String> {
    let mut runs: HashMap<EnsembleRunId, Vec<WorkerReviewState>> = HashMap::new();
    for item in items {
        let TranscriptItem::Ensemble(record) = item else {
            continue;
        };
        match record {
            EnsembleRecord::Started { start } if start.workflow == EnsembleWorkflow::Plan => {
                runs.insert(
                    start.run_id.clone(),
                    start
                        .agents
                        .iter()
                        .cloned()
                        .map(WorkerReviewState::new)
                        .collect(),
                );
            }
            EnsembleRecord::WorkerReview {
                run_id,
                worker_id,
                event,
                ..
            } => {
                let states = runs.get_mut(run_id).ok_or("review without Plan start")?;
                apply_worker_review_event(states, worker_id, event)?;
            }
            EnsembleRecord::WorkersConfirmed {
                run_id,
                final_confirmation,
                outcomes,
            } => {
                let states = runs.get_mut(run_id).ok_or("seal without Plan start")?;
                let control = &final_confirmation.control;
                apply_worker_review_event(
                    states,
                    &control.target.worker_id,
                    &control.sealing_event()?,
                )?;
                if states.len() != outcomes.len() {
                    return Err("incomplete frozen set".into());
                }
                for (state, outcome) in states.iter_mut().zip(outcomes) {
                    if &state.outcome() != outcome {
                        return Err("frozen outcome differs from root review".into());
                    }
                    if !state.abandoned {
                        state.apply(&WorkerReviewEvent::Sealed)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(runs)
}

pub fn validate_ensemble_review_history(items: &[TranscriptItem]) -> Result<(), String> {
    struct Run {
        start: EnsembleStart,
        versioned: bool,
        states: Vec<WorkerReviewState>,
        accepted: HashMap<WorkerControlId, WorkerControlResult>,
        frozen: Option<Vec<AgentRunOutcome>>,
        reports: bool,
        published: bool,
        terminal: bool,
    }
    let mut runs: HashMap<EnsembleRunId, Run> = HashMap::new();
    let mut synthesis_run = None;
    for item in items {
        if let TranscriptItem::Plan(PlanRecord::Published {
            artifact,
            provenance,
        }) = item
        {
            let run_id = synthesis_run
                .as_ref()
                .ok_or("Published Plan has no sealed Ensemble Plan reports")?;
            let run = runs
                .get_mut(run_id)
                .ok_or("Published Plan has no sealed Ensemble Plan reports")?;
            if run.terminal || !run.reports || run.frozen.is_none() || run.published {
                return Err(
                    "Published Plan does not follow exactly one sealed worker set and ReportsReady"
                        .into(),
                );
            }
            match provenance {
                PlanPublicationProvenance::Synthesized => {
                    if run.start.publishes_confirmed_worker_plan() {
                        return Err(
                            "single-worker Plan requires confirmed-worker publication provenance"
                                .into(),
                        );
                    }
                }
                PlanPublicationProvenance::ConfirmedWorker {
                    run_id: source_run,
                    worker_id,
                    revision,
                } => {
                    if source_run != run_id || !run.start.publishes_confirmed_worker_plan() {
                        return Err(
                            "direct publication requires the original single-worker Plan run"
                                .into(),
                        );
                    }
                    let outcomes = run.frozen.as_ref().expect("checked frozen set");
                    let outcome = outcomes
                        .first()
                        .ok_or("direct publication has no frozen worker")?;
                    let confirmed = outcome
                        .confirmation
                        .as_ref()
                        .ok_or("direct publication worker is unconfirmed")?;
                    if outcomes.len() != 1
                        || run.start.agents[0] != outcome.descriptor
                        || &outcome.descriptor.id != worker_id
                        || outcome.status != AgentRunStatus::Completed
                        || outcome.partial
                        || !confirmed.validate(worker_id)
                        || confirmed.receipt.target.run_id != *source_run
                        || &confirmed.snapshot.revision != revision
                        || outcome.plan.as_ref() != Some(&confirmed.snapshot.plan)
                        || confirmed.snapshot.plan.markdown.as_ref() != Some(&artifact.markdown)
                    {
                        return Err("direct publication differs from the exact sealed confirmed worker plan".into());
                    }
                }
            }
            run.published = true;
        }
        let TranscriptItem::Ensemble(record) = item else {
            continue;
        };
        if let EnsembleRecord::Started { start } = record {
            if runs.contains_key(&start.run_id) || start.agents.is_empty() {
                return Err("duplicate or empty ensemble start".into());
            }
            let mut ids = HashSet::new();
            if start.agents.iter().any(|agent| !ids.insert(&agent.id)) {
                return Err("duplicate selected worker identity".into());
            }
            runs.insert(
                start.run_id.clone(),
                Run {
                    start: start.clone(),
                    versioned: false,
                    states: start
                        .agents
                        .iter()
                        .cloned()
                        .map(WorkerReviewState::new)
                        .collect(),
                    accepted: HashMap::new(),
                    frozen: None,
                    reports: false,
                    published: false,
                    terminal: false,
                },
            );
            continue;
        }
        let run = runs
            .get_mut(record.run_id())
            .ok_or("ensemble record precedes its start")?;
        if run.terminal {
            return Err("ensemble record follows terminal boundary".into());
        }
        match record {
            EnsembleRecord::ReviewStarted { version, .. } => {
                if run.versioned
                    || *version != ENSEMBLE_REVIEW_VERSION
                    || run.start.workflow != EnsembleWorkflow::Plan
                {
                    return Err(
                        "incompatible ensemble review format or duplicate review start".into(),
                    );
                }
                run.versioned = true;
            }
            EnsembleRecord::WorkerReview {
                worker_id,
                event,
                result,
                ..
            } => {
                if !run.versioned || run.frozen.is_some() {
                    return Err(
                        "worker review transition outside unsealed current-format review".into(),
                    );
                }
                let state = run
                    .states
                    .iter()
                    .find(|state| &state.descriptor.id == worker_id)
                    .ok_or("foreign worker review transition")?;
                if let Some(result) = result {
                    if !result.accepted
                        || &result.control.target.worker_id != worker_id
                        || result.control.target.run_id != run.start.run_id
                        || result.control.request_id.0.trim().is_empty()
                        || result.control.request_id.0.len() > 128
                        || run
                            .accepted
                            .insert(result.control.request_id.clone(), result.clone())
                            .is_some()
                    {
                        return Err("invalid or duplicate accepted worker control".into());
                    }
                    match (&result.control.action, event.as_ref()) {
                        (
                            WorkerControlAction::SendFeedback { text },
                            WorkerReviewEvent::InputAccepted { input },
                        ) if text == &input.text
                            && input.request_id == result.control.request_id
                            && input.kind == WorkerPromptKind::UserFeedback => {}
                        (
                            WorkerControlAction::Retry,
                            WorkerReviewEvent::InputAccepted { input },
                        ) if input.request_id == result.control.request_id
                            && input.kind == WorkerPromptKind::RecoveryContinuation => {}
                        (
                            WorkerControlAction::Confirm { expected_revision },
                            WorkerReviewEvent::Confirmed { receipt },
                        ) if &receipt.revision == expected_revision
                            && receipt.target == result.control.target
                            && receipt.request_id == result.control.request_id => {}
                        (
                            WorkerControlAction::Baseline { expected_revision },
                            WorkerReviewEvent::BaselineMarked { receipt },
                        ) if &receipt.revision == expected_revision
                            && receipt.target == result.control.target
                            && receipt.request_id == result.control.request_id => {}
                        (
                            WorkerControlAction::Unbaseline { expected_revision },
                            WorkerReviewEvent::BaselineCleared { request_id },
                        ) if request_id == &result.control.request_id
                            && state
                                .confirmed_plan()
                                .as_ref()
                                .map(|plan| &plan.snapshot.revision)
                                == Some(expected_revision) => {}
                        (
                            WorkerControlAction::Abandon,
                            WorkerReviewEvent::Abandoned { request_id },
                        ) if request_id == &result.control.request_id => {}
                        (
                            WorkerControlAction::CancelPrompt,
                            WorkerReviewEvent::CancelRequested { .. },
                        ) => {}
                        (
                            WorkerControlAction::Unconfirm { expected_revision },
                            WorkerReviewEvent::Withdrawn { request_id },
                        ) if request_id == &result.control.request_id
                            && state.confirmation.as_ref().map(|receipt| &receipt.revision)
                                == Some(expected_revision) => {}
                        _ => {
                            return Err(
                                "worker control does not match its durable transition".into()
                            );
                        }
                    }
                } else if matches!(
                    event.as_ref(),
                    WorkerReviewEvent::Confirmed { .. }
                        | WorkerReviewEvent::BaselineMarked { .. }
                        | WorkerReviewEvent::BaselineCleared { .. }
                        | WorkerReviewEvent::CancelRequested { .. }
                        | WorkerReviewEvent::Withdrawn { .. }
                        | WorkerReviewEvent::Abandoned { .. }
                        | WorkerReviewEvent::Sealed
                ) {
                    return Err("host review transition lacks an explicit control result".into());
                } else if let WorkerReviewEvent::InputAccepted { input } = event.as_ref()
                    && (input.kind != WorkerPromptKind::Initial || input.text != run.start.prompt)
                {
                    return Err("worker input lacks an exact accepted host control".into());
                }
                apply_worker_review_event(&mut run.states, worker_id, event)?;
                if run.states.iter().any(|state| !state.abandoned)
                    && run
                        .states
                        .iter()
                        .all(|state| state.abandoned || state.confirmed_plan().is_some())
                {
                    return Err(
                        "final confirming control must atomically seal the worker set".into(),
                    );
                }
            }
            EnsembleRecord::ControlResult { result, .. } => {
                if !run.versioned
                    || run.frozen.is_some()
                    || !result.accepted
                    || !matches!(result.control.action, WorkerControlAction::CancelPrompt)
                    || result.control.target.run_id != run.start.run_id
                    || !run.states.iter().any(|state| {
                        state.descriptor.id == result.control.target.worker_id && !state.quiescent()
                    })
                    || result.control.request_id.0.trim().is_empty()
                    || result.control.request_id.0.len() > 128
                    || run
                        .accepted
                        .insert(result.control.request_id.clone(), result.clone())
                        .is_some()
                {
                    return Err("invalid durable worker control result".into());
                }
                let state = run
                    .states
                    .iter_mut()
                    .find(|state| state.descriptor.id == result.control.target.worker_id)
                    .ok_or("foreign cancellation target")?;
                let generation = state
                    .cancellable_generation()
                    .ok_or("cancellation has no active input")?;
                state.apply(&WorkerReviewEvent::CancelRequested { generation })?;
            }
            EnsembleRecord::WorkersConfirmed {
                final_confirmation,
                outcomes,
                ..
            } => {
                if !run.versioned || run.frozen.is_some() || !final_confirmation.accepted {
                    return Err("invalid or duplicate all-worker seal".into());
                }
                let control = &final_confirmation.control;
                let transition = control.sealing_event()?;
                if control.target.run_id != run.start.run_id
                    || run.accepted.contains_key(&control.request_id)
                {
                    return Err("foreign or duplicate final confirmation".into());
                }
                apply_worker_review_event(&mut run.states, &control.target.worker_id, &transition)?;
                if outcomes.len() != run.states.len()
                    || run.states.iter().all(|state| state.abandoned)
                {
                    return Err("incomplete or all-abandoned sealed worker set".into());
                }
                for (state, outcome) in run.states.iter_mut().zip(outcomes) {
                    if (!state.abandoned && state.confirmed_plan().is_none())
                        || &state.outcome() != outcome
                    {
                        return Err(
                            "sealed snapshot differs from authoritative confirmed review".into(),
                        );
                    }
                    if !state.abandoned {
                        state.apply(&WorkerReviewEvent::Sealed)?;
                    }
                }
                run.frozen = Some(outcomes.clone());
            }
            EnsembleRecord::ReportsReady {
                synthesis_input,
                agents,
                ..
            } => {
                if run.reports {
                    return Err("duplicate ReportsReady".into());
                }
                if run.start.workflow == EnsembleWorkflow::Plan {
                    let outcomes = run.frozen.as_ref().ok_or("incompatible proof-only Ensemble Plan ReportsReady: an explicit all-worker confirmation seal is required")?;
                    if &outcomes
                        .iter()
                        .map(AgentRunOutcome::summary)
                        .collect::<Vec<_>>()
                        != agents
                    {
                        return Err(
                            "ReportsReady does not contain the exact sealed worker summaries"
                                .into(),
                        );
                    }
                    let expected = crate::ensemble::build_synthesis_prompt_with_feedback(
                        EnsembleWorkflow::Plan,
                        &run.start.prompt,
                        outcomes,
                        &run.states,
                        usize::MAX,
                    )
                    .map_err(|error| error.to_string())?;
                    if synthesis_input != &expected {
                        return Err("Plan synthesis input differs from the frozen final plans and captured decisions".into());
                    }
                    synthesis_run = Some(run.start.run_id.clone());
                }
                run.reports = true;
            }
            EnsembleRecord::Completed { .. } => {
                if !run.reports || (run.start.workflow == EnsembleWorkflow::Plan && !run.published)
                {
                    return Err(
                        "ensemble completion precedes durable reports or canonical publication"
                            .into(),
                    );
                }
                run.terminal = true;
            }
            EnsembleRecord::Cancelled { .. } | EnsembleRecord::Failed { .. } => {
                run.terminal = true;
            }
            EnsembleRecord::Started { .. } => unreachable!(),
        }
    }
    if runs
        .values()
        .any(|run| run.start.workflow == EnsembleWorkflow::Plan && !run.versioned)
    {
        return Err("incompatible legacy Ensemble Plan history: explicit host review v1 is required; old plan proof cannot be migrated into user confirmation".into());
    }
    Ok(())
}
