//! ACP notification normalization and workflow constraint checks.

use super::*;

#[cfg(test)]
#[path = "tool_metadata_tests.rs"]
mod tool_metadata_tests;

pub(super) fn normalize_notification(notification: SessionNotification) -> Vec<AgentRunEvent> {
    let display = notification
        .meta
        .as_ref()
        .and_then(|meta| meta.get(zevria_content::web_search::RESPONSE_DISPLAY_META_KEY))
        .and_then(|value| {
            serde_json::from_value::<zevria_content::web_search::ResponseDisplay>(value.clone())
                .ok()
        })
        .filter(|display| display.validate().is_ok());
    let mut events = normalize_update(notification.update);
    if let Some(display) = display {
        // An empty metadata carrier has no ordinary content. In particular it
        // must not fence the host's text preview on every status-only update.
        events.retain(|event| {
            !matches!(
                event,
                AgentRunEvent::SessionInfo {
                    title: None,
                    updated_at: None,
                    metadata: None
                }
            )
        });
        events.push(AgentRunEvent::ResponseDisplay {
            display: Box::new(display),
        });
    }
    events
}

pub(super) fn normalize_update(update: AcpSessionUpdate) -> Vec<AgentRunEvent> {
    match update {
        AcpSessionUpdate::UserMessageChunk(chunk) => {
            normalize_chunk("user message", chunk, |text, message_id| {
                AgentRunEvent::UserMessage { text, message_id }
            })
        }
        AcpSessionUpdate::AgentMessageChunk(chunk) => {
            normalize_chunk("agent message", chunk, |text, message_id| {
                AgentRunEvent::AgentMessage { text, message_id }
            })
        }
        AcpSessionUpdate::AgentThoughtChunk(chunk) => {
            normalize_chunk("agent thought", chunk, |text, message_id| {
                AgentRunEvent::Thought { text, message_id }
            })
        }
        AcpSessionUpdate::ToolCall(call) => {
            let metadata =
                normalize_tool_metadata(call.meta.as_ref(), &call.tool_call_id.to_string());
            let mut events = vec![normalize_tool_call(call)];
            events.extend(metadata);
            events
        }
        AcpSessionUpdate::ToolCallUpdate(update) => {
            let metadata =
                normalize_tool_metadata(update.meta.as_ref(), &update.tool_call_id.to_string());
            let mut events = vec![normalize_tool_update(update)];
            events.extend(metadata);
            events
        }
        AcpSessionUpdate::Plan(plan) => vec![normalize_plan(plan)],
        AcpSessionUpdate::PlanUpdate(update) => vec![normalize_plan_update(update.plan)],
        AcpSessionUpdate::PlanRemoved(removed) => vec![AgentRunEvent::PlanRemoved {
            plan_id: removed.plan_id.to_string(),
        }],
        AcpSessionUpdate::CurrentModeUpdate(CurrentModeUpdate {
            current_mode_id, ..
        }) => {
            vec![AgentRunEvent::ModeChanged {
                mode: current_mode_id.to_string(),
            }]
        }
        AcpSessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate { config_options, .. }) => {
            vec![AgentRunEvent::ConfigOptionsChanged {
                options: serde_json::to_value(config_options).unwrap_or(serde_json::Value::Null),
            }]
        }
        AcpSessionUpdate::SessionInfoUpdate(info) => {
            let value = serde_json::to_value(&info).unwrap_or(serde_json::Value::Null);
            vec![AgentRunEvent::SessionInfo {
                title: value
                    .get("title")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                updated_at: value
                    .get("updatedAt")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                metadata: info.meta.and_then(|meta| serde_json::to_value(meta).ok()),
            }]
        }
        AcpSessionUpdate::UsageUpdate(usage) => vec![AgentRunEvent::Usage {
            usage: AgentUsage {
                used: usage.used,
                size: usage.size,
                cost: usage.cost.and_then(|cost| serde_json::to_value(cost).ok()),
            },
        }],
        AcpSessionUpdate::AvailableCommandsUpdate(commands) => vec![AgentRunEvent::Unsupported {
            context: "available commands update".to_string(),
            placeholder: serde_json::to_string(&commands)
                .unwrap_or_else(|_| "[unserializable commands]".to_string()),
        }],
        _ => vec![AgentRunEvent::Unsupported {
            context: "ACP session update".to_string(),
            placeholder: "[unsupported content]".into(),
        }],
    }
}

