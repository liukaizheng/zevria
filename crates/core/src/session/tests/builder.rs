use super::*;
use crate::session::pipeline_probe;
use zevria_instructions::skill::{SkillManagementRequest, SkillManagementService};

struct UnusedSkillService;
impl SkillManagementService for UnusedSkillService {
    fn update<'a>(
        &'a self,
        _: SkillManagementRequest,
        installed: Arc<SkillCatalog>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Arc<SkillCatalog>>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(installed) })
    }
}

#[tokio::test]
async fn configured_opening_accounts_once_with_identical_final_policy_and_request_bytes() {
    let (directory, writer) = test_transcript();
    let (_legacy_directory, legacy_writer) = test_transcript();
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "Captured project guidance",
    )
    .unwrap();
    let guidance = zevria_instructions::load_guidance(
        &zevria_instructions::GuidanceRoots::fixture(None, directory.path()),
    );
    let items = vec![
        TranscriptItem::Message(Message::user("retained input")),
        TranscriptItem::Message(Message::assistant("retained answer")),
    ];
    let skills = test_skills();
    let compaction = test_compaction_policy(20_000, 80, 0);
    let legacy = SessionEngine::new(
        ScriptedProvider::new([]),
        test_skill_tools(),
        test_policies(),
        legacy_writer,
        skills.clone(),
    )
    .unwrap()
    .with_transcript_items(items.clone())
    .unwrap()
    .with_guidance_snapshot(guidance.clone())
    .with_skill_catalog(skills.clone())
    .unwrap()
    .with_mode_management()
    .with_idle_notifications()
    .with_subtask_concurrency(2)
    .with_skill_management(Arc::new(UnusedSkillService), [true, false])
    .unwrap()
    .with_compaction_policy(compaction.clone());
    let (engine, counts) = pipeline_probe::measure(async {
        SessionEngine::builder(
            ScriptedProvider::new([]),
            test_skill_tools(),
            test_policies(),
            writer,
            skills.clone(),
        )
        .unwrap()
        .with_transcript_items(items)
        .unwrap()
        .with_guidance_snapshot(guidance.clone())
        .with_skill_catalog(skills.clone())
        .unwrap()
        .with_mode_management()
        .with_idle_notifications()
        .with_subtask_concurrency(2)
        .with_skill_management(Arc::new(UnusedSkillService), [true, false])
        .unwrap()
        .with_compaction_policy(compaction.clone())
        .build()
        .unwrap()
    })
    .await;
    assert_eq!(counts.accounting_passes, 1);
    assert!(Arc::ptr_eq(&engine.skills.catalog, &skills));
    assert!(!engine.policies.policy(SessionMode::Plan).skills_enabled);
    assert!(engine.capabilities.mode_management);
    assert_eq!(engine.capabilities.subtask_concurrency, Some(2));
    for mode in SessionMode::ALL {
        let policy = engine.policies.policy(mode);
        let old_policy = legacy.policies.policy(mode);
        assert_eq!(
            engine.rendered_instructions(policy),
            legacy.rendered_instructions(old_policy)
        );
        assert_eq!(
            engine.request_plan(policy).tool_bytes,
            legacy.request_plan(old_policy).tool_bytes
        );
    }
    assert_eq!(
        engine.restored_model_contexts().unwrap(),
        legacy.restored_model_contexts().unwrap()
    );
    let (_, live) = pipeline_probe::measure(async {
        engine
            .with_guidance_snapshot(guidance)
            .with_compaction_policy(compaction)
    })
    .await;
    assert_eq!(
        live.accounting_passes, 2,
        "already-built setters still invalidate live accounting"
    );
}

#[tokio::test]
async fn loaded_builder_reuses_validation_and_accounts_only_after_configuration() {
    let (_directory, mut writer) = test_transcript();
    writer
        .rewrite(&[
            TranscriptItem::SessionMode(SessionMode::Plan),
            TranscriptItem::Message(Message::user("saved input")),
        ])
        .unwrap();
    let loaded = zevria_transcript::load_report(writer.path()).unwrap();
    let ((engine, counts), replay) =
        zevria_transcript::replay_probe::measure_instruction_replay_async(pipeline_probe::measure(
            async {
                SessionEngine::builder_from_loaded(
                    ScriptedProvider::new([]),
                    test_skill_tools(),
                    test_policies(),
                    writer,
                    test_skills(),
                    loaded,
                )
                .unwrap()
                .with_compaction_policy(test_compaction_policy(20_000, 80, 0))
                .build()
                .unwrap()
            },
        ))
        .await;
    assert_eq!(counts.accounting_passes, 1);
    assert_eq!(replay.loads, 0);
    assert_eq!(replay.full_replays, 0);
    assert_eq!(replay.validations, 0);
    assert_eq!(engine.selected_mode(), SessionMode::Plan);
    assert!(
        !engine.capabilities.mode_management,
        "builders do not implicitly grant root capabilities"
    );
}
