//! Deterministic ACP form normalization.

use super::*;

impl ConvertedElicitation {
    pub(super) fn normalized_decision(
        &self,
        response: &QuestionResponse,
    ) -> Result<AgentUserDecisionBatch, String> {
        let QuestionResponse::Answered { answers } = response else {
            return Err("a dismissed form cannot become a normalized decision".to_string());
        };
        if answers.len() != self.request.questions.len() {
            return Err("the frontend returned the wrong number of elicitation fields".to_string());
        }
        let mut answers_by_id = HashMap::new();
        for answer in answers {
            if answers_by_id
                .insert(answer.id.as_str(), &answer.answer)
                .is_some()
            {
                return Err("the frontend returned a duplicate elicitation field".to_string());
            }
        }
        let mut normalized = Vec::with_capacity(self.request.questions.len());
        for question in &self.request.questions {
            let answer = answers_by_id
                .remove(question.id.as_str())
                .ok_or_else(|| "the frontend omitted an elicitation field result".to_string())?;
            let answer = match answer {
                Some(QuestionAnswerValue::String(value)) => AgentUserDecisionValue::String {
                    value: value.clone(),
                },
                Some(QuestionAnswerValue::Strings(values)) => AgentUserDecisionValue::Strings {
                    values: values.clone(),
                },
                None => AgentUserDecisionValue::Skipped,
            };
            normalized.push(AgentUserDecisionAnswer {
                decision_id: AgentUserDecisionId::from_question(&self.request.id, &question.id),
                question_id: question.id.clone(),
                header: question.header.clone(),
                question: question.question.clone(),
                answer,
            });
        }
        if !answers_by_id.is_empty() {
            return Err("the frontend returned an unknown elicitation field".to_string());
        }
        Ok(AgentUserDecisionBatch {
            request_id: self.request.id.clone(),
            answers: normalized,
        })
    }

    pub(super) fn accepted_content(
        &self,
        response: QuestionResponse,
    ) -> Result<BTreeMap<String, ElicitationContentValue>, String> {
        let QuestionResponse::Answered { answers } = response else {
            return Err("a dismissed form cannot be converted to accepted content".to_string());
        };
        if answers.len() != self.fields.len() {
            return Err("the frontend returned the wrong number of elicitation fields".to_string());
        }
        let mut answers_by_id = HashMap::new();
        for answer in answers {
            if answers_by_id.insert(answer.id, answer.answer).is_some() {
                return Err("the frontend returned a duplicate elicitation field".to_string());
            }
        }

        let mut content = BTreeMap::new();
        for field in &self.fields {
            let answer = answers_by_id
                .remove(&field.property)
                .ok_or_else(|| "the frontend omitted an elicitation field result".to_string())?;
            let Some(answer) = answer else {
                if field.required {
                    return Err("the frontend skipped a required elicitation field".to_string());
                }
                continue;
            };
            match (&field.kind, answer) {
                (ElicitationFieldKind::Text, QuestionAnswerValue::String(value)) => {
                    content.insert(
                        field.property.clone(),
                        ElicitationContentValue::String(value),
                    );
                }
                (
                    ElicitationFieldKind::Single { values, companion },
                    QuestionAnswerValue::String(value),
                ) => {
                    if let Some(wire_value) = values.get(&value) {
                        content.insert(
                            field.property.clone(),
                            ElicitationContentValue::String(wire_value.clone()),
                        );
                    } else if let Some(companion) = companion {
                        if let Some(token) = &companion.other_value {
                            if value.trim().is_empty() {
                                return Err("the frontend returned a blank native custom answer"
                                    .to_string());
                            }
                            content.insert(
                                field.property.clone(),
                                ElicitationContentValue::String(token.clone()),
                            );
                        }
                        content.insert(
                            companion.property.clone(),
                            ElicitationContentValue::String(value),
                        );
                    } else {
                        return Err(
                            "the frontend returned an unknown single-select value".to_string()
                        );
                    }
                }
                (
                    ElicitationFieldKind::Multi { values, companion },
                    QuestionAnswerValue::Strings(selected),
                ) => {
                    let mut wire_values = Vec::new();
                    let mut custom = None;
                    for value in selected {
                        if let Some(wire_value) = values.get(&value) {
                            wire_values.push(wire_value.clone());
                        } else if companion.is_some() && custom.is_none() {
                            custom = Some(value);
                        } else {
                            return Err(
                                "the frontend returned an unknown multi-select value".to_string()
                            );
                        }
                    }
                    if let (Some(companion), Some(custom)) = (companion, custom) {
                        if let Some(token) = &companion.other_value {
                            if custom.trim().is_empty() {
                                return Err("the frontend returned a blank native custom answer"
                                    .to_string());
                            }
                            wire_values.push(token.clone());
                        }
                        content.insert(
                            companion.property.clone(),
                            ElicitationContentValue::String(custom),
                        );
                    }
                    content.insert(
                        field.property.clone(),
                        ElicitationContentValue::StringArray(wire_values),
                    );
                }
                (ElicitationFieldKind::Boolean { values }, QuestionAnswerValue::String(value)) => {
                    let value = values.get(&value).copied().ok_or_else(|| {
                        "the frontend returned an unknown boolean value".to_string()
                    })?;
                    content.insert(
                        field.property.clone(),
                        ElicitationContentValue::Boolean(value),
                    );
                }
                _ => {
                    return Err(
                        "the frontend returned the wrong elicitation value type".to_string()
                    );
                }
            }
        }
        if !answers_by_id.is_empty() {
            return Err("the frontend returned an unknown elicitation field".to_string());
        }
        Ok(content)
    }
}

