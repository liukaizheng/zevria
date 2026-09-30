//! Shared terminal/ACP/native-worker startup must fail before mutation or work.
use super::*;
use crate::runtime;
use zevria_acp::{ExecutionProfile, SessionRuntimeFactory as _, SessionStart, StartSessionRequest};
use zevria_transcript::AgentRunTranscriptHeader;
use zevria_transcript::AgentRunTranscriptWriter;
use zevria_transcript::agent_run_path;
use zevria_transcript::agent_runs_dir;
use zevria_workflow::AgentRunDescriptor;
use zevria_workflow::AgentRunId;
use zevria_workflow::EnsembleRecord;
use zevria_workflow::EnsembleRunId;
use zevria_workflow::EnsembleStart;
use zevria_workflow::EnsembleWorkflow;

fn saved_root(fixture: &Fixture, id: &str, extra: Vec<TranscriptItem>) -> PathBuf {
    let mut writer =
        TranscriptWriter::create_with_id(&transcript::sessions_dir(fixture.directory.path()), id)
            .unwrap();
    let mut items = vec![TranscriptItem::SessionModels(
        zevria_model::models::SessionModels::new(
            fixture.config.modes().build.selection(),
            fixture.config.modes().plan.selection(),
        )
        .unwrap(),
    )];
    items.extend(extra);
    writer.rewrite(&items).unwrap();
    writer.path().to_path_buf()
}

fn start_record() -> EnsembleStart {
    EnsembleStart {
        run_id: EnsembleRunId::new(),
        workflow: EnsembleWorkflow::Review,
        prompt: "review".into(),
        agents: (0..3)
            .map(|index| AgentRunDescriptor {
                id: AgentRunId::new(),
                agent: "zevria".into(),
                label: format!("Worker {index}"),
                safe_mode: "review".into(),
            })
            .collect(),
    }
}

async fn assert_rejected(fixture: &Fixture, root: &Path, bad: &Path) {
    let root_before = std::fs::read(root).unwrap();
    let bad_before = std::fs::read(bad).unwrap();
    let config_before = std::fs::read(&fixture.path).unwrap();
    let error = match runtime::start_session(
        &fixture.config,
        fixture.directory.path(),
        runtime::SessionStart::Resume(root.to_path_buf()),
    )
    .await
    {
        Ok(running) => {
            running.shutdown().await.unwrap();
            panic!("terminal runtime published an unsupported session");
        }
        Err(error) => error,
    };
    assert!(
        error
            .downcast_ref::<transcript::UnsupportedHistory>()
            .is_some(),
        "{error:#}"
    );
    let factory = crate::acp_host::AcpHostFactory::new(Arc::new(load(&fixture.path).unwrap()));
    let error = match factory
        .start(StartSessionRequest {
            workspace: fixture.directory.path().to_path_buf(),
            start: SessionStart::Existing {
                session_id: root.file_stem().unwrap().to_str().unwrap().into(),
            },
        })
        .await
    {
        Ok(started) => {
            started.lifecycle.shutdown().await.unwrap();
            panic!("ACP published an unsupported session");
        }
        Err(error) => error,
    };
    assert!(
        error
            .downcast_ref::<transcript::UnsupportedHistory>()
            .is_some(),
        "{error:#}"
    );
    for (path, expected) in [(root, root_before), (bad, bad_before)] {
        assert_eq!(std::fs::read(path).unwrap(), expected);
        assert!(!path.with_extension("jsonl.pre-v3").exists());
        assert!(!path.with_extension("jsonl.pre-session-models-v1").exists());
    }
    assert_eq!(std::fs::read(&fixture.path).unwrap(), config_before);
    // Every rejected startup releases ownership and cleans its sidecar.
    assert!(!root.with_extension("jsonl.lock").exists());
    zevria_app::test_support::RootSessionLease::acquire(root).unwrap();
}

