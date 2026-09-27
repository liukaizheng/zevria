//! Request lifecycle validation, independent of the skill ledger and launch evidence.
use crate::TranscriptItem;
use rig_core::message::{AssistantContent, Message};
use std::collections::BTreeSet;
use zevria_foundation::{RequestBehavior, RequestMetadata};
use zevria_instructions::{RequestDirective, RequestDirectiveKind};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RequestReplayState {
    expected: Option<RequestMetadata>,
    boundary_allowed: bool,
    current: Option<RequestMetadata>,
    corrected: bool,
    final_response: bool,
    seen: BTreeSet<String>,
}

pub(crate) fn is_request_owner(item: &TranscriptItem) -> bool {
    crate::is_prompt_item(item)
        || matches!(
            item,
            TranscriptItem::Plan(zevria_workflow::PlanRecord::Handoff { .. })
        )
        || matches!(
            item,
            TranscriptItem::Ensemble(zevria_workflow::EnsembleRecord::ReportsReady { .. })
        )
}

impl RequestReplayState {
    pub(crate) fn has_boundaries(&self) -> bool {
        !self.seen.is_empty()
    }

    pub(crate) fn validate_complete(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.expected.is_none(),
            "request prompt is missing its immediately following boundary directive"
        );
        Ok(())
    }

    pub(crate) fn apply(
        &mut self,
        item: &TranscriptItem,
        pending_empty: bool,
    ) -> anyhow::Result<()> {
        if self.expected.is_some() {
            anyhow::ensure!(
                matches!(item, TranscriptItem::RequestDirective(d) if d.kind == RequestDirectiveKind::Boundary),
                "request boundary must immediately follow its owning prompt"
            );
        }
        match item {
            TranscriptItem::RequestDirective(directive) => {
                directive.validate()?;
                anyhow::ensure!(
                    pending_empty,
                    "a request directive cannot split a tool call/result batch"
                );
                match directive.kind {
                    RequestDirectiveKind::Boundary => {
                        anyhow::ensure!(
                            self.boundary_allowed,
                            "request boundary has no immediately preceding owner"
                        );
                        if let Some(expected) = self.expected.take() {
                            anyhow::ensure!(
                                expected == directive.request,
                                "request directive ownership mismatch"
                            );
                        } else {
                            anyhow::ensure!(
                                directive.request.behavior == RequestBehavior::Standard,
                                "orchestration requires typed intent on the owning user prompt"
                            );
                        }
                        anyhow::ensure!(
                            self.seen.insert(directive.request.id.clone()),
                            "duplicate request identity"
                        );
                        self.current = Some(directive.request.clone());
                        self.corrected = false;
                    }
                    RequestDirectiveKind::Correction => {
                        anyhow::ensure!(
                            self.current.as_ref() == Some(&directive.request),
                            "correction must belong to the current accepted request"
                        );
                        anyhow::ensure!(
                            !self.corrected && self.final_response,
                            "request permits one correction after a premature final response"
                        );
                        self.corrected = true;
                    }
                }
                self.boundary_allowed = false;
                self.final_response = false;
            }
            _ => {
                self.boundary_allowed = is_request_owner(item);
                if self.boundary_allowed {
                    self.current = None;
                    self.corrected = false;
                    if let TranscriptItem::RequestPrompt { message, request } = item {
                        zevria_content::UserPrompt::from_message(message)?;
                        request.validate()?;
                        self.expected = Some(request.clone());
                    }
                }
                if matches!(item, TranscriptItem::Error { .. }) {
                    self.current = None;
                }
                self.final_response = matches!(item.message(), Some(Message::Assistant { content, .. }) if !content.iter().any(|block| matches!(block, AssistantContent::ToolCall(_))));
            }
        }
        Ok(())
    }
}

/// Reproject only the latest typed request contract around a checkpoint. Never
/// derive authorization or qualifying launch evidence from summary prose.
pub(crate) fn effective_request_directives<'a>(
    items: impl Iterator<Item = &'a TranscriptItem>,
) -> Vec<&'a RequestDirective> {
    let mut directives = Vec::new();
    for item in items {
        if is_request_owner(item) || matches!(item, TranscriptItem::Error { .. }) {
            directives.clear();
        }
        if let TranscriptItem::RequestDirective(directive) = item {
            if directive.kind == RequestDirectiveKind::Boundary {
                directives.clear();
            }
            directives.push(directive);
        }
    }
    directives
}
