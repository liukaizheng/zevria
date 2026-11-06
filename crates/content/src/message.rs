use rig_core::message::{AssistantContent, Message, ToolCall, ToolFunction};

/// Preserve Responses' output-item ID separately from its call correlator.
pub fn tool_call_from_wire(
    item_id: impl Into<String>,
    call_id: impl Into<String>,
    function: ToolFunction,
) -> ToolCall {
    let arguments = function.raw_arguments();
    tool_call_from_wire_text(item_id, call_id, function, arguments)
}

/// Ingest the exact argument text before Rig normalizes its canonical object.
pub fn tool_call_from_wire_text(
    item_id: impl Into<String>,
    call_id: impl Into<String>,
    function: ToolFunction,
    arguments: String,
) -> ToolCall {
    let item_id: String = item_id.into();
    let call_id: String = call_id.into();
    let arguments = if arguments.trim().is_empty() {
        function.raw_arguments()
    } else {
        arguments
    };
    let item = serde_json::json!({
        "type": "function_call", "id": item_id, "call_id": call_id,
        "name": function.name, "arguments": arguments, "status": "completed",
    });
    let call = ToolCall::from_wire(item["call_id"].as_str().unwrap_or_default(), function);
    match AssistantContent::ToolCall(call).with_native(item) {
        AssistantContent::ToolCall(call) => call,
        _ => unreachable!("attaching native data preserves the block kind"),
    }
}

/// Copy the wire text while its native attachment still matches this call.
pub fn raw_tool_arguments(call: &ToolCall) -> String {
    let block = AssistantContent::ToolCall(call.clone());
    block
        .native_item()
        .and_then(|item| item.get("arguments"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| call.function.raw_arguments())
}

/// Preserve malformed arguments for display and validation, not a recovered object.
pub fn tool_arguments(function: &ToolFunction) -> serde_json::Value {
    function
        .invalid_arguments
        .as_ref()
        .map(|text| serde_json::Value::String(text.clone()))
        .unwrap_or_else(|| function.arguments_value())
}

/// The plain text of an assistant message: its text blocks joined with
/// newlines, preserving readable citation links and ignoring reasoning/tool calls.
pub fn assistant_plain_text(message: &Message) -> String {
    let Message::Assistant(assistant) = message else {
        return String::new();
    };
    assistant
        .content
        .iter()
        .filter_map(|item| match item {
            AssistantContent::Text(text) => Some(crate::citations::render_text(text)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::ToolName;
    use serde_json::json;

    #[test]
    fn wire_argument_text_survives_storage_but_not_a_canonical_edit() {
        let raw = r#"{ "command": "echo 界", "ratio": 1.0 }"#;
        let function = ToolFunction::parse(ToolName::new("command").unwrap(), raw);
        let call = tool_call_from_wire_text("fc_original", "call_original", function, raw.into());
        assert_eq!(raw_tool_arguments(&call), raw);
        let mut restored: ToolCall =
            serde_json::from_str(&serde_json::to_string(&call).unwrap()).unwrap();
        assert_eq!(raw_tool_arguments(&restored), raw);
        restored
            .function
            .arguments
            .insert("command".into(), json!("changed"));
        assert_eq!(
            raw_tool_arguments(&restored),
            restored.function.arguments_json()
        );
        assert_ne!(raw_tool_arguments(&restored), raw);
    }
}
