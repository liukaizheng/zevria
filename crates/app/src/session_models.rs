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
    inherited: Option<&SessionModels>,
) -> anyhow::Result<ResolvedSessionModels> {
    let (selections, origin) = match resumed {
        Some(outcome) => {
            outcome.ensure_resumable()?;
            let saved = outcome.session_models()?.ok_or_else(|| zevria_transcript::transcript::UnsupportedHistory::new(
                &outcome.path, Some(1), "version 1 Build/Plan model metadata with explicit reasoning levels; launch an independent new session or explicitly repair the saved metadata",
            ))?.clone();
            (saved, "saved")
        }
        None => match inherited {
            Some(selections) => (selections.clone(), "inherited"),
            None => (
                SessionModels::new(
                    global.modes.build.selection(),
                    global.modes.plan.selection(),
                )?,
                "configured",
            ),
        },
    };
    resolve_selections(global, session_id, selections, origin)
}

pub(crate) fn resolve_selections(
    global: &Config,
    session_id: &str,
    selections: SessionModels,
    origin: &str,
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
            "session {session_id:?} {mode:?} {origin} selection is unavailable: provider {:?}, model {:?}. Restore this exact case-sensitive provider/model catalog entry in models.jsonc or launch an independent new session with valid defaults (not an inheriting /new); no fallback is applied",
            profile.provider,
            profile.model
        );
        anyhow::ensure!(
            model
                .expect("checked model")
                .reasoning_levels
                .contains(&selection.reasoning_level),
            "session {session_id:?} {mode:?} {origin} reasoning level {} is unavailable for {profile}. Restore this supported level in models.jsonc or launch an independent new session with valid defaults (not an inheriting /new); no fallback is applied",
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

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::{ModelProfileRef, ModelRole, ReasoningLevel as Level};
    use zevria_model::models::ModelSelection;
    use zevria_transcript::transcript::{self, TranscriptItem, TranscriptWriter};

    #[test]
    fn fresh_inheritance_and_resume_use_complete_pairs_with_current_policies() {
        let mut config = crate::config::test_config();
        let provider = config.providers.get_mut("test").unwrap();
        let mut alternate = provider.models["test-model"].clone();
        alternate.context_window_tokens = 50000;
        alternate.input_token_limit = Some(40000);
        alternate.retained_user_tokens = 1000;
        provider.models.insert("alternate".into(), alternate);
        let inherited = SessionModels::new(
            ModelSelection::new(ModelProfileRef::new("test", "alternate"), Level::High),
            ModelSelection::new(ModelProfileRef::new("test", "test-model"), Level::Low),
        )
        .unwrap();
        let resolved = resolve(&config, "inherited-root", None, Some(&inherited)).unwrap();
        assert_eq!(resolved.selections, inherited);
        for (mode, role) in [
            (SessionMode::Build, ModelRole::Build),
            (SessionMode::Plan, ModelRole::Plan),
        ] {
            assert_eq!(
                &resolved.routing.for_role(role).profile,
                &inherited.for_mode(mode).profile
            );
            assert_eq!(
                resolved.routing.selection_for_role(role).reasoning_level,
                inherited.reasoning_for_mode(mode)
            );
            assert_eq!(
                resolved.compaction.for_role(role),
                &resolved.routing.for_role(role).context_policy()
            );
        }
        assert_eq!(
            resolved
                .compaction
                .for_role(ModelRole::Build)
                .input_token_limit,
            40000
        );
        assert_eq!(
            resolved
                .compaction
                .for_role(ModelRole::Build)
                .context_window_tokens,
            50000
        );
        let defaults = resolve(&config, "independent-root", None, None).unwrap();
        assert_eq!(
            defaults.selections.for_mode(SessionMode::Build),
            &config.modes.build.selection()
        );
        assert_eq!(
            defaults.selections.for_mode(SessionMode::Plan),
            &config.modes.plan.selection()
        );
        for role in [ModelRole::Review, ModelRole::Explore, ModelRole::Builder] {
            assert_eq!(
                resolved.routing.for_role(role).context_policy(),
                defaults.routing.for_role(role).context_policy()
            );
            assert_eq!(
                resolved.routing.selection_for_role(role).reasoning_level,
                defaults.routing.selection_for_role(role).reasoning_level
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let mut writer = TranscriptWriter::create(directory.path()).unwrap();
        writer
            .rewrite(&[
                TranscriptItem::SessionModels(inherited.clone()),
                TranscriptItem::SessionMode(SessionMode::Build),
            ])
            .unwrap();
        let loaded = transcript::load_report(writer.path()).unwrap();
        // Target transcript wins even if a caller supplied unrelated preferences.
        let resumed = resolve(
            &config,
            "resumed-root",
            Some(&loaded),
            Some(&defaults.selections),
        )
        .unwrap();
        assert_eq!(resumed.selections, inherited);
    }

    #[test]
    fn unavailable_inherited_profiles_and_levels_have_no_default_fallback() {
        let config = crate::config::test_config();
        let defaults = resolve(&config, "root", None, None).unwrap().selections;
        for target in [
            ModelSelection::new(ModelProfileRef::new("test", "removed"), Level::High),
            ModelSelection::new(ModelProfileRef::new("test", "test-model"), Level::Max),
        ] {
            for mode in SessionMode::ALL {
                let selections = defaults.with_selection(mode, target.clone()).unwrap();
                let error = match resolve(&config, "replacement-root", None, Some(&selections)) {
                    Ok(_) => panic!("inherited selection silently fell back"),
                    Err(error) => error.to_string(),
                };
                assert!(
                    error.contains("inherited") && error.contains(&format!("{mode:?}")),
                    "{error}"
                );
                assert!(
                    error.contains("independent new session")
                        && error.contains("not an inheriting /new"),
                    "{error}"
                );
                assert!(error.contains("no fallback"), "{error}");
            }
        }
    }
}
