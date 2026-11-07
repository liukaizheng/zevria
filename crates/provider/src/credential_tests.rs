use super::*;
use test_support::{KeyHelper, PYTHON_COMMAND};

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    api_key: ApiKeySource,
}

#[test]
fn json_and_toml_accept_both_forms_and_round_trip_only_configuration() {
    for (json, toml) in [
        (
            r#"{"api_key":"$(never execute)"}"#,
            "api_key = '$(never execute)'",
        ),
        (
            r#"{"api_key":{"command":"auth"}}"#,
            "api_key = { command = 'auth' }",
        ),
        (
            r#"{"api_key":{"command":"auth","args":["--key"],"timeout_seconds":2,"refresh_interval_seconds":0}}"#,
            "api_key = { command = 'auth', args = ['--key'], timeout_seconds = 2, refresh_interval_seconds = 0 }",
        ),
    ] {
        let json: Config = serde_json::from_str(json).unwrap();
        let toml: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            serde_json::to_value(&json).unwrap(),
            serde_json::to_value(&toml).unwrap()
        );
        for config in [json, toml] {
            let value = serde_json::to_value(&config).unwrap();
            let copy: Config = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&copy).unwrap(), value);
            let text = toml::to_string(&config).unwrap();
            let copy: Config = toml::from_str(&text).unwrap();
            assert_eq!(serde_json::to_value(&copy).unwrap(), value);
        }
    }
    let config: ApiKeySource = serde_json::from_str(r#"{"command":"auth"}"#).unwrap();
    let command = config.as_command().unwrap();
    assert_eq!(command.timeout_seconds, 30);
    assert_eq!(command.refresh_interval_seconds, 300);
    assert!(command.args.is_empty());
    assert!(!ApiKeySource::literal("x").is_refreshable());
}

