//! Regressions for the encoded-media spike. Fixtures are synthetic, never session data.
use crate::compaction::estimate_responses_input_tokens;
use serde_json::json;

fn images() -> [zevria_content::PromptImage; 2] {
    let small =
        zevria_content::PromptImage::from_rgba(2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]).unwrap();
    let mut bytes = small.encoded_bytes().to_vec();
    let mut chunk = b"tEXt".to_vec();
    chunk.extend_from_slice(b"padding\0");
    chunk.resize(800_012, b'x');
    let mut crc = !0u32;
    for byte in &chunk {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    let end = bytes.split_off(bytes.len() - 12);
    bytes.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
    bytes.extend_from_slice(&chunk);
    bytes.extend_from_slice(&(!crc).to_be_bytes());
    bytes.extend_from_slice(&end);
    let large = zevria_content::PromptImage::from_encoded("image/png", bytes).unwrap();
    assert_eq!(small.dimensions(), large.dimensions());
    [small, large]
}

#[test]
fn responses_tool_array_media_encoding_is_not_prose() {
    for kind in ["function_call_output", "custom_tool_call_output"] {
        let counts = images().map(|image| estimate_responses_input_tokens(&[json!({
            "type":kind, "call_id":"call", "output":[
                {"type":"input_text","text":"screenshot"},
                {"type":"input_image","image_url":format!("data:image/png;base64,{}", image.base64())}
            ]
        })], 0));
        assert_eq!(
            counts[0], counts[1],
            "{kind} must not count base64 as prose"
        );
    }
}

#[test]
fn responses_typed_file_encoding_is_not_prose() {
    let counts = ["YQ==".to_string(), "YQ==".repeat(200_000)].map(|data| {
        estimate_responses_input_tokens(&[json!({"role":"user","content":[{
            "type":"input_file", "filename":"report.pdf", "file_data":format!("data:application/pdf;base64,{data}")
        }]})], 0)
    });
    assert_eq!(
        counts[0], counts[1],
        "binary PDF processing is unknown, not bytes/4"
    );
}

#[test]
fn image_looking_user_text_remains_literal() {
    let estimate = |data: &str| {
        estimate_responses_input_tokens(
            &[json!({"type":"function_call_output", "call_id":"call", "output":data})],
            0,
        )
    };
    assert!(
        estimate(&"input_image image_url signature encrypted_content ".repeat(10_000))
            .known_tokens()
            > estimate("ordinary text").known_tokens()
    );
}

#[test]
fn destination_costs_count_each_occurrence_without_counting_bytes_or_fetching_sources() {
    use crate::media_accounting::{ResponsesInputEstimate, anthropic_input, responses_input};
    use zevria_foundation::image_accounting::{ImageAccountingPolicy, ImageDetail};
    let policy = ImageAccountingPolicy::preset("gpt-4.1-mini-2025-04-14").unwrap();
    let expected = policy
        .image_tokens(Some((2, 1)), ImageDetail::Auto)
        .unwrap();
    for image in images() {
        let url = format!("data:image/png;base64,{}", image.base64());
        let block = json!({"type":"input_image","image_url":url});
        let input = vec![
            json!({"role":"user","content":[block.clone(),block.clone()]}),
            json!({"type":"function_call_output","call_id":"call","output":[block]}),
        ];
        let estimate = responses_input(&input, 0, &policy);
        assert_eq!(estimate.media_tokens, 3 * expected);
        assert_eq!(estimate.image_count, 3);
        assert!(estimate.complete_tokens().is_some());
        assert!(estimate.estimated_tokens < 200);
        for split in 0..=input.len() {
            let mut incremental = ResponsesInputEstimate::default();
            incremental.append_with_policy(&input[..split], &policy);
            incremental.append_with_policy(&[], &policy);
            incremental.append_with_policy(&input[split..], &policy);
            assert_eq!(incremental.estimate(0), estimate);
        }
        let claude = ImageAccountingPolicy::preset("claude-sonnet-4-5-20250929").unwrap();
        let native = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":image.base64()}});
        let messages = vec![
            json!({"role":"user","content":[native.clone(),{"type":"tool_result","tool_use_id":"call","content":[native]}]}),
        ];
        let estimate = anthropic_input(&messages, 0, &claude);
        assert_eq!(estimate.image_count, 2);
        assert_eq!(estimate.media_tokens, 2);
        assert!(estimate.complete_tokens().is_some());
        assert!(estimate.estimated_tokens < 200);
    }
    for reference in [
        json!({"image_url":"https://invalid.example/not-fetched.png"}),
        json!({"file_id":"file-not-fetched"}),
    ] {
        let mut block = reference;
        block["type"] = json!("input_image");
        let estimate = responses_input(&[json!({"role":"user","content":[block]})], 0, &policy);
        assert_eq!(estimate.complete_tokens(), None);
        assert_eq!(estimate.unresolved_media.images, 1);
        assert_eq!(estimate.unresolved_media.external_reference, 1);
    }
}

