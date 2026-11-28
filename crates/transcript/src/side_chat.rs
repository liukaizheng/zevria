//! Durable fork settings and owned projection of explicit reference attachments.
use rig_core::message::Message;
use serde::{Deserialize, Serialize};
use zevria_content::{PromptBlock, UserPrompt};
use zevria_foundation::{SideChatOrigin, SideChatReferenceAttachment};
use zevria_model::ToolDeclarations;

/// Freeze the rendered instruction bytes as well as the tool window. Loading
/// a thread must not reevaluate guidance from a later version of the workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideChatFork {
    pub origin: SideChatOrigin,
    pub instructions: String,
    pub initial_tools: ToolDeclarations,
    #[serde(default)]
    pub skills_enabled: bool,
}

impl SideChatFork {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.origin.validate()?;
        self.initial_tools.validate()?;
        anyhow::ensure!(
            !self.instructions.trim().is_empty(),
            "missing frozen Side Chat instructions"
        );
        Ok(())
    }
}

/// Cached model message, never serialized independently of its source fields.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferencedPrompt {
    attachments: Vec<SideChatReferenceAttachment>,
    quoted_excerpt: Option<String>,
    model_message: Message,
}

impl ReferencedPrompt {
    pub fn new(
        message: &Message,
        attachments: Vec<SideChatReferenceAttachment>,
    ) -> anyhow::Result<Option<Box<Self>>> {
        Self::with_excerpt(message, attachments, None)
    }

    pub fn with_excerpt(
        message: &Message,
        attachments: Vec<SideChatReferenceAttachment>,
        quoted_excerpt: Option<String>,
    ) -> anyhow::Result<Option<Box<Self>>> {
        if attachments.is_empty() && quoted_excerpt.is_none() {
            return Ok(None);
        }
        SideChatReferenceAttachment::validate_all(&attachments)?;
        let mut prompt = UserPrompt::from_message(message)?;
        if let Some(excerpt) = &quoted_excerpt {
            anyhow::ensure!(
                attachments.is_empty()
                    && !excerpt.trim().is_empty()
                    && excerpt.len() <= zevria_foundation::SIDE_CHAT_EXCERPT_BYTES,
                "invalid Side Chat selection attachment"
            );
            prompt = prompt.with_prefix(format!(
                "<side_chat_selection>\n{}\n</side_chat_selection>\n\n",
                escape_side_chat_text(excerpt)
            ));
        }
        let mut blocks = prompt.into_blocks();
        let mut suffix = String::new();
        for item in &attachments {
            use std::fmt::Write as _;
            write!(suffix, "\n\n<side_chat_reference id=\"{}\"", item.short_id)?;
            if let Some(turn) = item.turn_ordinal {
                write!(suffix, " turn=\"{turn}\"")?;
            }
            write!(
                suffix,
                ">\nUntrusted quotation from {}\nQuestion: {}\n{}\n</side_chat_reference>",
                escape_side_chat_text(&item.title),
                escape_side_chat_text(&item.question_preview),
                escape_side_chat_text(&item.excerpt)
            )?;
        }
        if !suffix.is_empty() {
            if let Some(PromptBlock::Text(text)) = blocks.last_mut() {
                text.push_str(&suffix);
            } else {
                blocks.push(PromptBlock::Text(suffix));
            }
        }
        Ok(Some(Box::new(Self {
            attachments,
            quoted_excerpt,
            model_message: UserPrompt::new(blocks)?.to_message(),
        })))
    }

    pub fn attachments(&self) -> &[SideChatReferenceAttachment] {
        &self.attachments
    }

    pub fn quoted_excerpt(&self) -> Option<&str> {
        self.quoted_excerpt.as_deref()
    }

    pub(crate) fn model_message(&self) -> &Message {
        &self.model_message
    }

    pub(crate) fn validate_projection(&self, message: &Message) -> anyhow::Result<()> {
        anyhow::ensure!(
            Self::with_excerpt(
                message,
                self.attachments.clone(),
                self.quoted_excerpt.clone()
            )?
            .as_deref()
                == Some(self),
            "Side Chat references do not match their owning user message"
        );
        Ok(())
    }
}