pub(super) fn convert_elicitation(
    request: &CreateElicitationRequest,
    expected_session_id: Option<&str>,
    source_label: &str,
    request_id: QuestionRequestId,
) -> Result<ConvertedElicitation, ElicitationConversionError> {
    let field_count = elicitation_field_count(request);
    let scope = request.scope();
    match scope {
        ElicitationScope::Session(scope)
            if expected_session_id
                .is_some_and(|expected| scope.session_id.to_string() == expected) => {}
        ElicitationScope::Session(_) if expected_session_id.is_none() => {
            return Err(ElicitationConversionError::decline(
                field_count,
                "session-scoped elicitation arrived before the ACP session was ready",
            ));
        }
        ElicitationScope::Session(_) => {
            return Err(ElicitationConversionError::ProtocolViolation {
                field_count,
                reason: "elicitation targeted a different ACP session".to_string(),
            });
        }
        ElicitationScope::Request(_) => {
            return Err(ElicitationConversionError::decline(
                field_count,
                "request-scoped elicitation is unsupported",
            ));
        }
        _ => {
            return Err(ElicitationConversionError::decline(
                field_count,
                "unknown elicitation scope is unsupported",
            ));
        }
    }
    let ElicitationMode::Form(form) = &request.mode else {
        return Err(ElicitationConversionError::decline(
            field_count,
            "only form elicitation is supported",
        ));
    };
    if meta_marks_secret(request.meta.as_ref())
        || meta_marks_secret(form.requested_schema.meta.as_ref())
    {
        return Err(ElicitationConversionError::decline(
            field_count,
            "secret-marked elicitation is unsupported",
        ));
    }
    convert_elicitation_form(
        &request.message,
        &form.requested_schema.properties,
        form.requested_schema.required.as_deref(),
        source_label,
        request_id,
    )
}

pub(super) fn elicitation_field_count(request: &CreateElicitationRequest) -> usize {
    match &request.mode {
        ElicitationMode::Form(form) => {
            visible_elicitation_property_count(&form.requested_schema.properties)
        }
        _ => 0,
    }
}

pub(super) fn visible_elicitation_property_count(
    properties: &BTreeMap<String, ElicitationPropertySchema>,
) -> usize {
    properties
        .values()
        .filter(|property| {
            !matches!(
                custom_companion_target(property_meta(property)),
                Ok(Some(_))
            )
        })
        .count()
}

