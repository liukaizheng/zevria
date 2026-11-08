use super::*;
use std::time::Duration;
use std::time::SystemTime;
use zevria_foundation::{CommandJobMode, TerminationOrigin};

fn snapshot(status: CommandJobStatus, seconds: f64) -> CommandJobSnapshot {
    let started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    CommandJobSnapshot {
        id: CommandJobId::new("job-a1b2c3"),
        call_id: Some("call-1".into()),
        command: "cargo test --workspace".into(),
        mode: CommandJobMode::Background,
        status,
        started_at,
        finished_at: Some(started_at + Duration::from_secs_f64(seconds)),
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}
fn exited(code: i32, seconds: f64) -> CommandJobSnapshot {
    snapshot(
        CommandJobStatus::Exited {
            code: Some(code),
            success: code == 0,
            signal: None,
        },
        seconds,
    )
}
fn text(head: &str) -> StreamText {
    StreamText {
        head: head.into(),
        ..StreamText::default()
    }
}
fn streams(stdout: &str, stderr: &str) -> (StreamText, StreamText) {
    (text(stdout), text(stderr))
}
fn marker(n: usize) -> String {
    format!(
        "[... {n} bytes omitted; narrow the command or redirect its output to a file to see them ...]"
    )
}
fn assert_format(actual: &str, expected: &str) {
    assert_eq!(actual, expected);
    for forbidden in [
        "stdout:",
        "stderr:",
        "<stdout>\n</stdout>",
        "<stderr>\n</stderr>",
    ] {
        assert!(
            !actual.contains(forbidden),
            "unexpected {forbidden:?} in {actual:?}"
        );
    }
}

#[test]
fn foreground_shapes_are_exact_and_omit_empty_streams() {
    for (stdout, stderr, expected) in [
        ("", "", "exit_code: 0\nduration: 0.4s"),
        (
            "src/lib.rs\n",
            "",
            "exit_code: 0\nduration: 0.4s\n<stdout>\nsrc/lib.rs\n</stdout>",
        ),
        (
            "",
            "warning",
            "exit_code: 0\nduration: 0.4s\n<stderr>\nwarning\n</stderr>",
        ),
        (
            "out",
            "err\n",
            "exit_code: 0\nduration: 0.4s\n<stdout>\nout\n</stdout>\n<stderr>\nerr\n</stderr>",
        ),
    ] {
        assert_format(
            &foreground_result(&exited(0, 0.4), &streams(stdout, stderr)),
            expected,
        );
    }
    assert_format(
        &foreground_result(
            &exited(101, 38.2),
            &streams(
                "test edge ... FAILED\n",
                "error: test failed, to rerun pass `--lib`\n",
            ),
        ),
        "exit_code: 101\nduration: 38.2s\n<stdout>\ntest edge ... FAILED\n</stdout>\n<stderr>\nerror: test failed, to rerun pass `--lib`\n</stderr>",
    );
}

#[test]
fn stream_content_and_newlines_are_preserved_without_escaping() {
    assert_eq!(
        stream_blocks(&text("first\nstderr:\n</stdout>\n\n"), &text("\n")),
        "<stdout>\nfirst\nstderr:\n</stdout>\n\n</stdout>\n<stderr>\n\n</stderr>",
    );
    assert_eq!(stream_blocks(&text(""), &text("")), "");
}

#[test]
fn fitting_streams_are_verbatim_even_with_contiguous_segments() {
    assert_eq!(elide(&StreamText::default()), None);
    for input in [
        text(&"x".repeat(MAX_OUTPUT_STREAM_BYTES)),
        StreamText {
            head: "é".repeat(5_000),
            discarded: 0,
            tail: "x".repeat(500),
        },
    ] {
        assert_eq!(elide(&input), Some(format!("{}{}", input.head, input.tail)));
    }
}

#[test]
fn model_elision_keeps_eight_kib_on_each_side_with_one_marker() {
    let input = text(&format!("{}{}", "a".repeat(10_000), "b".repeat(10_000)));
    let expected = format!(
        "{}\n{}\n{}",
        "a".repeat(8_192),
        marker(3_616),
        "b".repeat(8_192)
    );
    assert_eq!(elide(&input).unwrap(), expected);
    assert_format(
        &stream_blocks(&input, &text("")),
        &format!("<stdout>\n{expected}\n</stdout>"),
    );
}

#[test]
fn capture_and_model_elision_counts_are_combined() {
    let input = StreamText {
        head: "a".repeat(20_000),
        discarded: 100_000,
        tail: "b".repeat(20_000),
    };
    let output = elide(&input).unwrap();
    assert_eq!(
        output,
        format!(
            "{}\n{}\n{}",
            "a".repeat(8_192),
            marker(123_616),
            "b".repeat(8_192)
        )
    );
    assert_eq!(output.matches("bytes omitted;").count(), 1);
    assert!(!output.contains("discarded"));
}

#[test]
fn tiny_capture_and_unequal_segments_never_invent_or_duplicate_bytes() {
    for (head, discarded, tail, expected) in [
        ("", 9, "z", format!("{}\nz", marker(9))),
        ("a", 8, "z", format!("a\n{}\nz", marker(8))),
        ("HEAD", 6, "TAIL", format!("HEAD\n{}\nTAIL", marker(6))),
        ("head\n", 42, "tail", format!("head\n{}\ntail", marker(42))),
        ("", 12, "", marker(12)),
        ("head", 12, "", format!("head\n{}", marker(12))),
    ] {
        assert_eq!(
            elide(&StreamText {
                head: head.into(),
                discarded,
                tail: tail.into()
            })
            .unwrap(),
            expected
        );
    }
    for limit in [1, 2, 21, 42] {
        let mut buffer = crate::StreamBuffer::new(limit);
        buffer.append(&vec![b'x'; 100]);
        let retained = buffer.retained();
        let rendered = elide(&retained).unwrap();
        assert_eq!(retained.discarded, 100 - limit);
        let expected = format!(
            "{}{}{}\n{}",
            "x".repeat(limit / 2),
            if limit / 2 > 0 { "\n" } else { "" },
            marker(100 - limit),
            "x".repeat(limit - limit / 2)
        );
        assert_eq!(rendered, expected);
    }
    let input = StreamText {
        head: "HEAD".into(),
        discarded: 100,
        tail: "z".repeat(10_000),
    };
    assert_eq!(
        elide(&input).unwrap(),
        format!("HEAD\n{}\n{}", marker(1_908), "z".repeat(8_192))
    );
    let input = StreamText {
        head: "a".repeat(10_000),
        discarded: 100,
        tail: "TAIL".into(),
    };
    assert_eq!(
        elide(&input).unwrap(),
        format!("{}\n{}\nTAIL", "a".repeat(8_192), marker(1_908))
    );
}

#[test]
fn elision_respects_multibyte_boundaries_and_existing_head_newlines() {
    let input = text(&"你".repeat(6_000));
    let result = elide(&input).unwrap();
    assert_eq!(
        result,
        format!(
            "{}\n{}\n{}",
            "你".repeat(2_730),
            marker(1_620),
            "你".repeat(2_730)
        )
    );
    assert!(!result.contains('\u{fffd}'));
    let input = StreamText {
        head: "é".repeat(5_000),
        discarded: 17,
        tail: "你".repeat(4_000),
    };
    assert_eq!(
        elide(&input).unwrap(),
        format!(
            "{}\n{}\n{}",
            "é".repeat(4_096),
            marker(5_635),
            "你".repeat(2_730)
        )
    );
    let input = text(&format!(
        "{}\n{}{}",
        "a".repeat(8_191),
        "cut".repeat(1_000),
        "b".repeat(8_192)
    ));
    assert_eq!(
        elide(&input).unwrap(),
        format!(
            "{}\n{}\n{}",
            "a".repeat(8_191),
            marker(3_000),
            "b".repeat(8_192)
        )
    );
}

#[test]
fn signal_and_unavailable_exit_headers() {
    let unavailable = snapshot(
        CommandJobStatus::Exited {
            code: None,
            success: false,
            signal: None,
        },
        0.1,
    );
    assert_format(
        &foreground_result(&unavailable, &streams("", "")),
        "exit_code: unavailable\nduration: 0.1s",
    );
    let unknown = snapshot(
        CommandJobStatus::Exited {
            code: None,
            success: false,
            signal: Some(12_345),
        },
        0.1,
    );
    assert_format(
        &foreground_result(&unknown, &streams("", "")),
        "signal: 12345\nduration: 0.1s",
    );
    #[cfg(unix)]
    {
        let killed = snapshot(
            CommandJobStatus::Exited {
                code: None,
                success: false,
                signal: Some(9),
            },
            0.1,
        );
        assert_format(
            &foreground_result(&killed, &streams("", "")),
            "signal: 9 (SIGKILL)\nduration: 0.1s",
        );
        assert_format(
            &job_report(&killed, &streams("", ""), None),
            "id: job-a1b2c3\nstatus: killed by signal 9 (SIGKILL)\nduration: 0.1s",
        );
    }
}

#[test]
fn interrupted_errors_have_only_the_core_status_and_no_exit_header() {
    let timeout = snapshot(CommandJobStatus::TimedOut { seconds: 300 }, 300.0);
    assert_format(
        &interrupted_body(&timeout, &streams("partial", "")),
        "duration: 300.0s\n<stdout>\npartial\n</stdout>",
    );
    let body = interrupted_body(
        &snapshot(
            CommandJobStatus::Terminated {
                origin: TerminationOrigin::User,
            },
            12.0,
        ),
        &streams("", ""),
    );
    assert_format(&body, "duration: 12.0s");
}

#[test]
fn background_start_and_move_shapes() {
    let running = snapshot(CommandJobStatus::Running, 12.0);
    assert_format(
        &background_start(&running.id),
        "id: job-a1b2c3\nstatus: running\nnote: started in the background; use command_status to read output or wait, and command_terminate to stop it",
    );
    assert_format(
        &job_report(
            &running,
            &streams("output so far", ""),
            Some(MOVED_TO_BACKGROUND_NOTE),
        ),
        "id: job-a1b2c3\nstatus: running\nduration: 12.0s\nnote: the user moved this command to the background; use command_status for later output and command_terminate to stop it\n<stdout>\noutput so far\n</stdout>",
    );
    assert_format(
        &job_report(&running, &streams("", ""), None),
        "id: job-a1b2c3\nstatus: running\nduration: 12.0s",
    );
}

#[test]
fn job_reports_include_only_status_duration_and_unread_streams() {
    assert_format(
        &job_report(&exited(1, 75.3), &streams("", "error: …"), None),
        "id: job-a1b2c3\nstatus: exited 1\nduration: 75.3s\n<stderr>\nerror: …\n</stderr>",
    );
    assert_format(
        &job_report(&exited(1, 75.3), &streams("", ""), None),
        "id: job-a1b2c3\nstatus: exited 1\nduration: 75.3s",
    );
    for (origin, phrase) in [
        (TerminationOrigin::Model, "terminated by the agent"),
        (TerminationOrigin::User, "terminated by the user"),
        (TerminationOrigin::TurnCancelled, "cancelled with the turn"),
        (TerminationOrigin::Aborted, "aborted"),
        (TerminationOrigin::Shutdown, "stopped at session shutdown"),
    ] {
        let job = snapshot(CommandJobStatus::Terminated { origin }, 12.0);
        assert_format(
            &job_report(&job, &streams("", ""), None),
            &format!("id: job-a1b2c3\nstatus: {phrase}\nduration: 12.0s"),
        );
    }
}

#[test]
fn duration_uses_finished_time_and_clamps_reversed_clocks() {
    assert_eq!(duration(&exited(0, 75.34)), "75.3s");
    assert_eq!(duration(&exited(0, 75.36)), "75.4s");
    let mut job = exited(0, 1.0);
    job.finished_at = Some(job.started_at - Duration::from_secs(1));
    assert_eq!(duration(&job), "0.0s");
    job.finished_at = None;
    job.started_at = SystemTime::now() + Duration::from_secs(60);
    assert_eq!(duration(&job), "0.0s");
    job.started_at = SystemTime::now() - Duration::from_secs(2);
    let elapsed: f64 = duration(&job).trim_end_matches('s').parse().unwrap();
    assert!(elapsed >= 2.0);
}

#[test]
fn list_uses_compact_job_lines() {
    let running = snapshot(CommandJobStatus::Running, 41.0);
    let mut done = exited(0, 3.2);
    done.id = CommandJobId::new("job-d4e5f6");
    done.command = "rg -n foo src".into();
    assert_format(
        &format!("{}\n{}", job_line(&running), job_line(&done)),
        "job-a1b2c3 · running · 41.0s · cargo test --workspace\njob-d4e5f6 · exited 0 · 3.2s · rg -n foo src",
    );
}

#[test]
fn job_previews_collapse_whitespace_and_cap_at_120_characters() {
    let mut job = exited(0, 3.2);
    job.command = "  rg\t-n\r\nfoo\u{2003}src\n ".into();
    assert_eq!(
        job_line(&job),
        "job-a1b2c3 · exited 0 · 3.2s · rg -n foo src"
    );
    job.command = "é".repeat(120);
    assert_eq!(
        job_line(&job),
        format!("job-a1b2c3 · exited 0 · 3.2s · {}", "é".repeat(120))
    );
    job.command.push('x');
    assert_eq!(
        job_line(&job),
        format!("job-a1b2c3 · exited 0 · 3.2s · {}…", "é".repeat(119))
    );
}
