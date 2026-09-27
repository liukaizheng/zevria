//! Real-process strict restoration, replay distinction, leases and removed CLI.
//! The shared harness insists every stdout line is JSON-RPC.
use super::acp_skills::Rpc;
use serde_json::json;
use std::{
    path::Path,
    process::{Command, Output},
};
use zevria_foundation::ModelProfileRef;
use zevria_foundation::SessionMode;
use zevria_model::models::SessionModels;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;

fn removed_recovery(home: &Path, workspace: &Path, id: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zevria"))
        .args([
            "sessions",
            "recover-models",
            id,
            "--build",
            "test",
            "test-model",
            "--plan",
            "test",
            "test-model",
        ])
        .env("HOME", home)
        .env_remove("ZEVRIA_CONFIG")
        .current_dir(workspace)
        .output()
        .unwrap()
}

#[test]
fn real_acp_mode_only_selection_survives_immediate_close_load_and_resume() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // No provider is reachable: mode selection and restoration must not generate.
    super::write_test_config(home.path());
    let config_path = home.path().join(".zevria/config.toml");
    let config_before = std::fs::read(&config_path).unwrap();
    let mut rpc = Rpc::start(home.path(), workspace.path());
    let initialized = rpc.request(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    assert!(initialized.get("error").is_none(), "{initialized}");
    for (mode_id, selected_mode) in [("build", SessionMode::Build), ("plan", SessionMode::Plan)] {
        let created = rpc.request(
            "session/new",
            json!({"cwd":workspace.path(),"mcpServers":[]}),
        );
        assert!(created.get("error").is_none(), "{created}");
        assert_eq!(created["result"]["modes"]["currentModeId"], "build");
        assert_eq!(
            created["result"]["modes"]["availableModes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            !created["result"]["modes"]["availableModes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mode| mode["id"] == "orchestrate")
        );
        let id = created["result"]["sessionId"].as_str().unwrap().to_string();
        let path = transcript::sessions_dir(workspace.path()).join(format!("{id}.jsonl"));
        let selected = rpc.request("session/set_mode", json!({"sessionId":id,"modeId":mode_id}));
        assert!(selected.get("error").is_none(), "{selected}");
        let items = transcript::load(&path).unwrap();
        assert!(matches!(
            items.as_slice(),
            [
                TranscriptItem::SessionModels(_),
                TranscriptItem::SessionMode(mode)
            ] if *mode == selected_mode
        ));
        assert!(transcript::model_input(&items).is_empty());
        let persisted = std::fs::read(&path).unwrap();
        let closed = rpc.request("session/close", json!({"sessionId":id}));
        assert!(closed.get("error").is_none(), "{closed}");
        assert_eq!(std::fs::read(&path).unwrap(), persisted);
        assert!(!transcript::is_abandoned_root(&path));
        assert!(
            transcript::list_sessions(&transcript::sessions_dir(workspace.path()))
                .unwrap()
                .iter()
                .any(|summary| summary.id == id)
        );
        for method in ["session/load", "session/resume"] {
            let restored = rpc.request(
                method,
                json!({"sessionId":id,"cwd":workspace.path(),"mcpServers":[]}),
            );
            assert!(restored.get("error").is_none(), "{restored}");
            assert_eq!(restored["result"]["modes"]["currentModeId"], mode_id);
            let closed = rpc.request("session/close", json!({"sessionId":id}));
            assert!(closed.get("error").is_none(), "{closed}");
            assert_eq!(std::fs::read(&path).unwrap(), persisted);
        }
    }
    assert_eq!(std::fs::read(config_path).unwrap(), config_before);
}

