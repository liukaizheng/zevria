//! Display-only review projection. Host receipts remain the sole authority for
//! confirmation; successful response settlement is deliberately not a receipt.

use super::*;
use zevria_workflow::WorkerPromptKind;
use zevria_workflow::WorkerReviewEvent as Review;
use zevria_workflow::WorkerReviewState;

#[derive(Default)]
pub(super) struct ReviewProjection {
    headers: HashMap<u64, BlockLocation>,
    inputs: HashMap<u64, PromptAnnotation>,
    active: Option<u64>,
    publication_enabled: bool,
    publication_replaying: bool,
    publications: HashMap<u64, AgentStructuredPlan>,
    retained: Option<(u64, AgentStructuredPlan)>,
    publication_floor: u64,
    settlements: HashMap<u64, Settlement>,
    notices: HashMap<String, BlockLocation>,
    provider_failures: HashMap<u64, String>,
    snapshot_generation: u64,
    payload_error: bool,
}

#[derive(Clone)]
struct Settlement {
    failure: Option<String>,
    interrupted: bool,
    has_publication: bool,
    retained_available: bool,
}

impl AgentTranscriptReducer {
    pub(super) fn apply_review_event(
        &mut self,
        conversation: &mut ConversationState,
        event: Review,
    ) {
        match event {
            Review::InputAccepted { input } => {
                // These guards precede prompt creation AND echo initialization.
                if self.review_inputs.contains(&input.request_id)
                    || self
                        .review
                        .inputs
                        .get(&input.generation)
                        .is_some_and(|old| old.request_id.is_some())
                {
                    return;
                }
                self.review_inputs.insert(input.request_id.clone());
                self.review_prompts
                    .insert(input.generation, input.text.clone());
                let annotation = self.review.inputs.entry(input.generation).or_default();
                annotation.origin = match input.kind {
                    WorkerPromptKind::Initial => PromptOrigin::Initial,
                    WorkerPromptKind::UserFeedback => PromptOrigin::Feedback,
                    WorkerPromptKind::RecoveryContinuation => PromptOrigin::Retry,
                    WorkerPromptKind::SemanticRecovery => PromptOrigin::Recovery,
                };
                annotation.generation = Some(input.generation);
                annotation.request_id = Some(input.request_id);
                annotation.phase.get_or_insert(PromptPhase::Queued);
                let annotation = annotation.clone();
                // All accepted review inputs preserve the plan scope, including
                // Initial. Origin is not a substitute for this bookkeeping flag.
                if let Some(location) =
                    self.begin_prompt(conversation, input.text, true, annotation)
                {
                    self.review.headers.insert(input.generation, location);
                }
                self.review_history(
                    conversation,
                    format!("accepted:{}", input.generation),
                    format!("Input generation {} · queued", input.generation),
                );
            }
            Review::Dispatched {
                generation,
                attempt,
            } => {
                if !self.review_transition(
                    conversation,
                    generation,
                    PromptPhase::Dispatched,
                    Some(attempt),
                ) {
                    return;
                }
                if self.review_prompts.contains_key(&generation)
                    && self.review.active.is_none_or(|active| active <= generation)
                {
                    self.review.active = Some(generation);
                    self.review.publication_enabled = false;
                    self.image_echoes = self
                        .review_prompts
                        .get(&generation)
                        .into_iter()
                        .flat_map(|prompt| prompt.images().cloned())
                        .collect();
                    self.prompt_echo =
                        self.review_prompts
                            .get(&generation)
                            .map(|prompt| PromptEcho {
                                prompt: prompt.text_projection(),
                                accumulated: String::new(),
                            });
                }
                self.review_history(
                    conversation,
                    format!("dispatch:{generation}:{attempt}"),
                    format!("Input generation {generation} · dispatched · attempt {attempt}"),
                );
            }
            Review::Recovering {
                generation,
                attempt,
            } => {
                if !self.review_transition(
                    conversation,
                    generation,
                    PromptPhase::Recovering,
                    Some(attempt),
                ) {
                    return;
                }
                if self.review.active == Some(generation) {
                    self.review.publication_enabled = false;
                }
                self.review_history(conversation, format!("recovery:{generation}:{attempt}"),
                    format!("Input generation {generation} · recovering the same session, attempt {attempt}"));
            }
            Review::CancelRequested { generation } => {
                if !self.review_transition(conversation, generation, PromptPhase::Cancelling, None)
                {
                    return;
                }
                self.review_history(conversation, format!("cancel:{generation}"),
                    format!("Input generation {generation} · worker-local cancellation durably requested"));
            }
            Review::Settled {
                generation,
                failure,
                ..
            } => {
                if self.review.settlements.contains_key(&generation) {
                    return;
                }
                self.settle_review(conversation, generation, failure, false);
            }
            Review::Interrupted { generation } => {
                // A host snapshot may have projected the generic failed
                // settlement first. The typed event refines that SAME notice.
                if let Some(settlement) = self.review.settlements.get_mut(&generation) {
                    if settlement.interrupted || settlement.failure.is_none() {
                        return;
                    }
                    settlement.interrupted = true;
                    if let Some(annotation) = self.review.inputs.get_mut(&generation) {
                        annotation.phase = Some(PromptPhase::Interrupted);
                    }
                    self.sync_review_header(conversation, generation);
                    self.refresh_settlement_notice(conversation, generation);
                } else {
                    self.settle_review(conversation, generation,
                        Some("process stopped during this interaction; delivery is ambiguous, so its text was not resent".into()), true);
                }
                self.review_history(
                    conversation,
                    format!("interrupted:{generation}"),
                    format!("Input generation {generation} · interrupted"),
                );
            }
            Review::Published {
                generation,
                plan,
                replay,
            } => {
                if !replay && self.review.active == Some(generation) {
                    self.record_review_publication(generation, &plan);
                }
            }
            Review::Removed { plan_id } => self.observe_review_removal(&plan_id),
            Review::Confirmed { receipt } => self.review_receipt(conversation, receipt, false),
            Review::BaselineMarked { receipt } => self.review_receipt(conversation, receipt, true),
            Review::BaselineCleared { request_id } => self.review_notice(
                conversation,
                format!("unbaseline:{}", request_id.0),
                DiagnosticTone::Info,
                "Baseline mark removed; confirmation retained".into(),
            ),
            Review::Withdrawn { request_id } => self.review_notice(
                conversation,
                format!("withdraw:{}", request_id.0),
                DiagnosticTone::Warning,
                "Worker confirmation withdrawn; any baseline mark cleared".into(),
            ),
            Review::Sealed => {
                self.review_sealed = true;
                self.review_notice(conversation, "sealed".into(), DiagnosticTone::Success,
                    "Final worker proposal frozen at the participating-worker confirmation boundary".into());
            }
            Review::Abandoned { .. } => {
                self.abandon(conversation);
            }
            Review::Connection {
                diagnostic: Some(error),
                ..
            } => {
                self.review_independent_error(conversation, "connection", &error);
            }
            Review::PayloadChecked { error } => {
                self.review.payload_error = error.is_some();
                if let Some(error) = error {
                    self.review_independent_error(conversation, "payload", &error);
                }
            }
            _ => {}
        }
    }

