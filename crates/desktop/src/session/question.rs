//! Renderer-free question form state and wire-value validation.
use zevria_foundation::{
    QuestionAnswer, QuestionAnswerValue, QuestionPromptKind, QuestionRequest, QuestionResponse,
    TurnId,
};
#[derive(Debug, Default, Clone)]
pub struct AnswerDraft {
    pub text: String,
    pub selected: Vec<usize>,
    pub other: bool,
    pub skip: bool,
}
#[derive(Debug)]
pub struct QuestionForm {
    pub turn_id: TurnId,
    pub request: QuestionRequest,
    pub answers: Vec<AnswerDraft>,
    pub pending: bool,
    pub error: Option<String>,
}
impl QuestionForm {
    pub fn new(turn_id: TurnId, request: QuestionRequest) -> Self {
        let answers = request
            .questions
            .iter()
            .map(|q| {
                let mut draft = AnswerDraft::default();
                match &q.default {
                    Some(QuestionAnswerValue::String(value)) => {
                        if let Some(i) = q.options.iter().position(|o| &o.label == value) {
                            draft.selected.push(i);
                        } else {
                            draft.text = value.clone();
                            draft.other = !matches!(q.kind, QuestionPromptKind::Text { .. });
                        }
                    }
                    Some(QuestionAnswerValue::Strings(values)) => {
                        for value in values {
                            if let Some(i) = q.options.iter().position(|o| &o.label == value) {
                                draft.selected.push(i);
                            } else {
                                draft.text = value.clone();
                                draft.other = true;
                            }
                        }
                    }
                    None => {}
                }
                draft
            })
            .collect();
        Self {
            turn_id,
            request,
            answers,
            pending: false,
            error: None,
        }
    }
    pub fn response(&self) -> anyhow::Result<QuestionResponse> {
        let mut answers = Vec::new();
        for (q, draft) in self.request.questions.iter().zip(&self.answers) {
            if draft.skip {
                anyhow::ensure!(!q.required, "{} is required", q.header);
                answers.push(QuestionAnswer {
                    id: q.id.clone(),
                    answer: None,
                });
                continue;
            }
            let value = match &q.kind {
                QuestionPromptKind::Text {
                    min_length,
                    max_length,
                } => {
                    let text = draft.text.trim();
                    let len = text.chars().count();
                    if text.is_empty() && !q.required {
                        answers.push(QuestionAnswer {
                            id: q.id.clone(),
                            answer: None,
                        });
                        continue;
                    }
                    anyhow::ensure!(
                        !text.is_empty()
                            && min_length.is_none_or(|min| len >= min)
                            && max_length.is_none_or(|max| len <= max),
                        "{}: enter a nonblank answer within the length limits",
                        q.header
                    );
                    QuestionAnswerValue::String(text.into())
                }
                QuestionPromptKind::SingleSelect { allow_other } => {
                    let value = if draft.other {
                        anyhow::ensure!(*allow_other, "Other is not allowed");
                        draft.text.trim().to_string()
                    } else {
                        anyhow::ensure!(
                            draft.selected.len() == 1,
                            "{}: select one answer",
                            q.header
                        );
                        q.options
                            .get(draft.selected[0])
                            .ok_or_else(|| anyhow::anyhow!("invalid choice"))?
                            .label
                            .clone()
                    };
                    anyhow::ensure!(!value.trim().is_empty(), "{}: enter an answer", q.header);
                    QuestionAnswerValue::String(value)
                }
                QuestionPromptKind::MultiSelect {
                    min_selections,
                    max_selections,
                    allow_other,
                } => {
                    let mut values = Vec::new();
                    let mut selected = draft.selected.clone();
                    selected.sort_unstable();
                    selected.dedup();
                    for i in &selected {
                        let value = &q
                            .options
                            .get(*i)
                            .ok_or_else(|| anyhow::anyhow!("invalid choice"))?
                            .label;
                        if !values.contains(value) {
                            values.push(value.clone());
                        }
                    }
                    if draft.other {
                        anyhow::ensure!(
                            *allow_other && !draft.text.trim().is_empty(),
                            "{}: enter Other",
                            q.header
                        );
                        values.push(draft.text.trim().into());
                    }
                    anyhow::ensure!(
                        (!q.required || !values.is_empty())
                            && min_selections.is_none_or(|min| values.len() >= min)
                            && max_selections.is_none_or(|max| values.len() <= max),
                        "{}: selection count is outside the limits",
                        q.header
                    );
                    QuestionAnswerValue::Strings(values)
                }
            };
            answers.push(QuestionAnswer {
                id: q.id.clone(),
                answer: Some(value),
            });
        }
        Ok(QuestionResponse::Answered { answers })
    }
    pub fn dismiss(&self) -> anyhow::Result<QuestionResponse> {
        anyhow::ensure!(
            self.request.dismissible,
            "this question cannot be dismissed"
        );
        Ok(QuestionResponse::Dismissed)
    }
}
