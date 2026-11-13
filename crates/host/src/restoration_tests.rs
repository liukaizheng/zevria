//! One-time payload ownership does not consume the runtime's cleanup metadata.
use super::*;

#[tokio::test]
async fn restoration_moves_heavy_data_once_and_retains_cleanup_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("payload.jsonl");
    let name = "review".parse().unwrap();
    let snapshot =
        zevria_instructions::SkillSnapshot::new(name, "Review", "Pinned body".repeat(1024))
            .unwrap();
    let items = vec![TranscriptItem::SkillInvocation(
        zevria_instructions::SkillInvocation::new(
            snapshot.name().clone(),
            "root",
            zevria_instructions::SkillApplication::Activate(snapshot),
        ),
    )];
    let active_skills = zevria_transcript::replay_active_skills(&items).unwrap();
    let pin_allocation = active_skills.snapshots().next().unwrap().body().as_ptr();
    let mut running = RunningSession {
        command_jobs: None,
        cleanup_abandoned_history: true,
        restored_taken: false,
        restored_subtasks: vec![RestoredSubtask {
            id: zevria_foundation::SubtaskId::new("child"),
            launch: None,
            items: vec![TranscriptItem::Message(rig_core::message::Message::user(
                "child",
            ))],
        }],
        restored_agent_runs: Vec::new(),
        _lease: None,
        restoration: SessionRestoration {
            model_contexts: crate::config::test_config().routing().context_policies(),
            reasoning_levels: [zevria_foundation::ReasoningLevel::Medium; ModelRole::COUNT],
            model_snapshots: Vec::new(),
            sessions_dir: directory.path().to_path_buf(),
            session_id: "payload".into(),
            subsessions_dir: directory.path().join("subsessions"),
            agent_runs_root: directory.path().join("agent-runs"),
            transcript_path: path.clone(),
            transcript_items: items,
            skill_catalog: Arc::new(SkillCatalog::default()),
            active_skills,
            startup_notices: vec!["startup notice".into()],
            plan_state: PlanWorkflowState::Idle,
            selected_mode: SessionMode::Plan,
        },
        command_tx: None,
        event_tx: None,
        event_rx: None,
        exit_rx: None,
        engine_task: None,
        supervisor_task: None,
    };
    let restored = running.take_restored();
    assert_eq!(restored.transcript_items.len(), 1);
    assert_eq!(
        restored
            .active_skills
            .snapshots()
            .next()
            .unwrap()
            .body()
            .as_ptr(),
        pin_allocation
    );
    assert!(running.restoration().active_skills.is_empty());
    assert_eq!(restored.subtasks.len(), 1);
    assert_eq!(restored.subtasks[0].items.len(), 1);
    assert_eq!(restored.startup_notices, ["startup notice"]);
    assert_eq!(restored.selected_mode, SessionMode::Plan);
    assert!(running.restoration().transcript_items.is_empty());
    assert_eq!(running.restoration().transcript_path, path);
    assert_eq!(running.restoration().session_id, "payload");
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| running.take_restored())).is_err()
    );
    running.shutdown().await.unwrap();
}