pub(super) fn normalize_chunk(
    context: &str,
    chunk: ContentChunk,
    text_event: impl FnOnce(String, Option<String>) -> AgentRunEvent,
) -> Vec<AgentRunEvent> {
    let message_id = chunk.message_id.map(|id| id.to_string());
    match chunk.content {
        ContentBlock::Text(text) => vec![text_event(text.text, message_id)],
        ContentBlock::Image(image) if context == "user message" => {
            match zevria_content::PromptImage::from_base64(&image.mime_type, &image.data) {
                Ok(image) => vec![AgentRunEvent::UserImage { image, message_id }],
                Err(error) => vec![AgentRunEvent::Unsupported {
                    context: context.into(),
                    placeholder: error.to_string(),
                }],
            }
        }
        content => vec![AgentRunEvent::Unsupported {
            context: context.to_string(),
            placeholder: match content {
                ContentBlock::Image(_) => "[unsupported image output]",
                _ => "[unsupported nontext content]",
            }
            .into(),
        }],
    }
}

fn normalize_tool_metadata(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
    id: &str,
) -> Option<AgentRunEvent> {
    let value = meta?.get(zevria_foundation::TOOL_RESULT_META_KEY)?;
    let metadata: zevria_foundation::ToolResultMetadata =
        serde_json::from_value(value.clone()).ok()?;
    (metadata.id == id).then(|| AgentRunEvent::ToolResultMetadata {
        metadata: Box::new(metadata),
    })
}

pub(super) fn normalize_tool_call(call: ToolCall) -> AgentRunEvent {
    AgentRunEvent::ToolCall {
        id: call.tool_call_id.to_string(),
        title: call.title,
        kind: wire_name(&call.kind),
        status: wire_name(&call.status),
        content: tool_content(call.content),
        locations: call
            .locations
            .into_iter()
            .map(|location| AgentRunLocation {
                path: location.path,
                line: location.line,
            })
            .collect(),
        raw_input: call
            .raw_input
            .map(zevria_content::image_diagnostics::json_copy),
        raw_output: call
            .raw_output
            .map(zevria_content::image_diagnostics::json_copy),
    }
}

pub(super) fn normalize_tool_update(update: ToolCallUpdate) -> AgentRunEvent {
    AgentRunEvent::ToolCallUpdate {
        id: update.tool_call_id.to_string(),
        title: update.fields.title,
        kind: update.fields.kind.as_ref().map(wire_name),
        status: update.fields.status.as_ref().map(wire_name),
        content: update.fields.content.map(tool_content),
        locations: update.fields.locations.map(|locations| {
            locations
                .into_iter()
                .map(|location| AgentRunLocation {
                    path: location.path,
                    line: location.line,
                })
                .collect()
        }),
        raw_input: update
            .fields
            .raw_input
            .map(zevria_content::image_diagnostics::json_copy),
        raw_output: update
            .fields
            .raw_output
            .map(zevria_content::image_diagnostics::json_copy),
    }
}

pub(super) fn tool_content(content: Vec<ToolCallContent>) -> Vec<String> {
    content
        .into_iter()
        .map(|content| match content {
            ToolCallContent::Content(content) => match content.content {
                ContentBlock::Text(text) => text.text,
                ContentBlock::Image(_) => "[unsupported tool image output]".into(),
                _ => "[unsupported nontext tool content]".into(),
            },
            ToolCallContent::Diff(diff) => serde_json::to_string(&diff)
                .unwrap_or_else(|_| format!("diff at {}", diff.path.display())),
            ToolCallContent::Terminal(terminal) => {
                serde_json::to_string(&terminal).unwrap_or_else(|_| "[terminal output]".to_string())
            }
            _ => "[unsupported tool content]".into(),
        })
        .collect()
}

pub(super) fn normalize_plan(plan: Plan) -> AgentRunEvent {
    AgentRunEvent::Plan {
        plan: AgentStructuredPlan {
            plan_id: None,
            markdown: None,
            entries: normalize_plan_entries(plan.entries),
        },
    }
}

