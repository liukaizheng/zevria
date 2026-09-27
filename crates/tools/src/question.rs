//! Blocking structured questions for Plan-mode user decisions.

use std::collections::HashSet;

use rig_agent::tool::{Tool, ToolContext};
use rig_core::tool::ToolExecutionError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use zevria_foundation::QUESTION_TOOL_NAME;
use zevria_foundation::QuestionAnswerValue;
use zevria_foundation::QuestionOption;
use zevria_foundation::QuestionPrompt;
use zevria_foundation::QuestionPromptKind;
use zevria_foundation::QuestionRequestError;
use zevria_foundation::QuestionResponse;
use zevria_foundation::QuestionTerminalDisposition;
use zevria_foundation::ToolCancelled;
use zevria_foundation::ToolResultDetail;
use zevria_session_api::QuestionRequester;
use zevria_session_api::TurnContext;

const DESCRIPTION: &str = r#"Ask the user for one to three consequential Plan decisions and continue this turn with their answers.

Use this only for a consequential user preference or tradeoff that remains unresolved after repository inspection and any recorded user decisions are applied. Repository evidence should settle factual claims, feasibility, equivalence, and objectively invalid alternatives, but the status quo, existing tests, a smaller diff, worker consensus, or model preference cannot choose what the user wants among multiple viable options. Prefer one batch of related questions over repeated interruptions.

Each question needs a stable snake_case `id`, a non-blank `header` label of any length, clear question text, and zero or more mutually exclusive options. Put the recommended option first and suffix its label with `(Recommended)`. Do not add an Other option; the terminal adds a free-form Other choice automatically.

The call blocks until the entire batch is answered or dismissed. Issue exactly one `question` call in an assistant response and do not mix it with other tool calls. A dismissal is a successful structured result; respect it and continue without repeating the same question."#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionArgs {
    pub questions: Vec<QuestionSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionSpec {
    /// Stable snake_case identifier used to correlate the answer.
    pub id: String,
    /// Terminal label shown with the question progress.
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOptionArgs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOptionArgs {
    /// Concise 1-5 word answer label.
    pub label: String,
    /// One short sentence explaining the impact or tradeoff.
    pub description: String,
}

#[derive(Debug)]
pub enum QuestionError {
    InvalidArguments(String),
    InvalidResponse(String),
    Unavailable(String),
    Cancelled(String),
}

impl std::fmt::Display for QuestionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArguments(message)
            | Self::InvalidResponse(message)
            | Self::Unavailable(message)
            | Self::Cancelled(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for QuestionError {}

#[derive(Clone)]
pub struct QuestionTool {
    requester: QuestionRequester,
}

impl QuestionTool {
    pub fn new(requester: QuestionRequester) -> Self {
        Self { requester }
    }

    async fn execute(
        &self,
        args: QuestionArgs,
        turn: TurnContext,
    ) -> Result<String, QuestionError> {
        let questions = validate_questions(args)?;
        let response = self
            .requester
            .ask(questions.clone(), turn)
            .await
            .map_err(|error| match error {
                QuestionRequestError::Cancelled => QuestionError::Cancelled(error.to_string()),
                QuestionRequestError::AlreadyPending
                | QuestionRequestError::FrontendUnavailable
                | QuestionRequestError::ResponseChannelClosed => {
                    QuestionError::Unavailable(error.to_string())
                }
            })?;
        validate_response(&questions, &response)?;
        serde_json::to_string(&response)
            .map_err(|error| QuestionError::InvalidResponse(error.to_string()))
    }
}

impl Tool for QuestionTool {
    const NAME: &'static str = QUESTION_TOOL_NAME;
    type Error = QuestionError;
    type Args = QuestionArgs;
    type Output = String;

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        schema_for!(QuestionArgs).to_value()
    }

    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        match &error {
            QuestionError::InvalidArguments(_) => {
                ToolExecutionError::invalid_args(error.to_string())
            }
            QuestionError::InvalidResponse(_)
            | QuestionError::Unavailable(_)
            | QuestionError::Cancelled(_) => ToolExecutionError::other(error.to_string()),
        }
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let turn = context.get::<TurnContext>().cloned().ok_or_else(|| {
            QuestionError::Unavailable(
                "question requires an active session turn context".to_string(),
            )
        })?;
        let result = self.execute(args, turn).await;
        match &result {
            Ok(output) => {
                let disposition = match serde_json::from_str::<QuestionResponse>(output) {
                    Ok(QuestionResponse::Answered { .. }) => {
                        Some(QuestionTerminalDisposition::Answered)
                    }
                    Ok(QuestionResponse::Dismissed) => Some(QuestionTerminalDisposition::Dismissed),
                    Err(_) => None,
                };
                if let Some(disposition) = disposition {
                    context.insert_result(ToolResultDetail::QuestionDisposition(disposition));
                }
            }
            Err(QuestionError::InvalidResponse(_)) => {
                context.insert_result(ToolResultDetail::QuestionDisposition(
                    QuestionTerminalDisposition::InvalidFrontendResponse,
                ));
            }
            Err(QuestionError::Unavailable(_)) => {
                context.insert_result(ToolResultDetail::QuestionDisposition(
                    QuestionTerminalDisposition::Unavailable,
                ));
            }
            Err(QuestionError::Cancelled(_)) => {
                context.insert_result(ToolCancelled);
            }
            Err(QuestionError::InvalidArguments(_)) => {}
        }
        result
    }
}