#[test]
fn strict_schema_and_wrong_type_diagnostics() {
    for invalid in ["null", "true", "42", "[]", "[\"secret\"]"] {
        let error = serde_json::from_str::<ApiKeySource>(invalid)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("literal API key string") && error.contains("command object"),
            "{error}"
        );
    }
    for invalid in [
        r#"{"command":"auth","env":{}}"#,
        r#"{"command":3}"#,
        r#"{"command":"auth","args":"--key"}"#,
        r#"{"command":"auth","timeout_seconds":-1}"#,
        r#"{"command":"auth","refresh_interval_seconds":1.5}"#,
        "{}",
    ] {
        assert!(
            serde_json::from_str::<ApiKeySource>(invalid).is_err(),
            "{invalid}"
        );
    }
    for invalid in [
        "api_key = 42",
        "api_key = true",
        "api_key = []",
        "api_key = { command = 'auth', env = {} }",
    ] {
        assert!(toml::from_str::<Config>(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn validation_is_local_precise_and_redacted() {
    for key in ["", " \t", "PRIVATE\nKEY", "PRIVATE\0KEY"] {
        let error = ApiKeySource::literal(key)
            .validate("p")
            .unwrap_err()
            .to_string();
        assert!(error.contains("providers.p.api_key"));
        assert!(!error.contains("PRIVATE"));
    }
    for (field, value) in [
        ("command", serde_json::json!(" \t")),
        ("command", serde_json::json!("bad\0program")),
        ("timeout_seconds", serde_json::json!(0)),
        ("timeout_seconds", serde_json::json!(u64::MAX)),
        ("refresh_interval_seconds", serde_json::json!(u64::MAX)),
    ] {
        let mut command = serde_json::json!({"command":"definitely-not-installed"});
        command[field] = value;
        let source: ApiKeySource = serde_json::from_value(command).unwrap();
        let error = source.validate("test").unwrap_err().to_string();
        assert!(
            error.contains(&format!("providers.test.api_key.{field}")),
            "{error}"
        );
    }
    let literal = LiteralApiKey::new("PRIVATE_LITERAL");
    assert_eq!(format!("{literal:?}"), "<redacted>");
    assert_eq!(format!("{:?}", ApiKeySource::from(literal)), "<redacted>");
    let source: ApiKeySource =
        serde_json::from_str(r#"{"command":"auth","args":["PRIVATE_ARG"]}"#).unwrap();
    source.validate("p").unwrap(); // Missing executable is not resolved here.
    let debug = format!("{source:?} {:?}", source.as_command().unwrap());
    assert!(debug.contains("auth") && debug.contains("1 args"));
    assert!(!debug.contains("PRIVATE_ARG"));
}

#[tokio::test]
async fn output_contract_trims_newlines_crlf_bom_and_whitespace() {
    for output in ["key\n", " \tkey\r\n", "\u{feff}  key \r\n", "key"] {
        let fixture = KeyHelper::new(&format!(
            "sys.stdout.buffer.write({}.encode('utf-8'))",
            serde_json::to_string(output).unwrap()
        ));
        let lease = fixture.source.current("p").await.unwrap();
        assert_eq!(lease.expose(), "key");
        assert_eq!(format!("{lease:?}"), "<redacted>");
    }
}

#[tokio::test]
async fn invalid_output_never_echoes_stdout() {
    for script in [
        "print('   ')",
        "print('PRIVATE_OUTPUT\\nother-line')",
        "sys.stdout.buffer.write(b'PRIVATE_OUTPUT\\x00')",
        "sys.stdout.buffer.write(b'PRIVATE_OUTPUT\\xff')",
        "sys.stdout.write('PRIVATE_OUTPUT' * 100000)",
        "sys.stderr.write('x' * 1000000)",
    ] {
        let fixture = KeyHelper::new(script);
        let error = fixture
            .source
            .current("test")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("providers.test.api_key") && error.contains(PYTHON_COMMAND),
            "{error}"
        );
        assert!(error.contains("exit status:"));
        assert!(!error.contains("PRIVATE_OUTPUT"), "{error}");
    }
}

#[tokio::test]
async fn failed_exit_and_missing_program_are_actionable_without_stdout_or_args() {
    let fixture = KeyHelper::new(
        "print('PRIVATE_STDOUT'); sys.stderr.write('login failed https://example.test/?PRIVATE_URL ' + 'x' * 10000); sys.exit(7)",
    );
    let error = fixture
        .source
        .current("test")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("7") && error.contains("login failed") && error.contains("[URL removed]"),
        "{error}"
    );
    assert!(
        !error.contains("PRIVATE_") && !error.contains("helper.py"),
        "{error}"
    );
    assert!(error.len() < 4500);
    let source = ApiKeySource::command(ApiKeyCommand {
        command: "zevria-missing-credential-helper-7bfa".into(),
        args: vec!["PRIVATE_ARG".into()],
        timeout_seconds: 1,
        refresh_interval_seconds: 300,
    });
    let error = source.current("missing").await.unwrap_err().to_string();
    assert!(
        error.contains("providers.missing.api_key")
            && error.contains("zevria-missing-credential-helper-7bfa")
    );
    assert!(!error.contains("PRIVATE_ARG"));
}

#[tokio::test]
async fn stderr_diagnostics_remove_echoed_arguments_and_stdout() {
    let fixture = KeyHelper::new(
        "print('PRIVATE_STDOUT'); sys.stderr.write('login failed ' + sys.argv[1] + ' PRIVATE_STDOUT'); sys.exit(1)",
    );
    let mut command = fixture.source.as_command().unwrap().clone();
    command.args.push("PRIVATE_ARGUMENT".into());
    let source = ApiKeySource::command(command).with_config_directory(fixture.directory.path());
    let error = source.current("p").await.unwrap_err().to_string();
    assert!(
        error.contains("login failed")
            && error.contains("[argument removed]")
            && error.contains("[stdout removed]"),
        "{error}"
    );
    assert!(!error.contains("PRIVATE_"), "{error}");
}

#[tokio::test]
async fn authentication_retries_only_once_and_discards_a_rejected_replacement() {
    let fixture = KeyHelper::rotating();
    let result = authenticated(
        &fixture.source,
        "p",
        |lease| async { (401, lease) },
        |(status, _)| {
            (*status == 401).then_some(Unauthorized {
                upstream_request_id: None,
            })
        },
    )
    .await
    .unwrap();
    assert_eq!(result.0, 401);
    assert_eq!(result.1.expose(), "key-2");
    assert_eq!(fixture.runs(), 2);
    assert_eq!(fixture.source.current("p").await.unwrap().expose(), "key-3");
    let literal = ApiKeySource::literal("literal-key");
    let calls = std::sync::atomic::AtomicUsize::new(0);
    authenticated(
        &literal,
        "p",
        |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { 401 }
        },
        |_| {
            Some(Unauthorized {
                upstream_request_id: None,
            })
        },
    )
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timeout_and_cancellation_kill_the_process_tree() {
    for cancel in [false, true] {
        let fixture = KeyHelper::new(
            "subprocess.Popen([sys.executable, '-c', \"import time, pathlib; time.sleep(1.5); pathlib.Path('child-survived').touch()\"]); pathlib.Path('started').touch(); time.sleep(60)",
        );
        let source = fixture
            .source
            .with_test_durations(Duration::from_secs(1), Duration::from_secs(300));
        if cancel {
            let source = source.clone();
            let task = tokio::spawn(async move { source.current("p").await });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !fixture.directory.path().join("started").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            let error = source.current("p").await.unwrap_err().to_string();
            assert!(
                error.contains("process timed out after 1 seconds"),
                "{error}"
            );
        }
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(!fixture.directory.path().join("child-survived").exists());
        // The canceled future also released the mutex rather than poisoning it.
        assert!(matches!(&source.0, Source::Command(command) if command.state.try_lock().is_ok()));
    }
}

#[tokio::test]
async fn working_directory_relative_executable_and_closed_stdin() {
    let fixture = KeyHelper::new(
        "assert pathlib.Path.cwd().resolve() == pathlib.Path(sys.argv[0]).parent.resolve(); assert sys.stdin.read() == ''; print('cwd-key')",
    );
    assert_eq!(
        fixture.source.current("p").await.unwrap().expose(),
        "cwd-key"
    );

    // Resolve a real executable, then address it relative to its own directory.
    // This works on Windows without relying on executable scripts or symlinks.
    let output = std::process::Command::new(PYTHON_COMMAND)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let executable = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let source = ApiKeySource::command(ApiKeyCommand {
        command: format!("./{}", executable.file_name().unwrap().to_str().unwrap()),
        args: vec!["-c".into(), "import os; print('relative-key')".into()],
        timeout_seconds: 30,
        refresh_interval_seconds: 300,
    })
    .with_config_directory(executable.parent().unwrap());
    assert_eq!(source.current("p").await.unwrap().expose(), "relative-key");
}

#[tokio::test]
async fn cache_reuse_zero_interval_expiry_and_generation_safe_rejection() {
    let fixture = KeyHelper::rotating();
    let source = &fixture.source;
    let first = source.current("p").await.unwrap();
    assert_eq!(first.expose(), "key-1");
    assert_eq!(
        source.clone().current("p").await.unwrap().generation,
        first.generation
    );
    assert_eq!(fixture.runs(), 1);
    source.reject(&first).await;
    let second = source.current("p").await.unwrap();
    assert_eq!(second.expose(), "key-2");
    source.reject(&first).await; // stale 401 cannot invalidate the replacement
    assert_eq!(
        source.current("p").await.unwrap().generation,
        second.generation
    );
    assert_eq!(fixture.runs(), 2);

    for interval in [Duration::ZERO, Duration::from_millis(80)] {
        let fixture = KeyHelper::rotating();
        let source = fixture
            .source
            .clone()
            .with_config_directory(fixture.directory.path())
            .with_test_durations(Duration::from_secs(30), interval);
        assert_eq!(source.current("p").await.unwrap().expose(), "key-1");
        if !interval.is_zero() {
            assert_eq!(source.current("p").await.unwrap().expose(), "key-1");
            tokio::time::sleep(interval + Duration::from_millis(20)).await;
        }
        assert_eq!(source.current("p").await.unwrap().expose(), "key-2");
        assert_eq!(fixture.runs(), 2);
    }
}

#[tokio::test]
async fn concurrent_cold_and_unauthorized_callers_share_one_run_per_generation() {
    let fixture = KeyHelper::new("time.sleep(0.1); print('key-' + str(n))");
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let source = fixture.source.clone();
        tasks.push(tokio::spawn(
            async move { source.current("p").await.unwrap() },
        ));
    }
    let mut leases = Vec::new();
    for task in tasks {
        leases.push(task.await.unwrap());
    }
    assert_eq!(fixture.runs(), 1);
    let barrier = Arc::new(tokio::sync::Barrier::new(leases.len()));
    let mut tasks = Vec::new();
    for lease in leases {
        let source = fixture.source.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            source.reject(&lease).await;
            source.current("p").await.unwrap()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().expose(), "key-2");
    }
    assert_eq!(fixture.runs(), 2);
    let serialized = serde_json::to_string(&fixture.source).unwrap();
    let debug = format!("{:?}", fixture.source);
    assert!(!serialized.contains("key-2") && !debug.contains("key-2"));
    assert_eq!(
        serialized,
        serde_json::to_string(
            &fixture
                .source
                .with_config_directory(Path::new("another-directory"))
        )
        .unwrap()
    );
}
