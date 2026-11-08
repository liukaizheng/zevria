//! The model-facing format for all shell results, lists, and completion notices.
use std::{fmt::Write as _, time::SystemTime};
use zevria_foundation::{CommandJobId, CommandJobSnapshot, CommandJobStatus};
use zevria_session_api::StreamText;

pub(crate) const MAX_OUTPUT_STREAM_BYTES: usize = 16 * 1024;
pub(super) const MOVED_TO_BACKGROUND_NOTE: &str = "the user moved this command to the background; use command_status for later output and command_terminate to stop it";
const STARTED_IN_BACKGROUND_NOTE: &str = "started in the background; use command_status to read output or wait, and command_terminate to stop it";

/// Account for capture gaps and model elision in exactly one marker per stream.
pub(crate) fn elide(stream: &StreamText) -> Option<String> {
    if stream.is_empty() {
        return None;
    }
    let retained = stream.head.len() + stream.tail.len();
    // Contiguous segments must be considered together, including when retention
    // left a segment shorter than the model's half-budget.
    let contiguous;
    let (head, tail) = if stream.discarded == 0 {
        contiguous = format!("{}{}", stream.head, stream.tail);
        if retained <= MAX_OUTPUT_STREAM_BYTES {
            return Some(contiguous);
        }
        (contiguous.as_str(), contiguous.as_str())
    } else {
        (stream.head.as_str(), stream.tail.as_str())
    };
    let mut head_end = head.len().min(MAX_OUTPUT_STREAM_BYTES / 2);
    while !head.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = tail.len().saturating_sub(MAX_OUTPUT_STREAM_BYTES / 2);
    while !tail.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let head = &head[..head_end];
    let tail = &tail[tail_start..];
    let omitted = stream.discarded + retained - head.len() - tail.len();
    let mut result = head.to_owned();
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    write!(result, "[... {omitted} bytes omitted; narrow the command or redirect its output to a file to see them ...]")
        .expect("writing to a string cannot fail");
    if !tail.is_empty() {
        result.push('\n');
        result.push_str(tail);
    }
    Some(result)
}

pub(crate) fn stream_blocks(stdout: &StreamText, stderr: &StreamText) -> String {
    let mut result = String::new();
    for (name, stream) in [("stdout", stdout), ("stderr", stderr)] {
        if let Some(text) = elide(stream) {
            if !result.is_empty() {
                result.push('\n');
            }
            writeln!(result, "<{name}>").expect("writing to a string cannot fail");
            result.push_str(&text);
            if !text.ends_with('\n') {
                result.push('\n');
            }
            write!(result, "</{name}>").expect("writing to a string cannot fail");
        }
    }
    result
}

pub(crate) fn duration(snapshot: &CommandJobSnapshot) -> String {
    let elapsed = snapshot
        .finished_at
        .unwrap_or_else(SystemTime::now)
        .duration_since(snapshot.started_at)
        .unwrap_or_default();
    format!("{:.1}s", elapsed.as_secs_f64())
}

pub(crate) fn exit_lines(status: &CommandJobStatus) -> String {
    match status {
        CommandJobStatus::Exited {
            signal: Some(signal),
            ..
        } => {
            #[cfg(unix)]
            if let Some(name) = zevria_foundation::signal_name(*signal) {
                return format!("signal: {signal} ({name})");
            }
            format!("signal: {signal}")
        }
        CommandJobStatus::Exited {
            code: Some(code), ..
        } => format!("exit_code: {code}"),
        _ => "exit_code: unavailable".into(),
    }
}

fn with_streams(mut header: String, streams: &(StreamText, StreamText)) -> String {
    let blocks = stream_blocks(&streams.0, &streams.1);
    if !blocks.is_empty() {
        header.push('\n');
        header.push_str(&blocks);
    }
    header
}

pub(crate) fn foreground_result(
    snapshot: &CommandJobSnapshot,
    streams: &(StreamText, StreamText),
) -> String {
    with_streams(
        format!(
            "{}\nduration: {}",
            exit_lines(&snapshot.status),
            duration(snapshot)
        ),
        streams,
    )
}

/// Core supplies the unchanged `status: error\nerror:` envelope and message.
/// An interrupted foreground command must not add a second status or exit code.
pub(crate) fn interrupted_body(
    snapshot: &CommandJobSnapshot,
    streams: &(StreamText, StreamText),
) -> String {
    with_streams(format!("duration: {}", duration(snapshot)), streams)
}

/// An immediate background acknowledgement has no output read or duration yet.
pub(crate) fn background_start(id: &CommandJobId) -> String {
    format!(
        "id: {id}\nstatus: {}\nnote: {STARTED_IN_BACKGROUND_NOTE}",
        CommandJobStatus::Running
    )
}

pub(crate) fn job_report(
    snapshot: &CommandJobSnapshot,
    streams: &(StreamText, StreamText),
    note: Option<&str>,
) -> String {
    let mut header = format!(
        "id: {}\nstatus: {}\nduration: {}",
        snapshot.id,
        snapshot.status,
        duration(snapshot)
    );
    if let Some(note) = note {
        write!(header, "\nnote: {note}").expect("writing to a string cannot fail");
    }
    with_streams(header, streams)
}

pub(crate) fn job_line(snapshot: &CommandJobSnapshot) -> String {
    let collapsed = snapshot
        .command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = collapsed.chars();
    let mut preview: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    format!(
        "{} · {} · {} · {preview}",
        snapshot.id,
        snapshot.status,
        duration(snapshot)
    )
}

pub(crate) fn job_footer(notices: &[CommandJobSnapshot]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let lines = notices.iter().map(job_line).collect::<Vec<_>>().join("\n");
    format!("\njob updates:\n{lines}")
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
