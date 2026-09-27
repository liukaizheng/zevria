use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationSchema, ElicitationSessionScope,
    EnumOption, MultiSelectPropertySchema, SessionId, StringPropertySchema,
};
use serde_json::json;
use zevria_foundation::QuestionAnswer;
use zevria_foundation::QuestionAnswerValue;
use zevria_foundation::QuestionPrompt;
use zevria_foundation::QuestionPromptKind;
use zevria_foundation::QuestionRequest;
use zevria_foundation::QuestionResponse;

const OTHER_TOKEN: &str = "__zevria_other__";

#[derive(Debug, Clone)]
pub(crate) struct QuestionForm {
    pub request: CreateElicitationRequest,
    bindings: Vec<QuestionBinding>,
}

#[derive(Debug, Clone)]
struct QuestionBinding {
    id: String,
    property: String,
    other_property: Option<String>,
    kind: BindingKind,
    required: bool,
}

#[derive(Debug, Clone)]
enum BindingKind {
    Text {
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Single {
        options: BTreeMap<String, String>,
        allow_other: bool,
    },
    Multi {
        options: BTreeMap<String, String>,
        min_selections: Option<usize>,
        max_selections: Option<usize>,
        allow_other: bool,
    },
}

impl QuestionForm {
    pub(crate) fn response(
        &self,
        response: CreateElicitationResponse,
    ) -> Result<QuestionResponse, String> {
        let ElicitationAction::Accept(accepted) = response.action else {
            return Ok(QuestionResponse::Dismissed);
        };
        let mut content = accepted.content.unwrap_or_default();
        let mut answers = Vec::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            let value = content.remove(&binding.property);
            let answer = match (&binding.kind, value) {
                (_, None) if !binding.required => None,
                (_, None) => {
                    return Err(format!(
                        "the client omitted required elicitation field {}",
                        binding.property
                    ));
                }
                (
                    BindingKind::Text {
                        min_length,
                        max_length,
                    },
                    Some(ElicitationContentValue::String(value)),
                ) => {
                    validate_length(&value, *min_length, *max_length)?;
                    Some(QuestionAnswerValue::String(value))
                }
                (
                    BindingKind::Single {
                        options,
                        allow_other,
                    },
                    Some(ElicitationContentValue::String(value)),
                ) => {
                    if value == OTHER_TOKEN && *allow_other {
                        let other = take_other(&mut content, binding)?;
                        Some(QuestionAnswerValue::String(other))
                    } else {
                        Some(QuestionAnswerValue::String(
                            options.get(&value).cloned().ok_or_else(|| {
                                "the client returned an unknown choice".to_string()
                            })?,
                        ))
                    }
                }
                (
                    BindingKind::Multi {
                        options,
                        min_selections,
                        max_selections,
                        allow_other,
                    },
                    Some(ElicitationContentValue::StringArray(values)),
                ) => {
                    if min_selections.is_some_and(|minimum| values.len() < minimum)
                        || max_selections.is_some_and(|maximum| values.len() > maximum)
                    {
                        return Err(
                            "the client returned an invalid number of selections".to_string()
                        );
                    }
                    let mut selected = Vec::with_capacity(values.len());
                    for value in values {
                        if value == OTHER_TOKEN && *allow_other {
                            selected.push(take_other(&mut content, binding)?);
                        } else {
                            selected.push(options.get(&value).cloned().ok_or_else(|| {
                                "the client returned an unknown selection".to_string()
                            })?);
                        }
                    }
                    Some(QuestionAnswerValue::Strings(selected))
                }
                _ => {
                    return Err(format!(
                        "the client returned the wrong value type for {}",
                        binding.property
                    ));
                }
            };
            answers.push(QuestionAnswer {
                id: binding.id.clone(),
                answer,
            });
        }
        Ok(QuestionResponse::Answered { answers })
    }
}