    fn review_receipt(
        &mut self,
        conversation: &mut ConversationState,
        receipt: zevria_workflow::WorkerConfirmationReceipt,
        baseline: bool,
    ) {
        let (key, label) = if baseline {
            ("baseline", "Baseline marked · proposal")
        } else {
            ("confirm", "Confirmed proposal")
        };
        self.review_notice(
            conversation,
            format!("{key}:{}", receipt.request_id.0),
            DiagnosticTone::Success,
            format!(
                "{label} revision {} · generation {} · {}{}",
                receipt.revision.revision,
                receipt.revision.generation,
                receipt.revision.digest,
                if baseline { "" } else { " (reopenable)" }
            ),
        );
    }

    fn review_transition(
        &mut self,
        conversation: &mut ConversationState,
        generation: u64,
        phase: PromptPhase,
        attempt: Option<u64>,
    ) -> bool {
        let annotation = self
            .review
            .inputs
            .entry(generation)
            .or_insert_with(|| PromptAnnotation {
                generation: Some(generation),
                ..PromptAnnotation::default()
            });
        if annotation.phase.is_some_and(PromptPhase::terminal) {
            return false;
        }
        if let Some(attempt) = attempt {
            if annotation
                .latest_attempt
                .is_some_and(|latest| attempt <= latest)
            {
                return false;
            }
            annotation.latest_attempt = Some(attempt);
        } else if phase == PromptPhase::Cancelling && annotation.cancel_requested {
            return false;
        }
        annotation.cancel_requested |= phase == PromptPhase::Cancelling;
        annotation.phase = Some(if annotation.cancel_requested && !phase.terminal() {
            PromptPhase::Cancelling
        } else {
            phase
        });
        self.sync_review_header(conversation, generation);
        true
    }