fn validate_questions(args: QuestionArgs) -> Result<Vec<QuestionPrompt>, QuestionError> {
    if !(1..=3).contains(&args.questions.len()) {
        return Err(QuestionError::InvalidArguments(format!(
            "questions must contain 1-3 items, got {}",
            args.questions.len()
        )));
    }

    let mut ids = HashSet::new();
    args.questions
        .into_iter()
        .enumerate()
        .map(|(index, question)| {
            let id = question.id.trim().to_string();
            if !is_snake_case_identifier(&id) {
                return Err(QuestionError::InvalidArguments(format!(
                    "questions[{index}].id must be a non-empty snake_case identifier"
                )));
            }
            if !ids.insert(id.clone()) {
                return Err(QuestionError::InvalidArguments(format!(
                    "question id {id:?} is duplicated"
                )));
            }

            let header = question.header.trim().to_string();
            if header.is_empty() {
                return Err(QuestionError::InvalidArguments(format!(
                    "questions[{index}].header must not be blank"
                )));
            }
            let text = question.question.trim().to_string();
            if text.is_empty() {
                return Err(QuestionError::InvalidArguments(format!(
                    "questions[{index}].question must not be blank"
                )));
            }
            let mut labels = HashSet::new();
            let options = question
                .options
                .into_iter()
                .enumerate()
                .map(|(option_index, option)| {
                    let label = option.label.trim().to_string();
                    let words = label.split_whitespace().count();
                    if !(1..=5).contains(&words) {
                        return Err(QuestionError::InvalidArguments(format!(
                            "questions[{index}].options[{option_index}].label must contain 1-5 words, got {words}"
                        )));
                    }
                    if !labels.insert(label.to_lowercase()) {
                        return Err(QuestionError::InvalidArguments(format!(
                            "questions[{index}] contains duplicate option label {label:?}"
                        )));
                    }
                    let description = option.description.trim().to_string();
                    if description.is_empty() {
                        return Err(QuestionError::InvalidArguments(format!(
                            "questions[{index}].options[{option_index}].description must not be blank"
                        )));
                    }
                    Ok(QuestionOption { label, description })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let kind = if options.is_empty() {
                QuestionPromptKind::Text {
                    min_length: Some(1),
                    max_length: None,
                }
            } else {
                QuestionPromptKind::SingleSelect { allow_other: true }
            };
            Ok(QuestionPrompt {
                id,
                header,
                question: text,
                options,
                kind,
                required: true,
                default: None,
            })
        })
        .collect()
}

fn is_snake_case_identifier(id: &str) -> bool {
    let mut segments = id.split('_');
    let valid_segment = |segment: &str| {
        let mut characters = segment.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
            && characters
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    };
    let has_segment = segments.clone().next().is_some();
    has_segment && segments.all(valid_segment)
}

fn validate_response(
    questions: &[QuestionPrompt],
    response: &QuestionResponse,
) -> Result<(), QuestionError> {
    let QuestionResponse::Answered { answers } = response else {
        return Ok(());
    };
    if answers.len() != questions.len() {
        return Err(QuestionError::InvalidResponse(format!(
            "the frontend returned {} answers for {} questions",
            answers.len(),
            questions.len()
        )));
    }
    for (index, (question, answer)) in questions.iter().zip(answers).enumerate() {
        if answer.id != question.id {
            return Err(QuestionError::InvalidResponse(format!(
                "answer {index} has id {:?}, expected {:?}",
                answer.id, question.id
            )));
        }
        let Some(QuestionAnswerValue::String(answer)) = &answer.answer else {
            return Err(QuestionError::InvalidResponse(format!(
                "answer for {:?} must contain exactly one string",
                question.id
            )));
        };
        if answer.trim().is_empty() {
            return Err(QuestionError::InvalidResponse(format!(
                "answer for {:?} must not be blank",
                question.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;
    use zevria_foundation::QuestionAnswer;
    use zevria_foundation::SessionMode;
    use zevria_foundation::TurnId;
    use zevria_session_api::SessionEvent;
    use zevria_session_api::SessionUpdate;
    use zevria_session_api::question_channels;
    use zevria_session_api::session_event_channel;

    fn args() -> QuestionArgs {
        QuestionArgs {
            questions: vec![QuestionSpec {
                id: "scope_choice".to_string(),
                header: "Scope".to_string(),
                question: "How broad should this be?".to_string(),
                options: vec![
                    QuestionOptionArgs {
                        label: "Focused (Recommended)".to_string(),
                        description: "Change only the requested workflow.".to_string(),
                    },
                    QuestionOptionArgs {
                        label: "Broad".to_string(),
                        description: "Include adjacent cleanup.".to_string(),
                    },
                ],
            }],
        }
    }

    fn turn(cancellation: CancellationToken) -> TurnContext {
        TurnContext::new(TurnId::new(3), SessionMode::Plan, cancellation)
    }

    #[test]
    fn schema_is_strict_and_description_pins_blocking_usage() {
        let (events, _receiver) = session_event_channel(8);
        let tool = QuestionTool::new(question_channels(events).requester);
        let schema = tool.parameters();
        assert_eq!(QuestionTool::NAME, "question");
        assert_eq!(schema["required"], serde_json::json!(["questions"]));
        assert_eq!(schema["additionalProperties"], false);
        let schema_text = schema.to_string();
        assert!(schema_text.contains("header"));
        assert!(schema_text.contains("options"));
        assert!(tool.description().contains("blocks until"));
        assert!(tool.description().contains("Do not add an Other option"));
        assert!(tool.description().contains("exactly one `question` call"));
        assert!(tool.description().contains("any length"));
        assert!(tool.description().contains("zero or more"));
    }

    #[test]
    fn validation_rejects_bad_batch_shapes_duplicate_ids_and_blank_headers() {
        let error = validate_questions(QuestionArgs { questions: vec![] })
            .expect_err("empty batch must fail");
        assert!(error.to_string().contains("1-3"));

        let mut duplicate = args();
        duplicate.questions.push(duplicate.questions[0].clone());
        let error = validate_questions(duplicate).expect_err("duplicate ids must fail");
        assert!(error.to_string().contains("duplicated"));

        let mut blank_header = args();
        blank_header.questions[0].header = " \t ".to_string();
        let error = validate_questions(blank_header).expect_err("blank header must fail");
        assert!(error.to_string().contains("must not be blank"));
    }

    #[test]
    fn validation_accepts_unbounded_headers_and_option_counts() {
        let mut zero_options = args();
        zero_options.questions[0].header =
            "This header is intentionally much longer than twelve characters".to_string();
        zero_options.questions[0].options.clear();
        let validated = validate_questions(zero_options).expect("zero options must be valid");
        assert!(validated[0].options.is_empty());
        assert_eq!(
            validated[0].header,
            "This header is intentionally much longer than twelve characters"
        );

        let mut one_option = args();
        one_option.questions[0].options.truncate(1);
        let validated = validate_questions(one_option).expect("one option must be valid");
        assert_eq!(validated[0].options.len(), 1);

        let mut many_options = args();
        many_options.questions[0]
            .options
            .extend((0..4).map(|index| QuestionOptionArgs {
                label: format!("Alternative {index}"),
                description: format!("Use alternative {index}."),
            }));
        let validated = validate_questions(many_options).expect("many options must be valid");
        assert_eq!(validated[0].options.len(), 6);
    }

    #[tokio::test]
    async fn answered_and_dismissed_batches_return_structured_json() {
        for response in [
            QuestionResponse::Answered {
                answers: vec![QuestionAnswer {
                    id: "scope_choice".to_string(),
                    answer: Some(QuestionAnswerValue::String(
                        "Focused (Recommended)".to_string(),
                    )),
                }],
            },
            QuestionResponse::Dismissed,
        ] {
            let (events_tx, mut events_rx) = session_event_channel(8);
            let channels = question_channels(events_tx);
            let tool = QuestionTool::new(channels.requester);
            let expected = response.clone();
            let call =
                tokio::spawn(
                    async move { tool.execute(args(), turn(CancellationToken::new())).await },
                );
            let request = match events_rx.recv().await.expect("question event") {
                SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
                update => panic!("unexpected update: {update:?}"),
            };
            assert!(channels.responder.respond(&request.id, response));
            let output = call.await.expect("join").expect("tool output");
            assert_eq!(
                serde_json::from_str::<QuestionResponse>(&output).expect("response json"),
                expected
            );
            if matches!(expected, QuestionResponse::Answered { .. }) {
                assert_eq!(
                    output,
                    r#"{"status":"answered","answers":[{"id":"scope_choice","answer":"Focused (Recommended)"}]}"#
                );
            }
        }
    }

    #[test]
    fn native_response_rejects_skipped_and_multi_value_answers() {
        let questions = validate_questions(args()).expect("valid native questions");
        for answer in [
            None,
            Some(QuestionAnswerValue::Strings(vec!["Focused".to_string()])),
        ] {
            let error = validate_response(
                &questions,
                &QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope_choice".to_string(),
                        answer,
                    }],
                },
            )
            .expect_err("non-string native answer must fail");
            assert!(error.to_string().contains("exactly one string"));
        }
    }

    #[tokio::test]
    async fn call_records_terminal_dispositions_for_answer_dismissal_and_invalid_frontend_data() {
        for (response, expected) in [
            (
                QuestionResponse::Answered {
                    answers: vec![QuestionAnswer {
                        id: "scope_choice".to_string(),
                        answer: Some(QuestionAnswerValue::String(
                            "Focused (Recommended)".to_string(),
                        )),
                    }],
                },
                QuestionTerminalDisposition::Answered,
            ),
            (
                QuestionResponse::Dismissed,
                QuestionTerminalDisposition::Dismissed,
            ),
            (
                QuestionResponse::Answered {
                    answers: Vec::new(),
                },
                QuestionTerminalDisposition::InvalidFrontendResponse,
            ),
        ] {
            let (events_tx, mut events_rx) = session_event_channel(8);
            let channels = question_channels(events_tx);
            let tool = QuestionTool::new(channels.requester);
            let call = tokio::spawn(async move {
                let mut context = ToolContext::new();
                context.insert(turn(CancellationToken::new()));
                let result = tool.call(&mut context, args()).await;
                (result, context)
            });
            let request = match events_rx.recv().await.expect("question event") {
                SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
                update => panic!("unexpected update: {update:?}"),
            };
            assert!(channels.responder.respond(&request.id, response));
            let (result, context) = call.await.expect("join");
            assert_eq!(
                result.is_err(),
                expected == QuestionTerminalDisposition::InvalidFrontendResponse
            );
            assert_eq!(
                context
                    .result::<ToolResultDetail>()
                    .and_then(ToolResultDetail::question_disposition),
                Some(expected)
            );
        }
    }

    #[tokio::test]
    async fn unavailable_frontend_records_a_terminal_disposition() {
        let (events_tx, events_rx) = session_event_channel(8);
        drop(events_rx);
        let tool = QuestionTool::new(question_channels(events_tx).requester);
        let mut context = ToolContext::new();
        context.insert(turn(CancellationToken::new()));
        assert!(matches!(
            tool.call(&mut context, args()).await,
            Err(QuestionError::Unavailable(_))
        ));
        assert_eq!(
            context
                .result::<ToolResultDetail>()
                .and_then(ToolResultDetail::question_disposition),
            Some(QuestionTerminalDisposition::Unavailable)
        );
    }

    #[tokio::test]
    async fn malformed_arguments_produce_no_detail() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let tool = QuestionTool::new(question_channels(events_tx).requester);
        let mut context = ToolContext::new();
        context.insert(turn(CancellationToken::new()));
        assert!(matches!(
            tool.call(
                &mut context,
                QuestionArgs {
                    questions: Vec::new()
                }
            )
            .await,
            Err(QuestionError::InvalidArguments(_))
        ));
        assert!(context.result::<ToolResultDetail>().is_none());
        assert!(events_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancellation_is_distinct_from_dismissal() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let channels = question_channels(events_tx);
        let tool = QuestionTool::new(channels.requester);
        let cancellation = CancellationToken::new();
        let child_cancellation = cancellation.clone();
        let call = tokio::spawn(async move {
            let mut context = ToolContext::new();
            context.insert(turn(child_cancellation));
            let result = tool.call(&mut context, args()).await;
            (result, context)
        });
        let _ = events_rx.recv().await.expect("question event");
        cancellation.cancel();
        let (result, context) = call.await.expect("join");
        assert!(matches!(result, Err(QuestionError::Cancelled(_))));
        assert!(context.result::<ToolCancelled>().is_some());
        assert!(context.result::<ToolResultDetail>().is_none());
    }
}
