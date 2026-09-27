//! Actual root/worker processes with isolated HOME and session workspaces.
#![cfg(unix)]
use super::{acp_skills::Rpc, acp_worker::ResponsesFixture};
use serde_json::{Value, json};
use std::{io::Write as _, path::Path};
use zevria_transcript::transcript;

fn success(response: Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}
fn initialize(rpc: &mut Rpc) {
    success(rpc.request(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    ));
}
fn open(rpc: &mut Rpc, workspace: &Path) -> Value {
    success(rpc.request("session/new", json!({"cwd":workspace,"mcpServers":[]})))["sessionId"]
        .clone()
}
fn prompt(rpc: &mut Rpc, id: &Value) {
    success(rpc.request("session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"Inspect the fixture without tools."}]})));
}
fn close(rpc: &mut Rpc, id: &Value) {
    success(rpc.request("session/close", json!({"sessionId":id})));
}
fn directives(path: &Path) -> Vec<zevria_instructions::DirectiveContent> {
    zevria_transcript::replay_directives(&transcript::load(path).unwrap())
        .unwrap()
        .snapshot()
        .directives
}

#[test]
fn actual_guidance_snapshots_isolate_acp_workspaces_workers_and_resume_clearing_notices() {
    let fixture = ResponsesFixture::start();
    let home = tempfile::tempdir().unwrap();
    let startup = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    super::write_test_config(home.path());
    let config = home.path().join(".zevria/config.toml");
    let models_path = zevria_foundation::config::models_path_for(&config);
    let model_source = std::fs::read_to_string(&models_path)
        .unwrap()
        .replace("http://127.0.0.1:1/v1/responses", &fixture.url)
        .replace("test-model", "guidance-fixture");
    let mut models: Value = serde_json::from_str(&model_source).unwrap();
    models["providers"]["test"]["input_token_count"] = json!({"enabled": false});
    models["providers"]["test"]["models"]["explore-guidance-fixture"] =
        models["providers"]["test"]["models"]["guidance-fixture"].clone();
    let mut assignments: toml::Table = toml::from_str(
        &std::fs::read_to_string(&config)
            .unwrap()
            .replace("test-model", "guidance-fixture"),
    )
    .unwrap();
    assignments["modes"]["explore"]["model"] = "explore-guidance-fixture".into();
    let model_source = serde_json::to_string_pretty(&models).unwrap();
    std::fs::write(&models_path, &model_source).unwrap();
    let source = format!(
        "{}\n[session]\npreamble = 'CUSTOM_PREAMBLE_UNCHANGED'\n",
        toml::to_string(&assignments).unwrap()
    );
    std::fs::write(&config, &source).unwrap();
    let global = home.path().join(".zevria/AGENTS.md");
    std::fs::write(&global, "GLOBAL_OLD_MARKER").unwrap();
    std::fs::write(
        startup.path().join("AGENTS.md"),
        "PROCESS_CWD_MUST_NOT_LEAK",
    )
    .unwrap();
    std::fs::write(a.path().join("AGENTS.md"), "PROJECT_A_OLD_MARKER").unwrap();
    std::fs::write(b.path().join("AGENTS.md"), "PROJECT_B_MARKER").unwrap();
    let mut rpc = Rpc::start(home.path(), startup.path());
    initialize(&mut rpc);
    let a_id = open(&mut rpc, a.path());
    let b_id = open(&mut rpc, b.path());
    // Edits after opening must affect neither of the existing ACP roots.
    std::fs::write(&global, "GLOBAL_NEW_MARKER").unwrap();
    std::fs::write(a.path().join("AGENTS.md"), "PROJECT_A_NEW_MARKER").unwrap();
    prompt(&mut rpc, &a_id);
    prompt(&mut rpc, &b_id);
    prompt(&mut rpc, &a_id);
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (index, expected) in [
            (0, "PROJECT_A_OLD_MARKER"),
            (1, "PROJECT_B_MARKER"),
            (2, "PROJECT_A_OLD_MARKER"),
        ] {
            let input = requests[index]["instructions"].to_string();
            assert!(input.contains(expected));
            assert!(input.contains("GLOBAL_OLD_MARKER"));
            assert!(!input.contains("GLOBAL_NEW_MARKER"));
            assert!(!input.contains("PROCESS_CWD_MUST_NOT_LEAK"));
            assert!(input.contains("CUSTOM_PREAMBLE_UNCHANGED"));
            assert!(!requests[index]["input"].to_string().contains("_MARKER"));
        }
        assert!(
            !requests[1]["instructions"]
                .to_string()
                .contains("PROJECT_A_")
        );
        assert_eq!(requests[0]["instructions"], requests[2]["instructions"]);
    }
    assert!(
        !serde_json::to_string(&rpc.notifications)
            .unwrap()
            .contains("_MARKER")
    );
    success(rpc.request(
        "session/prompt",
        json!({"sessionId":a_id,"prompt":[{"type":"text","text":"LATE_EXPLORE_REQUEST"}]}),
    ));
    {
        let requests = fixture.requests.lock().unwrap();
        let child = requests
            .iter()
            .find(|r| r["model"] == "explore-guidance-fixture")
            .expect("late child actually generated");
        let input = child["instructions"].as_str().unwrap();
        assert!(input.contains("GLOBAL_OLD_MARKER") && input.contains("PROJECT_A_OLD_MARKER"));
        assert!(!input.contains("NEW_MARKER"));
        assert!(input.contains("\"skills\":false"));
        assert_eq!(child["tools"].as_array().unwrap().len(), 1);
        assert_eq!(child["tools"][0]["name"], "command");
    }
    assert!(
        !serde_json::to_string(&rpc.notifications)
            .unwrap()
            .contains("Skipped")
    );

    // Config relocation does not relocate global guidance; a separately opened
    // native worker sees the new files but retains its registered restrictions.
    let alternate = home.path().join("alternate");
    std::fs::create_dir(&alternate).unwrap();
    std::fs::write(alternate.join("config.toml"), source).unwrap();
    std::fs::write(alternate.join("models.jsonc"), model_source).unwrap();
    std::fs::write(
        alternate.join("AGENTS.md"),
        "ALTERNATE_CONFIG_MUST_NOT_LEAK",
    )
    .unwrap();
    let mut worker = Rpc::start_worker(home.path(), startup.path(), &alternate.join("config.toml"));
    initialize(&mut worker);
    let worker_id = open(&mut worker, a.path());
    prompt(&mut worker, &worker_id);
    {
        let requests = fixture.requests.lock().unwrap();
        let request = requests.last().unwrap();
        let input = request["instructions"].as_str().unwrap();
        assert!(input.contains("GLOBAL_NEW_MARKER") && input.contains("PROJECT_A_NEW_MARKER"));
        assert!(input.contains("\"skills\":false"));
        assert!(!input.contains("ALTERNATE_CONFIG_MUST_NOT_LEAK"));
        let tools = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(tools, ["command", "question"]);
    }
    close(&mut worker, &worker_id);

    let path = transcript::sessions_dir(a.path()).join(format!("{}.jsonl", a_id.as_str().unwrap()));
    close(&mut rpc, &a_id);
    let original = std::fs::read(&path).unwrap();
    let params = || json!({"sessionId":a_id,"cwd":a.path(),"mcpServers":[]});
    success(rpc.request("session/resume", params()));
    assert!(std::fs::read(&path).unwrap().starts_with(&original));
    assert!(directives(&path).is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    prompt(&mut rpc, &a_id);
    {
        let requests = fixture.requests.lock().unwrap();
        let input = requests.last().unwrap()["instructions"].to_string();
        assert!(input.contains("GLOBAL_NEW_MARKER"));
        assert!(input.contains("PROJECT_A_NEW_MARKER"));
        assert!(!input.contains("GLOBAL_OLD_MARKER"));
        assert!(!input.contains("PROJECT_A_OLD_MARKER"));
    }
    close(&mut rpc, &a_id);
    let before_clear = std::fs::read(&path).unwrap();
    // A recoverable torn final record must retain its existing startup notice
    // alongside both skipped-file warnings, without restoring obsolete guidance.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"role\":\"user\",\"cont")
        .unwrap();
    std::fs::write(
        &global,
        vec![b'x'; zevria_instructions::MAX_GUIDANCE_BYTES + 1],
    )
    .unwrap();
    std::fs::write(a.path().join("AGENTS.md"), [0xff]).unwrap();
    rpc.notifications.clear();
    success(rpc.request("session/load", params()));
    prompt(&mut rpc, &a_id);
    let notices = serde_json::to_string(&rpc.notifications).unwrap();
    assert!(
        notices.contains("Recovered the session transcript"),
        "{notices}"
    );
    assert!(notices.contains("Skipped global AGENTS.md guidance"));
    assert!(notices.contains("Skipped project AGENTS.md guidance"));
    assert!(
        !notices.contains("_MARKER"),
        "file bodies stay hidden on load"
    );
    assert!(directives(&path).is_empty());
    {
        let requests = fixture.requests.lock().unwrap();
        let input = requests.last().unwrap()["instructions"].to_string();
        assert!(!input.contains("## File guidance"));
    }
    assert!(std::fs::read(&path).unwrap().starts_with(&before_clear));
    let items = transcript::load(&path).unwrap();
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, transcript::TranscriptItem::Directive(_)))
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("_MARKER"));
    assert!(!text.contains("zevria_instruction_prefix"));
    assert!(!text.contains("zevria_directive"));
    // Workers publish their own failure notices; they do not inherit stale bodies.
    worker.notifications.clear();
    let bad_worker = open(&mut worker, a.path());
    prompt(&mut worker, &bad_worker);
    let notices = serde_json::to_string(&worker.notifications).unwrap();
    assert!(notices.contains("Skipped global AGENTS.md guidance"));
    assert!(notices.contains("Skipped project AGENTS.md guidance"));
    close(&mut worker, &bad_worker);
    close(&mut rpc, &a_id);
    close(&mut rpc, &b_id);
}
