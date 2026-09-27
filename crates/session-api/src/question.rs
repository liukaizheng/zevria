//! Provider-neutral contracts for blocking user questions.
//!
//! The model-facing tool registers one pending request, announces it through
//! [`SessionEvent::QuestionAsked`], and waits on a oneshot response. The
//! session engine resolves that response from a typed frontend command, so an
//! answer resumes the existing model/tool loop instead of becoming a new user
//! turn.

use std::sync::{Arc, Mutex};

use crate::{SessionEvent, SessionEventSender, TurnContext};
use tokio::sync::oneshot;
use zevria_foundation::question::*;

struct PendingQuestion {
    id: QuestionRequestId,
    response: oneshot::Sender<QuestionResponse>,
}

#[derive(Default)]
struct QuestionBroker {
    pending: Mutex<Option<PendingQuestion>>,
}

impl QuestionBroker {
    fn clear(&self, id: &QuestionRequestId) {
        let mut pending = self.pending.lock().expect("question broker lock poisoned");
        if pending.as_ref().is_some_and(|pending| &pending.id == id) {
            *pending = None;
        }
    }

    fn register(
        &self,
        id: QuestionRequestId,
        response: oneshot::Sender<QuestionResponse>,
    ) -> Result<(), QuestionRequestError> {
        let mut pending = self.pending.lock().expect("question broker lock poisoned");
        if pending.is_some() {
            return Err(QuestionRequestError::AlreadyPending);
        }
        *pending = Some(PendingQuestion { id, response });
        Ok(())
    }
}

struct QuestionRegistrationGuard {
    broker: Arc<QuestionBroker>,
    events: SessionEventSender,
    turn_id: crate::TurnId,
    request_id: QuestionRequestId,
    announced: bool,
    finished: bool,
}

impl QuestionRegistrationGuard {
    fn new(
        broker: Arc<QuestionBroker>,
        events: SessionEventSender,
        turn_id: crate::TurnId,
        request_id: QuestionRequestId,
    ) -> Self {
        Self {
            broker,
            events,
            turn_id,
            request_id,
            announced: false,
            finished: false,
        }
    }

    fn announced(&mut self) {
        self.announced = true;
    }

    async fn finish(mut self) {
        self.broker.clear(&self.request_id);
        if self.announced {
            let _ = self
                .events
                .send(SessionEvent::QuestionClosed {
                    turn_id: self.turn_id,
                    request_id: self.request_id.clone(),
                })
                .await;
        }
        self.finished = true;
    }
}

impl Drop for QuestionRegistrationGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.broker.clear(&self.request_id);
        if !self.announced {
            return;
        }
        let event = SessionEvent::QuestionClosed {
            turn_id: self.turn_id,
            request_id: self.request_id.clone(),
        };
        if let Err(error) = self.events.try_send(event) {
            let event = error.into_inner();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let events = self.events.clone();
                runtime.spawn(async move {
                    let _ = events.send(event).await;
                });
            }
        }
    }
}

/// Cloneable endpoint held by the model-facing question tool.
#[derive(Clone)]
pub struct QuestionRequester {
    broker: Arc<QuestionBroker>,
    events: SessionEventSender,
}

impl QuestionRequester {
    /// Register and announce one batch, then wait for its correlated frontend
    /// response or parent-turn cancellation.
    pub async fn ask(
        &self,
        questions: Vec<QuestionPrompt>,
        turn: TurnContext,
    ) -> Result<QuestionResponse, QuestionRequestError> {
        let request = QuestionRequest {
            id: QuestionRequestId::generate(),
            questions,
            source_label: None,
            dismissible: true,
        };
        self.ask_request(request, turn).await
    }

    /// Register and announce a caller-allocated request. ACP handlers use this
    /// form so teardown can retain and target the exact visible request ID.
    pub async fn ask_request(
        &self,
        request: QuestionRequest,
        turn: TurnContext,
    ) -> Result<QuestionResponse, QuestionRequestError> {
        if turn.is_cancelled() {
            return Err(QuestionRequestError::Cancelled);
        }
        let (response_tx, response_rx) = oneshot::channel();
        self.broker.register(request.id.clone(), response_tx)?;
        let mut registration = QuestionRegistrationGuard::new(
            self.broker.clone(),
            self.events.clone(),
            turn.id,
            request.id.clone(),
        );
        if turn.is_cancelled() {
            return Err(QuestionRequestError::Cancelled);
        }

        if self
            .events
            .send(SessionEvent::QuestionAsked {
                turn_id: turn.id,
                request: request.clone(),
            })
            .await
            .is_err()
        {
            return Err(QuestionRequestError::FrontendUnavailable);
        }
        registration.announced();

        let result = tokio::select! {
            biased;
            () = turn.cancellation().cancelled() => {
                Err(QuestionRequestError::Cancelled)
            }
            response = response_rx => {
                response.map_err(|_| QuestionRequestError::ResponseChannelClosed)
            }
        };
        registration.finish().await;
        result
    }

