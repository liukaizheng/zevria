//! Renderer-neutral model identity and accounting for one status surface.

use crate::session::{ConfiguredModelProfiles, SessionState};
use zevria_foundation::{ModelProfileRef, ModelRole, ReasoningLevel};
use zevria_model::{ContextTokenSnapshot, TokenUsage};

#[derive(Clone, Copy, Debug)]
pub struct StatusSnapshot<'a> {
    pub role: ModelRole,
    pub profile: Option<&'a ModelProfileRef>,
    pub reasoning: Option<ReasoningLevel>,
    pub context: Option<&'a ContextTokenSnapshot>,
    pub response: Option<TokenUsage>,
}

impl<'a> StatusSnapshot<'a> {
    pub fn observe(
        session: &'a SessionState,
        configured: &'a ConfiguredModelProfiles,
        role: ModelRole,
    ) -> Self {
        let telemetry = session.telemetry(role);
        Self {
            role,
            profile: telemetry
                .authoritative_profile()
                .or_else(|| configured.get(role)),
            reasoning: session.reasoning(role),
            context: telemetry.context(),
            response: telemetry.response().map(|response| response.usage()),
        }
    }

    /// Filled fraction for known context usage, independent of frontend formatting.
    pub fn context_fraction(tokens: u64, window: u64) -> f32 {
        if window == 0 {
            0.0
        } else {
            (tokens as f64 / window as f64).clamp(0.0, 1.0) as f32
        }
    }
}
