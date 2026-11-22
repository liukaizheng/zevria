//! Display-only view of the shell tool's wire result. Clipboard actions keep the original.

#[derive(Debug)]
pub enum CommandOutput<'a> {
    Envelope(CommandResult<'a>),
    Raw(&'a str),
}

#[derive(Debug, Default)]
pub struct CommandResult<'a> {
    pub exit_code: Option<&'a str>,
    pub signal: Option<&'a str>,
    pub duration: Option<&'a str>,
    pub id: Option<&'a str>,
    pub status: Option<&'a str>,
    pub note: Option<&'a str>,
    pub error: Option<&'a str>,
    pub stdout: Option<&'a str>,
    pub stderr: Option<&'a str>,
}

impl<'a> CommandOutput<'a> {
    pub fn parse(raw: &'a str) -> Self {
        parse_envelope(raw).map_or(Self::Raw(raw), Self::Envelope)
    }

    pub fn duration(&self) -> Option<&'a str> {
        match self {
            Self::Envelope(result) => result.duration,
            Self::Raw(_) => None,
        }
    }

    pub fn exit_badge(&self) -> Option<String> {
        let Self::Envelope(result) = self else {
            return None;
        };
        if let Some(signal) = result
            .signal
            .or_else(|| result.status?.strip_prefix("killed by signal "))
        {
            return Some(
                signal
                    .split_once(" (")
                    .and_then(|(_, name)| name.strip_suffix(')'))
                    .map_or_else(|| format!("signal {signal}"), str::to_owned),
            );
        }
        let code = result
            .exit_code
            .or_else(|| result.status?.strip_prefix("exited "))?;
        (code != "0").then(|| format!("exit {code}"))
    }

    pub fn output_line_count(&self) -> Option<usize> {
        let lines = match self {
            Self::Envelope(result) => result
                .stdout
                .into_iter()
                .chain(result.stderr)
                .map(|s| s.lines().count())
                .sum(),
            Self::Raw(text) => text.lines().count(),
        };
        (lines > 0).then_some(lines)
    }
}

