//! Cancellation and correlation of ACP elicitation requests.

use super::{
    AcpError, Arc, CancellationToken, CreateElicitationResponse, Entry, HashMap, LiveSession,
    Mutex, PlanArtifact, PlanChoice, QuestionResponse, SentRequest, SessionCommand, StopReason,
    diagnostic_update, invalid_params, plan_choice, plan_decision_request, question_form,
};

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum ElicitationKey {
    Question(zevria_foundation::QuestionRequestId),
    PlanDecision,
}

#[derive(Default)]
pub(super) struct ElicitationDriver {
    pending: Mutex<HashMap<ElicitationKey, Arc<CancellationToken>>>,
}

impl ElicitationDriver {
    fn begin_question(&self, key: ElicitationKey) -> Arc<CancellationToken> {
        let token = Arc::new(CancellationToken::new());
        let replaced = self
            .pending
            .lock()
            .expect("ACP elicitation lock poisoned")
            .insert(key, token.clone());
        if let Some(replaced) = replaced {
            replaced.cancel();
        }
        token
    }

    fn begin_plan_decision(&self) -> Option<Arc<CancellationToken>> {
        let mut pending = self.pending.lock().expect("ACP elicitation lock poisoned");
        let Entry::Vacant(entry) = pending.entry(ElicitationKey::PlanDecision) else {
            return None;
        };
        let token = Arc::new(CancellationToken::new());
        entry.insert(token.clone());
        Some(token)
    }

    fn complete(&self, key: &ElicitationKey, token: &Arc<CancellationToken>) -> bool {
        let mut pending = self.pending.lock().expect("ACP elicitation lock poisoned");
        if pending
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, token))
        {
            pending.remove(key);
            return true;
        }
        false
    }

    pub(super) fn cancel(&self, key: &ElicitationKey) {
        let token = self
            .pending
            .lock()
            .expect("ACP elicitation lock poisoned")
            .remove(key);
        if let Some(token) = token {
            token.cancel();
        }
    }

    pub(super) fn cancel_all(&self) {
        let tokens =
            std::mem::take(&mut *self.pending.lock().expect("ACP elicitation lock poisoned"));
        for (_, token) in tokens {
            token.cancel();
        }
    }
}

async fn await_elicitation(
    request: SentRequest<CreateElicitationResponse>,
    cancellation: &CancellationToken,
) -> Result<CreateElicitationResponse, AcpError> {
    let mut response = Box::pin(request.block_task());
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(AcpError::request_cancelled()),
        response = &mut response => response,
    }
}

impl LiveSession {
    pub(super) fn start_question(
        self: &Arc<Self>,
        request: zevria_foundation::QuestionRequest,
    ) -> Result<(), AcpError> {
        if !self.profile.form_elicitation {
            self.send_command(SessionCommand::Control(
                zevria_session_api::ControlCommand::AnswerQuestion {
                    request_id: request.id.clone(),
                    response: QuestionResponse::Dismissed,
                },
            ))?;
            self.send_update(diagnostic_update(
                None,
                "question-unavailable",
                "The ACP client does not advertise form elicitation; the question was dismissed so the model can continue.",
            ))?;
            return Ok(());
        }
        let form = match question_form(&self.id, &request) {
            Ok(form) => form,
            Err(error) => {
                self.send_command(SessionCommand::Control(
                    zevria_session_api::ControlCommand::AnswerQuestion {
                        request_id: request.id.clone(),
                        response: QuestionResponse::Dismissed,
                    },
                ))?;
                self.send_update(diagnostic_update(None, "question-invalid", error))?;
                return Ok(());
            }
        };
        let key = ElicitationKey::Question(request.id.clone());
        let token = self.elicitations.begin_question(key.clone());
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(session) = weak.upgrade() else {
                return;
            };
            let sent = session.connection.send_request(form.request.clone());
            let response = await_elicitation(sent, &token).await;
            if !session.elicitations.complete(&key, &token) {
                return;
            }
            let answer = match response {
                Ok(response) => match form.response(response) {
                    Ok(answer) => answer,
                    Err(error) => {
                        let _ = session.send_update(diagnostic_update(
                            None,
                            "question-response-invalid",
                            format!(
                                "The ACP client returned an invalid question response: {error}"
                            ),
                        ));
                        QuestionResponse::Dismissed
                    }
                },
                Err(error) => {
                    let _ = session.send_update(diagnostic_update(
                        None,
                        "question-response-unavailable",
                        format!("The ACP question could not be completed: {error}"),
                    ));
                    QuestionResponse::Dismissed
                }
            };
            let _ = session.send_command(SessionCommand::Control(
                zevria_session_api::ControlCommand::AnswerQuestion {
                    request_id: request.id,
                    response: answer,
                },
            ));
        });
        Ok(())
    }

    pub(super) fn start_plan_decision(
        self: &Arc<Self>,
        artifact: PlanArtifact,
    ) -> Result<(), AcpError> {
        if self.profile.is_worker() {
            return Err(invalid_params(
                "ensemble worker Plans are reports, not approval requests",
            ));
        }
        if !self.profile.form_elicitation {
            self.finish_prompt(Ok(StopReason::EndTurn));
            return Ok(());
        }
        let key = ElicitationKey::PlanDecision;
        let Some(token) = self.elicitations.begin_plan_decision() else {
            return Ok(());
        };
        let request = plan_decision_request(&self.id, &artifact.title);
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(session) = weak.upgrade() else {
                return;
            };
            let sent = session.connection.send_request(request);
            let response = await_elicitation(sent, &token).await;
            if !session.elicitations.complete(&key, &token) {
                return;
            }
            let choice = match response {
                Ok(response) => match plan_choice(response) {
                    Ok(choice) => choice,
                    Err(error) => {
                        let _ = session.send_update(diagnostic_update(
                            None,
                            "plan-decision-invalid",
                            format!("The ACP client returned an invalid Plan decision: {error}"),
                        ));
                        PlanChoice::Declined
                    }
                },
                Err(error) => {
                    let _ = session.send_update(diagnostic_update(
                        None,
                        "plan-decision-unavailable",
                        format!("The ACP Plan decision could not be completed: {error}"),
                    ));
                    PlanChoice::Declined
                }
            };
            let _ = session.apply_plan_choice(artifact.version, choice);
        });
        Ok(())
    }
}