// Display copies only: IDs, validation inputs, wire constants, and user answers
// must never pass through this helper.
pub(super) fn elicitation_display_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "")
}

pub(super) fn convert_elicitation_form(
    message: &str,
    properties: &BTreeMap<String, ElicitationPropertySchema>,
    required_fields: Option<&[String]>,
    source_label: &str,
    request_id: QuestionRequestId,
) -> Result<ConvertedElicitation, ElicitationConversionError> {
    let field_count = visible_elicitation_property_count(properties);
    if properties.is_empty() {
        return Err(ElicitationConversionError::decline(
            0,
            "empty elicitation forms are unsupported",
        ));
    }
    if properties.keys().any(|name| name.trim().is_empty()) {
        return Err(ElicitationConversionError::decline(
            field_count,
            "elicitation property names must not be blank",
        ));
    }
    let mut required = HashSet::new();
    for name in required_fields.unwrap_or_default() {
        if !properties.contains_key(name) || !required.insert(name.clone()) {
            return Err(ElicitationConversionError::decline(
                field_count,
                "elicitation required fields are malformed",
            ));
        }
    }

    let mut companion_names = HashSet::new();
    let mut companions = HashMap::<String, CustomAnswerCompanion>::new();
    for (name, property) in properties {
        let meta = property_meta(property);
        if meta_marks_secret(meta) {
            return Err(ElicitationConversionError::decline(
                field_count,
                "secret-marked elicitation is unsupported",
            ));
        }
        let Some(target) = custom_companion_target(meta)
            .map_err(|reason| ElicitationConversionError::decline(field_count, reason))?
        else {
            continue;
        };
        let ElicitationPropertySchema::String(schema) = property else {
            return Err(ElicitationConversionError::decline(
                field_count,
                "custom-answer companions must be plain strings",
            ));
        };
        if required.contains(name)
            || schema.pattern.is_some()
            || schema.format.is_some()
            || schema.enum_values.is_some()
            || schema.one_of.is_some()
            || (schema.default.is_some() && target.other_value.is_none())
            || schema.min_length.is_some()
            || schema.max_length.is_some()
        {
            return Err(ElicitationConversionError::decline(
                field_count,
                "custom-answer companion constraints are unsupported",
            ));
        }
        if target.question_id == *name
            || !properties.contains_key(&target.question_id)
            || companions.contains_key(&target.question_id)
        {
            return Err(ElicitationConversionError::decline(
                field_count,
                "custom-answer companion metadata is inconsistent",
            ));
        }
        companion_names.insert(name.clone());
        companions.insert(
            target.question_id,
            CustomAnswerCompanion {
                property: name.clone(),
                other_value: target.other_value,
                default: schema.default.clone(),
            },
        );
    }
    if companions
        .keys()
        .any(|target| companion_names.contains(target))
    {
        return Err(ElicitationConversionError::decline(
            field_count,
            "custom-answer companions cannot target another companion",
        ));
    }

    let visible_count = properties.len().saturating_sub(companion_names.len());
    if visible_count == 0 {
        return Err(ElicitationConversionError::decline(
            field_count,
            "elicitation form contains no visible fields",
        ));
    }
    let mut prompts = Vec::with_capacity(visible_count);
    let mut mappings = Vec::with_capacity(visible_count);
    for (name, property) in properties {
        if companion_names.contains(name) {
            continue;
        }
        let is_required = required.contains(name);
        let companion = companions.get(name).cloned();
        if is_required
            && companion
                .as_ref()
                .is_some_and(|companion| companion.other_value.is_none())
        {
            return Err(ElicitationConversionError::decline(
                field_count,
                "a select with a legacy custom-answer companion must remain optional",
            ));
        }
        let (title, description) = property_title_description(property);
        let header = title
            .map(|title| elicitation_display_text(&title))
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| elicitation_display_text(name));
        let question = description
            .map(|description| elicitation_display_text(&description))
            .filter(|description| !description.trim().is_empty())
            .unwrap_or_else(|| {
                let message = elicitation_display_text(message);
                if message.trim().is_empty() {
                    header.clone()
                } else {
                    message
                }
            });

        let (prompt, mapping) = match property {
            ElicitationPropertySchema::String(schema) => convert_string_field(
                name,
                header,
                question,
                schema,
                is_required,
                companion,
                field_count,
            )?,
            ElicitationPropertySchema::Array(schema) => convert_multi_field(
                name,
                header,
                question,
                schema,
                is_required,
                companion,
                field_count,
            )?,
            ElicitationPropertySchema::Boolean(schema) => {
                if companion.is_some() {
                    return Err(ElicitationConversionError::decline(
                        field_count,
                        "boolean fields cannot have custom-answer companions",
                    ));
                }
                let options = vec![
                    QuestionOption {
                        label: "Yes".to_string(),
                        description: "True".to_string(),
                    },
                    QuestionOption {
                        label: "No".to_string(),
                        description: "False".to_string(),
                    },
                ];
                let default = schema.default.map(|value| {
                    QuestionAnswerValue::String(if value { "Yes" } else { "No" }.to_string())
                });
                (
                    QuestionPrompt {
                        id: name.clone(),
                        header,
                        question,
                        options,
                        kind: QuestionPromptKind::SingleSelect { allow_other: false },
                        required: is_required,
                        default,
                    },
                    ElicitationFieldMapping {
                        property: name.clone(),
                        required: is_required,
                        kind: ElicitationFieldKind::Boolean {
                            values: HashMap::from([
                                ("Yes".to_string(), true),
                                ("No".to_string(), false),
                            ]),
                        },
                    },
                )
            }
            ElicitationPropertySchema::Number(_)
            | ElicitationPropertySchema::Integer(_)
            | ElicitationPropertySchema::Other(_) => {
                return Err(ElicitationConversionError::decline(
                    field_count,
                    "elicitation property type is unsupported",
                ));
            }
            _ => {
                return Err(ElicitationConversionError::decline(
                    field_count,
                    "unknown elicitation property type is unsupported",
                ));
            }
        };
        prompts.push(prompt);
        mappings.push(mapping);
    }

    Ok(ConvertedElicitation {
        request: QuestionRequest {
            id: request_id,
            questions: prompts,
            source_label: Some(source_label.to_string()),
            dismissible: true,
        },
        fields: mappings,
    })
}

