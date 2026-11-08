//! Adaptive local summaries extend the normal request, never replace its prefix.
//! Prefix retries are one compaction operation. Synthetic output is not published.
use super::*;
use zevria_model::compaction::{replay::replay_safe_prefixes, summary_request_message};
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
        source: &CompactionSource,
        context: &ModelContextPolicy,
        policy: &TurnPolicy,
        events: &SessionEventSender,
        turn: &TurnContext,
    ) -> anyhow::Result<(String, usize)> {
        anyhow::ensure!(
            !source.history.is_empty(),
            "cannot summarize an empty compaction source"
        );
        anyhow::ensure!(!turn.is_cancelled(), "turn cancelled");
        let full = source
            .request
            .input
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        // Validate the original before considering a smaller replay-safe prefix.
        self.preflight_summary_input(context, &full)?;
        let history = source
            .history
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        let safe = replay_safe_prefixes(&history)?;
        let prompt = summary_request_message(self.compaction.summary_prompt());
        let catalog_backed = self
            .provider
            .model_catalog()
            .iter()
            .any(|entry| entry.context.profile == context.profile);
        let mut last_size_error = None;
        for k in (1..=history.len()).rev() {
            anyhow::ensure!(!turn.is_cancelled(), "turn cancelled");
            if !safe[k] {
                tracing::debug!(
                    source_items = history.len(),
                    prefix_items = k,
                    "skipping dependency-crossing summary boundary"
                );
                continue;
            }
            let mut input = full[..source.cut(k)].to_vec();
            input.push(ModelRequestItem::message(&prompt));
            let request = ModelRequest {
                instructions: &source.request.instructions,
                tools: &source.request.tools,
                forbid_tool_calls: false,
                input,
                model_role: policy.model_role,
                allowed_tool_names: policy.allowed_tool_names.as_deref(),
            };
            if catalog_backed {
                let reasoning = self
                    .provider
                    .model_selection(policy.model_role)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "catalog-backed provider requires an explicit role selection"
                        )
                    })?
                    .reasoning_level;
                if !self
                    .request_fits_view(context, reasoning, request.clone(), turn.cancellation())
                    .await?
                {
                    tracing::debug!(
                        source_items = history.len(),
                        prefix_items = k,
                        rejection = "local",
                        "summary prefix exceeds input capacity"
                    );
                    continue;
                }
            } else {
                self.preflight_summary_input(context, &request.input)?;
            }
            // A full-history attempt may continue the normal WebSocket chain.
            // The provider's exact-prefix check chooses full replay when needed.
            for forbid_tool_calls in [false, true] {
                anyhow::ensure!(!turn.is_cancelled(), "turn cancelled");
                let completion = {
                    let mut request = request.clone();
                    request.forbid_tool_calls = forbid_tool_calls;
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
                // Synthetic completions never own a subsequent normal chain.
                self.provider.reset();
                match completion.expect("not cancelled") {
                    Ok(response) => {
                        if !assistant_tool_calls(response.message()).is_empty()
                            && !forbid_tool_calls
                        {
                            tracing::warn!(
                                prefix_items = k,
                                "summary returned tool calls; retrying with tool_choice none (may invalidate the messages cache)"
                            );
                            continue;
                        }
                        anyhow::ensure!(
                            assistant_tool_calls(response.message()).is_empty(),
                            "summary returned tool calls; source history unchanged"
                        );
                        let summary = assistant_plain_text(response.message());
                        anyhow::ensure!(
                            !summary.trim().is_empty(),
                            "summary returned empty text; source history unchanged"
                        );
                        tracing::debug!(
                            source_items = history.len(),
                            prefix_items = k,
                            "summary prefix completed"
                        );
                        return Ok((summary, k));
                    }
                    Err(error) if error.chain().any(|cause| cause.is::<ModelInputTooLarge>()) => {
                        tracing::debug!(
                            source_items = history.len(),
                            prefix_items = k,
                            rejection = "provider",
                            "summary prefix exceeds input capacity"
                        );
                        last_size_error = Some(error);
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let diagnostic = format!(
            "no nonempty replay-safe summary prefix of {} source items fits profile {} input capacity; source context is unchanged; select a larger compatible model or start a fresh session",
            history.len(),
            context.profile
        );
        Err(match last_size_error {
            Some(error) => error.context(diagnostic),
            None => anyhow::anyhow!(diagnostic),
        })
    }
}
