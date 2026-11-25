//! Provider-independent execution of a local web search call.

use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zevria_content::HostedActivity;
use zevria_foundation::{TurnId, WebSearchDepth, WebSearchResultDetail};

use crate::{SessionEvent, SessionEventSender, TurnEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchRequest {
    pub query: String,
    pub context: Option<String>,
    pub depth: WebSearchDepth,
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchOutput {
    pub answer: String,
    pub detail: WebSearchResultDetail,
}

/// One in-flight call's display-only activity. Suppress identical snapshots;
/// never publish token usage or provider output to the session's model stream.
#[derive(Clone)]
pub struct WebSearchProgress {
    turn_id: TurnId,
    call_id: String,
    events: SessionEventSender,
    last_sent: Arc<Mutex<Vec<HostedActivity>>>,
}

impl WebSearchProgress {
    pub fn new(turn_id: TurnId, call_id: String, events: SessionEventSender) -> Self {
        Self {
            turn_id,
            call_id,
            events,
            last_sent: Default::default(),
        }
    }

    pub async fn report(&self, activity: &[HostedActivity]) {
        let mut last_sent = self.last_sent.lock().await;
        if last_sent.as_slice() != activity
            && self
                .events
                .send(SessionEvent::Turn(TurnEvent::WebSearchProgress {
                    turn_id: self.turn_id,
                    call_id: self.call_id.clone(),
                    activity: activity.to_vec(),
                }))
                .await
                .is_ok()
        {
            last_sent.clear();
            last_sent.extend_from_slice(activity);
        }
    }
}

pub trait WebSearchExecutor: Send + Sync + 'static {
    fn search<'a>(
        &'a self,
        request: WebSearchRequest,
        progress: Option<WebSearchProgress>,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<WebSearchOutput>> + Send + 'a>>;
}
