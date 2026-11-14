//! Borrowed structural accounting views. Substitution precedes byte counting;
//! no sanitized payload clones, URL fetches, raster decoding, or transport mutations.
use crate::compaction::{ContextTokenEstimate, approximate_tokens_from_bytes};
use rig_core::message::MimeType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zevria_content::image_facts::{self, ImageFacts, ImageSource};
use zevria_foundation::image_accounting::{ImageAccountingPolicy, ImageDetail};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedMedia {
    pub images: u64,
    pub files: u64,
    pub opaque_items: u64,
    pub unknown_policy: u64,
    pub unknown_geometry: u64,
    pub external_reference: u64,
    pub unsupported_detail: u64,
    pub request_constraint: u64,
}
impl UnresolvedMedia {
    pub const fn is_empty(self) -> bool {
        self.images == 0 && self.files == 0 && self.opaque_items == 0
    }
    pub const fn add(self, b: Self) -> Self {
        Self {
            images: self.images.saturating_add(b.images),
            files: self.files.saturating_add(b.files),
            opaque_items: self.opaque_items.saturating_add(b.opaque_items),
            unknown_policy: self.unknown_policy.saturating_add(b.unknown_policy),
            unknown_geometry: self.unknown_geometry.saturating_add(b.unknown_geometry),
            external_reference: self.external_reference.saturating_add(b.external_reference),
            unsupported_detail: self.unsupported_detail.saturating_add(b.unsupported_detail),
            request_constraint: self.request_constraint.saturating_add(b.request_constraint),
        }
    }
    pub fn description(self) -> String {
        let mut parts = Vec::new();
        if self.images > 0 {
            parts.push(format!(
                "{} unmeasured image{}",
                self.images,
                if self.images == 1 { "" } else { "s" }
            ));
        }
        if self.files > 0 {
            parts.push(format!(
                "{} unmeasured file{}",
                self.files,
                if self.files == 1 { "" } else { "s" }
            ));
        }
        if self.opaque_items > 0 {
            parts.push(format!(
                "{} unmeasured provider item{}",
                self.opaque_items,
                if self.opaque_items == 1 { "" } else { "s" }
            ));
        }
        parts.join("; ")
    }
}
impl ContextTokenEstimate {
    pub fn add_image(
        &mut self,
        facts: &ImageFacts,
        detail: ImageDetail,
        policy: &ImageAccountingPolicy,
    ) {
        self.image_count = self.image_count.saturating_add(1);
        self.max_image_edge = self
            .max_image_edge
            .max(facts.dimensions.map_or(0, |(w, h)| w.max(h)));
        if let Some(tokens) = policy.image_tokens(facts.dimensions, detail) {
            self.media_tokens = self.media_tokens.saturating_add(tokens);
        } else {
            let missing = &mut self.unresolved_media;
            missing.images += 1;
            if matches!(
                policy,
                ImageAccountingPolicy::Unresolved
                    | ImageAccountingPolicy::Auto
                    | ImageAccountingPolicy::Preset { .. }
            ) {
                missing.unknown_policy += 1;
            } else if !matches!(facts.source, ImageSource::Embedded) {
                missing.external_reference += 1;
            } else if facts.dimensions.is_none() {
                missing.unknown_geometry += 1;
            } else {
                missing.unsupported_detail += 1;
            }
        }
    }
    pub fn add_file(&mut self) {
        self.unresolved_media.files += 1;
    }
    pub fn validate_media_constraints(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.unresolved_media.request_constraint == 0,
            "image geometry violates the destination accounting policy's whole-request image constraint; reduce image count/dimensions explicitly (saved media is unchanged)"
        );
        Ok(())
    }
    pub fn enforce_request_constraints(mut self, policy: &ImageAccountingPolicy) -> Self {
        if let ImageAccountingPolicy::AnthropicPatch {
            many_image_threshold,
            many_image_max_edge,
            ..
        } = policy
            && self.image_count > u64::from(*many_image_threshold)
            && self.max_image_edge > *many_image_max_edge
        {
            self.unresolved_media.request_constraint = self.image_count;
            self.unresolved_media.images = self.image_count;
            self.media_tokens = 0; // all image costs are unresolved under this invalid request-wide geometry.
        }
        self
    }
}
#[derive(Clone, Copy)]
enum View {
    Plain,
    ResponsesItem,
    ResponsesBlock,
    AnthropicMessage,
    AnthropicBlock,
    ImageSource,
    Opaque,
}
fn string_size(text: &str) -> usize {
    // serde_json escaping without allocating a temporary string.
    2usize.saturating_add(
        text.bytes()
            .map(|b| match b {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
                0..=31 => 6,
                _ => 1,
            })
            .sum::<usize>(),
    )
}
fn detail(value: &Value) -> ImageDetail {
    match value.as_str() {
        Some("low") => ImageDetail::Low,
        Some("high") => ImageDetail::High,
        Some("original") => ImageDetail::Original,
        _ => ImageDetail::Auto,
    }
}
fn size(
    value: &Value,
    view: View,
    estimate: &mut ContextTokenEstimate,
    policy: &ImageAccountingPolicy,
) -> usize {
    let kind = value.get("type").and_then(Value::as_str);
    let unknown_item = match (view, kind) {
        (View::ResponsesItem, Some(kind)) => !matches!(
            kind,
            "message"
                | "function_call"
                | "custom_tool_call"
                | "function_call_output"
                | "custom_tool_call_output"
                | "reasoning"
                | "compaction"
                | "additional_tools"
        ),
        (View::AnthropicBlock, Some(kind)) => !matches!(
            kind,
            "text"
                | "image"
                | "document"
                | "tool_result"
                | "tool_use"
                | "thinking"
                | "redacted_thinking"
                | "tool_addition"
        ),
        _ => false,
    };
    if unknown_item {
        // Unknown provider-owned processing is opaque, not prose or an image
        // inferred from key names. Count its type envelope and expose the gap.
        estimate.unresolved_media.opaque_items =
            estimate.unresolved_media.opaque_items.saturating_add(1);
        return 9 + string_size(kind.expect("unknown typed item"));
    }
    let view = match view {
        View::ResponsesItem if matches!(kind, Some("reasoning" | "compaction")) => View::Opaque,
        View::AnthropicBlock if matches!(kind, Some("thinking" | "redacted_thinking")) => {
            View::Opaque
        }
        other => other,
    };
    let image = matches!(view, View::ResponsesBlock) && kind == Some("input_image");
    let file = matches!(view, View::ResponsesBlock) && kind == Some("input_file");
    let anthropic_image = matches!(view, View::AnthropicBlock) && kind == Some("image");
    let anthropic_file = matches!(view, View::AnthropicBlock) && kind == Some("document");
    if image {
        let facts = if let Some(url) = value.get("image_url").and_then(Value::as_str) {
            image_facts::url(url)
        } else {
            image_facts::reference(
                ImageSource::FileId,
                value
                    .get("file_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        };
        estimate.add_image(&facts, detail(&value["detail"]), policy);
    }
    if anthropic_image {
        let source = &value["source"];
        let facts = if source["type"] == "base64" {
            image_facts::embedded(
                source["media_type"].as_str(),
                source["data"].as_str().unwrap_or_default(),
            )
        } else if let Some(url) = source["url"].as_str() {
            image_facts::url(url)
        } else {
            image_facts::reference(
                ImageSource::FileId,
                source["file_id"].as_str().unwrap_or_default(),
            )
        };
        // The renderer sends no detail. Never use a Rig preference here.
        estimate.add_image(&facts, ImageDetail::Auto, policy);
    }
    if file || anthropic_file {
        estimate.add_file();
    }
    match value {
        Value::Null => 4,
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => string_size(s),
        Value::Array(values) => {
            2 + values.len().saturating_sub(1)
                + values
                    .iter()
                    .map(|v| size(v, view, estimate, policy))
                    .sum::<usize>()
        }
        Value::Object(values) => {
            let mut bytes = 2 + values.len().saturating_sub(1);
            for (key, v) in values {
                bytes += string_size(key) + 1;
                let substitute = (image && matches!(key.as_str(), "image_url" | "file_id"))
                    || (file && matches!(key.as_str(), "file_data" | "file_id" | "file_url"))
                    || (matches!(view, View::ImageSource)
                        && matches!(key.as_str(), "data" | "url" | "file_id"))
                    || (matches!(view, View::Opaque)
                        && matches!(key.as_str(), "encrypted_content" | "signature"))
                    || (matches!(view, View::Opaque)
                        && matches!(
                            kind,
                            Some("redacted_thinking" | "redacted_reasoning" | "redacted")
                        )
                        && matches!(key.as_str(), "data" | "content"));
                if substitute {
                    bytes += 2;
                    continue;
                }
                let next = match view {
                    View::ResponsesItem
                        if key == "content" && matches!(kind, None | Some("message")) =>
                    {
                        View::ResponsesBlock
                    }
                    View::ResponsesItem
                        if key == "output"
                            && v.is_array()
                            && matches!(
                                kind,
                                Some("function_call_output" | "custom_tool_call_output")
                            ) =>
                    {
                        View::ResponsesBlock
                    }
                    View::AnthropicMessage if key == "content" && v.is_array() => {
                        View::AnthropicBlock
                    }
                    View::AnthropicBlock
                        if key == "content" && kind == Some("tool_result") && v.is_array() =>
                    {
                        View::AnthropicBlock
                    }
                    View::AnthropicBlock
                        if key == "source" && (anthropic_image || anthropic_file) =>
                    {
                        View::ImageSource
                    }
                    View::AnthropicBlock
                        if matches!(kind, Some("thinking" | "redacted_thinking")) =>
                    {
                        View::Opaque
                    }
                    View::Opaque => View::Opaque,
                    _ => View::Plain,
                };
                bytes = bytes.saturating_add(size(v, next, estimate, policy));
            }
            bytes
        }
    }
}

/// Additive input accounting retaining only counts and facts, not encoded payloads.
#[derive(Debug, Default)]
pub struct ResponsesInputEstimate {
    bytes: usize,
    items: usize,
    media: ContextTokenEstimate,
}
impl ResponsesInputEstimate {
    pub fn append(&mut self, input: &[Value]) {
        self.append_with_policy(input, &ImageAccountingPolicy::Unresolved);
    }
    pub fn append_with_policy(&mut self, input: &[Value], policy: &ImageAccountingPolicy) {
        for item in input {
            self.bytes = self
                .bytes
                .saturating_add(usize::from(self.items > 0))
                .saturating_add(size(item, View::ResponsesItem, &mut self.media, policy));
            self.items += 1;
        }
    }
    pub fn estimate(&self, property_bytes: usize) -> ContextTokenEstimate {
        let mut estimate = self.media;
        let text = approximate_tokens_from_bytes(
            self.bytes.saturating_add(2).saturating_add(property_bytes),
        );
        estimate.payload_tokens = text;
        estimate.estimated_tokens = text;
        estimate
    }
    /// Compatibility helper returns structured accounting, never a fabricated total.
    pub fn tokens(&self, property_bytes: usize) -> ContextTokenEstimate {
        self.estimate(property_bytes)
    }
}
pub fn responses_input(
    input: &[Value],
    property_bytes: usize,
    policy: &ImageAccountingPolicy,
) -> ContextTokenEstimate {
    let mut estimate = ResponsesInputEstimate::default();
    estimate.append_with_policy(input, policy);
    estimate
        .estimate(property_bytes)
        .enforce_request_constraints(policy)
}
pub fn responses_replay(input: &[Value], policy: &ImageAccountingPolicy) -> ContextTokenEstimate {
    let mut estimate = ContextTokenEstimate::default();
    let bytes = 2
        + input.len().saturating_sub(1)
        + input
            .iter()
            .map(|item| size(item, View::ResponsesItem, &mut estimate, policy))
            .sum::<usize>();
    let text = approximate_tokens_from_bytes(bytes);
    estimate.payload_tokens = text;
    estimate.estimated_tokens = text;
    estimate
}
pub fn anthropic_input(
    input: &[Value],
    property_bytes: usize,
    policy: &ImageAccountingPolicy,
) -> ContextTokenEstimate {
    let mut estimate = ContextTokenEstimate::default();
    let bytes = 2
        + input.len().saturating_sub(1)
        + input
            .iter()
            .map(|v| size(v, View::AnthropicMessage, &mut estimate, policy))
            .sum::<usize>()
        + property_bytes;
    let text = approximate_tokens_from_bytes(bytes);
    estimate.payload_tokens = text;
    estimate.estimated_tokens = text;
    estimate.enforce_request_constraints(policy)
}
pub fn anthropic_replay(input: &[Value], policy: &ImageAccountingPolicy) -> ContextTokenEstimate {
    let mut estimate = ContextTokenEstimate::default();
    let bytes = 2
        + input.len().saturating_sub(1)
        + input
            .iter()
            .map(|v| size(v, View::AnthropicBlock, &mut estimate, policy))
            .sum::<usize>();
    let text = approximate_tokens_from_bytes(bytes);
    estimate.payload_tokens = text;
    estimate.estimated_tokens = text;
    estimate
}

pub fn canonical_image(
    image: &rig_core::message::Image,
    policy: &ImageAccountingPolicy,
) -> ContextTokenEstimate {
    use rig_core::message::{DocumentSourceKind as Source, ImageDetail as Detail};
    let facts = match &image.data {
        Source::Base64(data) => {
            image_facts::embedded(image.media_type.as_ref().map(|m| m.to_mime_type()), data)
        }
        Source::Url(url) => image_facts::url(url),
        Source::FileId(id) => image_facts::reference(ImageSource::FileId, id),
        _ => image_facts::reference(ImageSource::Other, ""),
    };
    let detail = if matches!(policy, ImageAccountingPolicy::AnthropicPatch { .. }) {
        ImageDetail::Auto
    } else {
        match image.detail {
            Some(Detail::Low) => ImageDetail::Low,
            Some(Detail::High) => ImageDetail::High,
            _ => ImageDetail::Auto,
        }
    };
    let mut estimate = ContextTokenEstimate::default();
    estimate.add_image(&facts, detail, policy);
    estimate
}
