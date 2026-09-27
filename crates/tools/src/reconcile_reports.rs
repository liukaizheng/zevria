//! Typed Ensemble Plan report reconciliation.

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::schema_for;
use zevria_foundation::RECONCILE_REPORTS_TOOL_NAME;
use zevria_workflow::ReportReconciliation;
use zevria_workflow::ReportReconciliationCatalog;
use zevria_workflow::ValidatedReportReconciliation;

const DESCRIPTION: &str = r#"Declare typed resolutions for worker-report disagreements and Zevria-captured decisions.

Classify each disagreement as `factual` or `preference_tradeoff` and supply its resolution. An empty disagreement list is valid. Use exact decision IDs and unavailable-decision markers from ReportsReady in the corresponding accounting arrays.

`baseline_precedence` is valid only for a `preference_tradeoff` and its `worker_id` must be the exact selected confirmation.target.worker_id, never a display label. Its nonblank `application` must explain the baseline's actual stated position and how it resolves the disagreement; do not invent a stance. Disambiguate repeated labels with worker IDs.

The result identifies the next valid workflow step: `question` or `submit_plan`."#;

#[derive(Debug)]
pub enum ReconcileReportsError {
    InvalidArguments(String),
    Unavailable(String),
}

impl std::fmt::Display for ReconcileReportsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArguments(message) | Self::Unavailable(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for ReconcileReportsError {}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileReportsTool;

impl Tool for ReconcileReportsTool {
    const NAME: &'static str = RECONCILE_REPORTS_TOOL_NAME;
    type Error = ReconcileReportsError;
    type Args = ReportReconciliation;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(ReportReconciliation).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match &error {
            ReconcileReportsError::InvalidArguments(_) => {
                ToolExecutionError::invalid_args(error.to_string())
            }
            ReconcileReportsError::Unavailable(_) => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let catalog = context
            .get::<ReportReconciliationCatalog>()
            .ok_or_else(|| {
                ReconcileReportsError::Unavailable(
                    "reconcile_reports requires an active Ensemble Plan ReportsReady catalog"
                        .to_string(),
                )
            })?;
        let accepted = args
            .validate(catalog)
            .map_err(|error| ReconcileReportsError::InvalidArguments(error.to_string()))?;
        let next_step = accepted.next_step;
        context.insert_result::<ValidatedReportReconciliation>(accepted);
        Ok(format!(
            "Reconciliation declaration accepted. The next valid step is `{next_step}`."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_workflow::AgentUserDecisionId;
    use zevria_workflow::RecordedDecisionAccounting;
    use zevria_workflow::RecordedDecisionDisposition;
    use zevria_workflow::ReportDisagreement;
    use zevria_workflow::ReportDisagreementClassification;
    use zevria_workflow::ReportDisagreementResolution;
    use zevria_workflow::ReportPosition;

    fn decision_id() -> AgentUserDecisionId {
        AgentUserDecisionId::from_question(
            &zevria_foundation::QuestionRequestId::new("request"),
            "scope",
        )
    }

    fn declaration(classification: ReportDisagreementClassification) -> ReportReconciliation {
        let decision_id = decision_id();
        ReportReconciliation {
            disagreements: vec![ReportDisagreement {
                id: "scope_choice".to_string(),
                summary: "Workers propose different user-visible scope.".to_string(),
                positions: vec![
                    ReportPosition {
                        label: "preserve".to_string(),
                        position: "Preserve Insert mode.".to_string(),
                    },
                    ReportPosition {
                        label: "exit".to_string(),
                        position: "Auto-exit Insert mode.".to_string(),
                    },
                ],
                classification,
                resolution: ReportDisagreementResolution::RecordedUserDecisions {
                    decision_ids: vec![decision_id.clone()],
                    application: "Apply the captured auto-exit selection.".to_string(),
                },
            }],
            decisions: vec![RecordedDecisionAccounting {
                decision_id,
                disposition: RecordedDecisionDisposition::Applied {
                    explanation: "Controls the Insert-mode scope fork.".to_string(),
                },
            }],
            unavailable_decisions: Vec::new(),
        }
    }

    #[test]
    fn schema_is_strict_and_describes_required_sequence() {
        let tool = ReconcileReportsTool;
        let schema = tool.parameters();
        assert_eq!(ReconcileReportsTool::NAME, "reconcile_reports");
        assert_eq!(
            schema["required"],
            serde_json::json!(["disagreements", "decisions", "unavailable_decisions"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            tool.description()
                .contains("`baseline_precedence` is valid only for a `preference_tradeoff`")
        );
        assert!(
            tool.description()
                .contains("exact selected confirmation.target.worker_id")
        );
        assert!(tool.description().contains("`application` must explain"));
        assert!(!tool.description().contains("cannot choose"));
        assert!(!tool.description().contains("is unavailable"));
    }

    #[tokio::test]
    async fn baseline_precedence_requires_selected_worker_id_and_keeps_decision_accounting() {
        let worker_id = zevria_workflow::AgentRunId::new();
        let catalog = ReportReconciliationCatalog {
            baseline: Some(zevria_workflow::ReportBaseline {
                worker_id: worker_id.clone(),
                label: "Repeated label".into(),
            }),
            decision_ids: vec![decision_id()],
            unavailable_decision_ids: vec![],
        };
        let mut args = declaration(ReportDisagreementClassification::PreferenceTradeoff);
        args.disagreements[0].resolution = ReportDisagreementResolution::BaselinePrecedence {
            worker_id: worker_id.to_string(),
            application:
                "Preserve the baseline's stated preference after applying the captured choice"
                    .into(),
        };
        let mut context = ToolContext::new();
        context.insert(catalog.clone());
        ReconcileReportsTool
            .call(&mut context, args.clone())
            .await
            .unwrap();
        assert!(context.result::<ValidatedReportReconciliation>().is_some());
        assert!(
            ReconcileReportsTool
                .parameters()
                .to_string()
                .contains("baseline_precedence")
        );
        for invalid_kind in [
            "label",
            "factual",
            "blank",
            "missing_decision",
            "no_baseline",
        ] {
            let mut invalid = args.clone();
            let mut catalog = catalog.clone();
            match invalid_kind {
                "label" => {
                    invalid.disagreements[0].resolution =
                        ReportDisagreementResolution::BaselinePrecedence {
                            worker_id: "Repeated label".into(),
                            application: "label cannot authorize".into(),
                        }
                }
                "factual" => {
                    invalid.disagreements[0].classification =
                        ReportDisagreementClassification::Factual
                }
                "blank" => {
                    invalid.disagreements[0].resolution =
                        ReportDisagreementResolution::BaselinePrecedence {
                            worker_id: worker_id.to_string(),
                            application: " ".into(),
                        }
                }
                "missing_decision" => invalid.decisions.clear(),
                "no_baseline" => catalog.baseline = None,
                _ => unreachable!(),
            }
            let mut context = ToolContext::new();
            context.insert(catalog);
            assert!(
                ReconcileReportsTool
                    .call(&mut context, invalid)
                    .await
                    .is_err(),
                "{invalid_kind}"
            );
            assert!(context.result::<ValidatedReportReconciliation>().is_none());
        }
    }

    #[tokio::test]
    async fn accepted_declaration_returns_typed_next_step() {
        let decision_id = decision_id();
        let mut context = ToolContext::new();
        context.insert(ReportReconciliationCatalog {
            baseline: None,
            decision_ids: vec![decision_id],
            unavailable_decision_ids: Vec::new(),
        });
        let output = ReconcileReportsTool
            .call(
                &mut context,
                declaration(ReportDisagreementClassification::PreferenceTradeoff),
            )
            .await
            .expect("valid reconciliation");
        assert!(output.contains("`submit_plan`"));
        assert!(context.result::<ValidatedReportReconciliation>().is_some());
    }

    #[tokio::test]
    async fn duplicate_unknown_and_missing_catalog_entries_are_rejected() {
        let decision_id = decision_id();
        let catalog = ReportReconciliationCatalog {
            baseline: None,
            decision_ids: vec![decision_id.clone()],
            unavailable_decision_ids: Vec::new(),
        };

        let mut duplicate = declaration(ReportDisagreementClassification::PreferenceTradeoff);
        duplicate.decisions.push(duplicate.decisions[0].clone());
        let mut context = ToolContext::new();
        context.insert(catalog.clone());
        assert!(
            ReconcileReportsTool
                .call(&mut context, duplicate)
                .await
                .expect_err("duplicate accounting")
                .to_string()
                .contains("more than once")
        );

        let mut missing = declaration(ReportDisagreementClassification::PreferenceTradeoff);
        missing.decisions.clear();
        let mut context = ToolContext::new();
        context.insert(catalog.clone());
        assert!(
            ReconcileReportsTool
                .call(&mut context, missing)
                .await
                .expect_err("missing accounting")
                .to_string()
                .contains("must be accounted exactly once")
        );

        let unknown_id = AgentUserDecisionId::from_question(
            &zevria_foundation::QuestionRequestId::new("unknown-request"),
            "scope",
        );
        let mut unknown = declaration(ReportDisagreementClassification::PreferenceTradeoff);
        unknown.decisions[0].decision_id = unknown_id;
        let mut context = ToolContext::new();
        context.insert(catalog);
        assert!(
            ReconcileReportsTool
                .call(&mut context, unknown)
                .await
                .expect_err("unknown accounting")
                .to_string()
                .contains("unknown decision id")
        );
    }

    #[tokio::test]
    async fn preference_repository_evidence_is_rejected_without_typed_state() {
        let mut args = declaration(ReportDisagreementClassification::PreferenceTradeoff);
        args.disagreements[0].resolution = ReportDisagreementResolution::RepositoryEvidence {
            kind: zevria_workflow::RepositoryEvidenceResolutionKind::FactualClaim,
            evidence: "The current tests preserve Insert mode.".to_string(),
        };
        let decision_id = decision_id();
        let mut context = ToolContext::new();
        context.insert(ReportReconciliationCatalog {
            baseline: None,
            decision_ids: vec![decision_id],
            unavailable_decision_ids: Vec::new(),
        });
        let error = ReconcileReportsTool
            .call(&mut context, args)
            .await
            .expect_err("preference cannot use repository evidence");
        assert!(error.to_string().contains("cannot be resolved"));
        assert!(context.result::<ValidatedReportReconciliation>().is_none());
    }
}
