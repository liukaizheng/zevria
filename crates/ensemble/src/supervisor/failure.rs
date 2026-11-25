//! ACP error classification and worker failure diagnostics.

use agent_client_protocol::is_incoming_transport_closed;
use agent_client_protocol::schema::v1::{Error as AcpError, ErrorCode};
use std::fmt::Display;
use std::sync::Mutex;

const ACP_PROCESS_EXIT_DATA_PREFIX: &str = "Process exited with ";

pub(super) fn error_with_login_hint(error: &AcpError, login_hint: &str) -> String {
    // Render data once in a stable form. ACP Display may pretty-print JSON or
    // quote/escape string data, which otherwise duplicates the same diagnostic.
    let mut message_only = error.clone();
    message_only.data = None;
    let mut rendered = message_only.to_string();
    // Native runtimes may carry their entire diagnostic in ACP error data.
    if let Some(data) = &error.data {
        let detail = data
            .as_str()
            .map_or_else(|| data.to_string(), str::to_string);
        if !rendered.contains(&detail) {
            rendered.push_str(": ");
            rendered.push_str(&detail);
        }
    }
    let lower = rendered.to_ascii_lowercase();
    if error.code == ErrorCode::AuthRequired
        || ["auth", "login", "unauthorized", "401"]
            .iter()
            .any(|needle| lower.contains(needle))
    {
        format!("{rendered}. {login_hint}")
    } else {
        rendered
    }
}

pub(super) fn is_unexpected_process_termination(error: &AcpError) -> bool {
    if is_incoming_transport_closed(error) {
        return true;
    }
    let mut data = error.data.as_ref();
    while let Some(value) = data {
        if value
            .as_str()
            .is_some_and(|data| data.starts_with(ACP_PROCESS_EXIT_DATA_PREFIX))
        {
            return true;
        }
        let Some(inner) = spawned_task_error_data(value) else {
            return false;
        };
        if is_incoming_transport_closed(&error.clone().data(inner.clone())) {
            return true;
        }
        data = Some(inner);
    }
    false
}

pub(super) fn transient_prompt_error_kind(error: &AcpError) -> Option<&'static str> {
    if is_unexpected_process_termination(error) {
        return None;
    }
    let mut data = error.data.as_ref();
    while let Some(value) = data {
        match value.get("errorKind").and_then(serde_json::Value::as_str) {
            Some("server_error") => return Some("server_error"),
            Some("overloaded") => return Some("overloaded"),
            Some("rate_limit") => return Some("rate_limit"),
            _ => data = spawned_task_error_data(value),
        }
    }
    None
}

fn spawned_task_error_data(value: &serde_json::Value) -> Option<&serde_json::Value> {
    let object = value.as_object()?;
    (object.len() == 2 && object.get("spawned_at")?.is_string())
        .then(|| object.get("data"))
        .flatten()
}

fn process_exit_data(error: &AcpError) -> Option<&str> {
    let mut data = error.data.as_ref();
    while let Some(value) = data {
        if let Some(data) = value.as_str()
            && data.starts_with(ACP_PROCESS_EXIT_DATA_PREFIX)
        {
            return Some(data);
        }
        data = spawned_task_error_data(value);
    }
    None
}

pub(super) fn concise_process_failure(error: &AcpError) -> String {
    const MAX_CHARS: usize = 512;

    let detail = process_exit_data(error).unwrap_or(&error.message);
    let compact = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        format!("{}…", compact.chars().take(MAX_CHARS).collect::<String>())
    }
}

pub(super) fn acp_error(error: impl Display) -> AcpError {
    AcpError::internal_error().data(error.to_string())
}

pub(super) fn set_violation(target: &Mutex<Option<String>>, error: String) {
    let mut target = target.lock().expect("ACP violation lock poisoned");
    target.get_or_insert(error);
}