pub(crate) fn question_form(
    session_id: &SessionId,
    request: &QuestionRequest,
) -> Result<QuestionForm, String> {
    if request.questions.is_empty() {
        return Err("question batch contains no fields".to_string());
    }
    let mut schema = ElicitationSchema::new().title(
        request
            .source_label
            .clone()
            .unwrap_or_else(|| "Zevria question".to_string()),
    );
    let mut bindings = Vec::with_capacity(request.questions.len());

    for (index, question) in request.questions.iter().enumerate() {
        let property = format!("question_{index}");
        let (property_schema, binding_kind, other_property) = question_property(question, index)?;
        schema = schema.property(property.clone(), property_schema, question.required);
        if let Some(other_property) = &other_property {
            let mut companion = StringPropertySchema::new()
                .title(format!("{} — Other", question.header))
                .description("Custom answer used when Other is selected.")
                .meta(serde_json::Map::from_iter([(
                    "zevria".to_string(),
                    json!({
                        "questionId": property,
                        "isOtherAnswer": true,
                        "otherValue": OTHER_TOKEN,
                    }),
                )]));
            if let Some(default) = other_default(question) {
                companion = companion.default_value(default);
            }
            schema = schema.property(other_property.clone(), companion, false);
        }
        bindings.push(QuestionBinding {
            id: question.id.clone(),
            property,
            other_property,
            kind: binding_kind,
            required: question.required,
        });
    }

    let mode = ElicitationFormMode::new(ElicitationSessionScope::new(session_id.clone()), schema);
    Ok(QuestionForm {
        request: CreateElicitationRequest::new(mode, "Zevria needs additional information."),
        bindings,
    })
}

fn question_property(
    question: &QuestionPrompt,
    index: usize,
) -> Result<
    (
        agent_client_protocol::schema::v1::ElicitationPropertySchema,
        BindingKind,
        Option<String>,
    ),
    String,
> {
    match &question.kind {
        QuestionPromptKind::Text {
            min_length,
            max_length,
        } => {
            let mut schema = StringPropertySchema::new()
                .title(question.header.clone())
                .description(question.question.clone())
                .min_length(min_length.map(to_u32).transpose()?)
                .max_length(max_length.map(to_u32).transpose()?);
            if let Some(QuestionAnswerValue::String(default)) = &question.default {
                schema = schema.default_value(default.clone());
            }
            Ok((
                schema.into(),
                BindingKind::Text {
                    min_length: *min_length,
                    max_length: *max_length,
                },
                None,
            ))
        }
        QuestionPromptKind::SingleSelect { allow_other } => {
            let (options, values) = select_options(question, *allow_other);
            let mut schema = StringPropertySchema::new()
                .title(question.header.clone())
                .description(question.question.clone())
                .one_of(values);
            if let Some(QuestionAnswerValue::String(default)) = &question.default {
                if let Some(token) = options
                    .iter()
                    .find_map(|(token, label)| (label == default).then_some(token.clone()))
                {
                    schema = schema.default_value(token);
                } else if *allow_other {
                    schema = schema.default_value(OTHER_TOKEN.to_string());
                }
            }
            Ok((
                schema.into(),
                BindingKind::Single {
                    options,
                    allow_other: *allow_other,
                },
                allow_other.then(|| format!("question_{index}_other")),
            ))
        }
        QuestionPromptKind::MultiSelect {
            min_selections,
            max_selections,
            allow_other,
        } => {
            let (options, values) = select_options(question, *allow_other);
            let mut schema = MultiSelectPropertySchema::titled(values)
                .title(question.header.clone())
                .description(question.question.clone())
                .min_items(min_selections.map(|value| value as u64))
                .max_items(max_selections.map(|value| value as u64));
            if let Some(QuestionAnswerValue::Strings(defaults)) = &question.default {
                let mut selected = defaults
                    .iter()
                    .filter_map(|default| {
                        options
                            .iter()
                            .find_map(|(token, label)| (label == default).then_some(token.clone()))
                    })
                    .collect::<Vec<_>>();
                if *allow_other
                    && defaults
                        .iter()
                        .any(|default| !options.values().any(|label| label == default))
                {
                    selected.push(OTHER_TOKEN.to_string());
                }
                schema = schema.default_value(selected);
            }
            Ok((
                schema.into(),
                BindingKind::Multi {
                    options,
                    min_selections: *min_selections,
                    max_selections: *max_selections,
                    allow_other: *allow_other,
                },
                allow_other.then(|| format!("question_{index}_other")),
            ))
        }
    }
}

