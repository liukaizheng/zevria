use crate::wire::ResponsesWebSocketEvent;
use crate::{accumulator::*, protocol::parse_server_event};
use rig_core::message::{AssistantContent, Message, Text, ToolFunction};
use serde_json::{Value, json};
use zevria_content::reasoning::ReasoningContent;
fn record_item(accumulator: &mut AssistantMessageAccumulator, value: Value) {
    let event = parse_server_event(&value.to_string())
        .expect("event should parse")
        .expect("event should be recognized");
    let ResponsesWebSocketEvent::Item(item) = event else {
        panic!("test event should be an item event");
    };
    accumulator
        .record_item(item)
        .expect("item should accumulate");
}

fn text_with_openai_extras(text: &str, extras: Value) -> Text {
    zevria_content::citations::text_with_openai_extras(text, extras)
}

#[test]
fn reasoning_text_done_is_a_known_modeled_event() {
    let event = parse_server_event(
        &json!({
            "type": "response.reasoning_text.done",
            "item_id": "rs_1",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 7,
            "text": "completed reasoning"
        })
        .to_string(),
    )
    .expect("reasoning terminator should decode");

    assert!(matches!(event, Some(ResponsesWebSocketEvent::Item(_))));
}

#[test]
fn reasoning_summary_text_done_preserves_indexes_and_replaces_deltas() {
    for with_deltas in [false, true] {
        for final_text in ["completed summary", ""] {
            let mut accumulator = AssistantMessageAccumulator::default();
            record_item(
                &mut accumulator,
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": "rs_summary",
                    "output_index": 2,
                    "summary_index": 0,
                    "sequence_number": 1,
                    "delta": "other summary"
                }),
            );
            if with_deltas {
                record_item(
                    &mut accumulator,
                    json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": "rs_summary",
                        "output_index": 2,
                        "summary_index": 3,
                        "sequence_number": 2,
                        "delta": "draft summary"
                    }),
                );
            }
            let event = parse_server_event(
                &json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": "rs_summary",
                    "output_index": 2,
                    "summary_index": 3,
                    "sequence_number": 3,
                    "text": final_text
                })
                .to_string(),
            )
            .expect("summary terminator should decode")
            .expect("summary terminator should be recognized");
            let ResponsesWebSocketEvent::Item(item) = event else {
                panic!("summary terminator should be an item event");
            };
            assert_eq!(item.item_id.as_deref(), Some("rs_summary"));
            assert_eq!(item.output_index, 2);
            accumulator.record_item(item).unwrap();

            assert_eq!(
                accumulator.assistant_message().unwrap(),
                Message::Assistant(rig_core::message::AssistantMessage::new(vec![
                    AssistantContent::Reasoning(message_reasoning(
                        Some("rs_summary".into()),
                        vec![
                            ReasoningContent::Summary("other summary".into()),
                            ReasoningContent::Summary(final_text.into()),
                        ],
                    ))
                ]))
            );
        }
    }
}

#[test]
fn reasoning_summary_text_done_rejects_missing_or_invalid_text() {
    let mut event = json!({
        "type": "response.reasoning_summary_text.done",
        "output_index": 0,
        "summary_index": 0,
        "sequence_number": 1
    });
    assert!(parse_server_event(&event.to_string()).is_err());
    for invalid_text in [Value::Null, json!(42)] {
        event["text"] = invalid_text;
        assert!(parse_server_event(&event.to_string()).is_err());
    }
}

#[test]
fn terminal_event_cache_write_tokens_distinguishes_present_zero_and_absent() {
    for event_type in ["response.completed", "response.done"] {
        let mut event = completed_event_with_usage("resp_cache_write", "msg_cache_write", "answer");
        event["type"] = json!(event_type);

        event["response"]["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(2560);
        assert_eq!(response_cache_write_tokens(&event.to_string()), Some(2560));

        event["response"]["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(0);
        assert_eq!(response_cache_write_tokens(&event.to_string()), Some(0));

        event["response"]["usage"]["input_tokens_details"]
            .as_object_mut()
            .expect("input token details should be an object")
            .remove("cache_write_tokens");
        assert_eq!(response_cache_write_tokens(&event.to_string()), None);
    }
}

#[test]
fn sparse_out_of_order_indexes_preserve_content_order() {
    let mut accumulator = AssistantMessageAccumulator::default();

    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_sparse",
            "output_index": 3,
            "content_index": 2,
            "sequence_number": 1,
            "delta": "content two"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_sparse",
            "output_index": 1,
            "summary_index": 2,
            "sequence_number": 2,
            "delta": "summary two"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_sparse",
            "output_index": 1,
            "content_index": 3,
            "sequence_number": 3,
            "delta": "reasoning three"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_sparse",
            "output_index": 3,
            "content_index": 0,
            "sequence_number": 4,
            "delta": "content zero"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_sparse",
            "output_index": 1,
            "summary_index": 0,
            "sequence_number": 5,
            "delta": "summary zero"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_sparse",
            "output_index": 1,
            "content_index": 1,
            "sequence_number": 6,
            "delta": "reasoning one"
        }),
    );

    let expected = Message::Assistant(rig_core::message::AssistantMessage::new(vec![
        AssistantContent::Reasoning(message_reasoning(
            Some("rs_sparse".to_string()),
            vec![
                ReasoningContent::Summary("summary zero".to_string()),
                ReasoningContent::Summary("summary two".to_string()),
                ReasoningContent::Text {
                    text: "reasoning one".to_string(),
                    signature: None,
                },
                ReasoningContent::Text {
                    text: "reasoning three".to_string(),
                    signature: None,
                },
            ],
        )),
        AssistantContent::text("content zero"),
        AssistantContent::text("content two"),
    ]));

    assert_eq!(
        accumulator
            .assistant_message()
            .expect("sparse streamed content should convert"),
        expected
    );
}

