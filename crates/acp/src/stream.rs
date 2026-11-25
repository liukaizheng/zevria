use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionUpdate, TextContent,
};
use rig_core::message::{AssistantContent, Message};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
use zevria_content::reasoning;
use zevria_content::response::{
    DisplayProjectionBinding as Binding, DisplayProjectionKind as Kind, ResponseDisplay,
};
use zevria_content::{
    AssistantPresentationContent as Content, ResponseAttempt, ResponseAttemptOutcome,
};
use zevria_foundation::TurnId;

const LIVE_DISPLAY_INTERVAL: Duration = Duration::from_millis(50);

/// Append-only projection of Zevria's complete watch snapshots.
///
/// ACP cannot retract chunks, so a divergent complete snapshot begins a new
/// authoritative segment instead of appending an invalid suffix to the prior
/// message ID.
#[derive(Debug, Default)]
pub(crate) struct StreamSegments {
    message: Segment,
    thought: Segment,
    attempt: Option<Arc<ResponseAttempt>>,
    completed_attempt: Option<Arc<ResponseAttempt>>,
    emitted_attempts: HashSet<String>,
    seen_attempts: HashMap<String, u64>,
    last_display: Option<DisplayReceipt>,
    next_display: Option<Instant>,
    display_pending: bool,
}

#[derive(Debug, Default)]
struct Segment {
    // Retain bytes for exact divergence detection. Hosted previews may stage
    // a suffix here until it can be sent alongside its display coverage.
    text: String,
    sent: usize,
    revision: u64,
    message_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Message,
    Thought,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisplayReceipt {
    attempt_revision: u64,
    // Segment revision and the end of the verified range.
    message: Option<(u64, usize)>,
    thought: Option<(u64, usize)>,
}

#[derive(Clone, Copy)]
enum DisplayUpdate {
    Refresh,
    CatchUp,
}

impl StreamSegments {
    #[cfg(test)]
    pub(crate) fn snapshot(&mut self, turn_id: TurnId, message: &Message) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();
        self.project(
            turn_id,
            MessageProjection::from_message(message),
            false,
            Instant::now(),
            &mut updates,
        );
        self.flush_preview(Instant::now(), &mut updates);
        updates
    }

    #[cfg(test)]
    pub(crate) fn terminal(&mut self, turn_id: TurnId, message: &Message) -> Vec<SessionUpdate> {
        self.terminal_bound(turn_id, message, None)
    }

    fn project(
        &mut self,
        turn_id: TurnId,
        projection: MessageProjection<'_>,
        terminal: bool,
        now: Instant,
        updates: &mut Vec<SessionUpdate>,
    ) -> bool {
        let (message, thought) = (&projection.message, &projection.thought);
        let message_replaced = diverged(&self.message.text, message);
        let thought_replaced = diverged(&self.thought.text, thought);
        // A correction must advertise the old, exact range before rekeying.
        if message_replaced || thought_replaced {
            self.flush_preview(now, updates);
        }
        self.message.project_complete(
            turn_id,
            StreamKind::Message,
            message,
            terminal,
            message_replaced,
        );
        self.thought.project_complete(
            turn_id,
            StreamKind::Thought,
            thought,
            terminal,
            thought_replaced,
        );
        message_replaced || thought_replaced
    }

