//! Structured synthesis reconciliation and complete decision accounting.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt};

use super::{
    AgentRunId, AgentRunStatus, AgentRunSummary, AgentUnavailableDecisionId, AgentUserDecisionId,
};

/// Decision identifiers fixed by the latest durable `ReportsReady` boundary.
/// This typed context is supplied only to the Ensemble Plan reconciliation
/// tool and is also reconstructed from root transcript summaries after restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportReconciliationCatalog {
    pub baseline: Option<ReportBaseline>,
    pub decision_ids: Vec<AgentUserDecisionId>,
    pub unavailable_decision_ids: Vec<AgentUnavailableDecisionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportBaseline {
    pub worker_id: AgentRunId,
    pub label: String,
}

impl ReportReconciliationCatalog {
    pub fn from_summaries(
        summaries: &[AgentRunSummary],
    ) -> Result<Self, ReportReconciliationError> {
        let mut baseline = None;
        let mut worker_ids = HashSet::new();
        let mut decision_ids = Vec::new();
        let mut seen_decisions = HashSet::new();
        let mut unavailable_decision_ids = Vec::new();
        let mut seen_unavailable = HashSet::new();
        for summary in summaries {
            if !worker_ids.insert(&summary.descriptor.id) {
                return Err(ReportReconciliationError(
                    "duplicate worker in ReportsReady".into(),
                ));
            }
            if summary.status == AgentRunStatus::Abandoned {
                if summary.confirmation.is_some() {
                    return Err(ReportReconciliationError(
                        "abandoned worker has confirmation metadata".into(),
                    ));
                }
                continue;
            }
            if let Some(confirmed) = &summary.confirmation {
                if !confirmed.validate(&summary.descriptor.id) {
                    return Err(ReportReconciliationError(
                        "invalid confirmed worker metadata".into(),
                    ));
                }
                if confirmed.baseline.is_some() {
                    if baseline.is_some()
                        || summary.status != AgentRunStatus::Completed
                        || summary.partial
                        || summary.failure.is_some()
                        || !summary.has_plan_proof
                    {
                        return Err(ReportReconciliationError(
                            "invalid or multiple baseline selections in ReportsReady".into(),
                        ));
                    }
                    baseline = Some(ReportBaseline {
                        worker_id: summary.descriptor.id.clone(),
                        label: summary.descriptor.label.clone(),
                    });
                }
            }
            for decision_id in &summary.decision_ids {
                if seen_decisions.insert(decision_id.clone()) {
                    decision_ids.push(decision_id.clone());
                }
            }
            for unavailable in &summary.unavailable_decisions {
                if seen_unavailable.insert(unavailable.id.clone()) {
                    unavailable_decision_ids.push(unavailable.id.clone());
                }
            }
        }
        Ok(Self {
            baseline,
            decision_ids,
            unavailable_decision_ids,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportDisagreementClassification {
    Factual,
    PreferenceTradeoff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryEvidenceResolutionKind {
    FactualClaim,
    ObjectiveEquivalence,
    Infeasible,
    Incorrect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportPosition {
    pub label: String,
    pub position: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReportDisagreementResolution {
    RepositoryEvidence {
        kind: RepositoryEvidenceResolutionKind,
        evidence: String,
    },
    ExplicitUserRequirement {
        requirement: String,
    },
    RecordedUserDecisions {
        decision_ids: Vec<AgentUserDecisionId>,
        application: String,
    },
    BaselinePrecedence {
        worker_id: String,
        application: String,
    },
    RootQuestion {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportDisagreement {
    pub id: String,
    pub summary: String,
    pub positions: Vec<ReportPosition>,
    pub classification: ReportDisagreementClassification,
    pub resolution: ReportDisagreementResolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedDecisionDisposition {
    Applied { explanation: String },
    ObjectivelyInapplicable { evidence: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordedDecisionAccounting {
    pub decision_id: AgentUserDecisionId,
    pub disposition: RecordedDecisionDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnavailableDecisionDisposition {
    RootQuestionRequired { reason: String },
    ObjectivelyInapplicable { evidence: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnavailableDecisionAccounting {
    pub unavailable_decision_id: AgentUnavailableDecisionId,
    pub disposition: UnavailableDecisionDisposition,
}

/// Durable declaration the root model must make after repository inspection
/// and before any optional root question or Plan submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportReconciliation {
    pub disagreements: Vec<ReportDisagreement>,
    pub decisions: Vec<RecordedDecisionAccounting>,
    pub unavailable_decisions: Vec<UnavailableDecisionAccounting>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationNextStep {
    Question,
    SubmitPlan,
}

impl fmt::Display for ReconciliationNextStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Question => "question",
            Self::SubmitPlan => "submit_plan",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedReportReconciliation {
    pub declaration: ReportReconciliation,
    pub next_step: ReconciliationNextStep,
}

impl rig_core::tool::ContextValue for ValidatedReportReconciliation {
    const KEY: &'static str = "zevria.report_reconciliation";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportReconciliationError(String);

impl fmt::Display for ReportReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ReportReconciliationError {}

impl ReportReconciliation {
    pub fn validate(
        self,
        catalog: &ReportReconciliationCatalog,
    ) -> Result<ValidatedReportReconciliation, ReportReconciliationError> {
        let invalid = |message: String| ReportReconciliationError(message);
        let known_decisions = catalog.decision_ids.iter().cloned().collect::<HashSet<_>>();
        let known_unavailable = catalog
            .unavailable_decision_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut disagreement_ids = HashSet::new();
        let mut referenced_decisions = HashSet::new();
        let mut requires_question = false;

        for (index, disagreement) in self.disagreements.iter().enumerate() {
            if !is_snake_case_identifier(&disagreement.id) {
                return Err(invalid(format!(
                    "disagreements[{index}].id must be a non-empty snake_case identifier"
                )));
            }
            if !disagreement_ids.insert(disagreement.id.clone()) {
                return Err(invalid(format!(
                    "disagreement id {:?} is duplicated",
                    disagreement.id
                )));
            }
            if disagreement.summary.trim().is_empty() {
                return Err(invalid(format!(
                    "disagreements[{index}].summary must not be blank"
                )));
            }
            if disagreement.positions.len() < 2 {
                return Err(invalid(format!(
                    "disagreements[{index}].positions must contain at least two labeled positions"
                )));
            }
            let mut labels = HashSet::new();
            for (position_index, position) in disagreement.positions.iter().enumerate() {
                if position.label.trim().is_empty() || position.position.trim().is_empty() {
                    return Err(invalid(format!(
                        "disagreements[{index}].positions[{position_index}] must have a nonblank label and position"
                    )));
                }
                if !labels.insert(position.label.trim().to_lowercase()) {
                    return Err(invalid(format!(
                        "disagreements[{index}] contains duplicate position label {:?}",
                        position.label
                    )));
                }
            }
            match &disagreement.resolution {
                ReportDisagreementResolution::RepositoryEvidence { evidence, .. } => {
                    if disagreement.classification
                        == ReportDisagreementClassification::PreferenceTradeoff
                    {
                        return Err(invalid(format!(
                            "preference_tradeoff disagreement {:?} cannot be resolved from repository_evidence",
                            disagreement.id
                        )));
                    }
                    require_nonblank(
                        evidence,
                        &format!("disagreements[{index}].resolution.evidence"),
                    )?;
                }
                ReportDisagreementResolution::ExplicitUserRequirement { requirement } => {
                    require_nonblank(
                        requirement,
                        &format!("disagreements[{index}].resolution.requirement"),
                    )?;
                }
                ReportDisagreementResolution::RecordedUserDecisions {
                    decision_ids,
                    application,
                } => {
                    if decision_ids.is_empty() {
                        return Err(invalid(format!(
                            "disagreements[{index}].resolution.decision_ids must not be empty"
                        )));
                    }
                    require_nonblank(
                        application,
                        &format!("disagreements[{index}].resolution.application"),
                    )?;
                    let mut local = HashSet::new();
                    for decision_id in decision_ids {
                        if !local.insert(decision_id.clone()) {
                            return Err(invalid(format!(
                                "disagreements[{index}] references decision id {decision_id:?} more than once"
                            )));
                        }
                        if !known_decisions.contains(decision_id) {
                            return Err(invalid(format!(
                                "disagreements[{index}] references unknown decision id {decision_id}"
                            )));
                        }
                        referenced_decisions.insert(decision_id.clone());
                    }
                }
                ReportDisagreementResolution::BaselinePrecedence {
                    worker_id,
                    application,
                } => {
                    let baseline = catalog.baseline.as_ref().ok_or_else(|| {
                        invalid(
                            "baseline_precedence requires a host-selected baseline in ReportsReady"
                                .into(),
                        )
                    })?;
                    if worker_id != baseline.worker_id.as_str()
                        || disagreement.classification
                            != ReportDisagreementClassification::PreferenceTradeoff
                    {
                        return Err(invalid("baseline_precedence requires the exact selected worker ID and a preference_tradeoff, never a factual disagreement".into()));
                    }
                    require_nonblank(
                        application,
                        &format!("disagreements[{index}].resolution.application"),
                    )?;
                }
                ReportDisagreementResolution::RootQuestion { reason } => {
                    require_nonblank(reason, &format!("disagreements[{index}].resolution.reason"))?;
                    requires_question = true;
                }
            }
        }

        let mut accounted_decisions = HashSet::new();
        let mut applied_decisions = HashSet::new();
        for (index, accounting) in self.decisions.iter().enumerate() {
            if !known_decisions.contains(&accounting.decision_id) {
                return Err(invalid(format!(
                    "decisions[{index}] references unknown decision id {}",
                    accounting.decision_id
                )));
            }
            if !accounted_decisions.insert(accounting.decision_id.clone()) {
                return Err(invalid(format!(
                    "decision id {} is accounted more than once",
                    accounting.decision_id
                )));
            }
            match &accounting.disposition {
                RecordedDecisionDisposition::Applied { explanation } => {
                    require_nonblank(explanation, &format!("decisions[{index}].explanation"))?;
                    applied_decisions.insert(accounting.decision_id.clone());
                }
                RecordedDecisionDisposition::ObjectivelyInapplicable { evidence } => {
                    require_nonblank(evidence, &format!("decisions[{index}].evidence"))?;
                }
            }
        }
        require_exact_catalog_coverage("decision", &known_decisions, &accounted_decisions)?;
        for decision_id in referenced_decisions {
            if !applied_decisions.contains(&decision_id) {
                return Err(invalid(format!(
                    "decision id {decision_id} resolves a disagreement but is not accounted as applied"
                )));
            }
        }

        let mut accounted_unavailable = HashSet::new();
        for (index, accounting) in self.unavailable_decisions.iter().enumerate() {
            if !known_unavailable.contains(&accounting.unavailable_decision_id) {
                return Err(invalid(format!(
                    "unavailable_decisions[{index}] references unknown marker id {}",
                    accounting.unavailable_decision_id
                )));
            }
            if !accounted_unavailable.insert(accounting.unavailable_decision_id.clone()) {
                return Err(invalid(format!(
                    "unavailable decision marker {} is accounted more than once",
                    accounting.unavailable_decision_id
                )));
            }
            match &accounting.disposition {
                UnavailableDecisionDisposition::RootQuestionRequired { reason } => {
                    require_nonblank(reason, &format!("unavailable_decisions[{index}].reason"))?;
                    requires_question = true;
                }
                UnavailableDecisionDisposition::ObjectivelyInapplicable { evidence } => {
                    require_nonblank(
                        evidence,
                        &format!("unavailable_decisions[{index}].evidence"),
                    )?;
                }
            }
        }
        require_exact_catalog_coverage(
            "unavailable decision marker",
            &known_unavailable,
            &accounted_unavailable,
        )?;

        Ok(ValidatedReportReconciliation {
            declaration: self,
            next_step: if requires_question {
                ReconciliationNextStep::Question
            } else {
                ReconciliationNextStep::SubmitPlan
            },
        })
    }
}

fn require_nonblank(value: &str, path: &str) -> Result<(), ReportReconciliationError> {
    if value.trim().is_empty() {
        Err(ReportReconciliationError(format!(
            "{path} must not be blank"
        )))
    } else {
        Ok(())
    }
}

fn require_exact_catalog_coverage<T>(
    label: &str,
    known: &HashSet<T>,
    accounted: &HashSet<T>,
) -> Result<(), ReportReconciliationError>
where
    T: fmt::Display + Eq + std::hash::Hash,
{
    if known == accounted {
        return Ok(());
    }
    let mut missing = known
        .difference(accounted)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    missing.sort();
    Err(ReportReconciliationError(format!(
        "every ReportsReady {label} must be accounted exactly once; missing: {}",
        if missing.is_empty() {
            "none".to_string()
        } else {
            missing.join(", ")
        }
    )))
}

fn is_snake_case_identifier(id: &str) -> bool {
    let mut segments = id.split('_');
    let valid_segment = |segment: &str| {
        let mut characters = segment.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
            && characters
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    };
    let has_segment = segments.clone().next().is_some();
    has_segment && segments.all(valid_segment)
}
