//! Request directives are engine input, never skill pins or summary authority.
use serde::{Deserialize, Serialize};
use zevria_foundation::{RequestBehavior, RequestMetadata};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestDirectiveKind {
    Boundary,
    Correction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDirective {
    pub version: u32,
    pub request: RequestMetadata,
    pub kind: RequestDirectiveKind,
}

impl RequestDirective {
    pub fn boundary(request: RequestMetadata) -> Self {
        Self {
            version: 1,
            request,
            kind: RequestDirectiveKind::Boundary,
        }
    }

    pub fn correction(request: RequestMetadata) -> Self {
        Self {
            version: 1,
            request,
            kind: RequestDirectiveKind::Correction,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported request directive version; expected 1"
        );
        self.request.validate()?;
        anyhow::ensure!(
            self.kind != RequestDirectiveKind::Correction
                || self.request.behavior == RequestBehavior::Orchestrate,
            "only an orchestrated request can own a correction"
        );
        Ok(())
    }

    pub fn render(&self) -> String {
        let body = match (self.kind, self.request.behavior) {
            (RequestDirectiveKind::Boundary, RequestBehavior::Standard) => {
                "Standard request. Any earlier orchestration authorization and delegation obligation have ended. Use only this workflow's ordinary permissions; Builder delegation is not authorized."
            }
            (RequestDirectiveKind::Boundary, RequestBehavior::Orchestrate) => {
                "Explicit orchestration for this Build request only. Explore and Build children are authorized through the existing launch_subtasks tool. Before successful completion, submit at least two meaningful independent subtasks together in one batch and receive at least two distinct accepted child identities in that correlated result. Multiple single-task calls do not qualify. Submit all ready independent work in one call; later dependent single-task batches are allowed. Investigate and implement directly as useful, integrate the reports/artifacts, handle child failures, and validate the result. Do not manufacture tasks to satisfy a counter. If safe decomposition is impossible, explain the blocker; the engine will fail an unsatisfied request, not silently downgrade it. This is concurrent submission to a capable scheduler, not a guarantee of wall-clock overlap. This contract survives continuations and compaction, but never applies to later requests, skills, Plan handoffs, or children."
            }
            (RequestDirectiveKind::Correction, _) => {
                "Concurrent delegation is still unfulfilled. This is the one engine correction for this request. Use launch_subtasks with at least two meaningful independent entries in one batch; at least two distinct accepted child identities in one result are required. Integrate and validate their work. Do not fabricate tasks or claim launches in prose. If decomposition is unsafe, explain the blocker. Another premature final response fails the turn."
            }
        };
        format!("Request directive:\n{body}")
    }
}
