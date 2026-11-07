use std::time::Instant;
use zevria_foundation::ModelRole;
use zevria_presentation::{ConversationTail, SessionModel};
pub fn primary(model: &SessionModel, cancelling: bool) -> String {
    if model.session.persistence_error().is_some() {
        return "Persistence degraded — new work is blocked".into();
    }
    if model.workflow.fresh_retry_version().is_some() {
        return "Fresh Plan handoff pending".into();
    }
    if model.workflow.dialog().is_some() {
        return "Plan approval".into();
    }
    if cancelling {
        return "Cancelling…".into();
    }
    let activity = match model.tail(Instant::now()) {
        ConversationTail::None => {
            if model.is_busy() {
                "Selecting mode"
            } else {
                "Ready"
            }
        }
        ConversationTail::Waiting { .. } => "Waiting",
        ConversationTail::Streaming { .. } => "Streaming",
        ConversationTail::Compacting { .. } => "Compacting",
        ConversationTail::Retrying { .. } => "Reconnecting",
    };
    format!("{} · {activity}", model.display_role().name())
}
pub fn telemetry(model: &SessionModel) -> String {
    let role = model.display_role();
    let telemetry = model.session.telemetry(role);
    let profile = telemetry
        .authoritative_profile()
        .or_else(|| model.model_profiles.get(role))
        .map_or_else(|| "No model".into(), ToString::to_string);
    let mut parts = vec![profile];
    if let Some(reasoning) = model.session.reasoning(role) {
        parts.push(format!("{reasoning:?}"));
    }
    if let Some(context) = telemetry.context() {
        parts.push(format!(
            "{} / {} context",
            context.projected_input_tokens, context.context_window_tokens
        ));
    }
    if let Some(response) = telemetry.response() {
        parts.push(format!(
            "{} response tokens",
            response.usage().output_tokens
        ));
    }
    if let Some((network, _)) = model.session.network_message() {
        parts.push(network);
    }
    parts.join(" · ")
}
pub fn accent(model: &SessionModel, cx: &gpui_kit::App) -> gpui_kit::Hsla {
    let t = super::theme::palette(cx);
    match model.display_role() {
        ModelRole::Build | ModelRole::Builder => t.workflow.build,
        ModelRole::Plan => t.workflow.plan,
        ModelRole::Review => t.workflow.review,
        ModelRole::Explore => t.workflow.explore,
    }
}
