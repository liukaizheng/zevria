//! Side Chat pane state and explicit quotations shared by native frontends.
use crate::{
    PaneScope, SessionModel,
    blocks::{PresentationBlockKind, PresentationRole},
    conversation::HistoryEntry,
};
use zevria_content::{PromptImage, UserPrompt};
use zevria_foundation::{
    ModelRole, SIDE_CHAT_ATTACHMENTS_BYTES, SIDE_CHAT_ATTACHMENTS_COUNT, SIDE_CHAT_EXCERPT_BYTES,
    SideChatDescriptor, SideChatId, SideChatReferenceAttachment, SideChatStatus,
    bound_side_chat_text,
};
use zevria_session_api::{SessionEvent, SideChatCommand, SideChatEvent, TurnEvent};
use zevria_transcript::TranscriptItem;

pub struct SideChatPane {
    pub descriptor: SideChatDescriptor,
    pub model: SessionModel,
    pub quoted_excerpt: Option<String>,
    pub draft: String,
    pub draft_images: Vec<PromptImage>,
    pub awaiting_initial_turn: bool,
    pub error: Option<String>,
}

pub struct RestoredSidePane {
    pub descriptor: SideChatDescriptor,
    pub quoted_excerpt: Option<String>,
    pub items: Vec<TranscriptItem>,
}

impl SideChatPane {
    pub fn new(descriptor: SideChatDescriptor, quoted_excerpt: Option<String>) -> Self {
        let mut model = SessionModel::default();
        let role = match descriptor.forked_mode {
            zevria_foundation::SessionMode::Build => ModelRole::Build,
            zevria_foundation::SessionMode::Plan => ModelRole::Plan,
        };
        model.apply_selected_mode(descriptor.forked_mode);
        model.set_scope(PaneScope::SideChat {
            id: descriptor.id,
            role,
        });
        model
            .session
            .install_model(role, descriptor.forked_profile.clone(), None);
        model
            .session
            .install_reasoning(role, descriptor.forked_reasoning);
        Self {
            descriptor,
            model,
            quoted_excerpt,
            draft: String::new(),
            draft_images: Vec::new(),
            awaiting_initial_turn: false,
            error: None,
        }
    }

    pub fn restore(saved: RestoredSidePane) -> Self {
        let mut side = Self::new(saved.descriptor, saved.quoted_excerpt);
        side.model.conversation.restore(saved.items);
        side
    }

    pub fn banner(&self) -> String {
        let d = &self.descriptor;
        format!(
            "Forked from {} · {} turns · {}",
            d.forked_mode, d.inherited_turn_count, d.forked_profile
        )
    }

    pub fn is_running(&self) -> bool {
        self.awaiting_initial_turn
            || self.descriptor.status == SideChatStatus::Running
            || self.model.is_busy()
    }

    pub fn apply(&mut self, event: SessionEvent) {
        match &event {
            SessionEvent::Turn(TurnEvent::Started { .. }) => {
                self.awaiting_initial_turn = false;
                if self.model.pending_prompt().is_some() {
                    self.draft.clear();
                    self.draft_images.clear();
                }
                self.descriptor.turn_count += 1;
                self.descriptor.status = SideChatStatus::Running;
                self.error = None;
            }
            SessionEvent::Turn(
                TurnEvent::Rejected { error, .. } | TurnEvent::Failed { error, .. },
            ) => {
                self.awaiting_initial_turn = false;
                self.descriptor.status = SideChatStatus::Failed;
                self.error = Some(error.clone());
            }
            SessionEvent::Turn(TurnEvent::Completed { .. } | TurnEvent::Cancelled { .. }) => {
                self.awaiting_initial_turn = false;
                self.descriptor.status = SideChatStatus::Idle;
            }
            _ => {}
        }
        self.model.retain_unsaved_partial(&event);
        self.model.reduce(event);
    }

    pub fn stopped(&mut self, error: String) {
        self.awaiting_initial_turn = false;
        self.descriptor.status = SideChatStatus::Failed;
        self.error = Some(error);
        self.model.stop_side_turn();
    }
}

