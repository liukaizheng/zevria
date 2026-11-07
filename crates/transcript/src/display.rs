use crate::TranscriptItem;
use zevria_content::{
    AssistantPresentationPart, HostedActivity, ResponseAttempt, ResponseAttemptOutcome,
};
use zevria_model::ReplayProtocol;

/// Committed Responses display is solely a projection of its native ledger.
/// Non-Responses and plain messages retain ordinary message rendering.
pub fn committed_display(
    item: &TranscriptItem,
) -> Option<(Vec<AssistantPresentationPart>, Vec<HostedActivity>)> {
    let replay = item.provider_replay()?;
    if replay.protocol() != Some(ReplayProtocol::Responses) {
        return None;
    }
    let display = zevria_content::response::responses_native_display(&replay.items);
    (!display.0.is_empty() || !display.1.is_empty()).then_some(display)
}

/// Derive the same display used at live commit, without mutating saved records.
pub fn restored_attempt(item: &TranscriptItem, index: usize) -> Option<ResponseAttempt> {
    if let TranscriptItem::IncompleteResponse(record) = item {
        return Some(record.to_attempt());
    }
    let (presentation, hosted_activity) = committed_display(item)?;
    Some(ResponseAttempt {
        id: format!("transcript-{index}"),
        profile: item.provider_replay()?.source_profile.clone(),
        response_id: None,
        outcome: ResponseAttemptOutcome::Completed,
        revision: 1,
        presentation,
        hosted_activity,
    })
}
