//! Credential-aware wrapper around the SDK's bounded reqwest transport.
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::Arc,
};

use futures_util::{StreamExt as _, stream::BoxStream};
use http::{HeaderName, HeaderValue, StatusCode};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
        StreamableHttpResponseLimits,
    },
};
use sse_stream::{Error as SseError, Sse};
use tokio_util::sync::CancellationToken;

use crate::credential::McpCredentialSource;

type Headers = HashMap<HeaderName, HeaderValue>;
type HttpError = StreamableHttpError<reqwest::Error>;
type Events = BoxStream<'static, Result<Sse, SseError>>;

#[derive(Clone)]
pub(crate) struct AuthenticatingHttpClient {
    client: reqwest::Client,
    credentials: Arc<Vec<(HeaderName, McpCredentialSource)>>,
    bearer_token: Option<McpCredentialSource>,
    stop: CancellationToken,
}

impl AuthenticatingHttpClient {
    pub(crate) fn new(
        client: reqwest::Client,
        headers: &BTreeMap<String, McpCredentialSource>,
        bearer_token: Option<McpCredentialSource>,
        stop: CancellationToken,
    ) -> anyhow::Result<Self> {
        let credentials = headers
            .iter()
            .map(|(name, source)| Ok((name.parse()?, source.clone())))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            client,
            credentials: Arc::new(credentials),
            bearer_token,
            stop,
        })
    }

    async fn with_auth<T, F, Fut>(
        &self,
        headers: Headers,
        auth: Option<String>,
        send: F,
    ) -> Result<T, HttpError>
    where
        F: Fn(Headers, Option<String>) -> Fut + Send,
        Fut: Future<Output = Result<T, HttpError>> + Send,
        T: Send,
    {
        let mut rejected = vec![None; self.credentials.len() + 1];
        for attempt in 0..2 {
            let mut resolved = headers.clone();
            for ((name, source), generation) in self.credentials.iter().zip(&mut rejected) {
                let credential = source
                    .resolve(*generation)
                    .await
                    .map_err(credential_error)?;
                resolved.insert(name.clone(), credential.header);
                *generation = credential.generation;
            }
            let auth = if let Some(bearer) = &self.bearer_token {
                let generation = rejected.last_mut().expect("bearer slot");
                let credential = bearer
                    .resolve(*generation)
                    .await
                    .map_err(credential_error)?;
                *generation = credential.generation;
                Some(
                    credential
                        .header
                        .to_str()
                        .expect("validated header")
                        .to_owned(),
                )
            } else {
                auth.clone()
            };
            match send(resolved, auth).await {
                Err(error)
                    if attempt == 0
                        && rejected.iter().any(Option::is_some)
                        && unauthorized(&error) => {}
                result => return result.map_err(scrub),
            }
        }
        unreachable!("retry is bounded")
    }
}

fn credential_error(error: anyhow::Error) -> HttpError {
    // Credential errors have static display text; do not retain their sources.
    HttpError::UnexpectedServerResponse(error.to_string().into())
}

fn unauthorized(error: &HttpError) -> bool {
    match error {
        HttpError::AuthRequired(_) => true,
        HttpError::Client(error) => error.status() == Some(StatusCode::UNAUTHORIZED),
        // The SDK formats unchallenged POST failures this way after applying its body limit.
        HttpError::UnexpectedServerResponse(message) => message.starts_with("HTTP 401 "),
        _ => false,
    }
}

fn scrub(error: HttpError) -> HttpError {
    if unauthorized(&error) {
        return rmcp::transport::streamable_http_client::HttpStatusError::new(401, None).into();
    }
    match error {
        HttpError::Client(error) => HttpError::Client(error.without_url()),
        // The server may echo credentials in error bodies and content types.
        HttpError::UnexpectedServerResponse(_) => {
            HttpError::UnexpectedServerResponse("invalid MCP HTTP response".into())
        }
        HttpError::UnexpectedContentType(_) => HttpError::UnexpectedContentType(None),
        HttpError::InsufficientScope(_) => {
            rmcp::transport::streamable_http_client::HttpStatusError::new(403, None).into()
        }
        HttpError::HttpStatus(error) => {
            rmcp::transport::streamable_http_client::HttpStatusError::new(error.status, None).into()
        }
        other => other,
    }
}

impl StreamableHttpClient for AuthenticatingHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: Headers,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.post_message_with_response_limits(
            uri,
            message,
            session,
            auth,
            headers,
            StreamableHttpResponseLimits::default(),
        )
        .await
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: Headers,
        limits: StreamableHttpResponseLimits,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let post = self.with_auth(headers, auth, |headers, auth| {
            self.client.post_message_with_response_limits(
                uri.clone(),
                message.clone(),
                session.clone(),
                auth,
                headers,
                limits,
            )
        });
        // The SDK worker's bootstrap POST runs before its cancellation loop.
        // Cover both waiting for the response and reading its event stream.
        let response = tokio::select! {
            biased;
            () = self.stop.cancelled() => return Err(HttpError::TransportChannelClosed),
            result = post => result?,
        };
        Ok(match response {
            StreamableHttpPostResponse::Sse(events, session) => StreamableHttpPostResponse::Sse(
                events
                    .take_until(self.stop.clone().cancelled_owned())
                    .boxed(),
                session,
            ),
            response => response,
        })
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: Headers,
    ) -> Result<Events, HttpError> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session,
            last_event_id,
            auth,
            headers,
            StreamableHttpResponseLimits::default().max_sse_event_size,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: Headers,
        limit: usize,
    ) -> Result<Events, HttpError> {
        self.with_auth(headers, auth, |headers, auth| {
            self.client.get_stream_with_max_sse_event_size(
                uri.clone(),
                session.clone(),
                last_event_id.clone(),
                auth,
                headers,
                limit,
            )
        })
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: Headers,
    ) -> Result<(), HttpError> {
        self.with_auth(headers, auth, |headers, auth| {
            self.client
                .delete_session(uri.clone(), session.clone(), auth, headers)
        })
        .await
    }
}
