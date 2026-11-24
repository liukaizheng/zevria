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
    let items = vec![TranscriptItem::SkillInvocation(Box::new(
        zevria_instructions::SkillInvocation::new(
            snapshot.name().clone(),
            "root",
            zevria_instructions::SkillApplication::Activate(snapshot),
        ),
    ))];
    let active_skills = zevria_transcript::replay_active_skills(&items).unwrap();
    let pin_allocation = active_skills.snapshots().next().unwrap().body().as_ptr();
    let running = RunningSession {
        mcp: None,
        command_jobs: None,
        cleanup_abandoned_history: true,
        identity: SessionIdentity {
            session_id: "payload".into(),
            transcript_path: path.clone(),
        },
        _lease: None,
        command_tx: None,
        event_tx: None,
        event_rx: None,
        exit_rx: None,
        engine_task: None,
        supervisor: None,
    };
    let started = StartedSession {
        running,
        restored: RestoredSession {
            mcp_prompts: vec![],
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
            subtasks: vec![RestoredSubtask {
                id: zevria_foundation::SubtaskId::new("child"),
                launch: None,
                items: vec![TranscriptItem::Message(rig_core::message::Message::user(
                    "child",
                ))],
            }],
            agent_runs: Vec::new(),
        },
    };
    let StartedSession { running, restored } = started;
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
    assert_eq!(restored.subtasks.len(), 1);
    assert_eq!(restored.subtasks[0].items.len(), 1);
    assert_eq!(restored.startup_notices, ["startup notice"]);
    assert_eq!(restored.selected_mode, SessionMode::Plan);
    assert_eq!(running.identity().transcript_path, path);
    assert_eq!(running.identity().session_id, "payload");
    running.shutdown().await.unwrap();
}