    /// Cancel one exact pending request without resolving it as a user
    /// dismissal. Dropping the response sender wakes the waiter for cleanup.
    pub fn cancel_request(&self, id: &QuestionRequestId) -> bool {
        let mut pending = self
            .broker
            .pending
            .lock()
            .expect("question broker lock poisoned");
        if !pending.as_ref().is_some_and(|pending| &pending.id == id) {
            return false;
        }
        pending.take();
        true
    }
}

/// Cloneable endpoint held by the session engine. A mismatched or duplicate
/// response is stale and leaves the current request untouched.
#[derive(Clone)]
pub struct QuestionResponder {
    broker: Arc<QuestionBroker>,
}

impl QuestionResponder {
    pub fn respond(&self, id: &QuestionRequestId, response: QuestionResponse) -> bool {
        let sender = {
            let mut pending = self
                .broker
                .pending
                .lock()
                .expect("question broker lock poisoned");
            if !pending.as_ref().is_some_and(|pending| &pending.id == id) {
                return false;
            }
            pending.take().map(|pending| pending.response)
        };
        sender.is_some_and(|sender| sender.send(response).is_ok())
    }
}

/// The paired endpoints for one root session.
pub struct QuestionChannels {
    pub requester: QuestionRequester,
    pub responder: QuestionResponder,
}

