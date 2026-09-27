//! The real CLI/ACP composition uses the same core disclosure as ordinary TUI
//! turns. All provider I/O is scripted on loopback; no Git actions are executed.
use super::{acp_skills::Rpc, acp_worker::ResponsesFixture};
use serde_json::{Value, json};
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;

fn success(response: Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}
fn prompt(rpc: &mut Rpc, session: &Value, text: &str) {
    success(rpc.request(
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
    ));
}
fn view(rpc: &mut Rpc, session: &Value) -> Value {
    success(rpc.request(
        "_zevria/skills/list",
        json!({"version":1,"sessionId":session}),
    ))["result"]["view"]
        .clone()
}
fn effective_catalog(request: &Value) -> Value {
    let instructions = request["instructions"].as_str().unwrap();
    assert!(instructions.contains("## Eligible skills"));
    serde_json::from_str(instructions.lines().last().unwrap()).unwrap()
}

#[test]
fn actual_plain_acp_requests_disclose_activate_reapply_and_respect_disable_and_explicit_only() {
    let fixture = ResponsesFixture::start();
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    super::write_test_config(home.path());
    let config = home.path().join(".zevria/config.toml");
    let models_path = zevria_foundation::config::models_path_for(&config);
    std::fs::write(
        &config,
        std::fs::read_to_string(&config)
            .unwrap()
            .replace("test-model", "automatic-skill-fixture"),
    )
    .unwrap();
    let source = std::fs::read_to_string(&models_path)
        .unwrap()
        .replace("http://127.0.0.1:1/v1/responses", &fixture.url)
        .replace("test-model", "automatic-skill-fixture");
    let mut models: Value = serde_json::from_str(&source).unwrap();
    models["providers"]["test"]["input_token_count"] = json!({"enabled": false});
    std::fs::write(&models_path, serde_json::to_string_pretty(&models).unwrap()).unwrap();
    let skill_dir = workspace.path().join(".zevria/skills");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill = include_str!("../../../tools/tests/fixtures/commit.md");
    std::fs::write(skill_dir.join("commit.md"), skill).unwrap();
    let mut rpc = Rpc::start(home.path(), workspace.path());
    success(rpc.request(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    ));
    let id = success(rpc.request(
        "session/new",
        json!({"cwd":workspace.path(),"mcpServers":[]}),
    ))["sessionId"]
        .clone();
    prompt(&mut rpc, &id, "commit the changes");
    prompt(&mut rpc, &id, "an unrelated task");
    prompt(&mut rpc, &id, "Please record this patch as a commit");
    let installed = view(&mut rpc, &id);
    assert_eq!(installed["counts"]["active"], 1);
    success(rpc.request("_zevria/skills/config/write", json!({"version":1,"sessionId":id,"expectedRevision":installed["revision"],"name":"commit","enabled":false})));
    prompt(&mut rpc, &id, "commit the changes");
    let disabled = view(&mut rpc, &id);
    success(rpc.request("_zevria/skills/config/write", json!({"version":1,"sessionId":id,"expectedRevision":disabled["revision"],"name":"commit","enabled":true})));
    success(rpc.request("session/close", json!({"sessionId":id})));
    std::fs::write(
        skill_dir.join("commit.md"),
        skill.replace(
            "name: commit\n",
            "name: commit\npolicy:\n  allow_implicit_invocation: false\n",
        ),
    )
    .unwrap();
    let explicit = success(rpc.request(
        "session/new",
        json!({"cwd":workspace.path(),"mcpServers":[]}),
    ))["sessionId"]
        .clone();
    prompt(&mut rpc, &explicit, "commit the changes");
    let before = view(&mut rpc, &explicit);
    assert_eq!(before["counts"]["active"], 0);
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[8, 16, 32, 255]).unwrap();
    let ordered_args = zevria_content::UserPrompt::new(vec![
        zevria_content::PromptBlock::Text("commit the changes".into()),
        zevria_content::PromptBlock::Image(image.clone()),
        zevria_content::PromptBlock::Text("Keep this after the image".into()),
    ])
    .unwrap();
    let invoked = success(rpc.request(
        "_zevria/skills/invoke",
        json!({"version":1,"sessionId":explicit,"name":"commit","args":[
            {"type":"text","text":"commit the changes"},
            {"type":"image","mimeType":image.mime_type(),"data":image.base64()},
            {"type":"text","text":"Keep this after the image"}
        ]}),
    ));
    assert_eq!(invoked["version"], 1);
    prompt(&mut rpc, &explicit, "commit the changes");
    success(rpc.request("session/close", json!({"sessionId":explicit})));
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 10);
    let first = effective_catalog(&requests[0]);
    assert_eq!(first[0]["name"], "commit");
    assert!(first[0].get("active").is_none());
    assert!(
        first[0]["description"]
            .as_str()
            .unwrap()
            .contains("detailed Git commit")
    );
    assert!(!requests[0].to_string().contains("Fixture commit procedure"));
    assert!(requests[1].to_string().contains("Fixture commit procedure"));
    assert_eq!(requests[0]["instructions"], requests[1]["instructions"]);
    assert_eq!(effective_catalog(&requests[5]), json!([]));
    assert_eq!(effective_catalog(&requests[6]), json!([]));
    assert!(!requests[6].to_string().contains("Fixture commit procedure"));
    assert_eq!(requests[6]["instructions"], requests[7]["instructions"]);
    assert_eq!(effective_catalog(&requests[7]), json!([]));
    for session in [&id, &explicit] {
        let items = transcript::load(
            &transcript::sessions_dir(workspace.path())
                .join(format!("{}.jsonl", session.as_str().unwrap())),
        )
        .unwrap();
        assert_eq!(transcript::replay_active_skills(&items).unwrap().len(), 1);
        if session == &explicit {
            let invocation = items
                .iter()
                .find_map(|item| match item {
                    TranscriptItem::SkillInvocation(invocation) => Some(invocation),
                    _ => None,
                })
                .expect("structured name-based invocation was persisted");
            assert_eq!(invocation.name().as_str(), "commit");
            assert_eq!(invocation.arguments(), &ordered_args);
        }
        assert!(
            items
                .iter()
                .any(|item| matches!(item, TranscriptItem::Directive(_)))
        );
        let pins = transcript::replay_active_skills(&items).unwrap();
        assert!(
            pins.snapshots()
                .next()
                .unwrap()
                .body()
                .contains("Fixture commit procedure")
        );
    }
    assert!(
        !serde_json::to_string(&rpc.notifications)
            .unwrap()
            .contains("Fixture commit procedure")
    );
    assert!(
        !serde_json::to_string(&rpc.notifications)
            .unwrap()
            .contains("## Eligible skills")
    );
}
