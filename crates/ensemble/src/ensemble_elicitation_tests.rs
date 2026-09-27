#[test]
fn elicitation_display_normalizes_before_fallbacks_and_label_disambiguation() {
    let fixture = serde_json::json!({
        "mode": "form", "sessionId": "session-1", "message": "Form\r\nmessage\r",
        "requestedSchema": {"type": "object", "properties": {
            "a\rid": {"type": "string", "title": "\r", "description": "\r\n",
                "oneOf": [
                    {"const": "exact\r\nwire", "title": "Same\r", "description": "Line\r\ntwo\r"},
                    {"const": "other-wire", "title": "Sa\rme"},
                    {"const": "fallback\rvalue", "title": "\r"},
                    {"const": "reserved", "title": "O\rther"},
                    {"const": "\r", "title": "\r"}
                ], "default": "exact\r\nwire"},
            "b": {"type": "array", "title": "Header\r\nnext\r", "description": "Question\r\nnext\r",
                "items": {"anyOf": [
                    {"const": "one\r", "title": "S\rkip", "description": "a\r\nb\r"},
                    {"const": "two", "title": "Skip"},
                    {"const": "fallback\r\nvalue", "title": "\r"}
                ]}, "default": ["one\r", "fallback\r\nvalue"]},
            "c": {"type": "string", "enum": ["A\r", "A", "O\rther", "\r"], "default": "A"},
            "d": {"type": "array", "items": {"type": "string", "enum": ["B\r", "B", "S\rkip"]}, "default": ["B", "S\rkip"]},
            "e": {"type": "string", "default": "text\r\ndefault", "minLength": 1}
        }}
    });
    let converted = convert_fixture(fixture);
    let prompts = &converted.request.questions;
    assert_eq!(prompts[0].id, "a\rid", "protocol ID remains exact");
    assert_eq!(prompts[0].header, "aid");
    assert_eq!(prompts[0].question, "Form\nmessage");
    assert_eq!(prompts[1].header, "Header\nnext");
    assert_eq!(prompts[1].question, "Question\nnext");
    for prompt in prompts {
        assert!(!prompt.header.contains('\r'));
        assert!(!prompt.question.contains('\r'));
        assert!(
            prompt
                .options
                .iter()
                .all(|option| !option.label.contains('\r') && !option.description.contains('\r'))
        );
    }
    let labels = |index: usize| {
        prompts[index]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(0),
        [
            "Same",
            "Same (option)",
            "fallbackvalue",
            "Other (option)",
            "Option"
        ]
    );
    assert_eq!(
        labels(1),
        ["Skip (option)", "Skip (option 3)", "fallback\nvalue"]
    );
    assert_eq!(labels(2), ["A", "A (option)", "Other (option)", "Option"]);
    assert_eq!(labels(3), ["B", "B (option)", "Skip (option)"]);
    assert_eq!(prompts[0].options[0].description, "Line\ntwo");
    assert_eq!(prompts[1].options[0].description, "a\nb");
    assert_eq!(
        prompts[2].default,
        Some(QuestionAnswerValue::String("A (option)".into()))
    );
    assert_eq!(
        prompts[4].default,
        Some(QuestionAnswerValue::String("text\r\ndefault".into()))
    );
    let response = fixture_answers(
        &converted,
        prompts.iter().map(|p| p.default.clone()).collect(),
    );
    let decision = converted.normalized_decision(&response).unwrap();
    assert_eq!(decision.answers[0].question_id, "a\rid");
    assert_eq!(decision.answers[0].header, "aid");
    assert_eq!(
        decision.answers[0].answer,
        AgentUserDecisionValue::String {
            value: "Same".into()
        }
    );
    assert_eq!(
        decision.answers[4].answer,
        AgentUserDecisionValue::String {
            value: "text\r\ndefault".into()
        }
    );
    let wire = converted.accepted_content(response).unwrap();
    assert_eq!(
        serde_json::to_value(wire).unwrap(),
        serde_json::json!({
            "a\rid": "exact\r\nwire", "b": ["one\r", "fallback\r\nvalue"], "c": "A", "d": ["B", "S\rkip"], "e": "text\r\ndefault"
        })
    );
    let blank_message = convert_fixture(serde_json::json!({
        "mode": "form", "sessionId": "session-1", "message": "\r\n",
        "requestedSchema": {"type": "object", "properties": {"fallback\rid": {"type": "boolean", "title": "\r", "description": "\r"}}}
    }));
    assert_eq!(blank_message.request.questions[0].question, "fallbackid");
}

#[test]
fn elicitation_normalization_preserves_validation_companions_skip_and_custom_answers() {
    let mut fixture = native_elicitation_fixture();
    fixture["requestedSchema"]["required"] = serde_json::json!(["question_1", "question_2"]);
    let properties = &mut fixture["requestedSchema"]["properties"];
    properties["question_1"]["oneOf"][2]["const"] = serde_json::json!("exact\rother\n");
    properties["question_1_other"]["_meta"]["zevria"]["otherValue"] =
        serde_json::json!("exact\rother\n");
    properties["question_2"]["oneOf"][0]["title"] = serde_json::json!("Focu\rsed");
    let converted = convert_fixture(fixture);
    assert!(!converted.request.questions[0].required);
    let response = fixture_answers(
        &converted,
        vec![
            None,
            Some(QuestionAnswerValue::String("my\r\ncustom\ranswer".into())),
            Some(QuestionAnswerValue::String("Focused".into())),
        ],
    );
    let decision = converted.normalized_decision(&response).unwrap();
    assert_eq!(decision.answers[0].answer, AgentUserDecisionValue::Skipped);
    assert_eq!(
        decision.answers[1].answer,
        AgentUserDecisionValue::String {
            value: "my\r\ncustom\ranswer".into()
        }
    );
    assert_eq!(
        serde_json::to_value(converted.accepted_content(response).unwrap()).unwrap(),
        serde_json::json!({
            "question_1": "exact\rother\n", "question_1_other": "my\r\ncustom\ranswer", "question_2": "option_0"
        })
    );
    // Validation must measure original constants, not the shorter display copy.
    for property in [
        serde_json::json!({"type": "string", "enum": ["x\r"], "maxLength": 1}),
        serde_json::json!({"type": "string", "default": "x\r", "maxLength": 1}),
        serde_json::json!({"type": "string", "enum": ["x\r"], "default": "x"}),
    ] {
        assert_native_declined(serde_json::json!({
            "mode": "form", "sessionId": "session-1", "message": "Validate exact inputs",
            "requestedSchema": {"type": "object", "properties": {"q": property}}
        }));
    }
}
