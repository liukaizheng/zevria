//! Immutable, model-inert evidence of one provider transmission that never committed.
use serde::{Deserialize, Serialize};
use zevria_content::{
    AssistantPresentationContent, AssistantPresentationPart, HostedActivity, ResponseAttempt,
    ResponseAttemptOutcome,
};
use zevria_foundation::ModelProfileRef;

pub const INCOMPLETE_RESPONSE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteResponseOutcome {
    Failed,
    Interrupted,
}

/// Durable diagnostic evidence only. Never authorizes replay on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRejectionKind {
    TokenWindowBeforeExecution,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncompleteResponse {
    pub version: u32,
    pub attempt_id: String,
    pub profile: ModelProfileRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub outcome: IncompleteResponseOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_rejection: Option<InputRejectionKind>,
    pub presentation: Vec<AssistantPresentationPart>,
    pub hosted_activity: Vec<HostedActivity>,
}

impl IncompleteResponse {
    pub fn from_attempt(attempt: &ResponseAttempt) -> anyhow::Result<Option<Self>> {
        let outcome = match attempt.outcome {
            ResponseAttemptOutcome::Failed => IncompleteResponseOutcome::Failed,
            ResponseAttemptOutcome::Interrupted => IncompleteResponseOutcome::Interrupted,
            _ => anyhow::bail!("only failed or interrupted attempts are recoverable"),
        };
        let presentation = attempt
            .presentation
            .iter()
            .filter(|part| {
                !matches!(
                    part.content,
                    AssistantPresentationContent::NativeTool { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if presentation.is_empty() && attempt.hosted_activity.is_empty() {
            return Ok(None);
        }
        let record = Self {
            version: INCOMPLETE_RESPONSE_VERSION,
            attempt_id: attempt.id.clone(),
            profile: attempt.profile.clone(),
            response_id: attempt.response_id.clone(),
            outcome,
            input_rejection: None,
            presentation,
            hosted_activity: attempt.hosted_activity.clone(),
        };
        record.validate()?;
        Ok(Some(record))
    }

    pub fn token_window_rejection(attempt: &ResponseAttempt) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !attempt.has_display() && attempt.response_id.is_none(),
            "pre-execution rejection cannot carry upstream output or an accepted response ID"
        );
        let record = Self {
            version: INCOMPLETE_RESPONSE_VERSION,
            attempt_id: attempt.id.clone(),
            profile: attempt.profile.clone(),
            response_id: attempt.response_id.clone(),
            outcome: IncompleteResponseOutcome::Failed,
            input_rejection: Some(InputRejectionKind::TokenWindowBeforeExecution),
            presentation: Vec::new(),
            hosted_activity: Vec::new(),
        };
        record.validate()?;
        Ok(record)
    }

    pub fn to_attempt(&self) -> ResponseAttempt {
        let mut presentation = self.presentation.clone();
        if self.input_rejection.is_some() {
            presentation.push(AssistantPresentationPart {
                source: zevria_content::AssistantSourceAddress { output_index:0,part:zevria_content::AssistantPartIdentity::Summary(0),item_id:None },
                content: AssistantPresentationContent::Reasoning {text:"Zevria diagnostic: provider rejected the input token window before execution. This attempt is display-only and cannot authorize replay on resume.".into()},
            });
        }
        ResponseAttempt {
            id: self.attempt_id.clone(),
            profile: self.profile.clone(),
            response_id: self.response_id.clone(),
            outcome: match self.outcome {
                IncompleteResponseOutcome::Failed => ResponseAttemptOutcome::Failed,
                IncompleteResponseOutcome::Interrupted => ResponseAttemptOutcome::Interrupted,
            },
            revision: 1,
            presentation,
            hosted_activity: self.hosted_activity.clone(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == INCOMPLETE_RESPONSE_VERSION,
            "unsupported incomplete response version; expected {INCOMPLETE_RESPONSE_VERSION}"
        );
        if self.input_rejection.is_some() {
            anyhow::ensure!(
                self.outcome == IncompleteResponseOutcome::Failed
                    && self.presentation.is_empty()
                    && self.hosted_activity.is_empty()
                    && self.response_id.is_none(),
                "pre-execution rejection evidence must be failed and contain no upstream output"
            );
        }
        let attempt = self.to_attempt();
        attempt.validate()?;
        anyhow::ensure!(
            attempt.has_display(),
            "incomplete response must have readable output or hosted activity"
        );
        anyhow::ensure!(
            !self.presentation.iter().any(|part| matches!(
                part.content,
                AssistantPresentationContent::NativeTool { .. }
            )),
            "incomplete response cannot bind native tools"
        );
        Ok(())
    }
}
