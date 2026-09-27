use zevria_content::web_search::*;
/// Shared native/ACP restoration. Operates on a display copy, never the saved
/// log. Only explicit attempt links authorize reconstruction from native replay.
/// Callers restore validated histories: an elided attempt without its later
/// linked native replay is a hard load error, not displayable fallback text.
pub fn reconstruct_transcript(
    items: &[crate::transcript::TranscriptItem],
) -> Vec<crate::transcript::TranscriptItem> {
    use crate::transcript::TranscriptItem;
    let mut result = items.to_vec();
    for (index, item) in items.iter().enumerate() {
        let TranscriptItem::WebSearchAttempt(saved) = item else {
            continue;
        };
        let mut attempt = saved.clone();
        attempt.finish(WebSearchAttemptOutcome::Interrupted);
        if let Some(replay) = items
            .iter()
            .find(|item| item.display_attempt_id() == Some(saved.id.as_str()))
            .and_then(TranscriptItem::provider_replay)
        {
            // Link commit elides the durable presentation. Rebuild it only on
            // this display copy, leaving replay and saved lifecycle evidence
            // untouched.
            attempt.reconcile_native_presentation(&replay.items);
        }
        result[index] = TranscriptItem::WebSearchAttempt(attempt);
    }
    result
}
