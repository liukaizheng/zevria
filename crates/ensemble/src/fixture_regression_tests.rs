//! Exercise the embedded Python support itself, independently of ACP supervision.
use super::{PYTHON_COMMAND, python_fixture_script};
use anyhow::{Context, ensure};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

enum Exchange<'a> {
    Send(&'a [u8]),
    Line(&'a str),
    CloseInput,
}

/// Every pipe operation and exit is bounded. Stderr goes to a file so a failed
/// fixture cannot deadlock on a full diagnostic pipe. On errors/timeouts, kill
/// and reap explicitly; kill-on-drop is the fallback if the test itself panics.
async fn run_python_fixture(
    body: &str,
    env: &[(&str, &str)],
    exchanges: &[Exchange<'_>],
) -> Vec<String> {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("fixture.py");
    let stderr_path = directory.path().join("stderr");
    std::fs::write(&script, python_fixture_script(body)).unwrap();
    let mut child = tokio::process::Command::new(PYTHON_COMMAND)
        .arg("-u")
        .arg(&script)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .kill_on_drop(true)
        .spawn()
        .expect("Python fixture starts");
    let mut stdin = child.stdin.take();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        for exchange in exchanges {
            match exchange {
                Exchange::Send(bytes) => {
                    stdin
                        .as_mut()
                        .context("fixture stdin already closed")?
                        .write_all(bytes)
                        .await?;
                }
                Exchange::Line(expected) => {
                    let actual = stdout.next_line().await?;
                    ensure!(
                        actual.as_deref() == Some(expected),
                        "expected {expected:?}, got {actual:?}"
                    );
                }
                Exchange::CloseInput => drop(stdin.take()),
            }
        }
        let mut remaining = Vec::new();
        while let Some(line) = stdout.next_line().await? {
            remaining.push(line);
        }
        let status = child.wait().await?;
        ensure!(status.success(), "Python fixture exited with {status}");
        Ok::<_, anyhow::Error>(remaining)
    })
    .await;
    drop(stdin);
    let cleanup = async {
        if child.try_wait()?.is_none() {
            child.start_kill()?;
            tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stderr = std::fs::read(&stderr_path).expect("captured fixture stderr");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        cleanup.is_ok(),
        "fixture cleanup: {cleanup:?}\nstderr:\n{stderr}"
    );
    result
        .unwrap_or_else(|error| {
            panic!("Python fixture timed out: {error}\nenv: {env:?}\nstderr:\n{stderr}")
        })
        .unwrap_or_else(|error| {
            panic!("Python fixture failed: {error:#}\nenv: {env:?}\nstderr:\n{stderr}")
        })
}

#[tokio::test]
async fn python_template_expansion_preserves_paths_and_independent_generations() {
    // Serialization inputs only: quotes, control characters, and some spellings
    // are intentionally not filenames that could be created on Windows.
    let paths = [
        "/tmp/plans/proposal.md",
        r"C:\Users\reviewer\AppData\Local\Temp\proposal.md",
        r"\\?\C:\Users\reviewer\AppData\Local\Temp\proposal.md",
        r"C:\new\test\return\b\f\u0041\proposal.md",
        r"\\?\C:\new\test\u0041\proposal.md",
        r#"C:\space dir\雪\say "hello"\proposal.md"#,
        "/tmp/雪 😀/\"quoted\"/proposal.md",
        "C:\\escape-like\\line\n tab\t quote\".md",
        r"C:\GENERATION\ARTIFACT\GENERATION-ARTIFACT.md",
        r"\\?\C:\ARTIFACT\GENERATION\proposal.md",
        "ARTIFACT",
        "GENERATION",
    ];
    let template: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native_handoff.json")).unwrap();
    let mut input =
        serde_json::to_vec(&serde_json::json!({"paths": paths, "template": template})).unwrap();
    input.push(b'\n');
    let output = run_python_fixture(
        r#"
# Embedding support must not steal stdin or start a thread until requested.
assert not any(thread.name == "fixture-stdin" for thread in threading.enumerate())
payload = json.loads(sys.stdin.buffer.readline())
template = payload["template"]
template["extras"] = ["ARTIFACT", {"toolCallId": "extra-GENERATION", "text": "GENERATION ARTIFACT"}, None, True, 7]
template["ARTIFACT"] = "prefix-ARTIFACT-suffix"
template["path_as_id"] = {"toolCallId": "ARTIFACT"}
original = json.loads(json.dumps(template))
for path in payload["paths"]:
    first = expand_fixture_template(template, path, 1)
    saved = json.loads(json.dumps(first))
    second = expand_fixture_template(template, path, 2)
    for generation, rendered in [(1, first), (2, second)]:
        assert json.loads(json.dumps(rendered)) == rendered
        assert rendered["write"]["rawInput"]["file_path"] == path
        assert rendered["edit_path"]["locations"] == [{"path": path}]
        for key in ["edit_preview", "edit_result"]:
            assert rendered[key]["content"][0]["path"] == path
        for key, prefix in [("write", "write"), ("write_terminal", "write"), ("exit", "exit"), ("exit_terminal", "exit"), ("edit_terminal", "edit")]:
            assert rendered[key]["toolCallId"] == f"{prefix}-{generation}"
        assert rendered["permission"]["toolCall"]["toolCallId"] == f"exit-{generation}"
        assert rendered["extras"] == [path, {"toolCallId": f"extra-{generation}", "text": "GENERATION ARTIFACT"}, None, True, 7]
        assert rendered["ARTIFACT"] == "prefix-ARTIFACT-suffix"
        assert rendered["path_as_id"]["toolCallId"] == path
    second["write"]["rawInput"]["content"] = "changed"
    second["permission"]["options"][0]["name"] = "changed"
    second["edit_path"]["locations"].append({"path": "changed"})
    second["extras"][1]["text"] = "changed"
    assert first == saved, path
    assert template == original, path
"#,
        &[],
        &[Exchange::Send(&input)],
    ).await;
    assert!(output.is_empty());
}

#[tokio::test]
async fn python_json_lines_preserve_fifo_and_fragmented_utf8() {
    let output = run_python_fixture(
        r#"
reader = JsonLineReader()
assert reader.receive(2) == {"id": 1}
assert reader.receive(2) == {"id": 2}
print("batch received", flush=True)
reader.assert_no_response(0.05)
print("partial line", flush=True)
reader.assert_no_response(0.05)
print("partial UTF-8", flush=True)
assert reader.receive(2) == {"id": 3, "text": "雪"}
assert reader.receive(2) == {"id": 4}
# Exit with the host's stdin pipe still open and the raw reader blocked.
"#,
        &[],
        &[
            Exchange::Send(b"{\"id\":1}\n{\"id\":2}\n"),
            Exchange::Line("batch received"),
            Exchange::Send(b"{\"id\":3,\"text\":\"\xe9"),
            Exchange::Line("partial line"),
            Exchange::Send(b"\x9b"),
            Exchange::Line("partial UTF-8"),
            Exchange::Send(b"\xaa\"}\n{\"id\":4}\r\n"),
        ],
    )
    .await;
    assert!(output.is_empty());
}

#[tokio::test]
async fn python_json_lines_bound_waits_and_detect_unexpected_responses() {
    let output = run_python_fixture(
        r#"
import time
reader = JsonLineReader()
start = time.monotonic()
try:
    reader.receive(0.05)
except TimeoutError as error:
    assert "no JSON response" in str(error), error
else:
    raise AssertionError("receive did not time out")
assert time.monotonic() - start >= 0.05
start = time.monotonic()
reader.assert_no_response(0.05)
assert time.monotonic() - start >= 0.05
print("silence checked", flush=True)
try:
    reader.assert_no_response(2, "grant before terminal")
except AssertionError as error:
    assert "grant before terminal" in str(error) and "91" in str(error), error
else:
    raise AssertionError("unexpected response was ignored")
assert reader.receive(2) == {"id": 92}
"#,
        &[],
        &[
            Exchange::Line("silence checked"),
            Exchange::Send(b"{\"id\":91}\n{\"id\":92}\n"),
        ],
    )
    .await;
    assert!(output.is_empty());
}

#[tokio::test]
async fn python_json_lines_propagate_eof_malformed_truncated_and_read_failures() {
    for (scenario, input) in [
        ("eof", b"{\"id\":1}\n".as_slice()),
        ("malformed", b"{\"id\":1}\nnot-json\n".as_slice()),
        ("truncated", b"{\"id\":1}\n{\"id\":".as_slice()),
        ("unterminated", b"{\"id\":1}\n{\"id\":2}".as_slice()),
        ("encoding", b"{\"id\":1}\n\xff\n".as_slice()),
        ("read-failure", b"".as_slice()),
        ("read-timeout", b"".as_slice()),
    ] {
        for first in ["receive", "assert_no_response"] {
            let output = run_python_fixture(
                r#"
scenario = os.environ["SCENARIO"]
expected = {
    "eof": (EOFError, "host closed connection"),
    "malformed": (json.JSONDecodeError, "Expecting value"),
    "truncated": (ValueError, "truncated JSON line at EOF"),
    "unterminated": (ValueError, "truncated JSON line at EOF"),
    "encoding": (UnicodeDecodeError, "utf-8"),
    "read-failure": (OSError, "scripted raw read failure"),
    "read-timeout": (TimeoutError, "scripted raw read timeout"),
}
error_type, text = expected[scenario]
if scenario.startswith("read-"):
    def fail_read(descriptor, size):
        raise error_type(text)
    os.read = fail_read
reader = JsonLineReader()
if not scenario.startswith("read-"):
    assert reader.receive(2) == {"id": 1}  # queued messages precede the failure
# Both APIs must fail, including repeated calls after the first observed error.
for method in [os.environ["FIRST"], "receive", "assert_no_response"]:
    try:
        getattr(reader, method)(2)
    except error_type as error:
        assert text in str(error), error
    else:
        raise AssertionError(f"{scenario}: {method} hid a dead reader")
"#,
                &[("SCENARIO", scenario), ("FIRST", first)],
                &[Exchange::Send(input), Exchange::CloseInput],
            )
            .await;
            assert!(output.is_empty(), "{scenario}/{first}: {output:?}");
        }
    }
}

#[tokio::test]
async fn python_native_fixture_writes_exact_utf8_lf_artifacts_and_appended_traces() {
    for scenario in ["prompt-first", "edit-refinement"] {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("Claude 雪 config");
        let artifact = config.join("plans").join("proposal.md");
        std::fs::create_dir_all(config.join("plans")).unwrap();
        let fixture = directory.path().join("native 雪.json");
        let trace = directory.path().join("responses");
        let mut template: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/native_handoff.json")).unwrap();
        for field in ["edit_initial_markdown", "edited_markdown"] {
            template[field] = template[field]
                .as_str()
                .unwrap()
                .replace("Frozen proposal", "Frozen proposal — 雪")
                .into();
        }
        template["write"]["rawInput"]["content"] =
            "# Frozen proposal — 雪\n\n- Preserve Plan mode.\n".into();
        std::fs::write(&fixture, serde_json::to_vec(&template).unwrap()).unwrap();
        let expected = if scenario == "edit-refinement" {
            template["edited_markdown"].as_str().unwrap()
        } else {
            template["write"]["rawInput"]["content"].as_str().unwrap()
        };
        let mut input = Vec::new();
        for generation in 1..=2 {
            for message in [
                serde_json::json!({"jsonrpc": "2.0", "id": generation, "method": "session/prompt", "params": {"sessionId": "native-session"}}),
                serde_json::json!({"jsonrpc": "2.0", "id": "exit-permission", "result": {"outcome": {"outcome": "selected", "optionId": "stay-planning-exact-id"}}}),
            ] {
                serde_json::to_writer(&mut input, &message).unwrap();
                input.push(b'\n');
            }
        }
        // Reuse the files across processes to cover append as well as write mode.
        for run in 1..=2 {
            let output = run_python_fixture(
                include_str!("fixtures/native_handoff.py"),
                &[
                    ("SCENARIO", scenario),
                    ("CLAUDE_CONFIG_DIR", config.to_str().unwrap()),
                    ("FIXTURE", fixture.to_str().unwrap()),
                    ("TRACE", trace.to_str().unwrap()),
                ],
                &[Exchange::Send(&input), Exchange::CloseInput],
            )
            .await;
            let messages = output
                .iter()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
            for generation in 1..=2 {
                let write = messages
                    .iter()
                    .map(|message| &message["params"]["update"])
                    .find(|update| {
                        update["sessionUpdate"] == "tool_call"
                            && update["toolCallId"] == format!("write-{generation}")
                    })
                    .unwrap_or_else(|| panic!("{scenario}: missing write generation {generation}"));
                assert_eq!(
                    write["rawInput"]["file_path"],
                    serde_json::json!(artifact),
                    "{scenario}, run {run}, generation {generation}: exact native path spelling"
                );
            }
            assert_eq!(
                std::fs::read(&artifact).unwrap(),
                expected.as_bytes(),
                "{scenario}, run {run}"
            );
            assert_eq!(
                std::fs::read(&trace).unwrap(),
                "reject_once\n".repeat(2 * run).as_bytes(),
                "{scenario}, run {run}"
            );
            assert_eq!(
                std::fs::read(trace.with_extension("processes")).unwrap(),
                "spawn\n".repeat(run).as_bytes(),
                "{scenario}, run {run}"
            );
        }
    }
}