pub fn apply_side_chat_event(panes: &mut Vec<SideChatPane>, event: SideChatEvent) {
    let id = match event {
        SideChatEvent::Created {
            descriptor,
            quoted_excerpt,
            awaiting_initial_turn,
            ..
        } => {
            let mut pane = SideChatPane::new(*descriptor, quoted_excerpt);
            pane.awaiting_initial_turn = awaiting_initial_turn;
            panes.push(pane);
            return;
        }
        SideChatEvent::CreateFailed { .. } => return,
        SideChatEvent::Session { id, .. }
        | SideChatEvent::Status { id, .. }
        | SideChatEvent::Failed { id, .. }
        | SideChatEvent::SubmitRejected { id, .. }
        | SideChatEvent::Unloaded { id } => id,
    };
    let Some(pane) = panes.iter_mut().find(|pane| pane.descriptor.id == id) else {
        return;
    };
    match event {
        SideChatEvent::Session { event, .. } => pane.apply(*event),
        SideChatEvent::Status { status, .. } => pane.descriptor.status = status,
        SideChatEvent::Failed { error, .. } => pane.stopped(error),
        SideChatEvent::SubmitRejected { error, .. } => {
            if pane.descriptor.status != SideChatStatus::Running {
                pane.stopped(error);
            }
        }
        SideChatEvent::Unloaded { .. } => pane.descriptor.status = SideChatStatus::Idle,
        _ => unreachable!(),
    }
}

pub fn new_side_chat(
    prompt: Option<UserPrompt>,
    quoted_excerpt: Option<String>,
) -> SideChatCommand {
    SideChatCommand::Create {
        request_id: SideChatId::random().to_string(),
        initial_prompt: prompt,
        quoted_excerpt,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideChatCompletionItem {
    pub label: String,
    pub insertion: String,
}

/// Locate a reference token at the caret, without hijacking normal file completion.
pub fn side_chat_query(text: &str, cursor: usize) -> Option<(&str, std::ops::Range<usize>)> {
    let before = text.get(..cursor)?;
    let start = before.rfind('@')?;
    if before[..start]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '\\' | '_' | '.' | '@' | '-' | '+'))
    {
        return None;
    }
    let token = &text[start..];
    let rest = token.strip_prefix("@side")?;
    if cursor < start + "@side".len() {
        return None;
    }
    let mut end = start + "@side".len();
    if let Some(title) = rest.strip_prefix(':') {
        end += 1;
        end += title
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
            .map(char::len_utf8)
            .sum::<usize>();
        if let Some(turn) = text[end..].strip_prefix('#') {
            end += 1 + turn.bytes().take_while(u8::is_ascii_digit).count();
        }
    } else if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    (cursor <= end).then_some((&text[start..cursor], start..end))
}

pub fn side_chat_reference_completions(
    panes: &[SideChatPane],
    query: &str,
) -> Vec<SideChatCompletionItem> {
    side_chat_completions_from(panes.iter().rev().map(|pane| &pane.descriptor), query)
}

pub fn side_chat_completions_from<'a>(
    descriptors: impl Iterator<Item = &'a SideChatDescriptor>,
    query: &str,
) -> Vec<SideChatCompletionItem> {
    let query = query
        .strip_prefix("@side:")
        .or_else(|| (query == "@side").then_some(""));
    let Some(query) = query else {
        return Vec::new();
    };
    let (query, turn) = query
        .split_once('#')
        .map_or((query, None), |(title, turn)| (title, Some(turn)));
    let query = query.to_lowercase();
    descriptors
        .filter(|descriptor| {
            descriptor.short_id.starts_with(&query)
                || descriptor
                    .short_id
                    .trim_start_matches("sc-")
                    .starts_with(&query)
                || descriptor.title.to_lowercase().contains(&query)
        })
        .take(8)
        .map(|descriptor| SideChatCompletionItem {
            label: descriptor.title.clone(),
            insertion: match turn.filter(|turn| !turn.is_empty()) {
                Some(turn) => format!("@side:{}#{turn}", descriptor.short_id),
                None => format!("@side:{}", descriptor.short_id),
            },
        })
        .collect()
}

