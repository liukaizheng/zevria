//! Structured status segments: never a punctuation-joined telemetry sentence.
use super::{
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use gpui_kit::{prelude::*, *};
use std::time::Instant;
use zevria_foundation::ModelRole;
use zevria_presentation::{ConversationTail, SessionModel, status_icon::StatusIcon};

#[derive(Debug, PartialEq)]
pub struct ContextGauge {
    pub label: String,
    pub fraction: f32,
    pub warning: bool,
}
impl ContextGauge {
    fn new(input: u64, window: u64) -> Self {
        let fraction = if window == 0 {
            0.
        } else {
            (input as f64 / window as f64).clamp(0., 1.) as f32
        };
        Self {
            label: format!("{} of {}", compact_number(input), compact_number(window)),
            fraction,
            warning: fraction > 0.8,
        }
    }
}
#[derive(Debug, PartialEq)]
pub struct StatusLine {
    pub glyph: StatusIcon,
    pub mode: String,
    pub activity: String,
    pub persistence_failed: bool,
    pub profile: String,
    pub reasoning: Option<String>,
    pub context: Option<ContextGauge>,
    pub output: Option<String>,
    pub network: Option<String>,
}
impl StatusLine {
    pub fn new(model: &SessionModel, cancelling: bool) -> Self {
        let persistence_failed = model.session.persistence_error().is_some();
        let activity = activity(
            persistence_failed,
            model.workflow.fresh_retry_version().is_some(),
            model.workflow.dialog().is_some(),
            cancelling,
            model.tail(Instant::now()),
        );
        let role = model.display_role();
        let telemetry = model.session.telemetry(role);
        Self {
            glyph: if persistence_failed {
                StatusIcon::Failed
            } else if model.is_busy() {
                StatusIcon::Running
            } else {
                StatusIcon::Pending
            },
            mode: match role {
                ModelRole::Build | ModelRole::Builder => "Build",
                ModelRole::Plan => "Plan",
                ModelRole::Review => "Review",
                ModelRole::Explore => "Explore",
            }
            .into(),
            activity: activity.into(),
            persistence_failed,
            profile: telemetry
                .authoritative_profile()
                .or_else(|| model.model_profiles.get(role))
                .map_or_else(|| "No model".into(), ToString::to_string),
            reasoning: model
                .session
                .reasoning(role)
                .map(|r| format!("{r:?}").to_lowercase()),
            context: telemetry
                .context()
                .map(|c| ContextGauge::new(c.projected_input_tokens, c.context_window_tokens)),
            output: telemetry
                .response()
                .map(|r| format!("{} output tokens", compact_number(r.usage().output_tokens))),
            network: model.session.network_message().map(|(m, _)| m),
        }
    }
}
fn activity(
    failed: bool,
    fresh: bool,
    plan: bool,
    cancelling: bool,
    tail: ConversationTail<'_>,
) -> &'static str {
    if failed {
        return "Saving failed. New work is blocked.";
    }
    if fresh {
        return "Fresh Plan handoff pending";
    }
    if plan {
        return "Plan approval";
    }
    if cancelling {
        return "Stopping…";
    }
    match tail {
        ConversationTail::None => "Ready",
        ConversationTail::Waiting { .. } => "Waiting",
        ConversationTail::Streaming { .. } => "Streaming",
        ConversationTail::Compacting { .. } => "Compacting",
        ConversationTail::Retrying { .. } => "Reconnecting",
    }
}
pub fn compact_number(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let (divisor, suffix) = if n >= 1_000_000 {
        (1_000_000., "m")
    } else {
        (1000., "k")
    };
    let value = n as f64 / divisor;
    let rounded = format!("{value:.1}");
    format!("{}{suffix}", rounded.strip_suffix(".0").unwrap_or(&rounded))
}
pub fn accent(model: &SessionModel, cx: &App) -> Hsla {
    let t = palette(cx);
    match model.display_role() {
        ModelRole::Build | ModelRole::Builder => t.workflow.build,
        ModelRole::Plan => t.workflow.plan,
        ModelRole::Review => t.workflow.review,
        ModelRole::Explore => t.workflow.explore,
    }
}
impl DesktopWindow {
    pub(super) fn status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let model = controller.model.visible();
        let status = StatusLine::new(model, controller.cancelling);
        let t = palette(cx);
        let mut row = div()
            .id("status-bar")
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(16.))
            .px(px(16.))
            .bg(t.surfaces.canvas)
            .border_t_1()
            .border_color(tones(cx).hairline)
            .text_size(px(11.))
            .line_height(px(14.))
            .font_features(style::tnum())
            .overflow_hidden();
        let mut activity = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .child(style::status_glyph(status.glyph, accent(model, cx), cx));
        if !status.persistence_failed {
            activity = activity.child(div().font_weight(FontWeight::MEDIUM).child(status.mode));
        }
        row = row.child(
            activity.child(
                div()
                    .truncate()
                    .text_color(if status.persistence_failed {
                        t.feedback.error
                    } else {
                        t.text.secondary
                    })
                    .child(status.activity),
            ),
        );
        if !self.compact {
            let mut profile = div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(t.text.secondary)
                .child(status.profile);
            if let Some(reasoning) = status.reasoning {
                profile = profile.child(div().text_color(t.text.muted).child(reasoning));
            }
            row = row.child(profile);
        }
        row = row.child(div().flex_1());
        if let Some(context) = status.context {
            row = row.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_color(t.text.muted)
                    .when(!self.compact, |d| {
                        d.child(
                            div()
                                .w(px(64.))
                                .h(px(4.))
                                .rounded_full()
                                .bg(tones(cx).hairline)
                                .child(
                                    div()
                                        .w(px(64. * context.fraction))
                                        .h_full()
                                        .rounded_full()
                                        .bg(if context.warning {
                                            t.feedback.warning
                                        } else {
                                            t.text.muted
                                        }),
                                ),
                        )
                    })
                    .child(context.label),
            );
        }
        if !self.compact
            && let Some(output) = status.output
        {
            row = row.child(div().text_color(t.text.muted).child(output));
        }
        if let Some(network) = status.network {
            row = row.child(
                div()
                    .truncate()
                    .text_color(t.feedback.warning)
                    .child(network),
            );
        }
        row.into_any_element()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn numbers_and_gauge_threshold() {
        for (n, expected) in [
            (0, "0"),
            (999, "999"),
            (1200, "1.2k"),
            (41000, "41k"),
            (200000, "200k"),
            (1250000, "1.2m"),
        ] {
            assert_eq!(compact_number(n), expected);
        }
        assert!(!ContextGauge::new(80, 100).warning);
        assert!(ContextGauge::new(81, 100).warning);
        assert_eq!(ContextGauge::new(150, 100).fraction, 1.);
        assert_eq!(ContextGauge::new(10, 0).fraction, 0.);
        assert_eq!(ContextGauge::new(41000, 200000).label, "41k of 200k");
    }
    #[::core::prelude::v1::test]
    fn authoritative_activity_priority() {
        assert_eq!(
            activity(true, true, true, true, ConversationTail::None),
            "Saving failed. New work is blocked."
        );
        assert_eq!(
            activity(false, true, true, true, ConversationTail::None),
            "Fresh Plan handoff pending"
        );
        assert_eq!(
            activity(false, false, true, true, ConversationTail::None),
            "Plan approval"
        );
        assert_eq!(
            activity(false, false, false, true, ConversationTail::None),
            "Stopping…"
        );
        assert_eq!(
            activity(false, false, false, false, ConversationTail::None),
            "Ready"
        );
    }
}