pub(super) fn normalize_plan_update(plan: PlanUpdateContent) -> AgentRunEvent {
    let plan = match plan {
        PlanUpdateContent::Items(items) => AgentStructuredPlan {
            plan_id: Some(items.plan_id.to_string()),
            markdown: None,
            entries: normalize_plan_entries(items.entries),
        },
        PlanUpdateContent::Markdown(markdown) => AgentStructuredPlan {
            plan_id: Some(markdown.plan_id.to_string()),
            markdown: Some(markdown.content),
            entries: Vec::new(),
        },
        PlanUpdateContent::File(file) => AgentStructuredPlan {
            plan_id: Some(file.plan_id.to_string()),
            markdown: None,
            entries: Vec::new(),
        },
        _ => AgentStructuredPlan {
            plan_id: None,
            markdown: None,
            entries: Vec::new(),
        },
    };
    AgentRunEvent::Plan { plan }
}

pub(super) fn normalize_plan_entries(
    entries: Vec<agent_client_protocol::schema::v1::PlanEntry>,
) -> Vec<AgentPlanEntry> {
    entries
        .into_iter()
        .map(|entry| AgentPlanEntry {
            content: entry.content,
            priority: wire_name(&entry.priority),
            status: wire_name(&entry.status),
        })
        .collect()
}

pub(super) fn session_configuration_violation(
    update: &AcpSessionUpdate,
    expectation: &Option<SessionConfigurationExpectation>,
) -> Option<String> {
    let expectation = expectation.as_ref()?;
    if let Some(error) = safe_mode_configuration_violation(update, &expectation.safe_mode) {
        return Some(error);
    }
    let AcpSessionUpdate::ConfigOptionUpdate(update) = update else {
        return None;
    };
    for (id, desired) in &expectation.workflow_options {
        let Some(option) = update
            .config_options
            .iter()
            .find(|option| option.id.to_string() == id.as_str())
        else {
            return Some(format!(
                "{} workflow-configuration violation: agent stopped reporting required option {id:?} = {desired:?}",
                expectation.workflow.slash_command()
            ));
        };
        if config_option_current(option) != Some(desired.as_str()) {
            return Some(format!(
                "{} workflow-configuration violation: agent changed required option {id:?} from {desired:?}",
                expectation.workflow.slash_command()
            ));
        }
    }
    None
}

pub(super) fn safe_mode_configuration_violation(
    update: &AcpSessionUpdate,
    expectation: &SafeModeExpectation,
) -> Option<String> {
    match update {
        AcpSessionUpdate::CurrentModeUpdate(update)
            if update.current_mode_id.to_string() != expectation.desired =>
        {
            Some(format!(
                "agent switched away from required safe mode {:?} to {:?}",
                expectation.desired, update.current_mode_id
            ))
        }
        AcpSessionUpdate::ConfigOptionUpdate(update) => {
            let option = if let Some(config_id) = expectation.config_id.as_deref() {
                let Some(option) = update
                    .config_options
                    .iter()
                    .find(|option| option.id.to_string() == config_id)
                else {
                    return Some(format!(
                        "agent stopped reporting required safe-mode configuration option {config_id:?}"
                    ));
                };
                Some(option)
            } else {
                let mode_options = update
                    .config_options
                    .iter()
                    .filter(|option| option.category == Some(SessionConfigOptionCategory::Mode))
                    .collect::<Vec<_>>();
                if mode_options.is_empty() {
                    None
                } else {
                    let Some(option) = mode_options
                        .into_iter()
                        .find(|option| config_option_supports(option, &expectation.desired))
                    else {
                        return Some(
                            "agent replaced legacy mode enforcement with an incompatible ACP mode configuration"
                                .to_string(),
                        );
                    };
                    Some(option)
                }
            };
            option.and_then(|option| {
                (config_option_current(option) != Some(expectation.desired.as_str())).then(|| {
                    format!(
                        "agent changed required safe mode {:?} through ACP configuration",
                        expectation.desired
                    )
                })
            })
        }
        _ => None,
    }
}