    fn sync_review_header(&mut self, conversation: &mut ConversationState, generation: u64) {
        if let Some(location) = self.review.headers.get(&generation)
            && let Some(annotation) = self.review.inputs.get(&generation)
            && let Some(block) = block_mut(conversation, *location)
            && block.prompt.as_ref() != Some(annotation)
        {
            block.prompt = Some(annotation.clone());
            block.touch();
        }
    }

    pub(super) fn rebuild_review_headers(&mut self, conversation: &ConversationState) {
        self.review.headers.clear();
        for (history_index, entry) in conversation.history().iter().enumerate() {
            let HistoryEntry::Conversation(entry) = entry else {
                continue;
            };
            for block in &entry.blocks {
                if let Some(generation) = block.prompt.as_ref().and_then(|prompt| prompt.generation)
                {
                    self.review.headers.insert(
                        generation,
                        BlockLocation {
                            history_index,
                            block_id: block.id,
                        },
                    );
                }
            }
        }
    }

    fn review_history(&mut self, conversation: &mut ConversationState, key: String, text: String) {
        if self.review_markers.insert(key) {
            self.append_transparent_diagnostic(
                conversation,
                PresentedDiagnostic {
                    label: "Host review".into(),
                    text,
                    tone: DiagnosticTone::Muted,
                },
            );
        }
    }

    fn review_notice(
        &mut self,
        conversation: &mut ConversationState,
        key: String,
        tone: DiagnosticTone,
        text: String,
    ) {
        let diagnostic = PresentedDiagnostic {
            label: "Host review".into(),
            text,
            tone,
        };
        if let Some(location) = self.review.notices.get(&key)
            && let Some(block) = block_mut(conversation, *location)
        {
            if !matches!(&block.kind, PresentationBlockKind::Diagnostic(old) if old == &diagnostic)
            {
                block.kind = PresentationBlockKind::Diagnostic(diagnostic);
                block.touch();
            }
            return;
        }
        let location = self.append_block(
            conversation,
            None,
            BlockVisibility::Always,
            PresentationBlockKind::Diagnostic(diagnostic),
        );
        self.review.notices.insert(key, location);
    }

    fn review_independent_error(
        &mut self,
        conversation: &mut ConversationState,
        source: &str,
        error: &str,
    ) {
        let generation = self
            .review
            .active
            .unwrap_or(self.review.snapshot_generation);
        let attempt = self
            .review
            .inputs
            .get(&generation)
            .and_then(|input| input.latest_attempt);
        self.review_notice(
            conversation,
            format!("{source}:{generation}:{attempt:?}:{error}"),
            DiagnosticTone::Error,
            error.into(),
        );
    }

    pub(super) fn review_replay_boundary(&mut self) {
        self.review.publication_replaying = true;
        self.review.publication_enabled = false;
    }

    pub(super) fn review_session_established(&mut self) {
        self.review.publication_replaying = false;
    }

    pub(super) fn enable_review_publication(&mut self) {
        self.review.publication_enabled =
            self.review.active.is_some() && !self.review.publication_replaying;
    }

    pub(super) fn observe_review_plan(&mut self, plan: &AgentStructuredPlan) {
        if self.review.publication_enabled
            && let Some(generation) = self.review.active
        {
            self.record_review_publication(generation, plan);
        }
    }

    fn record_review_publication(&mut self, generation: u64, plan: &AgentStructuredPlan) {
        if !self.review.settlements.contains_key(&generation)
            && plan
                .markdown
                .as_ref()
                .is_some_and(|markdown| !markdown.trim().is_empty())
        {
            self.review.publications.insert(generation, plan.clone());
        }
    }

