//! Root-owned complete selections, independent of later config defaults.
use zevria_foundation::SessionMode;
use zevria_model::CompactionPolicy;
use zevria_model::models::SessionModels;
use zevria_provider::{ModelAssignment, ModelRouting};
use zevria_transcript::transcript::TranscriptLoadOutcome;

use crate::config::Config;

pub(crate) struct ResolvedSessionModels {
    pub selections: SessionModels,
    pub routing: ModelRouting,
    pub compaction: CompactionPolicy,
}

pub(crate) fn resolve(
    global: &Config,
    session_id: &str,
    resumed: Option<&TranscriptLoadOutcome>,
) -> anyhow::Result<ResolvedSessionModels> {
    let selections = match resumed {
        Some(outcome) => {
            outcome.ensure_resumable()?;
            outcome.session_models()?.ok_or_else(|| zevria_transcript::transcript::UnsupportedHistory::new(
                &outcome.path, Some(1), "version 1 Build/Plan model metadata with explicit reasoning levels; start a new session or explicitly repair the saved metadata",
            ))?.clone()
        }
        None => SessionModels::new(
            global.modes.build.selection(),
            global.modes.plan.selection(),
        )?,
    };
    resolve_selections(global, session_id, selections)
}

pub(crate) fn resolve_selections(
    global: &Config,
    session_id: &str,
    selections: SessionModels,
) -> anyhow::Result<ResolvedSessionModels> {
    let mut assignments = global.modes.clone();
    for (mode, assignment) in [
        (SessionMode::Build, &mut assignments.build),
        (SessionMode::Plan, &mut assignments.plan),
    ] {
        let selection = selections.for_mode(mode);
        let profile = &selection.profile;
        let model = global
            .providers
            .get(&profile.provider)
            .and_then(|provider| provider.models.get(&profile.model));
        anyhow::ensure!(
            model.is_some(),
            "session {session_id:?} {mode:?} selection is unavailable: provider {:?}, model {:?}. Restore this exact case-sensitive provider/model catalog entry in models.jsonc or start a new session; saved selections are never replaced with defaults",
            profile.provider,
            profile.model
        );
        anyhow::ensure!(
            model
                .expect("checked model")
                .reasoning_levels
                .contains(&selection.reasoning_level),
            "session {session_id:?} {mode:?} saved reasoning level {} is unavailable for {profile}. Restore this supported level in models.jsonc or start a new session; no fallback is applied",
            selection.reasoning_level
        );
        *assignment = ModelAssignment {
            provider: profile.provider.clone(),
            model: profile.model.clone(),
            reasoning_level: selection.reasoning_level,
        };
    }
    let routing = ModelRouting::resolve(
        &global.providers,
        &assignments,
        global.session.compaction.auto_trigger_percent,
    )?;
    let compaction = CompactionPolicy::new(
        global.session.compaction.clone(),
        routing.context_policies(),
    )?;
    Ok(ResolvedSessionModels {
        selections,
        routing,
        compaction,
    })
}