    #[cfg(test)]
    pub(crate) fn display_snapshot(
        &mut self,
        turn_id: TurnId,
        snapshot: zevria_content::AssistantStreamSnapshot,
    ) -> Vec<SessionUpdate> {
        self.display_snapshot_at(turn_id, snapshot, Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn display_snapshot_at(
        &mut self,
        turn_id: TurnId,
        snapshot: zevria_content::AssistantStreamSnapshot,
        now: Instant,
    ) -> Vec<SessionUpdate> {
        self.display_stream_at(
            turn_id,
            snapshot.message.as_ref(),
            snapshot.attempt.map(Arc::new),
            now,
        )
    }

    pub(crate) fn display_stream_at(
        &mut self,
        turn_id: TurnId,
        message: Option<&Message>,
        attempt: Option<Arc<zevria_content::ResponseAttempt>>,
        now: Instant,
    ) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();
        let mut structural = false;
        let mut same_revision = false;
        if let Some(attempt) = attempt.as_deref() {
            if let Some(&highest) = self.seen_attempts.get(&attempt.id)
                && highest >= attempt.revision
            {
                if highest != attempt.revision || self.attempt.as_deref() != Some(attempt) {
                    return updates;
                }
                same_revision = true;
            } else {
                self.seen_attempts
                    .insert(attempt.id.clone(), attempt.revision);
                structural = !self
                    .attempt
                    .as_ref()
                    .is_some_and(|old| same_display_structure(old, attempt));
                if self
                    .attempt
                    .as_ref()
                    .is_some_and(|old| old.id != attempt.id)
                {
                    self.reset_at(now, &mut updates);
                }
            }
        }
        let projection = message.map(MessageProjection::from_message).or_else(|| {
            (!same_revision)
                .then(|| attempt.as_deref().map(MessageProjection::from_attempt))
                .flatten()
        });
        let replaced = projection
            .is_some_and(|projection| self.project(turn_id, projection, false, now, &mut updates));
        let revised = !same_revision && attempt.is_some();
        if revised && let Some(attempt) = attempt {
            if attempt.outcome == ResponseAttemptOutcome::Completed {
                self.completed_attempt = Some(attempt.clone());
            }
            self.attempt = Some(attempt);
        }
        let live = self.last_display.is_some()
            && self
                .attempt
                .as_ref()
                .is_some_and(|attempt| attempt.outcome == ResponseAttemptOutcome::InProgress);
        if structural || replaced || !live || self.next_display.is_none_or(|at| now >= at) {
            self.flush_preview(now, &mut updates);
        } else if revised || self.message.has_unsent() || self.thought.has_unsent() {
            // A previously missing channel can bind even at a higher source
            // revision, or when a repeated snapshot finally supplies its bytes.
            let last = self.last_display.expect("live display has a receipt");
            let catch_up = (last.message.is_none() && !self.message.text.is_empty())
                || (last.thought.is_none() && !self.thought.text.is_empty());
            if catch_up && let Some(update) = self.emit_display(DisplayUpdate::CatchUp) {
                self.publish_preview(now, Some(update), &mut updates);
            } else {
                self.display_pending = true;
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
        let now = Instant::now();
        let mut updates = Vec::new();
        let completed = self.completed_attempt.take();
        if self.attempt.as_ref().map(|attempt| attempt.id.as_str()) != attempt_id {
            self.reset_at(now, &mut updates);
            self.attempt = completed.filter(|attempt| Some(attempt.id.as_str()) == attempt_id);
        }
        // Terminal ACP display uses the sanitized provider ledger, not raw native text.
        let display = self
            .attempt
            .as_deref()
            .filter(|attempt| attempt.outcome == ResponseAttemptOutcome::Completed)
            .and_then(display_message);
        self.project(
            turn_id,
            MessageProjection::from_message(display.as_ref().unwrap_or(message)),
            true,
            now,
            &mut updates,
        );
        self.flush_preview(now, &mut updates);
        updates
    }

    pub(crate) fn preview_deadline(&self) -> Option<Instant> {
        self.next_display.filter(|_| self.display_pending)
    }

    pub(crate) fn flush_preview(&mut self, now: Instant, updates: &mut Vec<SessionUpdate>) {
        let display = self.emit_display(DisplayUpdate::Refresh);
        self.publish_preview(now, display, updates);
    }

    fn publish_preview(
        &mut self,
        now: Instant,
        display: Option<SessionUpdate>,
        updates: &mut Vec<SessionUpdate>,
    ) {
        self.message.flush(StreamKind::Message, updates);
        self.thought.flush(StreamKind::Thought, updates);
        if let Some(display) = display {
            updates.push(display);
            self.next_display = Some(now + LIVE_DISPLAY_INTERVAL);
        }
        self.display_pending = false;
    }

    fn emit_display(&mut self, mode: DisplayUpdate) -> Option<SessionUpdate> {
        let attempt = self.attempt.as_deref()?;
        if !attempt.has_display()
            || (attempt.hosted_activity.is_empty()
                && !matches!(
                    attempt.outcome,
                    ResponseAttemptOutcome::Failed | ResponseAttemptOutcome::Interrupted
                )
                && !self.emitted_attempts.contains(&attempt.id))
        {
            return None;
        }
        let mut bindings = Vec::new();
        let mut receipt = DisplayReceipt {
            attempt_revision: attempt.revision,
            message: None,
            thought: None,
        };
        for (kind, segment, bound) in [
            (Kind::Message, &self.message, &mut receipt.message),
            (Kind::Thought, &self.thought, &mut receipt.thought),
        ] {
            let Some(id) = &segment.message_id else {
                continue;
            };
            if segment.text.is_empty() {
                continue;
            }
            let mut sources = Vec::new();
            let mut remaining = segment.text.as_str();
            let mut matches = true;
            for part in &attempt.presentation {
                let Some((_, text)) =
                    text_part(&part.content).filter(|(channel, _)| *channel == kind)
                else {
                    continue;
                };
                if kind == Kind::Thought && text.is_empty() {
                    continue;
                }
                let prefix = if sources.is_empty() {
                    Some(remaining)
                } else {
                    remaining.strip_prefix('\n')
                };
                let Some(rest) = prefix.and_then(|prefix| prefix.strip_prefix(text)) else {
                    matches = false;
                    break;
                };
                remaining = rest;
                sources.push(part.source.clone());
            }
            // A lagging attempt can bind its exact prefix without hiding a
            // suffix for which it has no evidence.
            let end = segment.text.len() - remaining.len();
            if matches && end != 0 {
                *bound = Some((segment.revision, end));
                bindings.push(Binding::Text {
                    kind,
                    message_id: id.clone(),
                    start: 0,
                    end,
                    sources,
                });
            }
        }
        if self.last_display == Some(receipt)
            || (matches!(mode, DisplayUpdate::CatchUp)
                && self.last_display.is_some_and(|last| {
                    !(last.message.is_none() && receipt.message.is_some()
                        || last.thought.is_none() && receipt.thought.is_some())
                }))
        {
            return None;
        }
        bindings.extend(tool_bindings(attempt, |_| true));
        let update = display_carrier(ResponseDisplay {
            version: zevria_content::response::RESPONSE_DISPLAY_VERSION,
            attempt: attempt.clone(),
            bindings,
        })?;
        self.emitted_attempts.insert(attempt.id.clone());
        self.last_display = Some(receipt);
        Some(update)
    }

    /// End the current stream boundary. The next non-empty snapshot receives a
    /// fresh message ID even if its text happens to share a prefix.
    pub(crate) fn reset(&mut self) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();
        self.reset_at(Instant::now(), &mut updates);
        updates
    }

    fn reset_at(&mut self, now: Instant, updates: &mut Vec<SessionUpdate>) {
        self.flush_preview(now, updates);
        self.message.start_replacement();
        self.thought.start_replacement();
        self.attempt = None;
        self.last_display = None;
        self.next_display = None;
    }
}

fn same_display_structure(old: &ResponseAttempt, new: &ResponseAttempt) -> bool {
    old.id == new.id
        && old.outcome == new.outcome
        && old.response_id == new.response_id
        && old.profile == new.profile
        && old.hosted_activity == new.hosted_activity
        && old.presentation.len() == new.presentation.len()
        && old
            .presentation
            .iter()
            .zip(&new.presentation)
            .all(|(old, new)| {
                old.source == new.source
                    && match (&old.content, &new.content) {
                        (
                            Content::Answer { sources: old, .. },
                            Content::Answer { sources: new, .. },
                        ) => old == new,
                        (Content::Reasoning { .. }, Content::Reasoning { .. }) => true,
                        (
                            Content::NativeTool { call_id: old },
                            Content::NativeTool { call_id: new },
                        ) => old == new,
                        _ => false,
                    }
            })
}

#[derive(Default)]
struct MessageProjection<'a> {
    message: Vec<Cow<'a, str>>,
    thought: Vec<Cow<'a, str>>,
}

impl<'a> MessageProjection<'a> {
    fn from_message(message: &'a Message) -> Self {
        let mut projection = Self::default();
        if let Message::Assistant(assistant) = message {
            for part in &assistant.content {
                match part {
                    AssistantContent::Text(text) => {
                        let annotations = text
                            .native
                            .as_ref()
                            .and_then(|native| native.item.get("annotations"));
                        projection.message.push(
                            if annotations
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|values| !values.is_empty())
                            {
                                Cow::Owned(zevria_content::citations::render_text(text))
                            } else {
                                Cow::Borrowed(&text.text)
                            },
                        );
                    }
                    AssistantContent::Reasoning(reasoning) => {
                        let parts = reasoning_fragments(reasoning);
                        if parts.len() > 1 || parts.first().is_some_and(|text| !text.is_empty()) {
                            projection
                                .thought
                                .extend(parts.into_iter().map(Cow::Borrowed));
                        }
                    }
                    _ => {}
                }
            }
        }
        projection
    }