fn parse_envelope(raw: &str) -> Option<CommandResult<'_>> {
    let (header, streams) = raw
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let at = *offset;
            *offset += line.len();
            Some((at, line))
        })
        .find(|(_, line)| matches!(*line, "<stdout>\n" | "<stderr>\n"))
        .map_or((raw, None), |(at, _)| (&raw[..at], Some(&raw[at..])));

    let mut out = CommandResult::default();
    let header = if let Some(error) = header.strip_prefix("status: error\nerror: ") {
        out.status = Some("error");
        // Core's error message can span lines; optional duration/note follow it.
        if let Some((message, suffix)) = error.rsplit_once("\nduration: ")
            && suffix.lines().next().is_some_and(valid_duration)
        {
            out.error = Some(message);
            &error[message.len() + 1..]
        } else {
            out.error = Some(error);
            ""
        }
    } else {
        header
    };
    for line in header.lines() {
        let (key, value) = line.split_once(": ")?;
        let slot = match key {
            "exit_code" if value == "unavailable" || value.parse::<i32>().is_ok() => {
                &mut out.exit_code
            }
            "signal" if value.split_whitespace().next()?.parse::<i32>().is_ok() => &mut out.signal,
            "duration" if valid_duration(value) => &mut out.duration,
            "id" if !value.is_empty() => &mut out.id,
            "status" if !value.is_empty() => &mut out.status,
            "note" => &mut out.note,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    if out.duration.is_none() && !(out.id.is_some() && out.status.is_some()) && out.error.is_none()
    {
        return None;
    }
    if let Some(streams) = streams {
        // The writer emits the real closing tag last, not the first close-shaped
        // line in a stream. A complete forged boundary is ambiguous on this wire.
        if let Some(stdout) = streams.strip_prefix("<stdout>\n") {
            if let Some(text) = stdout.strip_suffix("\n</stdout>") {
                out.stdout = Some(text);
            } else {
                let pair = stdout.strip_suffix("\n</stderr>")?;
                let (stdout, stderr) = pair.rsplit_once("\n</stdout>\n<stderr>\n")?;
                out.stdout = Some(stdout);
                out.stderr = Some(stderr);
            }
        } else {
            out.stderr = Some(
                streams
                    .strip_prefix("<stderr>\n")?
                    .strip_suffix("\n</stderr>")?,
            );
        }
    }
    Some(out)
}

fn valid_duration(text: &str) -> bool {
    text.strip_suffix('s')
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(|seconds| seconds.is_finite() && seconds >= 0.)
}

pub fn is_redundant_input(header_cmd: &str, raw_cmd: &str) -> bool {
    !raw_cmd.contains(['\r', '\n']) && header_cmd == raw_cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use zevria_foundation::{CommandJobId, CommandJobMode, CommandJobSnapshot, CommandJobStatus};
    use zevria_session_api::{StreamText, command_render as wire};

    fn job(status: CommandJobStatus) -> CommandJobSnapshot {
        let at = std::time::SystemTime::UNIX_EPOCH;
        CommandJobSnapshot {
            id: CommandJobId::new("job-test"),
            call_id: None,
            command: "cargo fmt".into(),
            mode: CommandJobMode::Foreground,
            status,
            started_at: at,
            finished_at: Some(at + std::time::Duration::from_millis(1800)),
            stdout_bytes: 0,
            stderr_bytes: 0,
        }
    }

    fn streams(out: &str, err: &str) -> (StreamText, StreamText) {
        let text = |s: &str| StreamText {
            head: s.to_owned(),
            tail: String::new(),
            discarded: 0,
        };
        (text(out), text(err))
    }

    #[test]
    fn foreground_exit_duration_and_empty_output() {
        for (code, success, signal, badge) in [
            (Some(0), true, None, None),
            (Some(101), false, None, Some("exit 101")),
            (None, false, None, Some("exit unavailable")),
            (None, false, Some(12345), Some("signal 12345")),
        ] {
            let raw = wire::foreground_result(
                &job(CommandJobStatus::Exited {
                    code,
                    success,
                    signal,
                }),
                &streams("", ""),
                None,
            );
            let parsed = CommandOutput::parse(&raw);
            assert_eq!(parsed.duration(), Some("1.8s"));
            assert_eq!(parsed.exit_badge().as_deref(), badge);
            assert_eq!(parsed.output_line_count(), None);
        }
        assert_eq!(
            CommandOutput::parse("signal: 15 (SIGTERM)\nduration: 0.7s")
                .exit_badge()
                .as_deref(),
            Some("SIGTERM")
        );
    }

    #[test]
    fn background_job_report_and_interrupted_core_envelopes() {
        let job = job(CommandJobStatus::Running);
        let start = wire::background_start(&job.id);
        let CommandOutput::Envelope(start) = CommandOutput::parse(&start) else {
            panic!("background start")
        };
        assert_eq!(start.id, Some("job-test"));
        assert_eq!(start.status, Some("running"));
        assert!(start.note.unwrap().starts_with("started in the background"));
        assert_eq!(start.duration, None);
        let report = wire::job_report(
            &job,
            &streams("", "one\ntwo"),
            Some(wire::MOVED_TO_BACKGROUND_NOTE),
        );
        let CommandOutput::Envelope(report) = CommandOutput::parse(&report) else {
            panic!("job report")
        };
        assert_eq!(report.stderr, Some("one\ntwo"));
        assert_eq!(report.stdout, None);
        assert_eq!(report.note, Some(wire::MOVED_TO_BACKGROUND_NOTE));
        let body = wire::interrupted_body(&job, &streams("partial", ""), None);
        let interrupted = format!("status: error\nerror: Stopped\nUser cancelled\n{body}");
        let CommandOutput::Envelope(result) = CommandOutput::parse(&interrupted) else {
            panic!("interrupted")
        };
        assert_eq!(result.error, Some("Stopped\nUser cancelled"));
        assert_eq!(result.stdout, Some("partial"));
        assert_eq!(result.exit_code, None);
        assert!(matches!(
            CommandOutput::parse(&body),
            CommandOutput::Envelope(_)
        ));
        assert_eq!(
            CommandOutput::parse("id: j\nstatus: exited 101\nduration: 1.8s")
                .exit_badge()
                .as_deref(),
            Some("exit 101")
        );
    }

    #[test]
    fn outer_markers_are_removed_only_at_stream_boundaries() {
        for (out, err) in [
            ("first\n</stdout>\nlast", ""),
            ("first\n</stdout>\nlast", "</stderr>\nlast"),
            ("", "</stderr>\nlast"),
            ("</stdout>", ""),
            ("\n", "\n"),
        ] {
            let result =
                wire::foreground_result(&job(CommandJobStatus::Running), &streams(out, err), None);
            let CommandOutput::Envelope(parsed) = CommandOutput::parse(&result) else {
                panic!("{result}")
            };
            let expect =
                |s: &str| (!s.is_empty()).then(|| s.strip_suffix('\n').unwrap_or(s).to_owned());
            assert_eq!(parsed.stdout, expect(out).as_deref());
            assert_eq!(parsed.stderr, expect(err).as_deref());
        }
        assert_eq!(
            CommandOutput::parse("duration: 0.1s\n<stdout>\na\nb\n</stdout>").output_line_count(),
            Some(2)
        );
    }

    #[test]
    fn non_envelopes_and_incomplete_streams_remain_literal() {
        for raw in [
            "cannot start process",
            "id: only-a-fixture",
            "exit_code: nonsense\nduration: 5s",
            "duration: 5s\n<stdout>\nno end",
            "<stdout>\nraw",
        ] {
            assert!(matches!(CommandOutput::parse(raw), CommandOutput::Raw(value) if value == raw));
        }
        assert!(is_redundant_input("cargo fmt", "cargo fmt"));
        for raw in [
            "cargo fmt\n",
            "cargo fmt\necho done",
            "cargo fmt > /dev/null",
            "cargo fmt … more",
        ] {
            assert!(!is_redundant_input("cargo fmt", raw));
        }
    }
}
