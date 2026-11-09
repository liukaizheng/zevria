//! Frontend-neutral transcript projection, interaction grammar and session reduction.
//! No renderer, filesystem reads, provider, tools or engine ownership lives here.
#[path = "agent_transcript.rs"]
pub mod agents;
pub mod blocks;
pub mod command;
pub mod completion;
pub mod conversation;
pub mod jobs;
pub mod projection;
pub mod session;
pub mod session_model;
#[cfg(test)]
mod session_model_tests;
pub mod shell;
pub mod status_icon;
pub mod workflow;
pub use blocks::{TURN_CANCELLED, incomplete_response_outcome};
pub use session_model::{
    ConversationTail, PaneScope, Reduction, RestorationInput, SessionModel, Settlement,
};
// Stable internal aliases retained while moving the existing projection code.
#[cfg(test)]
use agents as agent_transcript;
use blocks as presentation;

#[cfg(test)]
mod notice_tests {
    use super::*;
    use blocks::PresentationBlockKind;
    use zevria_content::ResponseAttemptOutcome;

    #[test]
    fn incomplete_notices_round_trip_without_matching_ordinary_text() {
        for outcome in [
            ResponseAttemptOutcome::Failed,
            ResponseAttemptOutcome::Interrupted,
        ] {
            let marker = blocks::incomplete_response_marker(outcome).unwrap();
            assert_eq!(
                incomplete_response_outcome(&PresentationBlockKind::Placeholder(marker.into())),
                Some(outcome)
            );
        }
        assert!(
            incomplete_response_outcome(&PresentationBlockKind::Placeholder("ordinary".into()))
                .is_none()
        );
        assert!(blocks::incomplete_response_marker(ResponseAttemptOutcome::Completed).is_none());
        assert_eq!(TURN_CANCELLED, "Turn cancelled");
    }
}