    pub(super) fn observe_review_removal(&mut self, plan_id: &str) {
        if self.review.publication_replaying {
            return;
        }
        self.review
            .publications
            .retain(|_, plan| plan.plan_id.as_deref() != Some(plan_id));
        if self
            .review
            .retained
            .as_ref()
            .is_some_and(|(_, plan)| plan.plan_id.as_deref() == Some(plan_id))
        {
            self.review.retained = None;
        }
    }

    fn settle_review(
        &mut self,
        conversation: &mut ConversationState,
        generation: u64,
        failure: Option<String>,
        interrupted: bool,
    ) {
        let phase = if interrupted {
            PromptPhase::Interrupted
        } else if failure.is_some() {
            PromptPhase::Failed
        } else {
            PromptPhase::Succeeded
        };
        if !self.review_transition(conversation, generation, phase, None) {
            return;
        }
        let publication = self.review.publications.remove(&generation);
        let has_publication = publication.is_some();
        if failure.is_none() {
            self.review.publication_floor = self.review.publication_floor.max(generation);
            if let Some(plan) = publication
                && self
                    .review
                    .retained
                    .as_ref()
                    .is_none_or(|(old, _)| *old <= generation)
            {
                self.review.retained = Some((generation, plan));
            }
        }
        let retained_available = !self.review.payload_error
            && self
                .review
                .inputs
                .values()
                .all(|input| input.phase.is_none_or(PromptPhase::terminal))
            && self
                .review
                .retained
                .as_ref()
                .is_some_and(|(generation, _)| *generation >= self.review.publication_floor);
        self.review.settlements.insert(
            generation,
            Settlement {
                failure,
                interrupted,
                has_publication,
                retained_available,
            },
        );
        if self.review.active == Some(generation) {
            self.review.active = None;
            self.review.publication_enabled = false;
        }
        self.review_history(
            conversation,
            format!("settled:{generation}"),
            format!(
                "Input generation {generation} · {}",
                if phase == PromptPhase::Succeeded {
                    "settled successfully"
                } else {
                    "settled unsuccessfully"
                }
            ),
        );
        self.refresh_settlement_notice(conversation, generation);
    }

    pub(super) fn observe_provider_failure(
        &mut self,
        conversation: &mut ConversationState,
        error: &str,
    ) {
        // A failure outside a correlated review round is never suppressed.
        let generation = self.review.active.or_else(|| {
            self.review
                .settlements
                .keys()
                .max()
                .copied()
                .filter(|generation| {
                    self.review.settlements[generation].failure.as_deref() == Some(error)
                })
        });
        if let Some(generation) = generation {
            self.review
                .provider_failures
                .insert(generation, error.into());
            self.refresh_settlement_notice(conversation, generation);
        }
    }

    fn refresh_settlement_notice(&mut self, conversation: &mut ConversationState, generation: u64) {
        let Some(settlement) = self.review.settlements.get(&generation) else {
            return;
        };
        let key = format!("outcome:{generation}");
        let (tone, mut text) = if settlement.interrupted {
            (
                DiagnosticTone::Warning,
                format!(
                    "Input generation {generation} was interrupted; feedback was not incorporated or automatically resent."
                ),
            )
        } else if let Some(error) = &settlement.failure {
            let error_context = if self.review.provider_failures.get(&generation) == Some(error) {
                "See the worker or handoff error in this interaction.".into()
            } else {
                format!("{error}.")
            };
            let fallback = if settlement.retained_available {
                "The preceding successful proposal remains available for review; confirm it explicitly or retry."
            } else {
                "Retry or publish a fresh, complete Markdown proposal before confirmation."
            };
            (
                DiagnosticTone::Error,
                format!("Feedback was not incorporated: {error_context} {fallback}"),
            )
        } else if !settlement.has_publication {
            (
                DiagnosticTone::Info,
                format!(
                    "Input generation {generation} · discussion complete. Publish a fresh, complete Markdown proposal before confirmation."
                ),
            )
        } else {
            // A snapshot may arrive after a speculative no-publication notice.
            // Remove only this semantic notice; do not change other error rows.
            if let Some(location) = self.review.notices.remove(&key)
                && let Some(HistoryEntry::Conversation(entry)) =
                    conversation.entry_mut(location.history_index)
            {
                entry.blocks.retain(|block| block.id != location.block_id);
            }
            return;
        };
        if settlement.failure.is_some()
            && self
                .review
                .inputs
                .get(&generation)
                .is_some_and(|input| input.cancel_requested)
        {
            text.push_str(" Cancellation was requested; this outcome does not establish a completed cancellation.");
        }
        self.review_notice(conversation, key, tone, text);
    }

