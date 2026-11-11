use crate::replay::{InputProjection, PreflightCache, preflight_with_compatibility, project};
use crate::tools::ToolRendererOptions;
use rig_core::message::{AssistantContent, Message, ToolFunction, ToolName, ToolResultContent};
use serde_json::json;
use zevria_foundation::ModelProfileRef;
use zevria_model::{
    ModelInputIdentity, OwnedModelRequestItem, ProviderReplay, ToolDeclaration, ToolDeclarations,
};

fn history(target: &ModelProfileRef) -> Vec<OwnedModelRequestItem> {
    let call = zevria_content::message::tool_call_from_wire(
        "portable_item",
        "portable_call",
        ToolFunction::new(ToolName::new("inspect").unwrap(), json!({"path":"src"})),
    );
    let result = Message::from(call.result(vec![ToolResultContent::text("inspection result")]));
    vec![
        OwnedModelRequestItem::message(Message::user("inspect the source")),
        OwnedModelRequestItem::AdditionalTools(ToolDeclarations::new(vec![ToolDeclaration {
            name: "inspect".into(), description: "Inspect source".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        }]).unwrap()),
        OwnedModelRequestItem::message(Message::from(vec![AssistantContent::ToolCall(call)])),
        OwnedModelRequestItem::message(result),
        OwnedModelRequestItem::replay_backed(ProviderReplay::openai_responses(target.clone(), vec![
            json!({"type":"function_call","id":"native_item","call_id":"zevria_call_000000","name":"inspect","arguments":"{}","status":"completed"}),
        ])).unwrap(),
        OwnedModelRequestItem::message(Message::from(
            zevria_content::message::tool_call_from_wire("native_item", "zevria_call_000000",
                ToolFunction::new(ToolName::new("inspect").unwrap(), json!({})))
                .result(vec![ToolResultContent::text("native result")]),
        )),
        OwnedModelRequestItem::message(Message::user("continue")),
    ]
}

#[test]
fn incremental_projection_matches_full_at_every_split_and_rebuilds_for_native_collision() {
    let target = ModelProfileRef::new("test", "model");
    let history = history(&target);
    let input = history
        .iter()
        .map(OwnedModelRequestItem::as_borrowed)
        .collect::<Vec<_>>();
    let (expected, expected_source) = project(&input, &target).unwrap();
    assert_eq!(expected[2]["call_id"], "zevria_call_000000_1");
    for split in 0..=input.len() {
        let mut projection = InputProjection::default();
        projection
            .extend(
                &input[..split],
                &target,
                true,
                ToolRendererOptions::default(),
                true,
            )
            .unwrap();
        let can_extend = projection.can_extend(&input[split..], &target).unwrap();
        if (3..=4).contains(&split) {
            assert!(!can_extend, "future native ID must rebuild portable prefix");
        }
        if !can_extend {
            projection = InputProjection::default();
        }
        let suffix = if can_extend {
            &input[split..]
        } else {
            &input[..]
        };
        projection
            .extend(suffix, &target, true, ToolRendererOptions::default(), true)
            .unwrap();
        assert_eq!(projection.source().as_str(), expected_source.as_str());
        assert_eq!(projection.take_input(), expected, "split {split}");
    }
}

#[test]
fn incremental_preflight_matches_full_across_appends_edits_and_compatibility_changes() {
    let target = ModelProfileRef::new("test", "model");
    let mut history = history(&target);
    let mut cache = PreflightCache::default();
    for edit in [false, true] {
        if edit {
            history[0] = OwnedModelRequestItem::message(Message::user("edited source request"));
        }
        let input = history
            .iter()
            .map(OwnedModelRequestItem::as_borrowed)
            .collect::<Vec<_>>();
        for developer_messages in [true, false] {
            for strict_tools in [false, true] {
                for additional_tools in [true, false] {
                    for end in 0..=input.len() {
                        let prefix = &input[..end];
                        let identity = ModelInputIdentity::from_items(prefix).unwrap();
                        let options = ToolRendererOptions { strict_tools };
                        let expected = preflight_with_compatibility(
                            prefix,
                            &target,
                            developer_messages,
                            options,
                            additional_tools,
                        )
                        .unwrap();
                        let actual = cache
                            .assess(
                                prefix,
                                &identity,
                                &target,
                                developer_messages,
                                options,
                                additional_tools,
                            )
                            .unwrap();
                        assert_eq!(actual, expected, "prefix {end}, edit {edit}");
                        // An unchanged input must reuse the same accounting without adding it twice.
                        assert_eq!(
                            cache
                                .assess(
                                    prefix,
                                    &identity,
                                    &target,
                                    developer_messages,
                                    options,
                                    additional_tools
                                )
                                .unwrap(),
                            expected
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn failed_or_foreign_preflight_does_not_install_a_partial_cache() {
    let target = ModelProfileRef::new("test", "model");
    let history = history(&target);
    let input = history
        .iter()
        .map(OwnedModelRequestItem::as_borrowed)
        .collect::<Vec<_>>();
    let mut cache = PreflightCache::default();
    let assess = |cache: &mut PreflightCache, input: &[zevria_model::ModelRequestItem<'_>]| {
        cache.assess(
            input,
            &ModelInputIdentity::from_items(input).unwrap(),
            &target,
            true,
            ToolRendererOptions::default(),
            true,
        )
    };
    assess(&mut cache, &input[..3]).unwrap();
    let foreign = ProviderReplay::openai_responses(
        ModelProfileRef::new("foreign", "model"),
        vec![json!({"type":"compaction","encrypted_content":"opaque"})],
    );
    let mut incompatible = input[..3].to_vec();
    incompatible.push(zevria_model::ModelRequestItem::replay_only(&foreign));
    assert!(matches!(
        assess(&mut cache, &incompatible).unwrap(),
        zevria_model::models::ReplayPreflight::ConversionRequired { .. }
    ));
    let malformed = ProviderReplay::openai_responses(
        target.clone(),
        vec![json!({"type":"function_call","call_id":"broken"})],
    );
    let mut invalid = input[..3].to_vec();
    invalid.push(zevria_model::ModelRequestItem::replay_only(&malformed));
    assert!(assess(&mut cache, &invalid).is_err());
    assert_eq!(
        assess(&mut cache, &input).unwrap(),
        crate::replay::preflight(&input, &target).unwrap()
    );
}

#[test]
fn incremental_estimate_is_identical_to_full_for_images_opaque_data_and_tool_payloads() {
    let input = vec![
        json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,very-large-payload"},{"type":"input_text","text":"hello"}]}),
        json!({"type":"reasoning","encrypted_content":"opaque payload","summary":[{"type":"summary_text","text":"summary"}]}),
        json!({"type":"function_call_output","call_id":"x","output":"{\"type\":\"input_image\",\"image_url\":\"must remain\"}"}),
        json!({"type":"compaction","encrypted_content":"another opaque payload"}),
        json!({"type":"message","role":"user","content":[{"type":"input_image","file_id":"file-image"}]}),
    ];
    for split in 0..=input.len() {
        let mut estimate = zevria_model::compaction::ResponsesInputEstimate::default();
        estimate.append(&input[..split]);
        estimate.append(&[]);
        estimate.append(&input[split..]);
        for property_bytes in [0, 1, 27, 256] {
            assert_eq!(
                estimate.tokens(property_bytes),
                zevria_model::compaction::estimate_responses_input_tokens(&input, property_bytes),
                "split {split}"
            );
        }
    }
}
