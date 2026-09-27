//! Real-process coverage: binary composition, captured roots, shared offline
//! writer, engine installation, and exact ACP wire method dispatch.
use serde_json::{Value, json};
use std::{
    io::{BufRead as _, Write as _},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

pub(super) struct Rpc {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<Value>,
    next_id: u64,
    changes: Vec<Value>,
    pub(super) notifications: Vec<Value>,
    pub(super) questions: Vec<Value>,
}
impl Rpc {
    pub(super) fn start(home: &Path, workspace: &Path) -> Self {
        Self::start_profile(home, workspace, false, None)
    }
    pub(super) fn start_worker(home: &Path, workspace: &Path, config: &Path) -> Self {
        Self::start_profile(home, workspace, true, Some(config))
    }
    fn start_profile(home: &Path, workspace: &Path, worker: bool, config: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zevria"));
        command
            .arg("--acp")
            .env("HOME", home)
            .env_remove("ZEVRIA_CONFIG");
        if worker {
            command.arg("--ensemble-worker");
        }
        if let Some(path) = config {
            command.env("ZEVRIA_CONFIG", path);
        }
        let mut child = command
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                assert!(line.len() < 128 * 1024, "unbounded ACP response");
                let value = serde_json::from_str(&line).expect("JSON-RPC only on stdout");
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
            next_id: 0,
            changes: Vec::new(),
            notifications: Vec::new(),
            questions: Vec::new(),
        }
    }
    /// Crash fixtures must reap the owner before asserting that OS locks were
    /// released. Only callers' temporary workspaces are ever used by Rpc.
    pub(super) fn kill_and_reap(&mut self) {
        self.child.kill().expect("kill ACP owner");
        self.child.wait().expect("reap ACP owner");
    }
    pub(super) fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0", "id":self.next_id, "method":method, "params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        loop {
            let value = self
                .output
                .recv_timeout(Duration::from_secs(10))
                .expect("ACP response timeout");
            if value["method"] == "elicitation/create" {
                assert!(
                    value["params"]["requestedSchema"]["properties"]
                        .get("decision")
                        .is_none(),
                    "worker requested implementation approval: {value}"
                );
                self.questions.push(value.clone());
                writeln!(self.input, "{}", json!({"jsonrpc":"2.0", "id":value["id"], "result":{"action":"accept", "content":{"question_0":"option_0"}}})).unwrap();
                self.input.flush().unwrap();
                continue;
            }
            if value["method"] == "_zevria/skills/changed" {
                self.changes.push(value.clone());
            }
            if value["method"] == "session/update" {
                self.notifications.push(value.clone());
            }
            if value["id"] == self.next_id {
                return value;
            }
        }
    }
    fn list(&mut self, session: &Value) -> Value {
        let result = self.request(
            "_zevria/skills/list",
            json!({"version":1,"sessionId":session}),
        );
        assert!(result.get("error").is_none(), "{result}");
        assert_eq!(result["result"]["version"], 1);
        assert_eq!(result["result"]["result"]["kind"], "view");
        result["result"]["result"]["view"].clone()
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn actual_acp_skills_reload_is_explicit_atomic_and_isolated_from_offline_writes() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    super::write_test_config(home.path());
    let mut rpc = Rpc::start(home.path(), workspace.path());
    assert!(
        rpc.request(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}})
        )
        .get("error")
        .is_none()
    );
    let created = rpc.request(
        "session/new",
        json!({"cwd":workspace.path(),"mcpServers":[]}),
    );
    assert!(created.get("error").is_none(), "{created}");
    let first_id = created["result"]["sessionId"].clone();
    let empty = rpc.list(&first_id);
    assert_eq!(empty["counts"]["candidates"], 0);
    let root = workspace.path().join(".zevria/skills");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("review.md"),
        "---\ndescription: Review code\n---\nPRIVATE SKILL BODY\n",
    )
    .unwrap();
    assert_eq!(
        rpc.list(&first_id)["revision"],
        empty["revision"],
        "filesystem edits are not implicit reloads"
    );
    let reload = rpc.request(
        "_zevria/skills/reload",
        json!({"version":1,"sessionId":first_id,"expectedRevision":empty["revision"]}),
    );
    assert_eq!(reload["result"]["version"], 1);
    assert_eq!(reload["result"]["result"]["kind"], "changed", "{reload}");
    let first = rpc.list(&first_id);
    assert_eq!(first["counts"]["candidates"], 1);
    let inspected = rpc.request(
        "_zevria/skills/inspect",
        json!({"version":1,"sessionId":first_id,"name":"review"}),
    );
    assert_eq!(inspected["result"]["version"], 1);
    assert_eq!(
        inspected["result"]["result"]["view"]["entries"],
        first["entries"]
    );
    assert!(!first.to_string().contains("PRIVATE SKILL BODY"));
    assert_eq!(first["completions"][0]["name"], "review");
    assert!(first["entries"][0].get("binding").is_none());
    let second_id = rpc.request(
        "session/new",
        json!({"cwd":workspace.path(),"mcpServers":[]}),
    )["result"]["sessionId"]
        .clone();
    assert_eq!(rpc.list(&second_id)["revision"], first["revision"]);
    let offline = Command::new(env!("CARGO_BIN_EXE_zevria"))
        .args(["skills", "disable", "review"])
        .env("HOME", home.path())
        .env_remove("ZEVRIA_CONFIG")
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    assert_eq!(rpc.list(&first_id)["revision"], first["revision"]);
    assert_eq!(rpc.list(&second_id)["revision"], first["revision"]);
    let reload = rpc.request(
        "_zevria/skills/reload",
        json!({"version":1,"sessionId":first_id,"expectedRevision":first["revision"]}),
    );
    assert_eq!(reload["result"]["result"]["kind"], "changed");
    let disabled = rpc.list(&first_id);
    assert_eq!(disabled["entries"][0]["enabled"], false);
    assert_eq!(rpc.list(&second_id)["entries"][0]["enabled"], true);
    // Name-only invocations resolve against the installed context before provider I/O.
    let rejected = rpc.request(
        "_zevria/skills/invoke",
        json!({"version":1,"sessionId":first_id,"name":"review","args":[]}),
    );
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert!(rejected.to_string().contains("disabled"));
    let old_version = rpc.request(
        "_zevria/skills/list",
        json!({"version":3,"sessionId":first_id}),
    );
    assert_eq!(old_version["error"]["code"], -32602);
    assert!(old_version.to_string().contains("expected 1"));
    let config = home.path().join(".zevria/config.toml");
    let before = std::fs::read_to_string(&config).unwrap();
    // A fatal discovery failure cannot commit a config edit or replace the
    // installed catalog. The root path is fixed, not replaced by the request.
    let saved = workspace.path().join("saved-skills");
    std::fs::rename(&root, &saved).unwrap();
    std::fs::write(&root, "not a directory").unwrap();
    let failed = rpc.request("_zevria/skills/config/write", json!({"version":1,"sessionId":first_id,"expectedRevision":disabled["revision"],"name":"review","enabled":true}));
    assert_eq!(failed["result"]["version"], 1);
    assert_eq!(failed["result"]["result"]["kind"], "error", "{failed}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(rpc.list(&first_id)["revision"], disabled["revision"]);
    std::fs::remove_file(&root).unwrap();
    std::fs::rename(saved, &root).unwrap();
    let unchanged = rpc.request(
        "_zevria/skills/reload",
        json!({"version":1,"sessionId":first_id,"expectedRevision":disabled["revision"]}),
    );
    assert_eq!(unchanged["result"]["result"]["unchanged"], true);
    let override_error = rpc.request("_zevria/skills/reload", json!({"version":1,"sessionId":first_id,"expectedRevision":disabled["revision"],"workspace":"/tmp"}));
    assert_eq!(override_error["error"]["code"], -32602);
    assert_eq!(
        rpc.changes.len(),
        2,
        "no invalidation for failed or unchanged updates"
    );
    assert!(rpc.changes.iter().all(
        |notice| notice["params"]["sessionId"] == first_id && notice["params"]["version"] == 1
    ));
    rpc.request("session/close", json!({"sessionId":first_id}));
    rpc.request("session/close", json!({"sessionId":second_id}));
}
