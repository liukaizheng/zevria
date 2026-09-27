// Matches buildUserInputRequest in the inspected @agentclientprotocol/codex-acp
// 1.13.1 package, including primary metadata, note metadata and the wire token.
fn codex_acp_1_13_1_elicitation_fixture() -> serde_json::Value {
    use serde_json::json;
    let mut properties = serde_json::Map::new();
    for index in 0..3 {
        let name = format!("question_{index}");
        properties.insert(
            name.clone(),
            json!({
                "type": "string", "title": format!("How broad should change {index} be?"),
                "description": format!("Scope {index}"),
                "_meta": {"codex": {"isOther": true, "isSecret": false}},
                "oneOf": [
                    {"const": "Focused", "title": "Focused", "description": "Keep it narrow."},
                    {"const": "Broad", "title": "Broad"},
                    {"const": "None of the above", "title": "None of the above",
                        "description": "Provide a different answer in the note field."}
                ]
            }),
        );
        properties.insert(
            format!("{name}_note"),
            json!({
                "type": "string", "title": "Additional answer or note",
                "_meta": {"codex": {"questionId": name, "role": "user_note", "isSecret": false}}
            }),
        );
    }
    json!({
        "mode": "form", "sessionId": "session-1", "toolCallId": "tool-1",
        "message": "Codex needs your input to continue.",
        "requestedSchema": {"type": "object", "properties": properties,
            "required": ["question_0", "question_1", "question_2"]},
        "_meta": {"codex": {"autoResolutionMs": null}}
    })
}