#[test]
fn streaming_items_preserve_order_structure_and_done_values() {
    let mut accumulator = AssistantMessageAccumulator::default();

    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "sequence_number": 1,
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "content": [],
                "status": "in_progress"
            }
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_1",
            "output_index": 0,
            "summary_index": 0,
            "sequence_number": 2,
            "delta": "draft summary"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_1",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 3,
            "delta": "draft reasoning"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "sequence_number": 4,
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "final summary"}],
                "content": [{"type": "reasoning_text", "text": "final reasoning"}],
                "encrypted_content": "ciphertext",
                "status": "completed"
            }
        }),
    );

    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "sequence_number": 5,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "weather",
                "arguments": "",
                "status": "in_progress"
            }
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1",
            "output_index": 1,
            "sequence_number": 6,
            "delta": "{\"city\":\"Par"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1",
            "output_index": 1,
            "sequence_number": 7,
            "delta": "is\"}"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_1",
            "output_index": 1,
            "sequence_number": 8,
            "arguments": "{\"city\":\"Paris\"}"
        }),
    );

    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_item.done",
            "output_index": 2,
            "sequence_number": 9,
            "item": {
                "type": "function_call",
                "id": "fc_2",
                "call_id": "call_2",
                "name": "weather",
                "arguments": "{\"city\":\"Tokyo\"}",
                "status": "completed"
            }
        }),
    );

    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 3,
            "content_index": 0,
            "sequence_number": 10,
            "delta": "draft answer"
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_item.done",
            "output_index": 3,
            "sequence_number": 11,
            "item": {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "final answer",
                        "annotations": [{"type": "citation", "source": "stream"}]
                    },
                    {"type": "refusal", "refusal": "refusal text"}
                ]
            }
        }),
    );

    let final_text = text_with_openai_extras(
        "final answer",
        json!({
            "annotations": [{"type": "citation", "source": "stream"}]
        }),
    );
    let expected = Message::Assistant(rig_core::message::AssistantMessage::new(vec![
        AssistantContent::Reasoning(message_reasoning(
            Some("rs_1".to_string()),
            vec![
                ReasoningContent::Summary("final summary".to_string()),
                ReasoningContent::Text {
                    text: "final reasoning".to_string(),
                    signature: None,
                },
                ReasoningContent::Encrypted("ciphertext".to_string()),
            ],
        )),
        AssistantContent::ToolCall(zevria_content::message::tool_call_from_wire(
            "fc_1",
            "call_1",
            ToolFunction::new(
                rig_core::message::ToolName::new("weather".to_string())
                    .expect("nonempty tool name"),
                json!({"city": "Paris"}),
            ),
        )),
        AssistantContent::ToolCall(zevria_content::message::tool_call_from_wire(
            "fc_2",
            "call_2",
            ToolFunction::new(
                rig_core::message::ToolName::new("weather".to_string())
                    .expect("nonempty tool name"),
                json!({"city": "Tokyo"}),
            ),
        )),
        AssistantContent::Text(final_text),
        AssistantContent::text("refusal text"),
    ]));

    assert_eq!(
        accumulator
            .assistant_message()
            .expect("streamed content should convert"),
        expected
    );
}

fn completed_event(response_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "type": "response.completed",
        "sequence_number": 1,
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "model": "gpt-test",
            "usage": null,
            "output": [{
                "type": "message",
                "id": message_id,
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text
                }]
            }],
            "tools": []
        }
    })
}

fn completed_event_with_usage(response_id: &str, message_id: &str, text: &str) -> Value {
    let mut event = completed_event(response_id, message_id, text);
    event["response"]["usage"] = json!({
        "input_tokens": 1200,
        "input_tokens_details": {"cached_tokens": 1000},
        "output_tokens": 40,
        "total_tokens": 1240
    });
    event
}
