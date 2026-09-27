use rig_core::message::{AssistantContent, Message};
/// The plain text of an assistant message: its text blocks joined with
/// newlines, preserving readable citation links and ignoring reasoning/tool calls.
pub fn assistant_plain_text(message: &Message) -> String {
    let Message::Assistant { content, .. } = message else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|item| match item {
            AssistantContent::Text(text) => Some(crate::citations::render_text(text)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
