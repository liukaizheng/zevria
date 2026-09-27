use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionUpdate, TextContent,
};
use rig_core::message::{AssistantContent, Message, ReasoningContent};
use zevria_content::assistant_plain_text;
use zevria_foundation::TurnId;

/// Append-only projection of Zevria's complete watch snapshots.
///
/// ACP cannot retract chunks, so a divergent complete snapshot begins a new
/// authoritative segment instead of appending an invalid suffix to the prior
/// message ID.
#[derive(Debug, Default)]
pub(crate) struct StreamSegments {
    message: Segment,
    thought: Segment,
    pub(crate) attempt: Option<zevria_content::WebSearchAttemptRecord>,
    seen_attempts: std::collections::HashMap<String, zevria_content::WebSearchAttemptRecord>,
}

#[derive(Debug, Default)]
struct Segment {
    emitted: String,
    revision: u64,
    message_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Message,
    Thought,
}

impl StreamSegments {
    pub(crate) fn snapshot(&mut self, turn_id: TurnId, message: &Message) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();
        let text = assistant_plain_text(message);
        project_complete(
            &mut self.message,
            turn_id,
            StreamKind::Message,
            &text,
            false,
            &mut updates,
        );
        let reasoning = assistant_reasoning_text(message);
        project_complete(
            &mut self.thought,
            turn_id,
            StreamKind::Thought,
            &reasoning,
            false,
            &mut updates,
        );
        updates
    }

    pub(crate) fn terminal(&mut self, turn_id: TurnId, message: &Message) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();
        let text = assistant_plain_text(message);
        project_complete(
            &mut self.message,
            turn_id,
            StreamKind::Message,
            &text,
            true,
            &mut updates,
        );
        let reasoning = assistant_reasoning_text(message);
        project_complete(
            &mut self.thought,
            turn_id,
            StreamKind::Thought,
            &reasoning,
            true,
            &mut updates,
        );
        updates
    }

    pub(crate) fn display_snapshot(
        &mut self,
        turn_id: TurnId,
        snapshot: zevria_content::AssistantStreamSnapshot,
    ) -> Vec<SessionUpdate> {
        if let Some(attempt) = &snapshot.attempt {
            if self
                .seen_attempts
                .get(&attempt.id)
                .is_some_and(|old| old.revision >= attempt.revision)
            {
                // Raw accumulation can catch up after the indexed reducer has
                // published the same revision. Do not lose that ordinary
                // suffix (or its newly matching binding), but never revive a
                // stale/conflicting revision or a previous retry attempt.
                if self.attempt.as_ref() == Some(attempt) {
                    let mut updates = snapshot
                        .message
                        .as_ref()
                        .map_or_else(Vec::new, |message| self.snapshot(turn_id, message));
                    if !updates.is_empty()
                        && let Some(display) = self.display_update()
                    {
                        updates.push(display);
                    }
                    return updates;
                }
                return Vec::new();
            }
            self.seen_attempts
                .insert(attempt.id.clone(), attempt.clone());
            if self
                .attempt
                .as_ref()
                .is_some_and(|old| old.id != attempt.id)
            {
                self.reset();
            }
        }
        let message = snapshot
            .message
            .or_else(|| snapshot.attempt.as_ref().and_then(display_message));
        let mut updates = message
            .as_ref()
            .map_or_else(Vec::new, |message| self.snapshot(turn_id, message));
        if let Some(attempt) = snapshot.attempt {
            self.attempt = Some(attempt);
            if let Some(update) = self.display_update() {
                updates.push(update);
            }
        }
        updates
    }

    pub(crate) fn terminal_bound(
        &mut self,
        turn_id: TurnId,
        message: &Message,
        attempt_id: Option<&str>,
    ) -> Vec<SessionUpdate> {
        self.attempt = attempt_id
            .and_then(|id| self.seen_attempts.get(id))
            .cloned();
        // The canonical record remains raw native replay; ACP emits a display
        // copy just like the indexed TUI (including terminal sanitization).
        let display_message = self
            .attempt
            .as_ref()
            .filter(|attempt| attempt.outcome == zevria_content::WebSearchAttemptOutcome::Completed)
            .and_then(display_message);
        let mut updates = self.terminal(turn_id, display_message.as_ref().unwrap_or(message));
        if let Some(display) = self.display_update() {
            updates.push(display);
        }
        updates
    }

    pub(crate) fn display_update(&self) -> Option<SessionUpdate> {
        use zevria_content::web_search::DisplayProjectionBinding as Binding;
        use zevria_content::web_search::DisplayProjectionKind as Kind;
        use zevria_content::web_search::ResponseDisplay;
        let attempt = self.attempt.as_ref()?.clone();
        if !attempt.has_display() {
            return None;
        }
        let mut display = ResponseDisplay {
            version: 1,
            attempt,
            bindings: Vec::new(),
        };
        for (kind, segment) in [
            (Kind::Message, &self.message),
            (Kind::Thought, &self.thought),
        ] {
            if let Some(id) = &segment.message_id
                && !segment.emitted.is_empty()
            {
                let sources = display
                    .attempt
                    .presentation
                    .iter()
                    .filter(|part| {
                        matches!(
                            (&part.content, kind),
                            (
                                zevria_content::AssistantPresentationContent::Answer { .. },
                                Kind::Message
                            ) | (
                                zevria_content::AssistantPresentationContent::Reasoning { .. },
                                Kind::Thought
                            )
                        )
                    })
                    .map(|part| part.source.clone())
                    .collect();
                let binding = Binding::Text {
                    kind,
                    message_id: id.clone(),
                    start: 0,
                    end: segment.emitted.len(),
                    sources,
                };
                if display.binding_text(&binding).as_deref() == Some(segment.emitted.as_str()) {
                    display.bindings.push(binding);
                }
            }
        }
        display
            .bindings
            .extend(display.attempt.activity.iter().map(|action| Binding::Tool {
                tool_call_id: action.client_id(&display.attempt.id),
                output_index: action.output_index,
                native: false,
            }));
        display
            .bindings
            .extend(
                display
                    .attempt
                    .presentation
                    .iter()
                    .filter_map(|part| match &part.content {
                        zevria_content::AssistantPresentationContent::NativeTool { call_id } => {
                            Some(Binding::Tool {
                                tool_call_id: call_id.clone(),
                                output_index: part.source.output_index,
                                native: true,
                            })
                        }
                        _ => None,
                    }),
            );
        display_carrier(display)
    }

    /// End the current stream boundary. The next non-empty snapshot receives a
    /// fresh message ID even if its text happens to share a prefix.
    pub(crate) fn reset(&mut self) {
        self.message.start_replacement();
        self.thought.start_replacement();
        self.attempt = None;
    }
}