#[tokio::test]
async fn unsupported_child_prevents_root_repair_and_acp_publication() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let root = saved_root(
        &fixture,
        "children",
        vec![TranscriptItem::Message(rig_core::message::Message::user(
            "root history",
        ))],
    );
    let mut original = std::fs::read(&root).unwrap();
    original.extend_from_slice(b"{\"partial\":");
    std::fs::write(&root, &original).unwrap();
    let children = transcript::subsessions_dir(fixture.directory.path(), "children");
    let mut child = TranscriptWriter::create_with_id(&children, "valid-child").unwrap();
    child
        .append(&TranscriptItem::Message(rig_core::message::Message::user(
            "metadata-free child",
        )))
        .unwrap();
    let bad = children.join("unsupported-child.jsonl");
    std::fs::write(&bad, b"{\"zevria_skill_activation_v2\":{}}\n{\"partial\":").unwrap();
    assert_rejected(&fixture, &root, &bad).await;
    assert_eq!(std::fs::read(&root).unwrap(), original);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn all_referenced_workers_are_preflighted_before_any_session_or_launch() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let start = start_record();
    let root = saved_root(
        &fixture,
        "workers",
        vec![TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        })],
    );
    let logs = agent_runs_dir(fixture.directory.path(), "workers");
    let mut paths = Vec::new();
    for descriptor in &start.agents[..2] {
        let path = agent_run_path(&logs, &start.run_id, &descriptor.id);
        drop(
            AgentRunTranscriptWriter::create(
                path.clone(),
                AgentRunTranscriptHeader {
                    version: zevria_transcript::AGENT_RUN_TRANSCRIPT_VERSION,
                    ensemble_run_id: start.run_id.clone(),
                    workflow: start.workflow,
                    descriptor: descriptor.clone(),
                    prompt: start.prompt.clone(),
                },
            )
            .unwrap(),
        );
        paths.push(path);
    }
    let valid_before = std::fs::read(&paths[0]).unwrap();
    let bad = &paths[1];
    let mut original = std::fs::read(bad).unwrap();
    original.extend_from_slice(b"{\"record\":\"event\",\"event\":{\"type\":\"elicitation\",\"field_count\":1,\"outcome\":\"accepted\"}}\n{\"partial\":");
    std::fs::write(bad, &original).unwrap();
    assert_rejected(&fixture, &root, bad).await;
    assert_eq!(std::fs::read(&paths[0]).unwrap(), valid_before);
    assert!(!agent_run_path(&logs, &start.run_id, &start.agents[2].id).exists());
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn never_created_worker_logs_and_model_header_free_children_remain_valid() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let start = start_record();
    let root = saved_root(
        &fixture,
        "interrupted",
        vec![TranscriptItem::Ensemble(EnsembleRecord::Started {
            start: start.clone(),
        })],
    );
    let child_dir = transcript::subsessions_dir(fixture.directory.path(), "interrupted");
    let mut child = TranscriptWriter::create_with_id(&child_dir, "child").unwrap();
    child
        .append(&TranscriptItem::Message(rig_core::message::Message::user(
            "ordinary child",
        )))
        .unwrap();
    let before = std::fs::read(&root).unwrap();
    let logs = agent_runs_dir(fixture.directory.path(), "interrupted");
    crate::ensemble::preflight_existing_logs(&logs, &start).unwrap();
    assert!(!logs.join(start.run_id.as_str()).exists());
    let running = fixture
        .start(runtime::SessionStart::Resume(root.clone()))
        .await;
    running.shutdown().await.unwrap();
    assert!(
        std::fs::read(&root).unwrap().starts_with(&before),
        "current recovery may append a terminal cancellation, never rewrite the saved prefix"
    );
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn root_and_worker_startup_sweep_only_their_namespace_and_preserve_discovery() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let workspace = std::fs::canonicalize(fixture.directory.path()).unwrap();
    let roots = transcript::sessions_dir(&workspace);
    let workers = crate::runtime::sessions_dir(&workspace, ExecutionProfile::EnsembleWorker);
    let models = zevria_model::models::SessionModels::new(
        fixture.config.modes().build.selection(),
        fixture.config.modes().plan.selection(),
    )
    .unwrap();
    let mut originals = Vec::new();
    for directory in [&roots, &workers] {
        for (index, id) in ["older", "shared"].into_iter().enumerate() {
            let mut writer = TranscriptWriter::create_with_id(directory, id).unwrap();
            writer
                .rewrite(&[
                    TranscriptItem::SessionModels(models.clone()),
                    TranscriptItem::Message(rig_core::message::Message::user(format!(
                        "saved {id}"
                    ))),
                ])
                .unwrap();
            let path = writer.path().to_path_buf();
            drop(writer);
            // Deterministic discovery ordering without mtime sleeps.
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(100 + index as u64),
                ))
                .unwrap();
            originals.push((path.clone(), std::fs::read(&path).unwrap()));
            std::fs::write(path.with_extension("jsonl.lock"), []).unwrap();
        }
        std::fs::write(directory.join("orphan.jsonl.lock"), []).unwrap();
    }
    std::fs::write(workers.join(".gitignore"), "user worker guard\n").unwrap();
    std::fs::write(roots.join(".gitignore"), "user root guard\n").unwrap();
    let root_guard = std::fs::read(roots.join(".gitignore")).unwrap();
    let roots_before = transcript::list_sessions(&roots).unwrap();
    let workers_before = transcript::list_sessions(&workers).unwrap();
    assert_eq!(
        roots_before
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["shared", "older"]
    );
    let root = fixture
        .start(runtime::SessionStart::Resume(roots.join("shared.jsonl")))
        .await;
    for name in ["older.jsonl.lock", "orphan.jsonl.lock"] {
        assert!(!roots.join(name).exists());
        assert!(
            workers.join(name).exists(),
            "root startup must not sweep workers"
        );
    }
    // Seed a new root leftover to prove worker startup is independently scoped.
    std::fs::write(roots.join("root-only.jsonl.lock"), []).unwrap();
    let worker = runtime::start_session_with_profile(
        &fixture.config,
        &workspace,
        runtime::SessionStart::Resume(workers.join("shared.jsonl")),
        ExecutionProfile::EnsembleWorker,
    )
    .await
    .unwrap();
    for directory in [&roots, &workers] {
        assert!(
            zevria_app::test_support::RootSessionLease::acquire(&directory.join("shared.jsonl"))
                .is_err()
        );
    }
    assert!(roots.join("root-only.jsonl.lock").exists());
    for name in ["older.jsonl.lock", "orphan.jsonl.lock"] {
        assert!(!workers.join(name).exists());
    }
    root.shutdown().await.unwrap();
    assert!(!roots.join("shared.jsonl.lock").exists());
    assert!(workers.join("shared.jsonl.lock").exists());
    worker.shutdown().await.unwrap();
    assert!(!workers.join("shared.jsonl.lock").exists());
    for (path, original) in originals {
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
    assert_eq!(transcript::list_sessions(&roots).unwrap(), roots_before);
    assert_eq!(transcript::list_sessions(&workers).unwrap(), workers_before);
    assert_eq!(std::fs::read(roots.join(".gitignore")).unwrap(), root_guard);
    assert_eq!(
        std::fs::read(workers.join(".gitignore")).unwrap(),
        b"user worker guard\n"
    );
    assert!(roots.join(".leases.lock").is_file());
    assert!(workers.join(".leases.lock").is_file());
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn fresh_root_shutdown_preserves_selected_build_metadata_and_removes_sidecar() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let root = fixture
        .start(runtime::SessionStart::New {
            inherited_models: None,
        })
        .await;
    let path = root.restoration().transcript_path.clone();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(root.restoration().selected_mode, SessionMode::Build);
    assert!(matches!(
        root.restoration().transcript_items.as_slice(),
        [
            TranscriptItem::SessionModels(_),
            TranscriptItem::SessionMode(SessionMode::Build)
        ]
    ));
    assert!(!transcript::is_abandoned_root(&path));
    assert!(path.with_extension("jsonl.lock").exists());
    root.shutdown().await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("jsonl.lock").exists());
    assert!(path.parent().unwrap().join(".leases.lock").is_file());
    assert!(
        transcript::list_sessions(path.parent().unwrap())
            .unwrap()
            .iter()
            .any(|session| session.path == path)
    );
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn models_only_root_shutdown_removes_transcript_and_sidecar() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let path = saved_root(&fixture, "legacy-metadata-only", vec![]);
    assert!(matches!(
        transcript::load(&path).unwrap().as_slice(),
        [TranscriptItem::SessionModels(_)]
    ));
    assert!(transcript::is_abandoned_root(&path));
    assert!(
        transcript::list_sessions(path.parent().unwrap())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        transcript::latest_session_file(path.parent().unwrap()).unwrap(),
        None
    );
    let root = fixture
        .start(runtime::SessionStart::Resume(path.clone()))
        .await;
    assert_eq!(root.restoration().selected_mode, SessionMode::Build);
    assert!(path.with_extension("jsonl.lock").exists());
    root.shutdown().await.unwrap();
    assert!(!path.exists());
    assert!(!path.with_extension("jsonl.lock").exists());
    assert!(path.parent().unwrap().join(".leases.lock").is_file());
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn native_workers_reject_plan_handoffs_before_startup_work() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let workspace = fixture.directory.path();
    assert!(!workspace.join(".zevria").exists());
    let error = match runtime::start_session_with_profile(
        &fixture.config,
        workspace,
        runtime::SessionStart::FromPlan {
            handoff: plan_handoff("source-session"),
            inherited_models: None,
        },
        ExecutionProfile::EnsembleWorker,
    )
    .await
    {
        Ok(running) => {
            running.shutdown().await.unwrap();
            panic!("worker startup accepted a Plan handoff");
        }
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "ensemble workers cannot implement a Plan handoff"
    );
    assert!(
        !workspace.join(".zevria").exists(),
        "rejection must precede session storage, ignore guards, and leases"
    );
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn native_workers_require_saved_models_and_reject_old_formats() {
    let mut server = Server::new().await;
    let fixture = Fixture::new(&server.url);
    let workspace = std::fs::canonicalize(fixture.directory.path()).unwrap();
    let directory = crate::runtime::sessions_dir(&workspace, ExecutionProfile::EnsembleWorker);
    let path = TranscriptWriter::create_with_id(&directory, "native")
        .unwrap()
        .path()
        .to_path_buf();
    for original in [
        b"{\"error\":\"missing models\"}\n".as_slice(),
        b"{\"zevria_skill_activation_v2\":{}}\n",
    ] {
        std::fs::write(&path, original).unwrap();
        let result = runtime::start_session_with_profile(
            &fixture.config,
            &workspace,
            runtime::SessionStart::Resume(path.clone()),
            ExecutionProfile::EnsembleWorker,
        )
        .await;
        assert!(result.is_err());
        let factory = crate::acp_host::AcpHostFactory::with_profile(
            Arc::new(load(&fixture.path).unwrap()),
            ExecutionProfile::EnsembleWorker,
        );
        assert!(
            factory
                .start(StartSessionRequest {
                    workspace: workspace.clone(),
                    start: SessionStart::Existing {
                        session_id: "native".into()
                    }
                })
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!path.with_extension("jsonl.lock").exists());
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn root_and_native_worker_resume_refresh_current_application_including_clear() {
    for profile in [
        ExecutionProfile::Interactive,
        ExecutionProfile::EnsembleWorker,
    ] {
        let mut server = Server::new().await;
        let fixture = Fixture::new(&server.url);
        let workspace = std::fs::canonicalize(fixture.directory.path()).unwrap();
        let directory = runtime::sessions_dir(&workspace, profile);
        let mut writer = TranscriptWriter::create_with_id(&directory, "prompt-refresh").unwrap();
        let items = vec![
            TranscriptItem::SessionModels(
                zevria_model::models::SessionModels::new(
                    fixture.config.modes().build.selection(),
                    fixture.config.modes().plan.selection(),
                )
                .unwrap(),
            ),
            TranscriptItem::Message(rig_core::message::Message::user("previous turn")),
            TranscriptItem::Message(rig_core::message::Message::assistant("completed")),
        ];
        writer.rewrite(&items).unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        for current in ["new application from disk", "new application from disk", ""] {
            let mut document = std::fs::read_to_string(&fixture.path)
                .unwrap()
                .parse::<toml_edit::DocumentMut>()
                .unwrap();
            document["session"]["preamble"] = toml_edit::value(current);
            std::fs::write(&fixture.path, document.to_string()).unwrap();
            let before = transcript::load(&path).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            // Deliberately retain the stale startup Config, as both frontends do.
            let running = runtime::start_session_with_profile(
                &fixture.config,
                &workspace,
                runtime::SessionStart::Resume(path.clone()),
                profile,
            )
            .await
            .unwrap();
            let restored = &running.restoration().transcript_items;
            assert_eq!(restored, &before);
            let state = zevria_transcript::replay_directives(restored).unwrap();
            assert!(
                state.snapshot().directives.is_empty(),
                "application guidance does not create ordered skill directives"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(!String::from_utf8_lossy(&bytes).contains("old saved application"));
            running.shutdown().await.unwrap();
            assert!(
                server.requests.try_recv().is_err(),
                "restoration never calls a model"
            );
        }
    }
}
