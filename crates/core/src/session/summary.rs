//! Adaptive local summaries. Prefix retries are one compaction operation, not
//! repeated automatic compactions. No synthetic turn output is published.
use super::*;
use zevria_model::compaction::replay::replay_safe_prefixes;
use zevria_model::models::ReplayPreflight;

impl<P: ModelProvider> SessionEngine<P> {
    pub(super) fn preflight_summary_input(
        &self,
        context: &ModelContextPolicy,
        input: &[ModelRequestItem<'_>],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                self.provider.preflight_input(&context.profile, input)?,
                ReplayPreflight::Compatible(_)
            ),
            "profile {} cannot read this opaque checkpoint; select its exact source or use explicit /model conversion",
            context.profile
        );
        Ok(())
    }

    pub(super) async fn summarize_prefix(
        &mut self,
        source: &[OwnedModelRequestItem],
        instruction_set: &zevria_instructions::InstructionSet,
        context: &ModelContextPolicy,
        policy: &TurnPolicy,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> anyhow::Result<(String, usize)> {
        anyhow::ensure!(
            !source.is_empty(),
            "cannot summarize an empty compaction source"
        );
        anyhow::ensure!(!turn.is_cancelled(), "turn cancelled");
        let mut input = source
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        // Validate the original first: arbitrary replay errors must never become
        // permission to discard newer items until a malformed source disappears.
        self.preflight_summary_input(context, &input)?;
        let safe = replay_safe_prefixes(&input)?;
        let prompt = Message::user(self.compaction.summary_prompt().to_string());
        let instructions = instruction_set.render();
        let summary_policy = TurnPolicy::new(
            "Summarize without tools",
            Some(Vec::new()),
            policy.model_role,
            false,
        );
        let catalog_backed = self
            .provider
            .model_catalog()
            .iter()
            .any(|entry| entry.context.profile == context.profile);
        let mut last_size_error = None;
        for k in (1..=source.len()).rev() {
            anyhow::ensure!(!turn.is_cancelled(), "turn cancelled");
            input.truncate(k);
            if !safe[k] {
                tracing::debug!(
                    source_items = source.len(),
                    prefix_items = k,
                    tail_items = source.len() - k,
                    "skipping dependency-crossing summary boundary"
                );
                continue;
            }
            input.push(ModelRequestItem::message(&prompt));
            zevria_model::maintenance::validate_maintenance_input(&input)?;
            if catalog_backed {
                if !self
                    .selection_fits_view(
                        context,
                        self.provider
                            .model_selection(policy.model_role)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "catalog-backed provider requires an explicit role selection"
                                )
                            })?
                            .reasoning_level,
                        &input,
                        &summary_policy,
                        false,
                        turn.cancellation(),
                    )
                    .await?
                {
                    tracing::debug!(
                        source_items = source.len(),
                        prefix_items = k,
                        tail_items = source.len() - k,
                        rejection = "local",
                        "summary prefix exceeds input capacity"
                    );
                    continue;
                }
            } else {
                // Adapters without a catalog keep provider-owned size admission,
                // but still validate the admitted projection before dispatch.
                self.preflight_summary_input(context, &input)?;
            }
            anyhow::ensure!(!turn.is_cancelled(), "turn cancelled");
            // Neither a normal chain nor a preceding failed synthetic request is
            // a valid continuation for this prefix. Local rejects do not dispatch.
            self.provider.reset();
            let completion = {
                let request = ModelRequest {
                    instructions: &instructions,
                    input: input.clone(),
                    model_role: summary_policy.model_role,
                    allowed_tool_names: summary_policy.allowed_tool_names.as_deref(),
                };
                let progress = ProgressReporter::silent_for_turn(
                    events.clone(),
                    turn.clone(),
                    policy.model_role,
                    context,
                );
                let completion = self.provider.complete(request, progress);
                tokio::pin!(completion);
                tokio::select! {
                    biased;
                    () = turn.cancellation().cancelled() => None,
                    result = &mut completion => Some(result),
                }
            };
            if completion.is_none() || turn.is_cancelled() {
                self.provider.cancel();
                self.provider.reset();
                anyhow::bail!("turn cancelled");
            }
            self.provider.reset();
            match completion.expect("not cancelled") {
                Ok(response) => {
                    tracing::debug!(
                        source_items = source.len(),
                        prefix_items = k,
                        tail_items = source.len() - k,
                        "summary prefix completed"
                    );
                    anyhow::ensure!(
                        assistant_tool_calls(response.message()).is_empty(),
                        "summary returned tool calls; source history unchanged"
                    );
                    let summary = assistant_plain_text(response.message());
                    anyhow::ensure!(
                        !summary.trim().is_empty(),
                        "summary returned empty text; source history unchanged"
                    );
                    return Ok((summary, k));
                }
                Err(error) if error.chain().any(|cause| cause.is::<ModelInputTooLarge>()) => {
                    tracing::debug!(
                        source_items = source.len(),
                        prefix_items = k,
                        tail_items = source.len() - k,
                        rejection = "provider",
                        "summary prefix exceeds input capacity"
                    );
                    last_size_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        let diagnostic = format!(
            "no nonempty replay-safe summary prefix of {} source items fits profile {} input capacity; source context is unchanged; select a larger compatible model or start a fresh session",
            source.len(),
            context.profile
        );
        Err(match last_size_error {
            Some(error) => error.context(diagnostic),
            None => anyhow::anyhow!(diagnostic),
        })
    }
}