/// One row per answer, grouped by the user's displayed turn ordinal. Only
/// assistant prose is reused; tool output and hidden reasoning aren't conclusions.
pub fn side_chat_answers(model: &SessionModel) -> Vec<(usize, String, String)> {
    let mut ordinal = 0;
    let mut question = String::new();
    let mut answers: Vec<(usize, String, String)> = Vec::new();
    for (index, row) in model.conversation.history().iter().enumerate() {
        let HistoryEntry::Conversation(entry) = row else {
            continue;
        };
        let is_user = entry
            .blocks
            .iter()
            .any(|block| block.role == Some(PresentationRole::User));
        if is_user {
            ordinal += 1;
        }
        let mut parts = entry
            .blocks
            .iter()
            .filter_map(|block| {
                let PresentationBlockKind::Text { text, .. } = &block.kind else {
                    return None;
                };
                (!text.trim().is_empty()
                    && matches!(
                        (is_user, block.role),
                        (true, Some(PresentationRole::User))
                            | (false, Some(PresentationRole::Assistant))
                    )
                    && (is_user || model.conversation.is_committed_answer(index, block.id)))
                .then_some(bound_side_chat_text(text, SIDE_CHAT_EXCERPT_BYTES))
            })
            .peekable();
        if is_user {
            question = parts.next().unwrap_or_default().to_owned();
        } else if ordinal != 0 && parts.peek().is_some() {
            let answer = parts.collect::<Vec<_>>().join("\n\n");
            if let Some((turn, _, last)) = answers.last_mut()
                && *turn == ordinal
            {
                *last = answer;
            } else {
                answers.push((ordinal, question.clone(), answer));
            }
        }
    }
    answers
}

pub fn format_quote_for_main(title: &str, answer: &str) -> String {
    let excerpt = bound_side_chat_text(answer, SIDE_CHAT_EXCERPT_BYTES);
    let body = excerpt
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "> Side Chat: {}\n>\n{body}\n\n",
        title.lines().next().unwrap_or_default()
    )
}

pub fn extract_side_chat_references(
    prompt: &UserPrompt,
    panes: &[SideChatPane],
) -> Vec<SideChatReferenceAttachment> {
    extract_side_chat_references_from(
        prompt,
        panes.iter().map(|pane| (&pane.descriptor, &pane.model)),
    )
}

pub fn extract_side_chat_references_from<'a>(
    prompt: &UserPrompt,
    panes: impl Iterator<Item = (&'a SideChatDescriptor, &'a SessionModel)> + Clone,
) -> Vec<SideChatReferenceAttachment> {
    let mut references: Vec<SideChatReferenceAttachment> = Vec::new();
    let mut remaining = SIDE_CHAT_ATTACHMENTS_BYTES;
    let text = prompt.text_projection();
    for (index, rest) in text.match_indices("@side:") {
        if text[..index]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '\\' | '_' | '.' | '@' | '-' | '+'))
        {
            continue;
        }
        let token: String = text[index + rest.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '#'))
            .collect();
        if text[index + rest.len() + token.len()..].starts_with('@') {
            continue;
        }
        let token = token.to_ascii_lowercase();
        let (short, turn) = match token.split_once('#') {
            Some((short, turn)) => match turn.parse::<usize>() {
                Ok(turn) if turn > 0 => (short, Some(turn)),
                _ => continue,
            },
            None => (token.as_str(), None),
        };
        let prefix = short.strip_prefix("sc-").unwrap_or(short);
        if prefix.len() < 6 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let mut matches = panes.clone().filter(|(descriptor, _)| {
            descriptor
                .short_id
                .trim_start_matches("sc-")
                .starts_with(prefix)
        });
        let Some((descriptor, model)) = matches.next() else {
            continue;
        };
        if matches.next().is_some() {
            continue;
        }
        if references
            .iter()
            .any(|item| item.id == descriptor.id && item.turn_ordinal == turn)
        {
            continue;
        }
        let Some((_, question, answer)) = side_chat_answers(model)
            .into_iter()
            .rev()
            .find(|(ordinal, _, _)| turn.is_none_or(|turn| *ordinal == turn))
        else {
            continue;
        };
        let mut item = SideChatReferenceAttachment {
            id: descriptor.id,
            short_id: descriptor.short_id.clone(),
            title: descriptor.title.clone(),
            turn_ordinal: turn,
            question_preview: bound_side_chat_text(&question, 320).to_owned(),
            excerpt: String::new(),
        };
        item.excerpt = bound_side_chat_text(
            &answer,
            remaining
                .min(SIDE_CHAT_EXCERPT_BYTES)
                .saturating_sub(item.bytes()),
        )
        .into();
        if item.excerpt.is_empty() {
            continue;
        }
        remaining -= item.bytes();
        references.push(item);
        if references.len() == SIDE_CHAT_ATTACHMENTS_COUNT {
            break;
        }
    }
    references
}