impl Segment {
    fn start_replacement(&mut self) {
        if !self.emitted.is_empty() {
            self.revision = self.revision.saturating_add(1);
        }
        self.emitted.clear();
        self.message_id = None;
    }
}

fn project_complete(
    segment: &mut Segment,
    turn_id: TurnId,
    kind: StreamKind,
    complete: &str,
    terminal: bool,
    updates: &mut Vec<SessionUpdate>,
) {
    if complete.is_empty() {
        return;
    }

    let diverged = !complete.starts_with(&segment.emitted);
    let suffix = if diverged {
        segment.revision = segment.revision.saturating_add(1);
        segment.emitted.clear();
        complete
    } else {
        &complete[segment.emitted.len()..]
    };

    if suffix.is_empty() {
        return;
    }

    let authoritative_final = terminal && diverged;
    let message_id = MessageId::new(match (kind, authoritative_final) {
        (StreamKind::Message, true) => {
            format!(
                "zevria-turn-{}-message-{}-final",
                turn_id.get(),
                segment.revision
            )
        }
        (StreamKind::Message, false) => {
            format!("zevria-turn-{}-message-{}", turn_id.get(), segment.revision)
        }
        (StreamKind::Thought, true) => {
            format!(
                "zevria-turn-{}-thought-{}-final",
                turn_id.get(),
                segment.revision
            )
        }
        (StreamKind::Thought, false) => {
            format!("zevria-turn-{}-thought-{}", turn_id.get(), segment.revision)
        }
    });
    segment.message_id = Some(message_id.to_string());
    let chunk =
        ContentChunk::new(ContentBlock::Text(TextContent::new(suffix))).message_id(message_id);
    updates.push(match kind {
        StreamKind::Message => SessionUpdate::AgentMessageChunk(chunk),
        StreamKind::Thought => SessionUpdate::AgentThoughtChunk(chunk),
    });
    segment.emitted.push_str(suffix);
}