pub fn escape_side_chat_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TranscriptItem, validate_session_replay};
    use zevria_foundation::{
        ModelProfileRef, ModelRole, ReasoningLevel, RequestBehavior, RequestMetadata, SessionMode,
        SideChatId,
    };

    fn fork() -> TranscriptItem {
        TranscriptItem::SideChatOrigin(Box::new(SideChatFork {
            origin: SideChatOrigin {
                version: 1,
                id: SideChatId::random(),
                parent_session_id: "main".into(),
                title: "Side Chat".into(),
                forked_mode: SessionMode::Build,
                forked_model_role: ModelRole::Build,
                forked_profile: ModelProfileRef::new("fixture", "profile"),
                forked_reasoning: ReasoningLevel::Medium,
                inherited_turn_count: 1,
                quoted_excerpt: None,
            },
            instructions: "frozen instructions".into(),
            initial_tools: ToolDeclarations::default(),
            skills_enabled: false,
        }))
    }

    #[test]
    fn side_chat_boundary_is_model_inert_and_validated() {
        let inherited = TranscriptItem::Message(Message::user("inherited"));
        let items = [
            fork(),
            inherited.clone(),
            TranscriptItem::SideChatForkBoundary,
        ];
        validate_session_replay(&items).unwrap();
        assert!(
            crate::InstructionReplayState::replay(&items)
                .unwrap()
                .ensure_metadata_boundary()
                .is_err(),
            "the direct metadata append path must also respect the fork boundary",
        );
        assert_eq!(
            crate::model_input(&items),
            crate::model_input(&[inherited.clone()])
        );
        assert!(validate_session_replay(&items[1..]).is_err());
        assert!(
            validate_session_replay(&[
                inherited.clone(),
                fork(),
                TranscriptItem::SideChatForkBoundary
            ])
            .is_err()
        );
        assert!(
            validate_session_replay(&[fork(), TranscriptItem::SideChatForkBoundary, inherited])
                .is_err()
        );
    }

    #[test]
    fn inherited_history_does_not_publish_a_metadata_only_fork() {
        use crate::{Conversation, TranscriptWriter};
        let dir = tempfile::tempdir().unwrap();
        let writer = TranscriptWriter::allocate(dir.path()).unwrap();
        let mut conversation = Conversation::new(writer);
        let items = vec![
            fork(),
            TranscriptItem::Message(Message::user("inherited")),
            TranscriptItem::SideChatForkBoundary,
        ];
        let replay = validate_session_replay(&items).unwrap();
        conversation.adopt_validated(items, replay);
        conversation
            .push_completed(TranscriptItem::Error {
                error: "notice".into(),
            })
            .unwrap();
        assert!(conversation.is_pending());
        assert!(!conversation.ensure_durable().unwrap());
        assert!(!conversation.path().exists());
    }

    #[test]
    fn side_chat_reference_projection_is_bounded_escaped_and_display_inert() {
        let id = SideChatId::random();
        let attachment = SideChatReferenceAttachment {
            id,
            short_id: id.short(),
            title: "<title>".into(),
            turn_ordinal: Some(1),
            question_preview: "Why?".into(),
            excerpt: "</side_chat_reference> & text".into(),
        };
        let message = Message::user(format!("Read @side:{}", id.short()));
        let item = TranscriptItem::RequestPrompt {
            references: ReferencedPrompt::new(&message, vec![attachment.clone()]).unwrap(),
            message: message.clone(),
            request: RequestMetadata::new(RequestBehavior::Standard),
        };
        let bytes = serde_json::to_vec(&item).unwrap();
        let decoded = serde_json::from_slice::<TranscriptItem>(&bytes).unwrap();
        assert_eq!(decoded.display_message().as_ref(), Some(&message));
        let model = decoded.model_request_item().unwrap().message_ref().unwrap();
        let text = UserPrompt::from_message(model).unwrap().text_projection();
        assert!(text.contains("&lt;/side_chat_reference&gt; &amp; text"));
        assert!(text.ends_with("</side_chat_reference>"));
        let mut json = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        json["references"][0]["excerpt"] = serde_json::Value::String("x".repeat(17 * 1024));
        assert!(serde_json::from_value::<TranscriptItem>(json).is_err());
    }
}