#[test]
fn imported_header_facts_are_derived_once_and_reused_across_numerical_policies() {
    use crate::media_accounting::responses_input;
    use zevria_foundation::image_accounting::ImageAccountingPolicy;
    let image = zevria_content::PromptImage::from_rgba(
        3,
        1,
        &[38, 1, 10, 255, 9, 6, 42, 255, 17, 19, 121, 255],
    )
    .unwrap();
    let block = json!({"type":"input_image","image_url":format!("data:image/png;base64,{}",image.base64())});
    let input = vec![json!({"role":"user","content":[block]})];
    let before = zevria_content::image_facts::thread_fact_derivations();
    let unknown = responses_input(&input, 0, &ImageAccountingPolicy::Unresolved);
    assert_eq!(
        zevria_content::image_facts::thread_fact_derivations(),
        before + 1
    );
    assert_eq!(unknown.complete_tokens(), None);
    for _ in 0..10 {
        let _ = responses_input(
            &input,
            0,
            &ImageAccountingPolicy::preset("gpt-4.1-mini-2025-04-14").unwrap(),
        );
    }
    assert_eq!(
        zevria_content::image_facts::thread_fact_derivations(),
        before + 1,
        "policy changes must reuse intrinsic facts"
    );
}

#[test]
fn canonical_and_native_replay_tool_images_use_the_same_media_policy() {
    use crate::{ModelProfileRef, ProviderReplay};
    use zevria_foundation::image_accounting::ImageAccountingPolicy;
    let image = images().into_iter().next().unwrap();
    let policy = ImageAccountingPolicy::preset("gpt-4.1-mini-2025-04-14").unwrap();
    let call = zevria_content::message::tool_call_from_wire(
        "item",
        "call",
        rig_core::message::ToolFunction::new(
            rig_core::message::ToolName::new("screenshot").unwrap(),
            json!({}),
        ),
    );
    let message =
        rig_core::message::Message::from(call.result(vec![image.to_tool_result_content()]));
    let canonical = crate::compaction::estimate_message_tokens_with_policy(&message, &policy);
    let replay = ProviderReplay::openai_responses(
        ModelProfileRef::new("test", "model"),
        vec![
            json!({"type":"function_call_output","call_id":"call","output":[{"type":"input_image","image_url":format!("data:image/png;base64,{}",image.base64())}]}),
        ],
    );
    let native = replay.replay_only_token_estimate_with_policy(&policy);
    assert_eq!(canonical.media_tokens, native.media_tokens);
    assert_eq!(native.image_count, 1);
    assert_eq!(
        native.complete_tokens().is_some(),
        canonical.complete_tokens().is_some()
    );
    assert!(native.known_tokens() < 200);
}

#[test]
fn documented_whole_request_image_constraints_are_not_best_effort_cost_gaps() {
    use zevria_content::image_facts::{ImageFacts, ImageSource};
    use zevria_foundation::image_accounting::{ImageAccountingPolicy, ImageDetail};
    let policy = ImageAccountingPolicy::preset("claude-sonnet-4-5-20250929").unwrap();
    let facts = ImageFacts {
        source: ImageSource::Embedded,
        mime: Some("image/png".into()),
        dimensions: Some((3001, 1000)),
        identity: [9; 32],
    };
    let mut estimate = crate::ContextTokenEstimate::default();
    for _ in 0..21 {
        estimate.add_image(&facts, ImageDetail::Auto, &policy);
    }
    let estimate = estimate.enforce_request_constraints(&policy);
    assert_eq!(estimate.complete_tokens(), None);
    assert_eq!(estimate.unresolved_media.request_constraint, 21);
    assert!(
        estimate.validate_media_constraints().is_err(),
        "known unsupported content is not best effort"
    );
}

#[test]
fn unknown_native_processing_stays_opaque_instead_of_guessing_media_or_counting_blobs() {
    let estimate = |blob: String| {
        let input = vec![
            json!({"type":"future_provider_blob","id":"opaque","payload":{"image_url":blob,"signature":"provider-owned"}}),
        ];
        crate::media_accounting::responses_input(
            &input,
            0,
            &zevria_foundation::image_accounting::ImageAccountingPolicy::Unresolved,
        )
    };
    let small = estimate("x".into());
    assert_eq!(small, estimate("encoded-not-prose".repeat(200_000)));
    assert_eq!(small.image_count, 0, "unknown keys cannot invent images");
    assert_eq!(small.unresolved_media.opaque_items, 1);
    assert_eq!(small.complete_tokens(), None);
    assert!(small.known_tokens() < 100);
}
