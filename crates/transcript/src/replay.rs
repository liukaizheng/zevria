use crate::*;
use std::fmt;
/// Invalid committed session replay. The engine cannot resume work after this error.
/// Repair must happen outside the failed engine, preserving the original evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReplayError {
    Directives(String),
    Skills(String),
    Plan(String),
    Ensemble(String),
    Both { skills: String, plan: String },
}

impl fmt::Display for SessionReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Directives(error) => write!(
                f,
                "invalid instruction replay: {error}; start a fresh session"
            ),
            Self::Skills(error) => write!(f, "invalid skill replay: {error}"),
            Self::Plan(error) => write!(f, "invalid Plan replay: {error}"),
            Self::Ensemble(error) => write!(f, "invalid ensemble replay: {error}"),
            Self::Both { skills, plan } => {
                write!(
                    f,
                    "invalid skill replay: {skills}; invalid Plan replay: {plan}"
                )
            }
        }
    }
}

impl std::error::Error for SessionReplayError {}

impl SessionReplayError {
    /// Preflight replay linkage and reducers before opening writable handles or publishing projections.
    pub fn validate(items: &[TranscriptItem]) -> Result<(), Self> {
        validate_session_replay(items).map(|_| ())
    }
}

/// All reducers validated together; no writable handle or engine state is retained.
pub struct ValidatedSessionReplay {
    pub instructions: InstructionReplayState,
    pub plan: PlanWorkflowState,
}

pub fn validate_session_replay(
    items: &[TranscriptItem],
) -> Result<ValidatedSessionReplay, SessionReplayError> {
    #[cfg(feature = "test-support")]
    crate::replay_probe::record(|counts| counts.validations += 1);
    crate::validate_ensemble_review_history(items).map_err(SessionReplayError::Ensemble)?;
    let instructions = InstructionReplayState::replay(items);
    let plan = replay_plan_state(items.iter().filter_map(|item| match item {
        TranscriptItem::Plan(record) => Some(record),
        _ => None,
    }))
    .map_err(|error| format!("{error:#}"));
    match instructions {
        Ok(instructions) => Ok(ValidatedSessionReplay {
            instructions,
            plan: plan.map_err(SessionReplayError::Plan)?,
        }),
        Err(error) => {
            // A directive/header failure may precede a later invalid pin. Only
            // failed histories pay for this independent full-ledger check, which
            // preserves Skills/Plan/Both priority over directive failures.
            let skills = replay_active_skills(items).map_err(|error| format!("{error:#}"));
            match (skills, plan) {
                (Err(skills), Err(plan)) => Err(SessionReplayError::Both { skills, plan }),
                (Err(error), _) => Err(SessionReplayError::Skills(error)),
                (_, Err(error)) => Err(SessionReplayError::Plan(error)),
                (Ok(_), Ok(_)) => Err(SessionReplayError::Directives(error.to_string())),
            }
        }
    }
}
