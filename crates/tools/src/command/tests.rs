use super::*;

#[cfg(all(test, unix))]
#[path = "../../tests/support/inspection.rs"]
mod inspection_fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::SessionMode;
    use zevria_foundation::TurnId;

    async fn run(tool: &CommandTool, command: impl Into<String>) -> Result<String, CommandError> {
        tool.call(
            &mut ToolContext::new(),
            CommandArgs {
                command: command.into(),
                background: None,
            },
        )
        .await
    }

    #[test]
    fn schema_is_a_strict_single_field_object() {
        let tool = CommandTool::new(PathBuf::from("."));
        assert_eq!(CommandTool::NAME, "command");
        let description = tool.description();
        assert!(description.contains("working directory"));
        assert!(description.contains("not sandboxed, read-only, or confined"));
        assert_eq!(
            tool.parameters(),
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "CommandArgs",
                "description": "Arguments accepted by the [`CommandTool`].",
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            })
        );
    }

    #[tokio::test]
    async fn uses_the_configured_working_directory() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let expected = directory
            .path()
            .canonicalize()
            .expect("temporary directory should canonicalize");
        let output = run(
            &CommandTool::new(expected.clone()),
            "pwd; printf verified > cwd-marker",
        )
        .await
        .expect("pwd should run");

        assert!(output.contains(expected.file_name().unwrap().to_str().unwrap()));
        assert_eq!(
            std::fs::read_to_string(expected.join("cwd-marker")).unwrap(),
            "verified"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_absolute_home_parent_and_symlink_reads_keep_default_cwd() {
        let fixture = inspection_fixture::Fixture::new();
        let tool = CommandTool::new(fixture.workspace.clone());
        let result = run(&tool, fixture.read_command()).await.unwrap();
        assert!(result.contains("status: success"), "{result}");
        assert_eq!(
            result
                .matches(inspection_fixture::DIAGNOSTIC.trim())
                .count(),
            4
        );
        assert!(result.contains("project with spaces"));
        let home = run(&tool, fixture.home_read_command()).await.unwrap();
        assert!(home.contains(inspection_fixture::DIAGNOSTIC));
        let missing = run(&tool, fixture.missing_command()).await.unwrap();
        assert!(missing.contains("status: failure"));
        assert!(missing.contains("missing-diagnostic"));
        fixture.assert_unchanged();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_scratch_download_transform_script_and_local_build_preserve_sources() {
        // Execution success is NOT evidence that arbitrary writes are confined.
        // The policy tests separately pin the authoritative behavioral contract.
        let fixture = inspection_fixture::Fixture::new();
        let server = inspection_fixture::HttpFixture::start();
        let tool = CommandTool::new(fixture.workspace.clone());
        let inspection = run(&tool, fixture.read_command()).await.unwrap();
        assert!(inspection.contains("py_compile.compile"));
        let created = run(&tool, fixture.create_command()).await.unwrap();
        let scratch = fixture.own_scratch(&created);
        for command in [
            fixture.download_command(&scratch.0, &server.url),
            fixture.prepare_command(&scratch.0),
            fixture.execute_command(&scratch.0),
        ] {
            let result = run(&tool, command).await.unwrap();
            assert!(result.contains("status: success"), "{result}");
        }
        scratch.assert_results(&fixture);
        // A previous shell cd is not persistent, even while scratch exists.
        let cwd = run(&tool, "pwd").await.unwrap();
        assert!(cwd.contains(fixture.workspace.to_str().unwrap()));
        fixture.assert_unchanged();
        let cleanup = run(&tool, fixture.cleanup_command(&scratch.0))
            .await
            .unwrap();
        assert!(cleanup.contains("owned scratch cleaned"));
        assert!(!scratch.0.exists());
        fixture.assert_unchanged();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_command_environment_runs_rtk_git_and_native_development_tools() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fixture.txt"), "first\nsecond\n").unwrap();
        let tool = CommandTool::new(directory.path().to_path_buf());
        let output = run(&tool, "rtk read fixture.txt && rtk sed -n '2p' fixture.txt && git --version && rustc --version && printf '%s' \"$USERPROFILE\"").await.unwrap();
        assert!(output.contains("status: success"), "{output}");
        for expected in ["second", "git version", "rustc"] {
            assert!(output.contains(expected), "{output}");
        }
        assert!(
            output.contains(&std::env::var("USERPROFILE").unwrap()),
            "native environment is inherited"
        );
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "printf 'hello stdout'; printf 'hello stderr' >&2",
        )
        .await
        .expect("command should run");

        assert!(output.contains("status: success"));
        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("stdout:\nhello stdout"));
        assert!(output.contains("stderr:\nhello stderr"));
    }

    #[tokio::test]
    async fn inherits_the_parent_environment() {
        let expected = env!("CARGO_MANIFEST_DIR");
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "printf '%s' \"$CARGO_MANIFEST_DIR\"",
        )
        .await
        .expect("command should run");

        assert!(output.contains(expected));
    }

    #[tokio::test]
    async fn non_zero_exit_is_a_normal_result() {
        let output = run(&CommandTool::new(PathBuf::from(".")), "exit 7")
            .await
            .expect("non-zero status should still be a result");

        assert!(output.contains("status: failure"));
        assert!(output.contains("exit_code: 7"));
    }

    #[tokio::test]
    async fn rejects_empty_commands() {
        let error = run(&CommandTool::new(PathBuf::from(".")), "  \n\t")
            .await
            .expect_err("empty command should fail");
        assert!(matches!(error, CommandError::EmptyCommand));
    }

    #[tokio::test]
    async fn returns_output_larger_than_the_ui_preview_in_full() {
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "i=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i + 1)); done",
        )
        .await
        .expect("large-output command should run");

        let stdout = output
            .split_once("stdout:\n")
            .and_then(|(_, output)| output.split_once("\nstderr:\n"))
            .map(|(stdout, _)| stdout)
            .expect("result should contain stdout and stderr sections");
        assert_eq!(stdout.chars().count(), 5000);
        assert!(output.len() > 4096);
    }

    #[test]
    fn elide_middle_returns_streams_within_the_limit_unchanged() {
        let stream = "x".repeat(64);
        let result = elide_middle(&stream, 64);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, stream);
    }

    #[test]
    fn elide_middle_keeps_head_and_tail_and_reports_the_omission() {
        let stream = format!("{}{}", "a".repeat(1000), "b".repeat(1000));
        let result = elide_middle(&stream, 100);
        assert!(result.starts_with(&"a".repeat(50)));
        assert!(result.ends_with(&"b".repeat(50)));
        assert!(result.contains("1900 bytes omitted"));
    }

    #[test]
    fn elide_middle_respects_multibyte_character_boundaries() {
        // Two bytes per character, with an odd budget landing mid-character on
        // both sides of the elision.
        let stream = "é".repeat(200);
        let result = elide_middle(&stream, 99);
        assert!(result.starts_with(&"é".repeat(24)));
        assert!(result.ends_with(&"é".repeat(24)));
        assert!(result.contains("304 bytes omitted"));
    }

    #[tokio::test]
    async fn oversized_output_is_elided_before_reaching_the_model() {
        let output = run(
            &CommandTool::new(PathBuf::from(".")),
            "awk 'BEGIN { for (i = 0; i < 200000; i++) printf \"x\" }'",
        )
        .await
        .expect("oversized-output command should run");

        assert!(output.contains("status: success"));
        assert!(output.contains("183616 bytes omitted"));
        assert!(
            output.len() < MAX_OUTPUT_STREAM_BYTES + 1024,
            "result should stay near the per-stream limit, got {} bytes",
            output.len()
        );
        // The trailing stdout and the stderr section both survive the elision.
        assert!(output.ends_with("\nstderr:\n"));
    }

    #[tokio::test]
    async fn capture_stream_drains_past_its_limit_under_backpressure() {
        use tokio::io::AsyncWriteExt as _;

        const RETAINED_BYTES: usize = 1024;
        let payload = format!("HEAD{}TAIL", "x".repeat(128 * 1024));
        let (mut writer, reader) = tokio::io::duplex(64);
        let write = async {
            writer.write_all(payload.as_bytes()).await?;
            writer.shutdown().await
        };
        tokio::pin!(write);
        // Fill the small pipe before starting the reader: the writer cannot
        // finish unless capture keeps draining well past its retained limit.
        assert!(futures_util::poll!(&mut write).is_pending());
        let (_, captured) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(write, capture_stream(reader, RETAINED_BYTES))
        })
        .await
        .expect("bounded capture must drain a backpressured writer to EOF")
        .expect("in-memory capture should succeed");

        let padding = "x".repeat(RETAINED_BYTES / 2 - 4);
        assert_eq!(
            captured,
            format!(
                "HEAD{padding}\n[... {} bytes discarded while draining bounded command output ...]\n{padding}TAIL",
                payload.len() - RETAINED_BYTES,
            )
        );
    }

    #[tokio::test]
    async fn configured_capture_limit_drains_but_does_not_retain_unbounded_output() {
        let tool = CommandTool::with_limits(
            PathBuf::from("."),
            CommandLimits {
                // This is a capture test, not a shell-startup benchmark. Leave
                // headroom for loaded native Windows hosts; the in-memory test
                // above exercises backpressure without subprocess startup.
                timeout: Duration::from_secs(30),
                capture_bytes: 4 * 1024,
            },
        )
        .expect("valid limits");
        // Shell builtins avoid starting an external output generator. Emit
        // 256 KiB on EACH pipe in chunks, with distinct heads and tails.
        let output = run(
            &tool,
            "printf OUT_HEAD; printf ERR_HEAD >&2; \
             i=0; while [ \"$i\" -lt 64 ]; do \
                 printf '%4096s' ''; printf '%4096s' '' >&2; i=$((i + 1)); \
             done; printf OUT_TAIL; printf ERR_TAIL >&2",
        )
        .await
        .expect("large-output command should complete without pipe deadlock");

        let padding = " ".repeat(1024 - "OUT_HEAD".len());
        let discarded = 64 * 4096 + "OUT_HEAD".len() + "OUT_TAIL".len() - 2048;
        let expected_stream = |prefix| {
            format!(
                "{prefix}_HEAD{padding}\n[... {discarded} bytes discarded while draining bounded command output ...]\n{padding}{prefix}_TAIL"
            )
        };
        assert_eq!(
            output,
            format!(
                "status: success\nexit_code: 0\nstdout:\n{}\nstderr:\n{}",
                expected_stream("OUT"),
                expected_stream("ERR"),
            )
        );
        assert!(
            output.len() < 6 * 1024,
            "bounded capture should stay close to its configured limit: {} bytes",
            output.len()
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn timeout_terminates_the_process_group_and_returns_partial_output() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("finished");
        let tool = CommandTool::with_limits(
            directory.path().to_path_buf(),
            CommandLimits {
                // Leave enough time for a loaded CI host to spawn the shell
                // and flush its first bytes before exercising termination.
                timeout: Duration::from_millis(if cfg!(windows) { 1500 } else { 250 }),
                capture_bytes: 4 * 1024,
            },
        )
        .expect("valid limits");

        let error = run(
            &tool,
            "printf started; sleep 10; printf finished > finished",
        )
        .await
        .expect_err("command should time out");
        let CommandError::Timeout { output, .. } = error else {
            panic!("expected timeout error, got {error:?}");
        };
        assert!(output.contains("status: terminated"));
        assert!(output.contains("stdout:\nstarted"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "a timed-out descendant must not survive");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn turn_cancellation_terminates_the_process_group_and_marks_the_tool() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("finished");
        let tool = CommandTool::new(directory.path().to_path_buf());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let turn = TurnContext::new(TurnId::new(4), SessionMode::Build, cancellation.clone());
        let worker = tokio::spawn(async move {
            let mut context = ToolContext::new();
            context.insert_scope(turn);
            let result = tool
                .call(
                    &mut context,
                    CommandArgs {
                        command: "printf started; sleep 10; printf finished > finished".to_string(),
                        background: None,
                    },
                )
                .await;
            (result, context)
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
        let (result, context) = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("cancelled command should resolve promptly")
            .expect("command worker should not panic");
        let error = result.expect_err("cancelled command is a tool error");
        assert!(matches!(error, CommandError::Cancelled { .. }));
        assert!(
            context
                .result::<ToolCancelled>()
                .expect("result metadata decodes")
                .is_some()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "a cancelled descendant must not survive");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_completed_shell_cannot_leave_a_background_descendant_or_open_capture_pipe() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("leaked");
        let tool = CommandTool::new(directory.path().to_path_buf());

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            run(
                &tool,
                "(sleep 0.3; printf leaked > leaked) & printf foreground-done",
            ),
        )
        .await
        .expect("background-held pipes must not wedge completed commands")
        .expect("foreground shell should succeed");
        assert!(output.contains("foreground-done"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!marker.exists(), "background descendants must be reaped");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn aborting_the_worker_kills_the_command_child() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let marker = directory.path().join("finished");
        let tool = CommandTool::new(directory.path().to_path_buf());
        let worker =
            tokio::spawn(async move { run(&tool, "sleep 0.4; printf finished > finished").await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        worker.abort();
        let _ = worker.await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert!(
            !marker.exists(),
            "the shell should not survive an aborted command worker"
        );
    }
}
