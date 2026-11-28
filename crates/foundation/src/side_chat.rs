//! Model-inert Side Chat identities and explicit user attachments.
use crate::{ModelProfileRef, ModelRole, ReasoningLevel, SessionMode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Uuid", into = "Uuid")]
pub struct SideChatId(Uuid);

impl TryFrom<Uuid> for SideChatId {
    type Error = &'static str;

    fn try_from(id: Uuid) -> Result<Self, Self::Error> {
        (id.get_version() == Some(uuid::Version::Random))
            .then_some(Self(id))
            .ok_or("expected a v4 Side Chat ID")
    }
}

impl From<SideChatId> for Uuid {
    fn from(id: SideChatId) -> Self {
        id.0
    }
}

impl std::fmt::Display for SideChatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl SideChatId {
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn short(self) -> String {
        format!("sc-{}", &self.0.simple().to_string()[..8])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideChatStatus {
    Idle,
    Running,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideChatOrigin {
    pub version: u32,
    pub id: SideChatId,
    pub parent_session_id: String,
    pub title: String,
    pub forked_mode: SessionMode,
    pub forked_model_role: ModelRole,
    pub forked_profile: ModelProfileRef,
    pub forked_reasoning: ReasoningLevel,
    pub inherited_turn_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted_excerpt: Option<String>,
}

impl SideChatOrigin {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported Side Chat origin version");
        anyhow::ensure!(
            !self.parent_session_id.is_empty()
                && !self.title.trim().is_empty()
                && self.title.len() <= 256,
            "invalid Side Chat origin"
        );
        anyhow::ensure!(
            self.quoted_excerpt.as_ref().is_none_or(|text| !text.trim().is_empty() && text.len() <= SIDE_CHAT_EXCERPT_BYTES),
            "invalid Side Chat selection"
        );
        Ok(())
    }

    pub fn descriptor(&self, turn_count: usize) -> SideChatDescriptor {
        SideChatDescriptor {
            id: self.id,
            short_id: self.id.short(),
            title: self.title.clone(),
            forked_mode: self.forked_mode,
            forked_profile: self.forked_profile.clone(),
            forked_reasoning: self.forked_reasoning,
            inherited_turn_count: self.inherited_turn_count,
            turn_count,
            status: SideChatStatus::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideChatDescriptor {
    pub id: SideChatId,
    pub short_id: String,
    pub title: String,
    pub forked_mode: SessionMode,
    pub forked_profile: ModelProfileRef,
    pub forked_reasoning: ReasoningLevel,
    pub inherited_turn_count: usize,
    pub turn_count: usize,
    pub status: SideChatStatus,
}

pub const SIDE_CHAT_EXCERPT_BYTES: usize = 16 * 1024;
pub const SIDE_CHAT_ATTACHMENTS_BYTES: usize = 32 * 1024;
pub const SIDE_CHAT_ATTACHMENTS_COUNT: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideChatReferenceAttachment {
    pub id: SideChatId,
    pub short_id: String,
    pub title: String,
    /// A one-based displayed turn number, or None when referencing the latest answer.
    pub turn_ordinal: Option<usize>,
    pub question_preview: String,
    pub excerpt: String,
}

impl SideChatReferenceAttachment {
    pub fn bytes(&self) -> usize {
        self.short_id.len() + self.title.len() + self.question_preview.len() + self.excerpt.len()
    }

    pub fn validate_all(references: &[Self]) -> anyhow::Result<()> {
        anyhow::ensure!(
            references.len() <= SIDE_CHAT_ATTACHMENTS_COUNT,
            "too many Side Chat references"
        );
        let mut total = 0;
        for item in references {
            let bytes = item.bytes();
            anyhow::ensure!(
                item.short_id == item.id.short()
                    && item.turn_ordinal != Some(0)
                    && !item.title.trim().is_empty()
                    && !item.excerpt.trim().is_empty()
                    && bytes <= SIDE_CHAT_EXCERPT_BYTES,
                "invalid or oversized Side Chat reference"
            );
            total += bytes;
        }
        anyhow::ensure!(
            total <= SIDE_CHAT_ATTACHMENTS_BYTES,
            "Side Chat references exceed 32 KiB"
        );
        Ok(())
    }
}

pub fn bound_side_chat_text(text: &str, bytes: usize) -> &str {
    let text = text.trim();
    text[..text.floor_char_boundary(bytes.min(text.len()))].trim_end()
}