#[test]
fn codex_1_13_1_six_properties_become_three_required_questions_and_round_trip() {
    use serde_json::json;
    let fixture = codex_acp_1_13_1_elicitation_fixture();
    assert_eq!(
        fixture["requestedSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        elicitation_field_count(&elicitation_from_json(fixture.clone())),
        3
    );
    let converted = convert_fixture(fixture);
    assert_eq!(converted.request.questions.len(), 3);
    assert_eq!(converted.fields.len(), 3);
    for (index, (question, field)) in converted
        .request
        .questions
        .iter()
        .zip(&converted.fields)
        .enumerate()
    {
        assert_eq!(question.id, format!("question_{index}"));
        assert_eq!(field.property, question.id);
        assert!(question.required && field.required);
        assert_eq!(
            question.kind,
            QuestionPromptKind::SingleSelect { allow_other: true }
        );
        assert_eq!(
            question
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["Focused", "Broad"]
        );
        assert_eq!(question.options[0].description, "Keep it narrow.");
        let ElicitationFieldKind::Single {
            values,
            companion: Some(companion),
        } = &field.kind
        else {
            panic!("expected a token-bearing single-select companion");
        };
        assert_eq!(values.len(), 2);
        assert_eq!(companion.property, format!("question_{index}_note"));
        assert_eq!(companion.other_value.as_deref(), Some("None of the above"));
    }
    for (answers, expected) in [
        (
            ["Focused", "Broad", "Focused"],
            json!({"question_0": "Focused", "question_1": "Broad", "question_2": "Focused"}),
        ),
        (
            ["Focused", "  Custom\r\nscope — café  ", "Broad"],
            json!({
                "question_0": "Focused", "question_1": "None of the above",
                "question_1_note": "  Custom\r\nscope — café  ", "question_2": "Broad"
            }),
        ),
        (
            ["Custom zero", "Custom one", "Custom two"],
            json!({
                "question_0": "None of the above", "question_0_note": "Custom zero",
                "question_1": "None of the above", "question_1_note": "Custom one",
                "question_2": "None of the above", "question_2_note": "Custom two"
            }),
        ),
    ] {
        let mut response = fixture_answers(
            &converted,
            answers
                .iter()
                .map(|answer| Some(QuestionAnswerValue::String((*answer).into())))
                .collect(),
        );
        // Correlate by property ID, not the frontend's response order.
        if let QuestionResponse::Answered { answers } = &mut response {
            answers.reverse();
        }
        let decision = converted.normalized_decision(&response).unwrap();
        assert_eq!(decision.request_id, converted.request.id);
        assert_eq!(decision.answers.len(), 3);
        for (index, answer) in decision.answers.iter().enumerate() {
            assert_eq!(answer.question_id, format!("question_{index}"));
            assert_eq!(
                answer.decision_id,
                AgentUserDecisionId::from_question(&converted.request.id, &answer.question_id)
            );
            assert_eq!(
                answer.answer,
                AgentUserDecisionValue::String {
                    value: answers[index].into()
                }
            );
        }
        assert_eq!(
            serde_json::to_value(converted.accepted_content(response).unwrap()).unwrap(),
            expected
        );
    }
    for invalid in [
        None,
        Some(QuestionAnswerValue::String(" \n".into())),
        Some(QuestionAnswerValue::Strings(vec!["Focused".into()])),
    ] {
        assert!(
            converted
                .accepted_content(fixture_answers(
                    &converted,
                    vec![
                        invalid,
                        Some(QuestionAnswerValue::String("Focused".into())),
                        Some(QuestionAnswerValue::String("Broad".into()))
                    ]
                ))
                .is_err()
        );
    }
    assert!(
        converted
            .accepted_content(QuestionResponse::Dismissed)
            .is_err()
    );
    assert!(
        converted
            .normalized_decision(&QuestionResponse::Dismissed)
            .is_err()
    );
}

#[test]
fn codex_note_links_follow_question_ids_even_when_property_names_collide() {
    use serde_json::json;
    let mut fixture = codex_acp_1_13_1_elicitation_fixture();
    let properties = fixture["requestedSchema"]["properties"]
        .as_object_mut()
        .unwrap();
    let note = properties.remove("question_0_note").unwrap();
    properties.insert("question_0_note1".into(), note);
    let primary = properties.remove("question_1").unwrap();
    properties.insert("question_0_note".into(), primary);
    let mut note = properties.remove("question_1_note").unwrap();
    note["_meta"]["codex"]["questionId"] = json!("question_0_note");
    properties.insert("question_0_note_note".into(), note);
    // Even a name unrelated to its primary must use explicit metadata linkage.
    let note = properties.remove("question_2_note").unwrap();
    properties.insert("unrelated_property".into(), note);
    fixture["requestedSchema"]["required"] = json!(["question_0", "question_0_note", "question_2"]);
    assert_eq!(
        elicitation_field_count(&elicitation_from_json(fixture.clone())),
        3
    );
    let converted = convert_fixture(fixture);
    assert_eq!(converted.fields.len(), 3);
    assert_eq!(
        converted
            .request
            .questions
            .iter()
            .map(|question| question.id.as_str())
            .collect::<Vec<_>>(),
        ["question_0", "question_0_note", "question_2"]
    );
    let response = fixture_answers(
        &converted,
        ["Custom zero", "Broad", "Custom two"]
            .into_iter()
            .map(|answer| Some(QuestionAnswerValue::String(answer.into())))
            .collect(),
    );
    assert_eq!(
        serde_json::to_value(converted.accepted_content(response).unwrap()).unwrap(),
        json!({
            "question_0": "None of the above", "question_0_note1": "Custom zero",
            "question_0_note": "Broad", "question_2": "None of the above", "unrelated_property": "Custom two"
        })
    );
}

#[test]
fn codex_unmarked_and_unknown_role_notes_remain_real_optional_text_questions() {
    use serde_json::json;
    for marker in [
        json!({}),
        json!({"codex": {"questionId": "question_0"}}),
        json!({"codex": {"questionId": "question_0", "role": "future_role"}}),
        json!({"codex": {"questionId": "question_0", "role": "future_role", "isOtherAnswer": true}}),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0_note"]["_meta"] = marker;
        assert_eq!(
            elicitation_field_count(&elicitation_from_json(fixture.clone())),
            4
        );
        let converted = convert_fixture(fixture);
        assert_eq!(converted.request.questions.len(), 4);
        assert_eq!(converted.fields.len(), 4);
        let note = &converted.request.questions[1];
        assert_eq!(note.id, "question_0_note");
        assert_eq!(note.header, "Additional answer or note");
        assert!(!note.required);
        assert_eq!(
            note.kind,
            QuestionPromptKind::Text {
                min_length: None,
                max_length: None
            }
        );
        assert_eq!(
            converted.request.questions[0].kind,
            QuestionPromptKind::SingleSelect { allow_other: false }
        );
        let response = fixture_answers(
            &converted,
            vec![
                Some(QuestionAnswerValue::String("Focused".into())),
                None,
                Some(QuestionAnswerValue::String("Broad".into())),
                Some(QuestionAnswerValue::String("Focused".into())),
            ],
        );
        assert_eq!(
            converted.normalized_decision(&response).unwrap().answers[1].answer,
            AgentUserDecisionValue::Skipped
        );
        assert_eq!(
            serde_json::to_value(converted.accepted_content(response).unwrap()).unwrap(),
            json!({"question_0": "Focused", "question_1": "Broad", "question_2": "Focused"})
        );
    }
    let mut fixture = codex_acp_1_13_1_elicitation_fixture();
    fixture["requestedSchema"]["required"] = json!(["question_1", "question_2"]);
    let converted = convert_fixture(fixture);
    assert!(!converted.request.questions[0].required);
    assert!(!converted.fields[0].required);
    let response = fixture_answers(
        &converted,
        vec![
            None,
            Some(QuestionAnswerValue::String("Broad".into())),
            Some(QuestionAnswerValue::String("Focused".into())),
        ],
    );
    assert_eq!(
        serde_json::to_value(converted.accepted_content(response).unwrap()).unwrap(),
        json!({"question_1": "Broad", "question_2": "Focused"})
    );
}

#[test]
fn codex_recognized_malformed_note_companions_are_declined_not_partially_folded() {
    use serde_json::json;
    for question_id in [
        None,
        Some(json!(0)),
        Some(json!(" ")),
        Some(json!("missing")),
        Some(json!("question_0_note")),
        Some(json!("question_1")),
        Some(json!("question_1_note")),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        let meta = fixture["requestedSchema"]["properties"]["question_0_note"]["_meta"]["codex"]
            .as_object_mut()
            .unwrap();
        meta.remove("questionId");
        if let Some(question_id) = question_id {
            meta.insert("questionId".into(), question_id);
        }
        assert_native_declined(fixture);
    }
    let mut duplicate = codex_acp_1_13_1_elicitation_fixture();
    duplicate["requestedSchema"]["properties"]["duplicate_note"] =
        duplicate["requestedSchema"]["properties"]["question_0_note"].clone();
    assert_native_declined(duplicate);
    for (key, value) in [
        ("type", json!("boolean")),
        ("type", json!("integer")),
        ("pattern", json!(".*")),
        ("format", json!("email")),
        ("minLength", json!(0)),
        ("maxLength", json!(80)),
        ("enum", json!(["custom"])),
        ("oneOf", json!([{"const": "custom", "title": "Custom"}])),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0_note"][key] = value;
        assert_native_declined(fixture);
    }
    let mut required_note = codex_acp_1_13_1_elicitation_fixture();
    required_note["requestedSchema"]["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("question_0_note"));
    assert_native_declined(required_note);
    for meta in [
        json!({}),
        json!({"codex": {}}),
        json!({"codex": {"isOther": false}}),
        json!({"codex": {"isOther": "true"}}),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0"]["_meta"] = meta;
        assert_native_declined(fixture);
    }
    let mut contradictory = codex_acp_1_13_1_elicitation_fixture();
    contradictory["requestedSchema"]["properties"]["question_0_note"]["_meta"]["codex"]["isOtherAnswer"] =
        json!(false);
    assert_native_declined(contradictory);
    // An earlier namespace must not mask malformed Codex linkage/primary metadata.
    for marker in [
        json!({"isOtherAnswer": true, "questionId": "question_0", "otherValue": "None of the above"}),
        json!({"isOtherAnswer": true, "questionId": "question_1", "otherValue": "None of the above"}),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0_note"]["_meta"]["zevria"] = marker;
        fixture["requestedSchema"]["properties"]["question_0"]["_meta"]["codex"]["isOther"] =
            json!(false);
        assert_native_declined(fixture);
    }
    for mut primary in [
        json!({"type": "string"}),
        json!({"type": "boolean"}),
        json!({"type": "array", "items": {"type": "string", "enum": ["Focused", "None of the above"]}}),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        primary["_meta"] = json!({"codex": {"isOther": true}});
        fixture["requestedSchema"]["properties"]["question_0"] = primary;
        assert_native_declined(fixture);
    }
    for choices in [
        json!([]),
        json!(["Focused"]),
        json!(["None of the above"]),
        json!(["Focused", "None of the above", "None of the above"]),
        json!(["Focused", "Focused", "None of the above"]),
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        fixture["requestedSchema"]["properties"]["question_0"]["oneOf"] = serde_json::Value::Array(
            choices
                .as_array()
                .unwrap()
                .iter()
                .map(|value| json!({"const": value, "title": value}))
                .collect(),
        );
        assert_native_declined(fixture);
    }
    for path in [
        "",
        "/requestedSchema",
        "/requestedSchema/properties/question_0",
        "/requestedSchema/properties/question_0_note",
    ] {
        let mut fixture = codex_acp_1_13_1_elicitation_fixture();
        fixture.pointer_mut(path).unwrap()["_meta"]["codex"]["isSecret"] = json!(true);
        let error = convert_elicitation(
            &elicitation_from_json(fixture),
            Some("session-1"),
            "Any worker label",
            QuestionRequestId::new("secret"),
        )
        .unwrap_err();
        assert!(matches!(error, ElicitationConversionError::Decline { .. }));
        assert_eq!(error.field_count(), 3);
    }
}

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