#[test]
fn real_acp_crash_sidecars_are_reclaimed_on_next_startup_and_history_resumes() {
    for worker in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        super::write_test_config(home.path());
        let config = home.path().join(".zevria/config.toml");
        let directory = if worker {
            zevria_foundation::runtime_paths::workspace_state_root(workspace.path())
                .join("ensemble-sessions")
        } else {
            transcript::sessions_dir(workspace.path())
        };
        let mut writer = TranscriptWriter::create_with_id(&directory, "crashed").unwrap();
        writer
            .rewrite(&[
                TranscriptItem::SessionModels(
                    SessionModels::new(
                        zevria_model::models::ModelSelection::new(
                            ModelProfileRef::new("test", "test-model"),
                            zevria_foundation::ReasoningLevel::Medium,
                        ),
                        zevria_model::models::ModelSelection::new(
                            ModelProfileRef::new("test", "test-model"),
                            zevria_foundation::ReasoningLevel::Medium,
                        ),
                    )
                    .unwrap(),
                ),
                TranscriptItem::Message(rig_core::message::Message::user("saved before crash")),
                TranscriptItem::Message(rig_core::message::Message::assistant("durable reply")),
            ])
            .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        let original = std::fs::read(&path).unwrap();
        let sidecar = path.with_extension("jsonl.lock");
        let start = || {
            let mut rpc = if worker {
                Rpc::start_worker(home.path(), workspace.path(), &config)
            } else {
                Rpc::start(home.path(), workspace.path())
            };
            let initialized = rpc.request(
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":{}}),
            );
            assert!(initialized.get("error").is_none(), "{initialized}");
            rpc
        };
        let params = || json!({"sessionId":"crashed", "cwd":workspace.path(), "mcpServers":[]});
        let mut owner = start();
        let loaded = owner.request("session/load", params());
        assert!(loaded.get("error").is_none(), "{loaded}");
        assert!(sidecar.exists());
        let mut successor = start();
        let held = successor.request("session/load", params());
        assert!(
            held.get("error").is_some() && held.to_string().contains("held by another"),
            "{held}"
        );
        assert!(
            sidecar.exists(),
            "a failed contender cannot remove the owner"
        );
        // JSON-RPC responses above are IPC barriers proving ownership. Reaping
        // below proves process exit; no sleep is used to guess lock release.
        owner.kill_and_reap();
        assert!(
            sidecar.exists(),
            "crashes leave the on-disk identity behind"
        );
        let fresh = successor.request(
            "session/new",
            json!({"cwd":workspace.path(), "mcpServers":[]}),
        );
        assert!(fresh.get("error").is_none(), "{fresh}");
        assert!(
            !sidecar.exists(),
            "another session's startup sweeps the crashed owner"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let resumed = successor.request("session/resume", params());
        assert!(resumed.get("error").is_none(), "{resumed}");
        assert!(sidecar.exists());
        for id in [json!("crashed"), fresh["result"]["sessionId"].clone()] {
            let closed = successor.request("session/close", json!({"sessionId":id}));
            assert!(closed.get("error").is_none(), "{closed}");
            assert!(
                !directory
                    .join(format!("{}.jsonl.lock", id.as_str().unwrap()))
                    .exists()
            );
        }
        assert_eq!(
            directory
                .join(format!(
                    "{}.jsonl",
                    fresh["result"]["sessionId"].as_str().unwrap()
                ))
                .exists(),
            !worker,
            "ordinary root selection survives closing; workers retain legacy cleanup"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(directory.join(".leases.lock").is_file());
    }
}

#[test]
fn real_acp_load_replays_resume_does_not_and_unsupported_roots_fail() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    super::write_test_config(home.path());
    let config_path = home.path().join(".zevria/config.toml");
    let config_before = std::fs::read(&config_path).unwrap();
    let mut writer =
        TranscriptWriter::create_with_id(&transcript::sessions_dir(workspace.path()), "saved")
            .unwrap();
    let selections = SessionModels::new(
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("test", "test-model"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
        zevria_model::models::ModelSelection::new(
            ModelProfileRef::new("test", "test-model"),
            zevria_foundation::ReasoningLevel::Medium,
        ),
    )
    .unwrap();
    writer
        .rewrite(&[
            TranscriptItem::SessionModels(selections),
            TranscriptItem::Message(rig_core::message::Message::user("durable user")),
            TranscriptItem::Message(rig_core::message::Message::assistant("durable assistant")),
        ])
        .unwrap();
    let path = writer.path().to_path_buf();
    drop(writer);
    let original = std::fs::read(&path).unwrap();
    let mut rpc = Rpc::start(home.path(), workspace.path());
    assert!(
        rpc.request(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}})
        )
        .get("error")
        .is_none()
    );
    let params = || json!({"sessionId":"saved", "cwd":workspace.path(), "mcpServers":[]});
    let loaded = rpc.request("session/load", params());
    assert!(loaded.get("error").is_none(), "{loaded}");
    let replay = serde_json::to_string(&rpc.notifications).unwrap();
    assert!(replay.contains("durable user") && replay.contains("durable assistant"));
    assert!(
        !replay.contains("zevria_session_models")
            && !replay.contains("zevria_instruction_prefix")
            && !replay.contains("test-model")
    );
    let rejected = removed_recovery(home.path(), workspace.path(), "saved");
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unknown"));
    let mut competing = Rpc::start(home.path(), workspace.path());
    competing.request(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let held = competing.request("session/load", params());
    assert!(held.get("error").is_some(), "{held}");
    assert!(held.to_string().contains("held by another"));
    assert!(path.with_extension("jsonl.lock").exists());
    assert!(
        rpc.request("session/close", json!({"sessionId":"saved"}))
            .get("error")
            .is_none()
    );
    assert!(!path.with_extension("jsonl.lock").exists());
    rpc.notifications.clear();
    let resumed = rpc.request("session/resume", params());
    assert!(resumed.get("error").is_none(), "{resumed}");
    let replay = serde_json::to_string(&rpc.notifications).unwrap();
    assert!(!replay.contains("durable user") && !replay.contains("durable assistant"));
    assert!(
        rpc.request("session/close", json!({"sessionId":"saved"}))
            .get("error")
            .is_none()
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!path.with_extension("jsonl.lock").exists());

    let legacy =
        TranscriptWriter::create_with_id(&transcript::sessions_dir(workspace.path()), "legacy")
            .unwrap();
    let path = legacy.path().to_path_buf();
    drop(legacy);
    let original = b"{ \"error\" : \"legacy formatting, no final newline\" }";
    std::fs::write(&path, original).unwrap();
    for method in ["session/load", "session/resume"] {
        let failed = rpc.request(
            method,
            json!({"sessionId":"legacy", "cwd":workspace.path(), "mcpServers":[]}),
        );
        assert!(failed.get("error").is_some(), "{failed}");
        assert!(
            failed
                .to_string()
                .contains("resolve the reported history issue"),
            "{failed}"
        );
        assert!(!failed.to_string().contains("fresh session"), "{failed}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!path.with_extension("jsonl.lock").exists());
    }
    let rejected = removed_recovery(home.path(), workspace.path(), "legacy");
    assert!(!rejected.status.success());
    assert!(!path.with_extension("jsonl.pre-session-models-v1").exists());
    assert!(!path.with_extension("jsonl.pre-v3").exists());
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}
