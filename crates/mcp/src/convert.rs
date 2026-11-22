//! Bounded conversion from external MCP content to trusted Zevria representations.
use rig_core::{
    message::ToolResultContent,
    tool::{ToolExecutionError, ToolOutput},
};
use rmcp::model::{
    CallToolResult, ContentBlock, GetPromptResult, ReadResourceResult, ResourceContents, Role,
};
use serde_json::{Map, Value, json};
use zevria_content::{PromptBlock, PromptImage, UserPrompt, image_preparation};
use zevria_foundation::McpServerName;

pub(crate) fn normalize_input_schema(schema: &Map<String, Value>) -> Value {
    let mut schema = schema.clone();
    if schema.get("type") != Some(&Value::String("object".into())) {
        schema.insert("type".into(), json!("object"));
    }
    if !schema.get("properties").is_some_and(Value::is_object) {
        schema.insert("properties".into(), json!({}));
    }
    Value::Object(schema)
}

/// Server display text is never allowed to inject arbitrary terminal controls.
pub(crate) fn display_text(text: &str, cap: usize) -> String {
    text[..text.floor_char_boundary(cap)]
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

pub(crate) fn elide(text: &str, limit: usize) -> String {
    const MARKER: &str = "\n[MCP output truncated]\n";
    if text.len() <= limit {
        return text.to_owned();
    }
    let Some(room) = limit.checked_sub(MARKER.len()) else {
        return MARKER[..limit].into();
    };
    let head = text.floor_char_boundary(room / 2);
    let tail = text.ceil_char_boundary(text.len() - (room - room / 2));
    format!("{}{MARKER}{}", &text[..head], &text[tail..])
}

#[derive(Clone, Copy)]
pub(crate) struct ConversionStats {
    pub blocks: usize,
    pub truncated: bool,
}

struct Output {
    blocks: Vec<PromptBlock>,
    limit: usize,
    image_count: usize,
    image_bytes: usize,
    truncated: bool,
}

impl Output {
    fn new(limit: usize) -> Self {
        Self {
            blocks: vec![],
            limit,
            image_count: 0,
            image_bytes: 0,
            truncated: false,
        }
    }

    fn can_append(&mut self) -> bool {
        // A million empty blocks can otherwise exceed the provider context even
        // if the transport and aggregate text stay within byte limits.
        if self.blocks.len() < 128 {
            return true;
        }
        self.truncated = true;
        false
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() || !self.can_append() {
            return;
        }
        if text.len() > self.limit {
            self.truncated = true;
        }
        if self.limit != 0 {
            let text = elide(text, self.limit);
            self.limit -= text.len();
            self.blocks.push(PromptBlock::Text(text));
        }
    }

    fn image(&mut self, mime: &str, data: &str) {
        if !self.can_append() {
            return;
        }
        if self.image_count >= 8 || self.image_bytes >= 20 * 1024 * 1024 {
            self.truncated = true;
            self.text("[MCP image omitted: image budget exhausted]");
            return;
        }
        match image_preparation::prepare_base64(mime, data).map(|prepared| prepared.image) {
            Ok(image) if self.image_bytes + image.encoded_len() <= 20 * 1024 * 1024 => {
                self.prepared_image(image)
            }
            Ok(_) => {
                self.truncated = true;
                self.text("[MCP image omitted: image budget exhausted]");
            }
            Err(error) => {
                self.truncated = true;
                self.text(&format!("[MCP image omitted: {error}]"));
            }
        }
    }

    fn prepared_image(&mut self, image: PromptImage) {
        self.image_count += 1;
        self.image_bytes += image.encoded_len();
        self.blocks.push(PromptBlock::Image(image));
    }

    fn content(&mut self, content: &ContentBlock, server: &McpServerName, allow_images: bool) {
        match content {
            ContentBlock::Text(text) => self.text(&text.text),
            ContentBlock::Image(image) if allow_images => self.image(&image.mime_type, &image.data),
            ContentBlock::Image(_) => self.text("[MCP image omitted from tool error]"),
            ContentBlock::Audio(audio) => self.text(&format!(
                "[MCP audio omitted: {}, approximately {} bytes]",
                display_text(&audio.mime_type, 128),
                audio.data.len() * 3 / 4
            )),
            ContentBlock::Resource(resource) => self.resource(&resource.resource, allow_images),
            ContentBlock::ResourceLink(link) => self.text(&format!(
                "[MCP resource link: {}. Read with mcp_read_resource (server: {server}, URI: {}).]",
                display_text(&link.name, 256),
                display_text(&link.uri, 8192),
            )),
            _ => self.text("[Unsupported MCP content omitted]"),
        }
    }

    fn resource(&mut self, resource: &ResourceContents, allow_images: bool) {
        match resource {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => {
                self.resource_header(uri, mime_type.as_deref().unwrap_or("text/plain"));
                self.text(text);
            }
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => {
                let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                self.resource_header(uri, mime);
                if allow_images && mime.starts_with("image/") {
                    self.image(mime, blob);
                } else {
                    self.text(&format!(
                        "[MCP binary resource omitted: approximately {} bytes]",
                        blob.len() * 3 / 4
                    ));
                }
            }
            _ => self.text("[Unsupported MCP resource omitted]"),
        }
    }

    fn resource_header(&mut self, uri: &str, mime: &str) {
        self.text(&format!(
            "[MCP resource: {} ({})]\n",
            display_text(uri, 8192),
            display_text(mime, 128)
        ));
    }

    fn into_tool_output(self) -> Result<ToolOutput, ToolExecutionError> {
        let mut blocks: Vec<_> = self
            .blocks
            .into_iter()
            .map(|block| match block {
                PromptBlock::Text(text) => ToolResultContent::text(text),
                PromptBlock::Image(image) => image.to_tool_result_content(),
            })
            .collect();
        if blocks.is_empty() {
            blocks.push(ToolResultContent::text("[MCP returned no content]"));
        }
        ToolOutput::content(blocks).map_err(|error| ToolExecutionError::other(error.to_string()))
    }
}

pub(crate) fn call_tool_result_to_output(
    result: CallToolResult,
    server: &McpServerName,
    limit: usize,
) -> (Result<ToolOutput, ToolExecutionError>, ConversionStats) {
    let structured = result
        .structured_content
        .map(|json| json.to_string())
        .filter(|json| {
            !result
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text(text) if text.text.trim() == json))
        });
    let reserve = structured
        .as_ref()
        .map_or(0, |json| json.len().min(limit / 3));
    let mut output = Output::new(limit - reserve);
    let is_error = result.is_error == Some(true);
    for content in &result.content {
        output.content(content, server, !is_error);
    }
    output.limit += reserve;
    if let Some(json) = structured {
        output.text(&json);
    }
    let stats = ConversionStats {
        blocks: result.content.len(),
        truncated: output.truncated,
    };
    let result = if is_error {
        let text = output
            .blocks
            .into_iter()
            .filter_map(|block| match block {
                PromptBlock::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Err(ToolExecutionError::other(if text.is_empty() {
            "MCP server reported a tool error".into()
        } else {
            elide(&text, limit)
        }))
    } else {
        output.into_tool_output()
    };
    (result, stats)
}

pub(crate) fn read_resource_result_to_output(
    result: ReadResourceResult,
    limit: usize,
) -> (Result<ToolOutput, ToolExecutionError>, ConversionStats) {
    let mut output = Output::new(limit);
    for resource in &result.contents {
        output.resource(resource, true);
    }
    let stats = ConversionStats {
        blocks: result.contents.len(),
        truncated: output.truncated,
    };
    (output.into_tool_output(), stats)
}

pub(crate) fn get_prompt_result_to_user_prompt(
    result: GetPromptResult,
    server: &McpServerName,
    prompt: &str,
    limit: usize,
) -> anyhow::Result<UserPrompt> {
    anyhow::ensure!(
        !result.messages.is_empty(),
        "MCP prompt returned no messages"
    );
    let mut output = Output::new(limit);
    output.text(&format!(
        "[MCP prompt: {server}:{}]\n",
        display_text(prompt, 256)
    ));
    for message in &result.messages {
        // Keep the entire expansion user-controlled, never manufacture assistant
        // messages or elevate server text to model instructions.
        output.text(match message.role {
            Role::User => "\n[MCP user]\n",
            Role::Assistant => "\n[MCP assistant example]\n",
        });
        output.content(&message.content, server, true);
    }
    Ok(UserPrompt::new(output.blocks)?)
}