fn select_options(
    question: &QuestionPrompt,
    allow_other: bool,
) -> (BTreeMap<String, String>, Vec<EnumOption>) {
    let mut options = BTreeMap::new();
    let mut values = Vec::new();
    for (index, option) in question.options.iter().enumerate() {
        let token = format!("option_{index}");
        options.insert(token.clone(), option.label.clone());
        values.push(
            EnumOption::new(token, option.label.clone()).description(option.description.clone()),
        );
    }
    if allow_other {
        values.push(
            EnumOption::new(OTHER_TOKEN, "Other")
                .description("Provide a custom value in the companion field."),
        );
    }
    (options, values)
}

fn other_default(question: &QuestionPrompt) -> Option<String> {
    let predefined = |value: &str| question.options.iter().any(|option| option.label == value);
    match (&question.kind, &question.default) {
        (
            QuestionPromptKind::SingleSelect { allow_other: true },
            Some(QuestionAnswerValue::String(default)),
        ) if !predefined(default) => Some(default.clone()),
        (
            QuestionPromptKind::MultiSelect {
                allow_other: true, ..
            },
            Some(QuestionAnswerValue::Strings(defaults)),
        ) => defaults
            .iter()
            .find(|default| !predefined(default))
            .cloned(),
        _ => None,
    }
}

fn take_other(
    content: &mut BTreeMap<String, ElicitationContentValue>,
    binding: &QuestionBinding,
) -> Result<String, String> {
    let property = binding
        .other_property
        .as_ref()
        .ok_or_else(|| "Other was returned for a field that does not allow it".to_string())?;
    let Some(ElicitationContentValue::String(value)) = content.remove(property) else {
        return Err("Other was selected without a companion text value".to_string());
    };
    if value.trim().is_empty() {
        return Err("Other companion text must not be blank".to_string());
    }
    Ok(value)
}

fn validate_length(
    value: &str,
    minimum: Option<usize>,
    maximum: Option<usize>,
) -> Result<(), String> {
    let length = value.chars().count();
    if minimum.is_some_and(|minimum| length < minimum)
        || maximum.is_some_and(|maximum| length > maximum)
    {
        return Err("the client returned text outside the requested length bounds".to_string());
    }
    Ok(())
}

fn to_u32(value: usize) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| "question length bound exceeds ACP's range".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanChoice {
    ImplementCurrent,
    Revise,
    Declined,
}

pub(crate) fn plan_decision_request(
    session_id: &SessionId,
    title: &str,
) -> CreateElicitationRequest {
    let decision = StringPropertySchema::new()
        .title("Plan decision")
        .description("Choose what Zevria should do with the submitted Plan artifact.")
        .one_of(vec![
            EnumOption::new("implement_current", "Implement in this session")
                .description("Approve the artifact and continue in Build mode."),
            EnumOption::new("revise", "Revise")
                .description("Return to Plan mode for another revision."),
        ]);
    let schema = ElicitationSchema::new()
        .title(title.to_string())
        .property("decision", decision, true);
    CreateElicitationRequest::new(
        ElicitationFormMode::new(ElicitationSessionScope::new(session_id.clone()), schema),
        "The Plan artifact is ready.",
    )
}

pub(crate) fn plan_choice(response: CreateElicitationResponse) -> Result<PlanChoice, String> {
    let ElicitationAction::Accept(accepted) = response.action else {
        return Ok(PlanChoice::Declined);
    };
    let mut content = accepted.content.unwrap_or_default();
    match content.remove("decision") {
        Some(ElicitationContentValue::String(value)) if value == "implement_current" => {
            Ok(PlanChoice::ImplementCurrent)
        }
        Some(ElicitationContentValue::String(value)) if value == "revise" => Ok(PlanChoice::Revise),
        _ => Err("the client returned an invalid Plan decision".to_string()),
    }
}