pub(crate) fn display_message(attempt: &zevria_content::WebSearchAttemptRecord) -> Option<Message> {
    let content = attempt
        .presentation
        .iter()
        .filter_map(|part| match &part.content {
            zevria_content::AssistantPresentationContent::Answer { text } => {
                Some(AssistantContent::Text(rig_core::message::Text::new(text)))
            }
            zevria_content::AssistantPresentationContent::Reasoning { text } => {
                Some(AssistantContent::Reasoning(
                    rig_core::message::Reasoning::summaries(vec![text.clone()]),
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    (!content.is_empty()).then_some(Message::Assistant { id: None, content })
}

/// Carried internally on an otherwise empty update; `send_update` lifts this
/// extension to notification-level `_meta` before sending it over the SDK.
pub(crate) fn session_notification(
    id: agent_client_protocol::schema::v1::SessionId,
    mut update: SessionUpdate,
) -> agent_client_protocol::schema::v1::SessionNotification {
    let meta = match &mut update {
        SessionUpdate::SessionInfoUpdate(info)
            if info.meta.as_ref().is_some_and(|meta| {
                meta.contains_key(zevria_content::web_search::RESPONSE_DISPLAY_META_KEY)
            }) =>
        {
            info.meta.take()
        }
        _ => None,
    };
    agent_client_protocol::schema::v1::SessionNotification::new(id, update).meta(meta)
}

pub(crate) fn replay_display(
    attempt: &zevria_content::WebSearchAttemptRecord,
    updates: &[SessionUpdate],
) -> Option<SessionUpdate> {
    use zevria_content::web_search::DisplayProjectionBinding as Binding;
    use zevria_content::web_search::DisplayProjectionKind as Kind;
    use zevria_content::web_search::ResponseDisplay;
    let mut display = ResponseDisplay {
        version: 1,
        attempt: attempt.clone(),
        bindings: Vec::new(),
    };
    let mut used = std::collections::HashSet::new();
    for update in updates {
        let (kind, chunk) = match update {
            SessionUpdate::AgentMessageChunk(chunk) => (Kind::Message, chunk),
            SessionUpdate::AgentThoughtChunk(chunk) => (Kind::Thought, chunk),
            _ => continue,
        };
        let (Some(id), ContentBlock::Text(text)) = (&chunk.message_id, &chunk.content) else {
            continue;
        };
        let mut sources = Vec::new();
        let mut expected = String::new();
        for part in &attempt.presentation {
            if used.contains(&part.source) {
                continue;
            }
            let value = match (&part.content, kind) {
                (zevria_content::AssistantPresentationContent::Answer { text }, Kind::Message)
                | (
                    zevria_content::AssistantPresentationContent::Reasoning { text },
                    Kind::Thought,
                ) => text,
                _ => continue,
            };
            if !expected.is_empty() {
                expected.push('\n');
            }
            expected.push_str(value);
            sources.push(part.source.clone());
            if expected == text.text {
                break;
            }
        }
        if expected == text.text && !expected.is_empty() {
            used.extend(sources.iter().cloned());
            display.bindings.push(Binding::Text {
                kind,
                message_id: id.to_string(),
                start: 0,
                end: expected.len(),
                sources,
            });
        }
    }
    display
        .bindings
        .extend(attempt.activity.iter().map(|action| Binding::Tool {
            tool_call_id: action.client_id(&attempt.id),
            output_index: action.output_index,
            native: false,
        }));
    display.bindings.extend(attempt.presentation.iter().filter_map(|part| match &part.content {
        zevria_content::AssistantPresentationContent::NativeTool { call_id } if updates.iter().any(|update| matches!(update, SessionUpdate::ToolCall(call) if call.tool_call_id.to_string() == *call_id)) => Some(Binding::Tool { tool_call_id: call_id.clone(), output_index: part.source.output_index, native: true }),
        _ => None,
    }));
    display_carrier(display)
}

pub(crate) fn display_carrier(
    display: zevria_content::web_search::ResponseDisplay,
) -> Option<SessionUpdate> {
    display.validate().ok()?;
    let mut meta = serde_json::Map::new();
    meta.insert(
        zevria_content::web_search::RESPONSE_DISPLAY_META_KEY.into(),
        serde_json::to_value(display).ok()?,
    );
    Some(SessionUpdate::SessionInfoUpdate(
        agent_client_protocol::schema::v1::SessionInfoUpdate::new().meta(meta),
    ))
}

pub(crate) fn assistant_reasoning_text(message: &Message) -> String {
    let Message::Assistant { content, .. } = message else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|content| {
            let AssistantContent::Reasoning(reasoning) = content else {
                return None;
            };
            let text = reasoning
                .content
                .iter()
                .filter_map(|part| match part {
                    ReasoningContent::Summary(text) | ReasoningContent::Text { text, .. } => {
                        Some(text.as_str())
                    }
                    ReasoningContent::Encrypted(_) | ReasoningContent::Redacted { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