pub fn selected_side_chat(panes: &[SideChatPane], id: SideChatId) -> Option<&SideChatPane> {
    panes.iter().find(|pane| pane.descriptor.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::{ModelProfileRef, RequestBehavior, SessionMode};

    #[test]
    fn side_chat_commands_bypass_root_busy_admission() {
        let mut workspace = crate::workspace::WorkspaceModel::default();
        assert!(
            workspace
                .root
                .submit("root turn".into(), RequestBehavior::Standard)
                .is_some()
        );
        assert!(workspace.root.is_busy());
        assert!(
            matches!(workspace.route_prompt(&"/btw while busy".into(), false),
            crate::workspace::InputRoute::SideChat(Some(prompt)) if prompt.text_projection() == "while busy")
        );
        assert!(matches!(
            workspace.route_prompt(&"/side".into(), false),
            crate::workspace::InputRoute::SideChat(None)
        ));
        assert!(workspace.root.is_busy());
    }

    #[test]
    fn references_project_identically_on_acceptance_and_restore() {
        use rig_core::message::Message;
        use zevria_foundation::{RequestMetadata, TurnId};
        let id = SideChatId::random();
        let reference = SideChatReferenceAttachment {
            id,
            short_id: id.short(),
            title: "Replay check".into(),
            turn_ordinal: Some(1),
            question_preview: "Which prefix?".into(),
            excerpt: "The inherited prefix.".into(),
        };
        let message = Message::user(format!("See @side:{}#1", id.short()));
        let mut live = SessionModel::default();
        live.reduce(SessionEvent::Turn(TurnEvent::Started {
            turn_id: TurnId::new(1),
            message: message.clone(),
            mode: SessionMode::Build,
            references: vec![reference.clone()],
        }));
        let saved = TranscriptItem::RequestPrompt {
            references: zevria_transcript::ReferencedPrompt::new(&message, vec![reference.clone()])
                .unwrap(),
            message,
            request: RequestMetadata::new(RequestBehavior::Standard),
        };
        let mut restored = SessionModel::default();
        restored.conversation.restore(vec![saved]);
        for model in [live, restored] {
            let Some(HistoryEntry::Conversation(entry)) = model.conversation.entry(0) else {
                panic!("user row")
            };
            assert_eq!(entry.references, [reference.clone()]);
            assert_eq!(
                entry.blocks.len(),
                1,
                "references are not editable transcript blocks"
            );
        }
    }

    #[test]
    fn completion_replaces_a_whole_unicode_token_and_keeps_an_explicit_turn() {
        let id = SideChatId::random();
        let descriptor = SideChatDescriptor {
            id,
            short_id: id.short(),
            title: "缓存_检查 Cache".into(),
            forked_mode: SessionMode::Build,
            forked_profile: ModelProfileRef::new("test", "root"),
            forked_reasoning: zevria_foundation::ReasoningLevel::Medium,
            inherited_turn_count: 0,
            turn_count: 12,
            status: SideChatStatus::Idle,
        };
        let token = "@side:缓存_检查#12";
        let text = format!("See {token}, please");
        let (query, range) = side_chat_query(&text, 4 + "@side:缓存_".len()).unwrap();
        assert_eq!(query, "@side:缓存_");
        assert_eq!(&text[range], token);
        let matches = side_chat_completions_from([&descriptor].into_iter(), token);
        assert_eq!(matches[0].insertion, format!("@side:{}#12", id.short()));
        assert!(side_chat_query("@sidekick", "@side".len()).is_none());
    }

    #[test]
    fn side_chat_reference_matching_is_explicit_and_deduplicates() {
        let id = SideChatId::random();
        let mut pane = SideChatPane::new(
            SideChatDescriptor {
                id,
                short_id: id.short(),
                title: "Fixture".into(),
                forked_mode: SessionMode::Build,
                forked_profile: ModelProfileRef::new("test", "root"),
                forked_reasoning: zevria_foundation::ReasoningLevel::Medium,
                inherited_turn_count: 0,
                turn_count: 1,
                status: SideChatStatus::Idle,
            },
            None,
        );
        use rig_core::message::Message;
        pane.model.conversation.restore(vec![
            TranscriptItem::Message(Message::user("question")),
            TranscriptItem::Message(Message::assistant("conclusion")),
        ]);
        let id = pane.descriptor.short_id.clone();
        let prompt = format!(
            "See @side:{id} twice: @side:{id}; literal \\@side:{id}#1 and a@side:{id}#1 and @side:{id}#1@example.com"
        );
        let references = extract_side_chat_references(&prompt.into(), &[pane]);
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].excerpt, "conclusion");
        assert_eq!(references[0].turn_ordinal, None);
    }
}
