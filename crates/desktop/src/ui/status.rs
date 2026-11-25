//! Structured status segments: never a punctuation-joined telemetry sentence.
use super::{
    style,
    theme::{palette, tones},
    window::DesktopWindow,
};
use crate::actions::Shortcut;
use gpui_kit::component::{
    Disableable, Selectable, Sizable,
    button::{Button, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use std::time::Instant;
use zevria_foundation::ModelRole;
use zevria_presentation::{
    ConversationTail, SessionModel, jobs::JobsInspector, status_icon::StatusIcon, subtasks,
};
#[cfg(test)]
use zevria_session_api::TurnEvent;

pub(super) const HEIGHT: f32 = 28.;

#[derive(Debug, PartialEq)]
pub struct ContextGauge {
    pub label: String,
    pub fraction: Option<f32>,
    pub warning: bool,
}
impl ContextGauge {
    fn new(input: u64, window: u64) -> Self {
        let fraction = zevria_presentation::status::StatusSnapshot::context_fraction(input, window);
        Self {
            label: format!("{} of {}", compact_number(input), compact_number(window)),
            fraction: Some(fraction),
            warning: fraction > 0.8,
        }
    }
    fn from_snapshot(context: &zevria_model::ContextTokenSnapshot) -> Self {
        match context.projected_input_tokens {
            Some(tokens) => {
                let mut gauge = Self::new(tokens, context.context_window_tokens);
                gauge.label = format!(
                    "next {}{}",
                    if context.source.is_estimated() {
                        "~"
                    } else {
                        ""
                    },
                    gauge.label
                );
                gauge
            }
            None => Self {
                label: format!(
                    "estimated known portion: {}; {}",
                    compact_number(context.known_input_tokens),
                    context.unresolved_media.description()
                ),
                fraction: None,
                warning: false,
            },
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
    pub cache: Option<String>,
    pub total: Option<String>,
    pub network: Option<String>,
}
impl StatusLine {
    pub fn new(model: &SessionModel, cancelling: bool) -> Self {
        let persistence_failed = model.session.persistence_error().is_some();
        let activity = if model.is_busy() && !persistence_failed {
            working_activity(model, cancelling)
        } else {
            activity(
                persistence_failed,
                model.workflow.fresh_retry_version().is_some(),
                model.workflow.dialog().is_some(),
                cancelling,
                model.tail(Instant::now()),
            )
            .into()
        };
        let role = model.display_role();
        let snapshot = zevria_presentation::status::StatusSnapshot::observe(
            &model.session,
            &model.model_profiles,
            role,
        );
        let response_usage = snapshot.response;
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
            activity,
            persistence_failed,
            profile: snapshot
                .profile
                .map_or_else(|| "No model".into(), ToString::to_string),
            reasoning: snapshot.reasoning.map(|r| format!("{r:?}").to_lowercase()),
            context: snapshot.context.map(ContextGauge::from_snapshot),
            cache: response_usage.map(|usage| {
                format!(
                    "cached {} ({:.0}%)",
                    compact_number(usage.cached_tokens),
                    usage.cached_percent(),
                )
            }),
            total: response_usage
                .map(|usage| format!("total {}", compact_number(usage.total_tokens))),
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
/// One truthful vocabulary shared by the thread tip and status bar.
pub fn working_activity(model: &SessionModel, cancelling: bool) -> String {
    use zevria_presentation::{
        blocks::{PresentationBlockKind, PresentedToolStatus},
        conversation::HistoryEntry,
    };
    if model.session.operation_kind()
        == Some(zevria_presentation::session::OperationKind::ModelManagement)
    {
        return "Choosing a model".into();
    }
    if cancelling {
        return "Stopping".into();
    }
    let streamed = match model.tail(Instant::now()) {
        ConversationTail::Compacting { .. } => return "Compacting context".into(),
        ConversationTail::Retrying {
            notice,
            retrying_for,
            ..
        } => {
            return format!(
                "Reconnecting (attempt {}, next try in {:.1} s)",
                notice.attempt,
                retrying_for.as_secs_f32()
            );
        }
        ConversationTail::Streaming {
            message: rig_core::message::Message::Assistant(message),
            ..
        } => message.content.iter().rev().find_map(|part| match part {
            rig_core::message::AssistantContent::Reasoning(reasoning)
                if zevria_content::reasoning::readable_parts(reasoning)
                    .next()
                    .is_some() =>
            {
                Some("Thinking")
            }
            rig_core::message::AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                Some("Writing")
            }
            _ => None,
        }),
        _ => None,
    };
    let indices = model.session.call_header().map_or(&[][..], |header| {
        model.conversation.represented_header_indices(header)
    });
    let mut newest_web = true;
    for &index in indices.iter().rev() {
        let Some(HistoryEntry::Conversation(entry)) = model.conversation.entry(index) else {
            continue;
        };
        for block in entry.blocks.iter().rev().filter(|b| b.visible(false)) {
            match &block.kind {
                PresentationBlockKind::WebActivity(activity) if newest_web => {
                    newest_web = false;
                    if let Some(phrase) = crate::timeline::web::live_phrase(&activity.actions) {
                        return phrase.into();
                    }
                }
                PresentationBlockKind::Tool(tool)
                    if match tool {
                        zevria_presentation::blocks::PresentedTool::Native { state, .. } => {
                            zevria_presentation::blocks::native_tool_status(state)
                                == PresentedToolStatus::Running
                        }
                        zevria_presentation::blocks::PresentedTool::Acp(tool) => {
                            tool.presented_status() == PresentedToolStatus::Running
                        }
                    } =>
                {
                    let name = match tool {
                        zevria_presentation::blocks::PresentedTool::Native { call, state } => {
                            if call.function.name == zevria_foundation::WEB_SEARCH_TOOL_NAME {
                                let (phrase, _) =
                                    zevria_content::response::web_search_progress_step(
                                        &state.web_search_activity,
                                    );
                                return phrase.into();
                            }
                            super::blocks::tool_label(call.function.name.as_str())
                        }
                        zevria_presentation::blocks::PresentedTool::Acp(tool) => {
                            std::borrow::Cow::Borrowed(tool.title.as_str())
                        }
                    };
                    return streamed
                        .map_or_else(|| format!("Running {}", name.to_lowercase()), str::to_owned);
                }
                PresentationBlockKind::Reasoning { parts }
                    if parts.iter().any(|part| !part.trim().is_empty()) =>
                {
                    return streamed.unwrap_or("Thinking").into();
                }
                PresentationBlockKind::Text { text, .. } if !text.trim().is_empty() => {
                    return streamed.unwrap_or("Writing").into();
                }
                _ => {}
            }
        }
    }
    streamed.unwrap_or("Waiting for the model").into()
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
fn panel_chip(
    id: &'static str,
    name: &str,
    label: String,
    running_ink: Option<Hsla>,
    selected: bool,
    shortcut: Shortcut,
    cx: &App,
) -> Button {
    let t = palette(cx);
    let action = if selected { "Hide" } else { "Show" };
    Button::new(id)
        .ghost()
        .small()
        .h(px(22.))
        .flex_shrink_0()
        .selected(selected)
        .tooltip(format!("{action} {name} ({})", shortcut.label()))
        .accessibility_label(format!("{action} {name}: {label}"))
        .when_some(running_ink, |button, ink| {
            button.child(style::status_glyph(StatusIcon::Running, ink, cx))
        })
        .child(
            div()
                .text_color(if running_ink.is_some() {
                    t.text.secondary
                } else {
                    t.text.muted
                })
                .child(label),
        )
}

impl DesktopWindow {
    pub(super) fn status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let controller = self.controller.read(cx);
        let model = controller.model.visible();
        let mut status = StatusLine::new(model, controller.cancelling);
        if controller.model.controls.models.is_open() && !status.persistence_failed {
            use zevria_presentation::models::ModelPhase;
            status.activity = match controller.model.controls.models.phase {
                ModelPhase::Cancelling { .. } => "Cancelling model selection…",
                ModelPhase::PendingChange { .. } => "Applying model selection…",
                _ => "Choosing a model…",
            }
            .into();
        }
        let t = palette(cx);
        let mut row = div()
            .id("status-bar")
            .test_support()
            .h(px(HEIGHT))
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
            .id("status-activity")
            .test_support()
            // Keep activity readable without letting long tool titles crowd out warnings.
            .flex_shrink_0()
            .max_w(relative(1. / 3.))
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
        let label = format!(
            "{}{}",
            status.profile,
            status
                .reasoning
                .as_ref()
                .map_or_else(String::new, |r| format!(" · {r}"))
        );
        let plan_open = controller.model.root.workflow.dialog().is_some();
        if controller.model.controls.selected_child.is_none() {
            let unavailable = controller.model_selection_available().err();
            let tooltip = format!(
                "{label}\nChange model and thinking effort for this session only ({}).{}",
                Shortcut::OPEN_MODELS.label(),
                unavailable.map_or_else(String::new, |reason| format!("\n{reason}"))
            );
            row = row.child(
                Button::new("status-model")
                    .ghost()
                    .small()
                    .h(px(22.))
                    .min_w(px(64.))
                    .max_w(px(if self.compact { 160. } else { 300. }))
                    .accessibility_label(format!("Change model and thinking effort: {label}"))
                    .tooltip(tooltip)
                    .disabled(unavailable.is_some())
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(t.text.secondary)
                            .child(label),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_models(cx))),
            );
            let jobs = controller.model.root.jobs_inspector().jobs();
            if !jobs.is_empty() {
                let running = jobs
                    .iter()
                    .filter(|job| JobsInspector::can_terminate(job))
                    .count();
                let label = if running == 0 {
                    "Jobs".to_owned()
                } else if running == 1 {
                    "1 job".to_owned()
                } else {
                    format!("{running} jobs")
                };
                row = row.child(
                    panel_chip(
                        "status-jobs",
                        "jobs",
                        label,
                        (running != 0).then(|| accent(model, cx)),
                        controller.model.jobs_panel_visible(),
                        Shortcut::TOGGLE_JOBS,
                        cx,
                    )
                    .disabled(plan_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |controller, cx| controller.toggle_jobs(cx));
                    })),
                );
            }
        } else {
            row = row.child(
                div()
                    .id("status-child-model")
                    .min_w_0()
                    .max_w(px(200.))
                    .truncate()
                    .text_color(t.text.secondary)
                    .child(label),
            );
        }
        if !controller.model.children.is_empty() {
            let running =
                subtasks::active_count(controller.model.children.iter().map(|child| child.status));
            row = row.child(
                panel_chip(
                    "status-subtasks",
                    "subtasks",
                    subtasks::chip_label(running),
                    (running != 0).then(|| accent(&controller.model.root, cx)),
                    controller.model.subtasks_panel_visible(),
                    Shortcut::TOGGLE_SUBTASKS,
                    cx,
                )
                .disabled(plan_open)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller
                        .update(cx, |controller, cx| controller.toggle_subtasks(cx));
                })),
            );
        }
        row = row.child(div().flex_1());
        if let Some(context) = status.context {
            row = row.child(
                div()
                    .id("status-context")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .min_w_0()
                    .text_color(t.text.muted)
                    .when(!self.compact && context.fraction.is_some(), |d| {
                        d.child(
                            div()
                                .id("status-context-gauge")
                                .test_support()
                                // Let the gauge shrink with the context group so the label stays visible.
                                .w(px(64.))
                                .h(px(4.))
                                .rounded_full()
                                .bg(tones(cx).hairline)
                                .child(
                                    div()
                                        .id("status-context-fill")
                                        .test_support()
                                        .w(relative(context.fraction.expect("complete gauge")))
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
                    .child(
                        div()
                            .id("status-context-label")
                            .test_support()
                            .truncate()
                            .child(context.label),
                    ),
            );
        }
        if !self.compact {
            if let Some(cache) = status.cache {
                row = row.child(
                    div()
                        .id("status-cache")
                        .test_support()
                        .flex_shrink_0()
                        .text_color(t.text.muted)
                        .child(cache),
                );
            }
            if let Some(total) = status.total {
                row = row.child(
                    div()
                        .id("status-total")
                        .test_support()
                        .flex_shrink_0()
                        .text_color(t.text.muted)
                        .child(total),
                );
            }
        }
        if let Some(network) = status.network {
            row = row.child(
                div()
                    .id("status-network")
                    .test_support()
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
    use rig_core::message::Message;
    use zevria_foundation::{ModelProfileRef, SessionMode, TurnId};
    use zevria_model::{ContextTokenSnapshot, ContextTokenSource, TokenUsage};
    use zevria_session_api::SessionEvent as E;

    const TURN: TurnId = TurnId::new(1);

    fn running_model() -> SessionModel {
        let mut model = SessionModel::default();
        model.reduce(E::Turn(TurnEvent::Started {
            turn_id: TURN,
            mode: SessionMode::Build,
            message: Message::user("Check cache usage"),
        }));
        model
    }

    fn response_event(input_tokens: u64, cached_tokens: u64, output_tokens: u64) -> E {
        E::Turn(TurnEvent::UsageUpdated {
            turn_id: TURN,
            usage: TokenUsage {
                input_tokens,
                cached_tokens,
                output_tokens,
                total_tokens: input_tokens + output_tokens,
            },
            profile: ModelProfileRef::new("test", "model"),
            model_role: ModelRole::Build,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        })
    }

    fn context_event(profile: ModelProfileRef, model_role: ModelRole, tokens: u64) -> E {
        E::Turn(TurnEvent::ContextUsageUpdated {
            turn_id: TURN,
            snapshot: Box::new(ContextTokenSnapshot {
                profile,
                model_role,
                projected_input_tokens: Some(tokens),
                known_input_tokens: tokens,
                unresolved_media: Default::default(),
                source: ContextTokenSource::Exact,
                automatic_trigger: 80_000,
                input_token_limit: 100_000,
                context_window_tokens: 128_000,
            }),
        })
    }

    #[::core::prelude::v1::test]
    fn working_activity_tracks_search_until_sources_are_summarized() {
        use rig_core::message::{
            AssistantContent, AssistantMessage, CallId, ToolCall, ToolFunction, ToolName,
        };
        use zevria_content::{HostedActivity, HostedActivityKind, HostedActivityStatus};
        use zevria_foundation::WEB_SEARCH_TOOL_NAME;
        let mut model = running_model();
        model.reduce(E::Turn(TurnEvent::ModelCallStarted {
            turn_id: TURN,
            call: 1,
        }));
        model.reduce(E::Turn(TurnEvent::Intermediate {
            turn_id: TURN,
            message: Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                ToolCall::new(
                    CallId::from_wire("search"),
                    ToolFunction::new(
                        ToolName::new(WEB_SEARCH_TOOL_NAME).unwrap(),
                        serde_json::json!({"query":"Find the version"}),
                    ),
                ),
            )])),
            attempt_id: None,
        }));
        assert_eq!(working_activity(&model, false), "Searching the web");
        for (status, label) in [
            (HostedActivityStatus::InProgress, "Opening a page"),
            (HostedActivityStatus::Completed, "Summarizing sources"),
        ] {
            model.reduce(E::Turn(TurnEvent::WebSearchProgress {
                turn_id: TURN,
                call_id: "search".into(),
                activity: vec![HostedActivity {
                    kind: HostedActivityKind::WebSearch,
                    item_id: None,
                    output_index: 0,
                    status,
                    action: Some(
                        serde_json::json!({"type":"open_page","url":"https://example.com"}),
                    ),
                }],
            }));
            assert_eq!(working_activity(&model, false), label);
        }
    }

    #[::core::prelude::v1::test]
    fn cache_count_and_rate_match_last_response_rounding() {
        let mut model = running_model();
        for (input, cached, expected_cache, expected_total) in [
            (12_300, 10_200, "cached 10.2k (83%)", "total 14.4k"),
            (1_000, 124, "cached 124 (12%)", "total 3.1k"),
            (1_000, 126, "cached 126 (13%)", "total 3.1k"),
            (12_300, 0, "cached 0 (0%)", "total 14.4k"),
            (12_300, 12_300, "cached 12.3k (100%)", "total 14.4k"),
            (0, 0, "cached 0 (0%)", "total 2.1k"),
            (0, 100, "cached 100 (0%)", "total 2.1k"),
            // Preserve the shared helper's semantics, including no clamping.
            (1_000, 1_200, "cached 1.2k (120%)", "total 3.1k"),
        ] {
            model.reduce(response_event(input, cached, 2_100));
            let status = StatusLine::new(&model, false);
            assert_eq!(status.cache.as_deref(), Some(expected_cache));
            assert_eq!(status.total.as_deref(), Some(expected_total));
        }
    }

    #[::core::prelude::v1::test]
    fn absent_response_usage_has_no_cache_or_total_segment() {
        let mut model = running_model();
        assert_eq!(StatusLine::new(&model, false).cache, None);
        model.reduce(context_event(
            ModelProfileRef::new("test", "model"),
            ModelRole::Build,
            12_300,
        ));
        let status = StatusLine::new(&model, false);
        assert!(status.context.is_some());
        assert_eq!(status.cache, None);
        assert_eq!(status.total, None);
    }

    #[::core::prelude::v1::test]
    fn tool_loop_usage_replaces_cache_rate_but_next_context_does_not() {
        let mut model = running_model();
        model.reduce(E::Turn(TurnEvent::ModelCallStarted {
            turn_id: TURN,
            call: 1,
        }));
        model.reduce(response_event(12_300, 10_200, 2_100));
        model.reduce(E::Turn(TurnEvent::Intermediate {
            turn_id: TURN,
            message: Message::assistant("Checking the next tool result"),
            attempt_id: None,
        }));
        model.reduce(E::Turn(TurnEvent::ModelCallStarted {
            turn_id: TURN,
            call: 2,
        }));
        model.reduce(context_event(
            ModelProfileRef::new("test", "model"),
            ModelRole::Build,
            41_000,
        ));
        let status = StatusLine::new(&model, false);
        assert_eq!(status.cache.as_deref(), Some("cached 10.2k (83%)"));
        assert_eq!(status.total.as_deref(), Some("total 14.4k"));
        assert_eq!(status.context.unwrap().label, "next 41k of 128k");

        model.reduce(response_event(20_000, 10_000, 3_000));
        let status = StatusLine::new(&model, false);
        assert_eq!(status.cache.as_deref(), Some("cached 10k (50%)"));
        assert_eq!(status.total.as_deref(), Some("total 23k"));
        model.reduce(E::Turn(TurnEvent::Completed {
            turn_id: TURN,
            message: Message::assistant("Done"),
            attempt_id: None,
        }));
        // Late usage cannot replace the most recently accepted response.
        model.reduce(response_event(1_000, 0, 1));
        assert_eq!(
            StatusLine::new(&model, false).cache.as_deref(),
            Some("cached 10k (50%)")
        );
    }

    #[::core::prelude::v1::test]
    fn visible_role_and_authoritative_profile_do_not_leak_cached_usage() {
        let mut model = running_model();
        model.reduce(response_event(12_300, 10_200, 2_100));
        model.reduce(context_event(
            ModelProfileRef::new("test", "plan"),
            ModelRole::Plan,
            5_000,
        ));
        assert_eq!(
            StatusLine::new(&model, false).cache.as_deref(),
            Some("cached 10.2k (83%)")
        );

        // A new authoritative profile clears the old response even if only
        // next-request context is available for that profile so far.
        model.reduce(context_event(
            ModelProfileRef::new("test", "replacement"),
            ModelRole::Build,
            20_000,
        ));
        let status = StatusLine::new(&model, false);
        assert_eq!(status.profile, "test/replacement");
        assert_eq!(status.cache, None);
        assert_eq!(status.total, None);
        model.reduce(response_event(12_300, 10_200, 2_100));
        model.reduce(E::Turn(TurnEvent::Completed {
            turn_id: TURN,
            message: Message::assistant("Done"),
            attempt_id: None,
        }));
        model.reduce(E::Turn(TurnEvent::ModeChanged {
            mode: SessionMode::Plan,
        }));
        let status = StatusLine::new(&model, false);
        assert_eq!(status.mode, "Plan");
        assert_eq!(status.profile, "test/plan");
        assert!(status.context.is_some());
        assert_eq!(status.cache, None);
        assert_eq!(status.total, None);
    }

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
        assert_eq!(ContextGauge::new(150, 100).fraction, Some(1.));
        assert_eq!(ContextGauge::new(10, 0).fraction, Some(0.));
        assert_eq!(ContextGauge::new(41000, 200000).label, "41k of 200k");
    }
    #[::core::prelude::v1::test]
    fn context_labels_only_mark_estimates() {
        for (source, expected) in [
            (ContextTokenSource::Exact, "next 17.8k of 128k"),
            (ContextTokenSource::UsagePlusDelta, "next ~17.8k of 128k"),
            (ContextTokenSource::LocalEstimate, "next ~17.8k of 128k"),
            (ContextTokenSource::ProviderEstimate, "next ~17.8k of 128k"),
        ] {
            let context = ContextTokenSnapshot {
                profile: ModelProfileRef::new("test", "model"),
                model_role: ModelRole::Build,
                projected_input_tokens: Some(17_800),
                known_input_tokens: 17_800,
                unresolved_media: Default::default(),
                source,
                automatic_trigger: 80_000,
                input_token_limit: 100_000,
                context_window_tokens: 128_000,
            };
            let gauge = ContextGauge::from_snapshot(&context);
            assert_eq!(gauge.label, expected, "{source:?}");
            assert_eq!(gauge.fraction, ContextGauge::new(17_800, 128_000).fraction);
            assert!(!gauge.warning);
        }
    }

    #[::core::prelude::v1::test]
    fn incomplete_context_has_no_numeric_gauge_or_threshold_warning() {
        let context = zevria_model::ContextTokenSnapshot {
            profile: zevria_foundation::ModelProfileRef::new("test", "model"),
            model_role: ModelRole::Build,
            projected_input_tokens: None,
            known_input_tokens: 90_000,
            unresolved_media: zevria_model::media_accounting::UnresolvedMedia {
                images: 3,
                ..Default::default()
            },
            source: zevria_model::ContextTokenSource::IncompleteEstimate,
            automatic_trigger: 80_000,
            input_token_limit: 100_000,
            context_window_tokens: 128_000,
        };
        let gauge = ContextGauge::from_snapshot(&context);
        assert_eq!(gauge.fraction, None);
        assert!(!gauge.warning);
        assert_eq!(
            gauge.label,
            "estimated known portion: 90k; 3 unmeasured images"
        );
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