pub fn question_channels(events: SessionEventSender) -> QuestionChannels {
    let broker = Arc::new(QuestionBroker::default());
    QuestionChannels {
        requester: QuestionRequester {
            broker: broker.clone(),
            events,
        },
        responder: QuestionResponder { broker },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionMode, SessionUpdate, TurnId, session_event_channel};
    use tokio_util::sync::CancellationToken;

    fn turn(cancellation: CancellationToken) -> TurnContext {
        TurnContext::new(TurnId::new(7), SessionMode::Plan, cancellation)
    }

    fn prompt(id: &str) -> QuestionPrompt {
        QuestionPrompt {
            id: id.to_string(),
            header: "Scope".to_string(),
            question: "Which scope?".to_string(),
            options: vec![
                QuestionOption {
                    label: "Focused".to_string(),
                    description: "Keep the change narrow.".to_string(),
                },
                QuestionOption {
                    label: "Broad".to_string(),
                    description: "Cover adjacent behavior.".to_string(),
                },
            ],
            kind: QuestionPromptKind::SingleSelect { allow_other: true },
            required: true,
            default: None,
        }
    }

    #[tokio::test]
    async fn matching_response_resumes_the_waiter_and_stale_response_does_not() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let channels = question_channels(events_tx);
        let requester = channels.requester.clone();
        let wait = tokio::spawn(async move {
            requester
                .ask(vec![prompt("scope")], turn(CancellationToken::new()))
                .await
        });

        let request = match events_rx.recv().await.expect("question event") {
            SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
            update => panic!("unexpected update: {update:?}"),
        };
        assert!(!channels.responder.respond(
            &QuestionRequestId::new("stale"),
            QuestionResponse::Dismissed
        ));
        let response = QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                id: "scope".to_string(),
                answer: Some(QuestionAnswerValue::String("Focused".to_string())),
            }],
        };
        assert!(channels.responder.respond(&request.id, response.clone()));
        assert_eq!(wait.await.expect("join").expect("answer"), response);
        assert!(matches!(
            events_rx.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::QuestionClosed {
                turn_id,
                request_id,
            })) if turn_id == TurnId::new(7) && request_id == request.id
        ));
        assert!(
            !channels
                .responder
                .respond(&request.id, QuestionResponse::Dismissed)
        );
    }

    #[tokio::test]
    async fn cancellation_clears_the_pending_request() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let channels = question_channels(events_tx);
        let cancellation = CancellationToken::new();
        let requester = channels.requester.clone();
        let child_cancellation = cancellation.clone();
        let wait = tokio::spawn(async move {
            requester
                .ask(vec![prompt("scope")], turn(child_cancellation))
                .await
        });
        let request = match events_rx.recv().await.expect("question event") {
            SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
            update => panic!("unexpected update: {update:?}"),
        };
        cancellation.cancel();
        assert_eq!(
            wait.await.expect("join").expect_err("cancelled"),
            QuestionRequestError::Cancelled
        );
        assert!(matches!(
            events_rx.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::QuestionClosed {
                request_id,
                ..
            })) if request_id == request.id
        ));
        assert!(
            !channels
                .responder
                .respond(&request.id, QuestionResponse::Dismissed)
        );
    }

    #[tokio::test]
    async fn a_second_live_batch_is_rejected() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let channels = question_channels(events_tx);
        let first_requester = channels.requester.clone();
        let first = tokio::spawn(async move {
            first_requester
                .ask(vec![prompt("first")], turn(CancellationToken::new()))
                .await
        });
        let request = match events_rx.recv().await.expect("question event") {
            SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
            update => panic!("unexpected update: {update:?}"),
        };

        let error = channels
            .requester
            .ask(vec![prompt("second")], turn(CancellationToken::new()))
            .await
            .expect_err("second request must fail");
        assert_eq!(error, QuestionRequestError::AlreadyPending);
        assert!(
            channels
                .responder
                .respond(&request.id, QuestionResponse::Dismissed)
        );
        assert_eq!(
            first.await.expect("join").expect("dismissed"),
            QuestionResponse::Dismissed
        );
    }

    #[tokio::test]
    async fn a_closed_frontend_fails_instead_of_waiting_forever() {
        let (events_tx, events_rx) = session_event_channel(8);
        drop(events_rx);
        let channels = question_channels(events_tx);
        let error = channels
            .requester
            .ask(vec![prompt("scope")], turn(CancellationToken::new()))
            .await
            .expect_err("closed frontend must fail");
        assert_eq!(error, QuestionRequestError::FrontendUnavailable);
    }

    #[tokio::test]
    async fn cancellation_before_registration_announces_nothing() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let channels = question_channels(events_tx);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = channels
            .requester
            .ask(vec![prompt("scope")], turn(cancellation))
            .await
            .expect_err("pre-cancelled question must fail");
        assert_eq!(error, QuestionRequestError::Cancelled);
        assert_eq!(
            events_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
    }

    #[tokio::test]
    async fn dropping_an_announced_waiter_closes_and_releases_the_broker() {
        let (events_tx, mut events_rx) = session_event_channel(8);
        let channels = question_channels(events_tx);
        let requester = channels.requester.clone();
        let waiter = tokio::spawn(async move {
            requester
                .ask(vec![prompt("first")], turn(CancellationToken::new()))
                .await
        });
        let first = match events_rx.recv().await.expect("first question") {
            SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
            update => panic!("unexpected update: {update:?}"),
        };
        waiter.abort();
        assert!(waiter.await.expect_err("aborted waiter").is_cancelled());
        assert!(matches!(
            events_rx.recv().await,
            Some(SessionUpdate::Lifecycle(SessionEvent::QuestionClosed {
                request_id,
                ..
            })) if request_id == first.id
        ));

        let requester = channels.requester.clone();
        let second_waiter = tokio::spawn(async move {
            requester
                .ask(vec![prompt("second")], turn(CancellationToken::new()))
                .await
        });
        let second = match events_rx.recv().await.expect("second question") {
            SessionUpdate::Lifecycle(SessionEvent::QuestionAsked { request, .. }) => request,
            update => panic!("unexpected update: {update:?}"),
        };
        assert!(
            channels
                .responder
                .respond(&second.id, QuestionResponse::Dismissed)
        );
        assert_eq!(
            second_waiter.await.expect("join").expect("dismissed"),
            QuestionResponse::Dismissed
        );
    }

    #[test]
    fn kind_is_required_and_native_string_answers_remain_current() {
        let legacy_prompt = serde_json::json!({
            "id": "scope",
            "header": "Scope",
            "question": "Which scope?",
            "options": [{"label": "Focused", "description": "Keep it narrow."}]
        });
        assert!(serde_json::from_value::<QuestionPrompt>(legacy_prompt.clone()).is_err());
        let mut current = legacy_prompt;
        current["kind"] = serde_json::json!({"type":"single_select", "allow_other":true});
        let prompt: QuestionPrompt = serde_json::from_value(current).unwrap();
        assert!(matches!(
            prompt.kind,
            QuestionPromptKind::SingleSelect { allow_other: true }
        ));
        assert!(prompt.required);

        let native_response =
            r#"{"status":"answered","answers":[{"id":"scope","answer":"Focused"}]}"#;
        let response: QuestionResponse =
            serde_json::from_str(native_response).expect("native response deserializes");
        assert_eq!(
            serde_json::to_string(&response).expect("response serializes"),
            native_response
        );

        let malformed_select = serde_json::json!({
            "id": "empty",
            "header": "Empty",
            "question": "Choose",
            "options": [],
            "kind": {"type": "single_select", "allow_other": false}
        });
        assert!(
            serde_json::from_value::<QuestionPrompt>(malformed_select).is_err(),
            "explicit select prompts need at least one option"
        );
    }
}
