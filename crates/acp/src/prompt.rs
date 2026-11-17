//! Shared ordered embedded-content adapter. No URL or resource resolution.
use agent_client_protocol::schema::v1::{ContentBlock, Error, ImageContent, TextContent};
use zevria_content::PromptBlock;
#[cfg(test)]
use zevria_content::PromptImage;
use zevria_content::UserPrompt;
use zevria_content::image_preparation;

pub const ORCHESTRATION_EXTENSION: &str = "zevria.orchestration";

/// Only this extension is interpreted. All other ACP metadata remains opaque.
pub(crate) fn request_behavior(
    value: Option<&serde_json::Value>,
) -> Result<zevria_foundation::RequestBehavior, Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OptionV1 {
        version: u32,
        enabled: bool,
    }
    let Some(value) = value else {
        return Ok(zevria_foundation::RequestBehavior::Standard);
    };
    let option: OptionV1 = serde_json::from_value(value.clone()).map_err(|error| {
        Error::invalid_params().data(format!("invalid zevria.orchestration metadata: {error}"))
    })?;
    if option.version != 1 {
        return Err(
            Error::invalid_params().data("unsupported zevria.orchestration version; expected 1")
        );
    }
    Ok(if option.enabled {
        zevria_foundation::RequestBehavior::Orchestrate
    } else {
        zevria_foundation::RequestBehavior::Standard
    })
}

pub fn from_content(blocks: &[ContentBlock]) -> Result<UserPrompt, Error> {
    let mut prompt = Vec::with_capacity(blocks.len());
    let mut images = 0;
    let mut bytes = 0usize;
    for block in blocks {
        prompt.push(match block {
            ContentBlock::Text(text) => PromptBlock::Text(text.text.clone()),
            ContentBlock::Image(image) => {
                images += 1;
                if images > zevria_content::prompt::MAX_PROMPT_IMAGES {
                    return Err(Error::invalid_params().data("a prompt may contain at most eight images"));
                }
                let image = image_preparation::prepare_base64(&image.mime_type, &image.data)
                    .map_err(|error| Error::invalid_params().data(error.to_string()))?.image;
                bytes += image.encoded_len();
                if bytes > zevria_content::prompt::MAX_PROMPT_IMAGE_BYTES {
                    return Err(Error::invalid_params().data("prompt images exceed 20 MiB"));
                }
                PromptBlock::Image(image)
            }
            _ => return Err(Error::invalid_params().data("unsupported prompt block; only text and embedded PNG, JPEG, WebP, and GIF images are accepted")),
        });
    }
    UserPrompt::new(prompt).map_err(|error| Error::invalid_params().data(error.to_string()))
}

pub fn to_content(prompt: &UserPrompt) -> Vec<ContentBlock> {
    prompt
        .blocks()
        .iter()
        .map(|block| match block {
            PromptBlock::Text(text) => ContentBlock::Text(TextContent::new(text)),
            PromptBlock::Image(image) => {
                ContentBlock::Image(ImageContent::new(image.base64(), image.mime_type()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orchestration_metadata_is_explicit_versioned_and_strict() {
        use zevria_foundation::RequestBehavior;
        assert_eq!(request_behavior(None).unwrap(), RequestBehavior::Standard);
        for enabled in [false, true] {
            assert_eq!(
                request_behavior(Some(&serde_json::json!({"version":1,"enabled":enabled})))
                    .unwrap(),
                if enabled {
                    RequestBehavior::Orchestrate
                } else {
                    RequestBehavior::Standard
                }
            );
        }
        for value in [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"version":2,"enabled":true}),
            serde_json::json!({"version":1,"enabled":"true"}),
            serde_json::json!({"version":1,"enabled":true,"other":0}),
        ] {
            assert!(request_behavior(Some(&value)).is_err(), "{value}");
        }
        let literal =
            from_content(&[ContentBlock::from("/orchestrate $orchestrate literal")]).unwrap();
        assert_eq!(
            literal.text_projection(),
            "/orchestrate $orchestrate literal"
        );
    }

    #[test]
    fn oversized_images_are_prepared_in_order_and_aggregate_is_checked_afterwards() {
        let original = zevria_content::test_support::oversized_image();
        let blocks = vec![
            ContentBlock::from("before"),
            ContentBlock::Image(ImageContent::new(original.base64(), original.mime_type())),
            ContentBlock::from("after"),
            ContentBlock::Image(ImageContent::new(original.base64(), original.mime_type())),
        ];
        let prepared = from_content(&blocks).unwrap();
        assert_eq!(prepared.text_projection(), "beforeafter");
        assert_eq!(prepared.images().count(), 2);
        assert_eq!(prepared.images().next(), prepared.images().nth(1));
        assert!(
            prepared
                .images()
                .all(|i| image_preparation::fits_transport(i.encoded_len())
                    && i.dimensions().0 < original.dimensions().0)
        );
        assert_eq!(from_content(&to_content(&prepared)).unwrap(), prepared);
        let bytes = zevria_content::test_support::padded_png(4_730_667);
        let source = PromptImage::from_file_bytes(bytes).unwrap();
        let block = ContentBlock::Image(ImageContent::new(source.base64(), "image/png"));
        assert_eq!(from_content(&vec![block; 6]).unwrap().images().count(), 6);
        let source = PromptImage::from_file_bytes(zevria_content::test_support::padded_png(
            image_preparation::MAX_PREPARED_IMAGE_BYTES,
        ))
        .unwrap();
        let block = ContentBlock::Image(ImageContent::new(source.base64(), "image/png"));
        assert!(
            from_content(&vec![block; 6]).is_err(),
            "compliant images must not be shrunk to fit an aggregate budget"
        );
    }

    #[test]
    fn mixed_and_image_only_inputs_round_trip_in_order() {
        let image = PromptImage::from_rgba(1, 1, &[10, 20, 30, 255]).unwrap();
        let blocks = vec![
            ContentBlock::from("before "),
            ContentBlock::Image(ImageContent::new(image.base64(), image.mime_type())),
            ContentBlock::from(" after"),
        ];
        let prompt = from_content(&blocks).unwrap();
        assert_eq!(prompt.text_projection(), "before  after");
        assert_eq!(to_content(&prompt), blocks);
        assert!(!from_content(&blocks[1..2]).unwrap().is_blank());
        assert!(
            from_content(&[ContentBlock::Image(ImageContent::new(
                "not-base64",
                "image/png"
            ))])
            .is_err()
        );
        let updates = crate::project::project_completed_message(&prompt.to_message(), "images");
        assert_eq!(updates.len(), 3);
        assert!(
            matches!(&updates[1], agent_client_protocol::schema::v1::SessionUpdate::UserMessageChunk(chunk) if matches!(&chunk.content, ContentBlock::Image(value) if value.data == image.base64()))
        );
    }
}
