//! Durable identities and ordered reductions for captured ACP decisions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{collections::HashSet, fmt};

use crate::QuestionRequestId;

/// Maximum compact JSON size of one exact normalized ACP answer batch. Larger
/// accepted batches are represented by an explicit unavailable marker rather
/// than truncated or partially reinterpreted.
pub const MAX_NORMALIZED_USER_DECISION_BYTES: usize = 64 * 1024;

/// Terminal classification for one ACP form elicitation. Non-accepted events
/// contain no decision payload; accepted non-secret events may carry the exact
/// normalized display answers described below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentElicitationOutcome {
    Accepted,
    Declined,
    Cancelled,
    RequestCancelled,
    Unsupported,
    ProtocolViolation,
    Unavailable,
}

impl fmt::Display for AgentElicitationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Cancelled => "cancelled",
            Self::RequestCancelled => "request cancelled",
            Self::Unsupported => "unsupported",
            Self::ProtocolViolation => "protocol violation",
            Self::Unavailable => "unavailable",
        })
    }
}

/// Stable root-visible identity for one exact answer in a Zevria question
/// request. The digest keeps identifiers compact while deriving them solely
/// from the durable request and question identifiers.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct AgentUserDecisionId(String);

impl AgentUserDecisionId {
    pub fn from_question(request_id: &QuestionRequestId, question_id: &str) -> Self {
        Self(stable_digest_id(
            "decision",
            [request_id.as_str(), question_id],
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentUserDecisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Exact display-form value accepted from the shared Zevria question UI.
/// Provider wire constants and unselected choices are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentUserDecisionValue {
    String { value: String },
    Strings { values: Vec<String> },
    Skipped,
}

/// One authoritative user answer captured for the original ensemble request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentUserDecisionAnswer {
    pub decision_id: AgentUserDecisionId,
    pub question_id: String,
    pub header: String,
    pub question: String,
    pub answer: AgentUserDecisionValue,
}

/// All accepted answers from one ACP elicitation request. Question wording is
/// retained only as quoted context for the exact Zevria-captured values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentUserDecisionBatch {
    pub request_id: QuestionRequestId,
    pub answers: Vec<AgentUserDecisionAnswer>,
}

impl AgentUserDecisionBatch {
    pub fn decision_ids(&self) -> impl Iterator<Item = &AgentUserDecisionId> {
        self.answers.iter().map(|answer| &answer.decision_id)
    }
}

/// Stable identity for an accepted answer batch whose exact normalized value
/// could not be retained.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct AgentUnavailableDecisionId(String);

impl AgentUnavailableDecisionId {
    pub fn from_request(request_id: &QuestionRequestId) -> Self {
        Self(stable_digest_id(
            "unavailable_decision",
            [request_id.as_str()],
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentUnavailableDecisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentUnavailableDecisionReason {
    NormalizedPayloadTooLarge,
}

/// Explicit marker for an accepted ACP interaction whose exact user decision
/// is unavailable. The marker is bounded and contains no partial answer data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentUnavailableDecision {
    pub id: AgentUnavailableDecisionId,
    pub request_id: QuestionRequestId,
    pub field_count: usize,
    pub reason: AgentUnavailableDecisionReason,
}

impl AgentUnavailableDecision {
    pub fn normalized_payload_too_large(request_id: QuestionRequestId, field_count: usize) -> Self {
        Self {
            id: AgentUnavailableDecisionId::from_request(&request_id),
            request_id,
            field_count,
            reason: AgentUnavailableDecisionReason::NormalizedPayloadTooLarge,
        }
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = [0; 64];
    write_lower_hex(&Sha256::digest(bytes), &mut hex);
    String::from_utf8(hex.to_vec()).expect("hex is ASCII")
}

fn write_lower_hex(bytes: &[u8], out: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    assert_eq!(bytes.len() * 2, out.len());
    for (&byte, pair) in bytes.iter().zip(out.chunks_exact_mut(2)) {
        pair[0] = HEX[(byte >> 4) as usize];
        pair[1] = HEX[(byte & 15) as usize];
    }
}

fn stable_digest_id<'a>(prefix: &str, components: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    for component in components {
        digest.update(
            u64::try_from(component.len())
                .expect("decision identifier component length fits u64")
                .to_be_bytes(),
        );
        digest.update(component.as_bytes());
    }
    let digest = digest.finalize();
    let mut id = String::with_capacity(prefix.len() + 1 + 32);
    id.push_str(prefix);
    id.push('_');
    let mut hex = [0; 32];
    write_lower_hex(&digest[..16], &mut hex);
    id.push_str(std::str::from_utf8(&hex).expect("hex is ASCII"));
    id
}

pub fn merge_decision_evidence(
    target_batches: &mut Vec<AgentUserDecisionBatch>,
    target_ids: &mut Vec<AgentUserDecisionId>,
    target_unavailable: &mut Vec<AgentUnavailableDecision>,
    batches: &[AgentUserDecisionBatch],
    decision_ids: &[AgentUserDecisionId],
    unavailable: &[AgentUnavailableDecision],
) {
    let all_batches = std::mem::take(target_batches)
        .into_iter()
        .chain(batches.iter().cloned());
    let all_ids = std::mem::take(target_ids)
        .into_iter()
        .chain(decision_ids.iter().cloned())
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    for mut batch in all_batches {
        batch.answers.retain(|answer| {
            if seen.insert(answer.decision_id.clone()) {
                target_ids.push(answer.decision_id.clone());
                true
            } else {
                false
            }
        });
        if !batch.answers.is_empty() {
            target_batches.push(batch);
        }
    }
    for decision_id in all_ids {
        if seen.insert(decision_id.clone()) {
            target_ids.push(decision_id);
        }
    }
    merge_unavailable_decisions(target_unavailable, unavailable);
}

pub fn merge_decision_ids_from_batches(
    target_ids: &mut Vec<AgentUserDecisionId>,
    batches: &[AgentUserDecisionBatch],
) {
    let mut seen = HashSet::new();
    target_ids.retain(|decision_id| seen.insert(decision_id.clone()));
    for decision_id in batches
        .iter()
        .flat_map(AgentUserDecisionBatch::decision_ids)
    {
        if seen.insert(decision_id.clone()) {
            target_ids.push(decision_id.clone());
        }
    }
}

pub fn merge_unavailable_decisions(
    target: &mut Vec<AgentUnavailableDecision>,
    unavailable: &[AgentUnavailableDecision],
) {
    let mut seen = HashSet::new();
    target.retain(|marker| seen.insert(marker.id.clone()));
    target.extend(
        unavailable
            .iter()
            .filter(|marker| seen.insert(marker.id.clone()))
            .cloned(),
    );
}