    /// Reliable host snapshots and journal mirrors share these semantic notice
    /// identities. Snapshot errors unrelated to settlement stay independent.
    pub(crate) fn apply_review_snapshot(
        &mut self,
        conversation: &mut ConversationState,
        state: &WorkerReviewState,
    ) {
        if self.review_abandoned
            || state.accepted_generation < self.review.snapshot_generation
            || self
                .review
                .settlements
                .keys()
                .any(|generation| *generation > state.settled_generation)
            || state.active.as_ref().is_some_and(|input| {
                self.review
                    .inputs
                    .get(&input.generation)
                    .and_then(|input| input.latest_attempt)
                    .is_some_and(|attempt| attempt > state.attempt)
            })
        {
            return;
        }
        self.review.snapshot_generation = state.accepted_generation;
        self.review.payload_error = state.synthesis_error.is_some();
        // Snapshots correlate outcomes and receipts, not intermediate attempts:
        // active + attempt cannot distinguish dispatch from recovery. Preserve
        // the ordered journal's input/echo projection rather than inventing
        // transitions that could outrun its mirrors.
        if let Some(candidate) = &state.candidate {
            self.record_review_publication(candidate.revision.generation, &candidate.plan);
        }
        self.review.retained = state
            .retained
            .as_ref()
            .filter(|_| !state.retained_removed)
            .map(|snapshot| (snapshot.revision.generation, snapshot.plan.clone()));
        self.review.publication_floor = state.publication_floor;
        let generation = state.settled_generation;
        let settlement_diagnostic = state.diagnostic.as_deref().is_some_and(|text| {
            text.starts_with("Feedback was not incorporated:")
                || text.starts_with("Discussion completed.")
        });
        if generation > 0 {
            // Core advances the publication floor on successful settlement,
            // including prose-only success. Interrupted snapshots can retain
            // an older evidence.failure, so prefer this host boundary and the
            // current outcome diagnostic rather than misattributing that error.
            let failure = (state.publication_floor < generation).then(|| {
                state
                    .diagnostic
                    .as_deref()
                    .and_then(|text| text.strip_prefix("Feedback was not incorporated: "))
                    .map(|text| {
                        text.split(". The preceding successful plan is retained;")
                            .next()
                            .unwrap_or(text)
                            .to_string()
                    })
                    .or_else(|| state.evidence.failure.clone())
                    .unwrap_or_else(|| "The interaction did not settle successfully".into())
            });
            let has_publication = self
                .review
                .retained
                .as_ref()
                .is_some_and(|(retained, _)| *retained == generation);
            let retained_available = state.eligible_snapshot().is_some();
            if !self.review.settlements.contains_key(&generation) {
                if has_publication && let Some((_, plan)) = &self.review.retained {
                    self.review.publications.insert(generation, plan.clone());
                } else {
                    self.review.publications.remove(&generation);
                }
                self.settle_review(conversation, generation, failure, false);
            }
            if let Some(settlement) = self.review.settlements.get_mut(&generation) {
                settlement.has_publication = has_publication;
                settlement.retained_available = retained_available;
            }
            self.refresh_settlement_notice(conversation, generation);
        }
        if let Some(receipt) = &state.confirmation
            && state
                .baseline
                .as_ref()
                .is_none_or(|baseline| baseline.request_id != receipt.request_id)
        {
            self.review_receipt(conversation, receipt.clone(), false);
        }
        if let Some(receipt) = &state.baseline {
            self.review_receipt(conversation, receipt.clone(), true);
        }
        if state.sealed {
            self.apply_review_event(conversation, Review::Sealed);
        }
        if let Some(error) = &state.diagnostic
            && !settlement_diagnostic
        {
            self.review_independent_error(conversation, "connection", error);
        }
        if let Some(error) = &state.synthesis_error {
            self.review_independent_error(conversation, "payload", error);
        }
    }
}