    fn from_attempt(attempt: &'a ResponseAttempt) -> Self {
        let mut projection = Self::default();
        for part in &attempt.presentation {
            match text_part(&part.content) {
                Some((Kind::Message, text)) => projection.message.push(Cow::Borrowed(text)),
                Some((Kind::Thought, text)) if !text.is_empty() => {
                    projection.thought.push(Cow::Borrowed(text))
                }
                _ => {}
            }
        }
        projection
    }
}

fn projected_fragments<'a>(values: &'a [Cow<'_, str>]) -> impl Iterator<Item = &'a str> {
    values.iter().enumerate().flat_map(|(index, part)| {
        (index != 0)
            .then_some("\n")
            .into_iter()
            .chain(std::iter::once(part.as_ref()))
    })
}

fn diverged(emitted: &str, complete: &[Cow<'_, str>]) -> bool {
    if complete.is_empty() || (complete.len() == 1 && complete[0].is_empty()) {
        return false;
    }
    let mut remaining = emitted.as_bytes();
    for part in projected_fragments(complete) {
        let n = part.len().min(remaining.len());
        if remaining[..n] != part.as_bytes()[..n] {
            return true;
        }
        remaining = &remaining[n..];
        if remaining.is_empty() {
            return false;
        }
    }
    !remaining.is_empty()
}

impl Segment {
    fn start_replacement(&mut self) {
        if !self.text.is_empty() {
            self.revision = self.revision.saturating_add(1);
        }
        self.text.clear();
        self.sent = 0;
        self.message_id = None;
    }

    fn project_complete(
        &mut self,
        turn_id: TurnId,
        kind: StreamKind,
        complete: &[Cow<'_, str>],
        terminal: bool,
        diverged: bool,
    ) {
        if diverged {
            self.start_replacement();
        }
        let mut offset = self.text.len();
        for part in projected_fragments(complete) {
            if offset >= part.len() {
                offset -= part.len();
            } else {
                self.text.push_str(&part[offset..]);
                offset = 0;
            }
        }
        if !self.text.is_empty() && self.message_id.is_none() {
            let channel = match kind {
                StreamKind::Message => "message",
                StreamKind::Thought => "thought",
            };
            let final_suffix = if terminal && diverged { "-final" } else { "" };
            self.message_id = Some(format!(
                "zevria-turn-{}-{channel}-{}{final_suffix}",
                turn_id.get(),
                self.revision,
            ));
        }
    }

    fn has_unsent(&self) -> bool {
        self.sent < self.text.len()
    }

    fn flush(&mut self, kind: StreamKind, updates: &mut Vec<SessionUpdate>) {
        if !self.has_unsent() {
            return;
        }
        let id = self.message_id.as_ref().expect("projected message ID");
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(
            &self.text[self.sent..],
        )))
        .message_id(MessageId::new(id.clone()));
        self.sent = self.text.len();
        updates.push(match kind {
            StreamKind::Message => SessionUpdate::AgentMessageChunk(chunk),
            StreamKind::Thought => SessionUpdate::AgentThoughtChunk(chunk),
        });
    }
}

pub(crate) fn display_message(attempt: &zevria_content::ResponseAttempt) -> Option<Message> {
    let content = attempt
        .presentation
        .iter()
        .filter_map(|part| match &part.content {
            zevria_content::AssistantPresentationContent::Answer { text, .. } => {
                Some(AssistantContent::Text(rig_core::message::Text::new(text)))
            }
            zevria_content::AssistantPresentationContent::Reasoning { text } => {
                Some(AssistantContent::Reasoning(reasoning::summaries(vec![
                    text.clone(),
                ])))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    (!content.is_empty()).then_some(Message::from(content))
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
                meta.contains_key(zevria_content::response::RESPONSE_DISPLAY_META_KEY)
            }) =>
        {
            info.meta.take()
        }
        _ => None,
    };
    agent_client_protocol::schema::v1::SessionNotification::new(id, update).meta(meta)
}

fn text_part(content: &Content) -> Option<(Kind, &str)> {
    match content {
        Content::Answer { text, .. } => Some((Kind::Message, text)),
        Content::Reasoning { text } => Some((Kind::Thought, text)),
        _ => None,
    }
}

fn tool_bindings(
    attempt: &ResponseAttempt,
    native: impl Fn(&str) -> bool,
) -> impl Iterator<Item = Binding> {
    attempt
        .hosted_activity
        .iter()
        .map(|action| Binding::Tool {
            tool_call_id: action.client_id(&attempt.id),
            output_index: action.output_index,
            native: false,
        })
        .chain(
            attempt
                .presentation
                .iter()
                .filter_map(move |part| match &part.content {
                    Content::NativeTool { call_id } if native(call_id) => Some(Binding::Tool {
                        tool_call_id: call_id.clone(),
                        output_index: part.source.output_index,
                        native: true,
                    }),
                    _ => None,
                }),
        )
}

pub(crate) fn replay_display(
    attempt: &ResponseAttempt,
    updates: &[SessionUpdate],
) -> Option<SessionUpdate> {
    let mut display = ResponseDisplay {
        version: zevria_content::response::RESPONSE_DISPLAY_VERSION,
        attempt: attempt.clone(),
        bindings: Vec::new(),
    };
    let mut text = [Vec::new(), Vec::new()];
    for part in &attempt.presentation {
        if let Some((kind, value)) = text_part(&part.content) {
            text[usize::from(kind == Kind::Thought)].push((&part.source, value));
        }
    }
    let mut cursor = [0; 2];
    let mut tools = HashSet::new();
    for update in updates {
        let (kind, chunk) = match update {
            SessionUpdate::AgentMessageChunk(chunk) => (Kind::Message, chunk),
            SessionUpdate::AgentThoughtChunk(chunk) => (Kind::Thought, chunk),
            SessionUpdate::ToolCall(call) => {
                tools.insert(call.tool_call_id.to_string());
                continue;
            }
            _ => continue,
        };
        let (Some(id), ContentBlock::Text(content)) = (&chunk.message_id, &chunk.content) else {
            continue;
        };
        if content.text.is_empty() {
            continue;
        }
        let channel = usize::from(kind == Kind::Thought);
        let start = cursor[channel];
        let mut remaining = content.text.as_str();
        for (index, (_, value)) in text[channel][start..].iter().enumerate() {
            let prefix = if index == 0 {
                Some(remaining)
            } else {
                remaining.strip_prefix('\n')
            };
            let Some(suffix) = prefix.and_then(|prefix| prefix.strip_prefix(value)) else {
                break;
            };
            remaining = suffix;
            if remaining.is_empty() {
                let end = start + index + 1;
                display.bindings.push(Binding::Text {
                    kind,
                    message_id: id.to_string(),
                    start: 0,
                    end: content.text.len(),
                    sources: text[channel][start..end]
                        .iter()
                        .map(|(source, _)| (*source).clone())
                        .collect(),
                });
                cursor[channel] = end;
                break;
            }
        }
    }
    display
        .bindings
        .extend(tool_bindings(attempt, |id| tools.contains(id)));
    display_carrier(display)
}

pub(crate) fn display_carrier(
    display: zevria_content::response::ResponseDisplay,
) -> Option<SessionUpdate> {
    display.validate().ok()?;
    let mut meta = serde_json::Map::new();
    meta.insert(
        zevria_content::response::RESPONSE_DISPLAY_META_KEY.into(),
        serde_json::to_value(display).ok()?,
    );
    Some(SessionUpdate::SessionInfoUpdate(
        agent_client_protocol::schema::v1::SessionInfoUpdate::new().meta(meta),
    ))
}

fn reasoning_fragments(reasoning: &rig_core::message::Reasoning) -> Vec<&str> {
    use serde_json::Value;
    let Some(native) = &reasoning.native else {
        return if reasoning.redacted {
            Vec::new()
        } else {
            vec![reasoning.text.as_str()]
        };
    };
    let get = |key: &str| {
        native
            .item
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    get("summary")
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .chain(
            get("content")
                .filter(|part| {
                    part.get("type").and_then(Value::as_str) != Some("redacted_thinking")
                })
                .filter_map(|part| {
                    part.as_str()
                        .or_else(|| part.get("text").and_then(Value::as_str))
                }),
        )
        .collect()
}

pub(crate) fn reasoning_text(reasoning: &rig_core::message::Reasoning) -> String {
    reasoning_fragments(reasoning).join("\n")
}