pub(super) fn convert_string_field(
    name: &str,
    header: String,
    question: String,
    schema: &agent_client_protocol::schema::v1::StringPropertySchema,
    required: bool,
    companion: Option<CustomAnswerCompanion>,
    field_count: usize,
) -> Result<(QuestionPrompt, ElicitationFieldMapping), ElicitationConversionError> {
    if schema.pattern.is_some() || schema.format.is_some() {
        return Err(ElicitationConversionError::decline(
            field_count,
            "string pattern and format validation are unsupported",
        ));
    }
    let min_length = schema
        .min_length
        .map(usize::try_from)
        .transpose()
        .map_err(|_| {
            ElicitationConversionError::decline(field_count, "string length bound is too large")
        })?;
    let max_length = schema
        .max_length
        .map(usize::try_from)
        .transpose()
        .map_err(|_| {
            ElicitationConversionError::decline(field_count, "string length bound is too large")
        })?;
    if min_length
        .zip(max_length)
        .is_some_and(|(min, max)| min > max)
    {
        return Err(ElicitationConversionError::decline(
            field_count,
            "string length bounds are inconsistent",
        ));
    }
    if schema.enum_values.is_some() && schema.one_of.is_some() {
        return Err(ElicitationConversionError::decline(
            field_count,
            "string field cannot combine enum and oneOf",
        ));
    }
    let raw_options = if let Some(values) = &schema.enum_values {
        Some(
            values
                .iter()
                .map(|value| (value.clone(), value.clone(), String::new()))
                .collect::<Vec<_>>(),
        )
    } else {
        schema.one_of.as_ref().map(|options| {
            options
                .iter()
                .map(|option| {
                    (
                        option.value.clone(),
                        option.title.clone(),
                        option.description.clone().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        })
    };
    let Some(raw_options) = raw_options else {
        if companion.is_some() {
            return Err(ElicitationConversionError::decline(
                field_count,
                "custom-answer companion target must be a select field",
            ));
        }
        if let Some(default) = &schema.default {
            validate_text_default(default, min_length, max_length, field_count)?;
        }
        return Ok((
            QuestionPrompt {
                id: name.to_string(),
                header,
                question,
                options: Vec::new(),
                kind: QuestionPromptKind::Text {
                    min_length,
                    max_length,
                },
                required,
                default: schema.default.clone().map(QuestionAnswerValue::String),
            },
            ElicitationFieldMapping {
                property: name.to_string(),
                required,
                kind: ElicitationFieldKind::Text,
            },
        ));
    };
    if raw_options.is_empty() {
        return Err(ElicitationConversionError::decline(
            field_count,
            "select field has no choices",
        ));
    }
    for (value, _, _) in &raw_options {
        validate_text_default(value, min_length, max_length, field_count)?;
    }
    let raw_options = filter_native_other(raw_options, companion.as_ref(), field_count)?;
    let (options, values) = display_options(raw_options, field_count)?;
    let default = schema
        .default
        .as_ref()
        .map(|default| {
            select_default_label(default, &values, companion.as_ref(), field_count)
                .map(QuestionAnswerValue::String)
        })
        .transpose()?;
    let allow_other = companion.is_some();
    Ok((
        QuestionPrompt {
            id: name.to_string(),
            header,
            question,
            options,
            kind: QuestionPromptKind::SingleSelect { allow_other },
            required,
            default,
        },
        ElicitationFieldMapping {
            property: name.to_string(),
            required,
            kind: ElicitationFieldKind::Single { values, companion },
        },
    ))
}

pub(super) fn convert_multi_field(
    name: &str,
    header: String,
    question: String,
    schema: &agent_client_protocol::schema::v1::MultiSelectPropertySchema,
    required: bool,
    companion: Option<CustomAnswerCompanion>,
    field_count: usize,
) -> Result<(QuestionPrompt, ElicitationFieldMapping), ElicitationConversionError> {
    let min_selections = schema
        .min_items
        .map(usize::try_from)
        .transpose()
        .map_err(|_| {
            ElicitationConversionError::decline(field_count, "selection bound is too large")
        })?;
    let max_selections = schema
        .max_items
        .map(usize::try_from)
        .transpose()
        .map_err(|_| {
            ElicitationConversionError::decline(field_count, "selection bound is too large")
        })?;
    if min_selections
        .zip(max_selections)
        .is_some_and(|(min, max)| min > max)
    {
        return Err(ElicitationConversionError::decline(
            field_count,
            "selection bounds are inconsistent",
        ));
    }
    let raw_options = match &schema.items {
        MultiSelectItems::String(items) => items
            .values
            .iter()
            .map(|value| (value.clone(), value.clone(), String::new()))
            .collect(),
        MultiSelectItems::Titled(items) => items
            .options
            .iter()
            .map(|option| {
                (
                    option.value.clone(),
                    option.title.clone(),
                    option.description.clone().unwrap_or_default(),
                )
            })
            .collect(),
        MultiSelectItems::Other(_) => {
            return Err(ElicitationConversionError::decline(
                field_count,
                "custom array item schemas are unsupported",
            ));
        }
        _ => {
            return Err(ElicitationConversionError::decline(
                field_count,
                "unknown array item schema is unsupported",
            ));
        }
    };
    let raw_options = filter_native_other(raw_options, companion.as_ref(), field_count)?;
    let (options, values) = display_options(raw_options, field_count)?;
    if min_selections
        .is_some_and(|minimum| minimum > options.len() + usize::from(companion.is_some()))
    {
        return Err(ElicitationConversionError::decline(
            field_count,
            "minimum selection count exceeds available choices",
        ));
    }
    let default = schema
        .default
        .as_ref()
        .map(|defaults| {
            let mut labels = Vec::with_capacity(defaults.len());
            let mut seen = HashSet::new();
            for default in defaults {
                if !seen.insert(default) {
                    return Err(ElicitationConversionError::decline(
                        field_count,
                        "multi-select default contains duplicates",
                    ));
                }
                let label =
                    select_default_label(default, &values, companion.as_ref(), field_count)?;
                labels.push(label);
            }
            if min_selections.is_some_and(|minimum| labels.len() < minimum)
                || max_selections.is_some_and(|maximum| labels.len() > maximum)
            {
                return Err(ElicitationConversionError::decline(
                    field_count,
                    "multi-select default violates selection bounds",
                ));
            }
            Ok(QuestionAnswerValue::Strings(labels))
        })
        .transpose()?;
    let allow_other = companion.is_some();
    Ok((
        QuestionPrompt {
            id: name.to_string(),
            header,
            question,
            options,
            kind: QuestionPromptKind::MultiSelect {
                min_selections,
                max_selections,
                allow_other,
            },
            required,
            default,
        },
        ElicitationFieldMapping {
            property: name.to_string(),
            required,
            kind: ElicitationFieldKind::Multi { values, companion },
        },
    ))
}

/// Remove only the explicitly marked wire option, before display disambiguation.
pub(super) fn filter_native_other(
    mut options: Vec<(String, String, String)>,
    companion: Option<&CustomAnswerCompanion>,
    field_count: usize,
) -> Result<Vec<(String, String, String)>, ElicitationConversionError> {
    if let Some(token) = companion.and_then(|companion| companion.other_value.as_ref()) {
        if options
            .iter()
            .filter(|(value, _, _)| value == token)
            .count()
            != 1
        {
            return Err(ElicitationConversionError::decline(
                field_count,
                "native Other token must occur exactly once in select choices",
            ));
        }
        options.retain(|(value, _, _)| value != token);
    }
    // display_options validates remaining duplicates and rejects an empty list.
    Ok(options)
}

pub(super) fn select_default_label(
    default: &str,
    values: &HashMap<String, String>,
    companion: Option<&CustomAnswerCompanion>,
    field_count: usize,
) -> Result<String, ElicitationConversionError> {
    if let Some(companion) = companion
        && companion.other_value.as_deref() == Some(default)
    {
        let custom = companion.default.as_ref().ok_or_else(|| {
            ElicitationConversionError::decline(
                field_count,
                "native Other default requires companion text",
            )
        })?;
        if custom.trim().is_empty() || values.contains_key(custom.trim()) {
            return Err(ElicitationConversionError::decline(
                field_count,
                "native Other default must be nonblank and distinct from display choices",
            ));
        }
        return Ok(custom.clone());
    }
    values
        .iter()
        .find_map(|(label, value)| (value == default).then(|| label.clone()))
        .ok_or_else(|| {
            ElicitationConversionError::decline(
                field_count,
                "select default is not an allowed value",
            )
        })
}

pub(super) fn validate_text_default(
    value: &str,
    minimum: Option<usize>,
    maximum: Option<usize>,
    field_count: usize,
) -> Result<(), ElicitationConversionError> {
    let length = value.chars().count();
    if minimum.is_some_and(|minimum| length < minimum)
        || maximum.is_some_and(|maximum| length > maximum)
    {
        return Err(ElicitationConversionError::decline(
            field_count,
            "string value violates its length bounds",
        ));
    }
    Ok(())
}

pub(super) fn display_options(
    raw: Vec<(String, String, String)>,
    field_count: usize,
) -> Result<(Vec<QuestionOption>, HashMap<String, String>), ElicitationConversionError> {
    if raw.is_empty() {
        return Err(ElicitationConversionError::decline(
            field_count,
            "select field has no choices",
        ));
    }
    let mut wire_values = HashSet::new();
    let mut labels = HashSet::from(["other".to_string(), "skip".to_string()]);
    let mut options = Vec::with_capacity(raw.len());
    let mut mapping = HashMap::with_capacity(raw.len());
    for (wire_value, title, description) in raw {
        if !wire_values.insert(wire_value.clone()) {
            return Err(ElicitationConversionError::decline(
                field_count,
                "select field contains duplicate constants",
            ));
        }
        let title = elicitation_display_text(&title);
        let description = elicitation_display_text(&description);
        let base = if title.trim().is_empty() {
            let display_value = elicitation_display_text(&wire_value);
            if display_value.trim().is_empty() {
                "Option".to_string()
            } else {
                display_value
            }
        } else {
            title.trim().to_string()
        };
        let mut label = base.clone();
        let mut suffix = 1usize;
        while !labels.insert(label.to_lowercase()) {
            suffix += 1;
            label = if suffix == 2 {
                format!("{base} (option)")
            } else {
                format!("{base} (option {suffix})")
            };
        }
        mapping.insert(label.clone(), wire_value);
        options.push(QuestionOption { label, description });
    }
    Ok((options, mapping))
}

pub(super) fn property_title_description(
    property: &ElicitationPropertySchema,
) -> (Option<String>, Option<String>) {
    match property {
        ElicitationPropertySchema::String(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Boolean(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Array(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Number(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Integer(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Other(_) => (None, None),
        _ => (None, None),
    }
}

pub(super) fn property_meta(property: &ElicitationPropertySchema) -> Option<&Meta> {
    match property {
        ElicitationPropertySchema::String(schema) => schema.meta.as_ref(),
        ElicitationPropertySchema::Boolean(schema) => schema.meta.as_ref(),
        ElicitationPropertySchema::Array(schema) => schema.meta.as_ref(),
        ElicitationPropertySchema::Number(schema) => schema.meta.as_ref(),
        ElicitationPropertySchema::Integer(schema) => schema.meta.as_ref(),
        ElicitationPropertySchema::Other(_) => None,
        _ => None,
    }
}

pub(super) fn custom_companion_target(
    meta: Option<&Meta>,
) -> Result<Option<CustomCompanionTarget>, &'static str> {
    let Some(meta) = meta else {
        return Ok(None);
    };
    for (namespace, flag) in [
        ("zevria", "isOtherAnswer"),
        ("codex", "isOtherAnswer"),
        ("_askUserQuestionCustomAnswer", "isCustomAnswer"),
    ] {
        let Some(value) = meta.get(namespace) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            return Err("custom-answer metadata must be an object");
        };
        let Some(flag_value) = object.get(flag) else {
            continue;
        };
        let Some(enabled) = flag_value.as_bool() else {
            return Err("custom-answer metadata flag must be boolean");
        };
        if !enabled {
            continue;
        }
        let Some(question_id) = object.get("questionId").and_then(serde_json::Value::as_str) else {
            return Err("custom-answer metadata is missing questionId");
        };
        if question_id.trim().is_empty() {
            return Err("custom-answer metadata questionId must not be blank");
        }
        let other_value = if namespace == "zevria" {
            let Some(token) = object.get("otherValue").and_then(serde_json::Value::as_str) else {
                return Err("native custom-answer metadata is missing otherValue");
            };
            if token.trim().is_empty() {
                return Err("native custom-answer metadata otherValue must not be blank");
            }
            Some(token.to_string())
        } else {
            None
        };
        return Ok(Some(CustomCompanionTarget {
            question_id: question_id.to_string(),
            other_value,
        }));
    }
    Ok(None)
}

pub(super) fn meta_marks_secret(meta: Option<&Meta>) -> bool {
    pub(super) fn marked(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
                let key = key.to_ascii_lowercase();
                ((key.contains("secret") || key.contains("password"))
                    && match value {
                        serde_json::Value::Bool(value) => *value,
                        serde_json::Value::Null => false,
                        serde_json::Value::String(value) => !value.is_empty(),
                        _ => true,
                    })
                    || marked(value)
            }),
            serde_json::Value::Array(values) => values.iter().any(marked),
            _ => false,
        }
    }
    meta.is_some_and(|meta| marked(&serde_json::Value::Object(meta.clone())))
}
