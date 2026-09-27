use super::*;
#[cfg(feature = "cache-diagnostics")]
#[path = "cache_diagnostics/lifecycle_tests.rs"]
mod cache_diagnostic_lifecycle;
#[path = "cache_diagnostics/probe.rs"]
mod cache_preparation_probe;
#[path = "directive_wire_tests.rs"]
mod directive_wire;
#[path = "inline_search_tests.rs"]
mod inline_search_tests;
#[path = "model_tests.rs"]
mod models;
#[path = "prompt_cache_tests.rs"]
mod prompt_cache_options;
#[path = "resume_wire_tests.rs"]
mod resume_wire;
#[path = "search_tests.rs"]
mod search_tests;
#[path = "session_header_tests.rs"]
mod session_headers;
#[path = "subtask_batch_tests.rs"]
mod subtask_batches;
#[path = "summary_tests.rs"]
mod summaries;
use crate::connection::ContinuationState;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use rig_agent::tool::{
    Tool, ToolContext,
    server::{ToolServer, ToolServerHandle},
};
use rig_core::providers::openai::responses_api::ReasoningSummaryLevel;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::{
    accept_async, accept_hdr_async,
    tungstenite::{
        handshake::server::{
            Callback, ErrorResponse as ServerErrorResponse, Request as ServerRequest,
            Response as ServerResponse,
        },
        http::HeaderValue,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::fmt::MakeWriter;
use zevria_core::SessionEngine;
use zevria_foundation::ModelRole;
use zevria_foundation::ReasoningLevel as ReasoningEffort;
use zevria_foundation::SessionMode;
use zevria_foundation::SessionPolicies;
use zevria_foundation::TurnPolicy;
use zevria_instructions::SkillCatalog;
use zevria_model::CompactResult;
use zevria_model::InputTokenCount;
use zevria_model::ModelRequest;
use zevria_model::ModelRequestItem;
use zevria_model::ProviderReplay;
use zevria_model::TokenUsage;
use zevria_session_api::ModelProvider;
use zevria_session_api::SessionCommand;
use zevria_session_api::SessionEvent;
use zevria_session_api::SessionEventReceiver;
use zevria_session_api::SessionUpdate;
use zevria_session_api::session_event_channel;
use zevria_transcript::transcript;
use zevria_transcript::transcript::TranscriptItem;
use zevria_transcript::transcript::TranscriptWriter;

async fn receive_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read HTTP request");
        assert!(read > 0, "HTTP request ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .expect("content-length header");
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read HTTP body");
        assert!(read > 0, "HTTP request ended before its body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(bytes).expect("HTTP request is UTF-8")
}

fn test_profile_ref() -> zevria_foundation::ModelProfileRef {
    zevria_foundation::ModelProfileRef::new("test-provider", "gpt-test")
}

fn test_policies() -> SessionPolicies {
    SessionPolicies::new(
        TurnPolicy::new(
            "Build provider test instructions",
            None,
            ModelRole::Build,
            false,
        ),
        TurnPolicy::new(
            "Plan continuation instructions",
            Some(vec!["count_once".to_string()]),
            ModelRole::Plan,
            false,
        ),
    )
}

/// Owned parts of a borrowed [`ModelRequest`]. A real request borrows the
/// session that owns its messages; a test owns them locally, so it builds
/// these with the same field syntax and passes [`RequestParts::request`].
// Bounded test fixtures outlive the borrowed wire requests they construct.
fn test_instruction_set(application: &str) -> zevria_instructions::InstructionSet {
    zevria_instructions::InstructionSet {
        application: application.into(),
        system: vec![],
        catalog: None,
        workflow: zevria_instructions::DirectivePolicy::new(
            "build",
            test_policies().policy(SessionMode::Build),
        ),
    }
}
pub(crate) fn test_instructions() -> &'static str {
    static INSTRUCTIONS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTRUCTIONS.get_or_init(|| test_instruction_set("").render())
}
fn rendered_test_instructions(application: &str) -> &'static str {
    Box::leak(test_instruction_set(application).render().into_boxed_str())
}
fn test_skill(body: &str) -> &'static zevria_instructions::DirectiveContent {
    Box::leak(Box::new(zevria_instructions::DirectiveContent::skill(
        &zevria_instructions::SkillSnapshot::new("review".parse().unwrap(), "Review", body)
            .unwrap(),
    )))
}

struct RequestParts {
    prompt: Message,
    history: Vec<Message>,
    instructions: String,
    allowed_tool_names: Option<Vec<String>>,
}

impl RequestParts {
    fn maintenance_request(&self) -> ModelRequest<'_> {
        let mut request = self.request();
        request.instructions = Box::leak(
            zevria_instructions::InstructionSet::maintenance(&self.instructions, [])
                .render()
                .into_boxed_str(),
        );
        request.allowed_tool_names = Some(&[]);
        request
    }

    fn request(&self) -> ModelRequest<'_> {
        self.request_with_role(ModelRole::Build)
    }

    fn request_with_role(&self, model_role: ModelRole) -> ModelRequest<'_> {
        self.request_with_role_and_skills(model_role, None)
    }

    fn request_with_role_and_skills<'a>(
        &'a self,
        model_role: ModelRole,
        skill_context: Option<&'a str>,
    ) -> ModelRequest<'a> {
        let mut input = self
            .history
            .iter()
            .map(ModelRequestItem::message)
            .collect::<Vec<_>>();
        input.push(ModelRequestItem::message(&self.prompt));
        if let Some(body) = skill_context {
            input.push(ModelRequestItem::DeveloperInstruction(test_skill(body)));
        }
        ModelRequest {
            instructions: rendered_test_instructions(&self.instructions),
            input,
            model_role,
            allowed_tool_names: self.allowed_tool_names.as_deref(),
        }
    }
}

fn request_with_replays<'a>(
    history: &'a [Message],
    history_replays: &'a [Option<zevria_model::ReplayMessage>],
    prompt: &'a Message,
    instructions: &'a str,
    skill_context: Option<&'a str>,
) -> ModelRequest<'a> {
    let mut input = history
        .iter()
        .enumerate()
        .map(
            |(index, message)| match history_replays.get(index).and_then(Option::as_ref) {
                Some(replay) => ModelRequestItem::replay_backed(replay),
                None => ModelRequestItem::message(message),
            },
        )
        .collect::<Vec<_>>();
    input.push(ModelRequestItem::message(prompt));
    if let Some(body) = skill_context {
        input.push(ModelRequestItem::DeveloperInstruction(test_skill(body)));
    }
    ModelRequest {
        instructions: rendered_test_instructions(instructions),
        input,
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    }
}

/// A throwaway update sender for turns that don't assert on UI output. The
/// receiver is dropped immediately, so `run_turn`'s sends fail harmlessly.
fn discard_updates() -> ProgressReporter {
    ProgressReporter::new(session_event_channel(1).0)
}

// Tracing callsite interest is process-wide. Do not register/drop temporary
// capture subscribers concurrently across the log-asserting tests.
static CAPTURED_LOG_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// A lock between capture tests alone cannot prevent other parallel fixtures
// from visiting these callsites without a subscriber while interest changes.
// Isolate log assertions, not transport behavior, in a single-test process.
fn isolate_log_test(name: &str) -> bool {
    if std::env::var("ZEVRIA_PROVIDER_LOG_TEST").as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env("ZEVRIA_PROVIDER_LOG_TEST", name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated log test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "log test selector must execute exactly one test"
    );
    true
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().expect("captured log lock").clone())
            .expect("tracing output is UTF-8")
    }
}

struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured log lock")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for CapturedLogs {
    type Writer = CapturedLogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        CapturedLogWriter(self.0.clone())
    }
}

fn captured_log_subscriber(logs: CapturedLogs) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_writer(logs)
        .finish()
}

fn test_model() -> ResponsesCompletionModel {
    let client = openai::Client::builder()
        .api_key("test-key")
        .base_url("http://127.0.0.1/v1")
        .build()
        .expect("test client should build");

    client.completion_model("gpt-test")
}

#[derive(Clone)]
struct TestModelsConfig {
    build: String,
    plan: String,
    explore: String,
    builder: String,
    review: String,
}

fn test_models() -> TestModelsConfig {
    TestModelsConfig {
        build: "gpt-test".to_string(),
        plan: "gpt-test".to_string(),
        explore: "gpt-test".to_string(),
        builder: "builder-test".to_string(),
        review: "gpt-test".to_string(),
    }
}

#[derive(Clone)]
struct TestReasoningConfig {
    effort: ReasoningEffort,
    summary_level: ReasoningSummaryLevel,
}

impl Default for TestReasoningConfig {
    fn default() -> Self {
        Self {
            effort: ReasoningEffort::Medium,
            summary_level: ReasoningSummaryLevel::Detailed,
        }
    }
}

#[derive(Clone)]
struct TestProviderConfig {
    base_url: String,
    api_key: String,
    models: TestModelsConfig,
    supports_websockets: bool,
    reasoning: TestReasoningConfig,
    compatibility: ResponsesCompatibilityConfig,
    additional_params: BTreeMap<String, Value>,
    compaction: RemoteCompactionConfig,
}

#[allow(clippy::too_many_arguments)]
fn resolved_profile(
    provider: &str,
    model: &str,
    base_url: String,
    api_key: &str,
    supports_websockets: bool,
    reasoning_summary_level: ReasoningSummaryLevel,
    compatibility: ResponsesCompatibilityConfig,
    additional_params: BTreeMap<String, Value>,
    compaction: RemoteCompactionConfig,
    context_window_tokens: u64,
) -> ResolvedModelProfile {
    ResolvedModelProfile {
        profile: zevria_foundation::ModelProfileRef::new(provider, model),
        endpoint: ProviderEndpoint {
            base_url,
            api_key: LiteralApiKey::new(api_key),
            supports_websockets,
            session_id_header: None,
            compatibility,
            additional_params,
            compaction,
            input_token_count: InputTokenCountConfig::default(),
            web_search: WebSearchConfig::default(),
        },
        context_window_tokens,
        input_token_limit: context_window_tokens,
        retained_user_tokens: context_window_tokens / 10,
        reasoning_levels: ReasoningEffort::ALL.to_vec(),
        reasoning_summary_level,
    }
}

impl TestProviderConfig {
    fn resolved(&self, role: ModelRole) -> ResolvedModelProfile {
        let model = match role {
            ModelRole::Build => &self.models.build,
            ModelRole::Plan => &self.models.plan,
            ModelRole::Review => &self.models.review,
            ModelRole::Explore => &self.models.explore,
            ModelRole::Builder => &self.models.builder,
        };
        resolved_profile(
            "test-provider",
            model,
            self.base_url.clone(),
            &self.api_key,
            self.supports_websockets,
            self.reasoning.summary_level.clone(),
            self.compatibility.clone(),
            self.additional_params.clone(),
            self.compaction.clone(),
            272_000,
        )
    }
}

#[derive(Clone)]
struct TestProviderFactory {
    config: TestProviderConfig,
    preamble: String,
    tools: ToolServerHandle,
}

impl TestProviderFactory {
    fn new(
        config: TestProviderConfig,
        preamble: impl Into<String>,
        tools: ToolServerHandle,
    ) -> Self {
        Self {
            config,
            preamble: preamble.into(),
            tools,
        }
    }

    async fn connect(&self, cache_key: &str) -> anyhow::Result<OpenAiProvider> {
        OpenAiProvider::connect(
            &self.config.resolved(ModelRole::Build),
            self.config.reasoning.effort,
            &self.preamble,
            self.tools.clone(),
            cache_key,
            cache_key,
        )
        .await
    }
}

pub(crate) async fn connect_http_test_provider(
    base_url: String,
    tools: ToolServerHandle,
) -> OpenAiProvider {
    connect_http_test_provider_with_options(
        base_url,
        tools,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        "test-session",
    )
    .await
}

async fn connect_http_test_provider_with_options(
    base_url: String,
    tools: ToolServerHandle,
    compatibility: ResponsesCompatibilityConfig,
    additional_params: BTreeMap<String, Value>,
    cache_key: &str,
) -> OpenAiProvider {
    let config = TestProviderConfig {
        base_url,
        api_key: "test-key".to_string(),
        models: test_models(),
        supports_websockets: false,
        reasoning: TestReasoningConfig::default(),
        compatibility,
        additional_params,
        compaction: RemoteCompactionConfig::default(),
    };
    OpenAiProvider::connect(
        &config.resolved(ModelRole::Build),
        config.reasoning.effort,
        "Test instructions",
        tools,
        "test-session-id",
        cache_key,
    )
    .await
    .expect("HTTP test provider should connect")
}

async fn capture_http_request_with_options(
    compatibility: ResponsesCompatibilityConfig,
    additional_params: BTreeMap<String, Value>,
    tools: ToolServerHandle,
) -> CapturedHttpRequest {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_shape", "msg_shape", "shape answer").to_string()),
        )
        .await;
        request
    });

    let mut openai = connect_http_test_provider_with_options(
        format!("http://{address}/v1/responses"),
        tools,
        compatibility,
        additional_params,
        "compat-session",
    )
    .await;
    let mut state = AttemptState::default();
    let parts = RequestParts {
        prompt: Message::user("compatibility request"),
        history: Vec::new(),
        instructions: "Compatibility instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(1),
    )
    .await
    .expect("request-shape turn should complete");
    server.await.expect("request-shape server")
}

#[test]
fn responses_gateway_accepts_arbitrary_model_families_for_every_role() {
    let config = TestProviderConfig {
        base_url: "http://127.0.0.1:1/v1/responses".to_string(),
        api_key: "test-key".to_string(),
        models: TestModelsConfig {
            build: "deepseek-reasoner".to_string(),
            plan: "glm-4.5".to_string(),
            explore: "deepseek-chat".to_string(),
            builder: "builder-model".to_string(),
            review: "glm-4.5-air".to_string(),
        },
        supports_websockets: false,
        reasoning: TestReasoningConfig::default(),
        compatibility: ResponsesCompatibilityConfig::default(),
        additional_params: Default::default(),
        compaction: RemoteCompactionConfig::default(),
    };

    for (role, expected) in [
        (ModelRole::Build, "deepseek-reasoner"),
        (ModelRole::Plan, "glm-4.5"),
        (ModelRole::Review, "glm-4.5-air"),
        (ModelRole::Explore, "deepseek-chat"),
        (ModelRole::Builder, "builder-model"),
    ] {
        assert_eq!(config.resolved(role).profile.model, expected);
    }
}

fn test_session(
    socket: OpenAiWebSocketStream,
    pending_done_response_id: Option<&str>,
) -> OpenAiProvider {
    test_session_with_url("ws://127.0.0.1:0", socket, pending_done_response_id)
}

fn test_session_with_url(
    url: &str,
    socket: OpenAiWebSocketStream,
    pending_done_response_id: Option<&str>,
) -> OpenAiProvider {
    test_session_with_pump(
        url,
        OpenAiWebSocketSession::new(socket),
        pending_done_response_id,
    )
}

fn test_session_with_pump(
    url: &str,
    session: OpenAiWebSocketSession,
    pending_done_response_id: Option<&str>,
) -> OpenAiProvider {
    let tools = ToolServer::new().run();
    let endpoints = OpenAiResponseEndpoints::parse(url).expect("test endpoint");
    OpenAiProvider {
        #[cfg(feature = "cache-diagnostics")]
        cache_diagnostics: Default::default(),
        profile: zevria_foundation::ModelProfileRef::new("test-provider", "gpt-test"),
        model: test_model(),
        context_window_tokens: 272_000,
        input_token_limit: 272_000,
        preamble: "Test instructions".to_string(),
        responses_parameters: None,
        reasoning_level: ReasoningEffort::Medium,
        reasoning_summary_level: ReasoningSummaryLevel::Detailed,
        web_search: WebSearchConfig::default(),
        compatibility: ResponsesCompatibilityConfig {
            send_reasoning: false,
            send_prompt_cache_key: false,
            ..ResponsesCompatibilityConfig::default()
        },
        additional_params: Default::default(),
        prompt_cache_key: "test-session".to_string(),
        tools,
        ws: OpenAiParkedWebSocket {
            session,
            config: OpenAiWebSocketConfig {
                url: url.to_string(),
                headers: HeaderMap::new(),
            },
            socket_generation: 0,
            websocket_connection_request_id: None,
            continuation: None,
            pending_done_response_id: pending_done_response_id.map(ToOwned::to_owned),
            last_activity: std::time::Instant::now(),
        },
        responses_url: endpoints.http,
        transport: OpenAiTransport::WebSocket,
        compaction_url: None,
        compaction_timeout: std::time::Duration::from_secs(300),
        input_token_count_url: None,
        input_token_count_timeout: std::time::Duration::from_secs(30),
        input_token_count_unsupported: false,
        api_key: "test-key".to_string(),
        http: reqwest::Client::new(),
    }
}

fn continuation_response_id(openai: &OpenAiProvider) -> Option<&str> {
    openai
        .ws
        .continuation
        .as_ref()
        .map(|continuation| continuation.response_id.as_str())
}

async fn receive_json(socket: &mut WebSocketStream<TcpStream>) -> Value {
    let message = socket
        .next()
        .await
        .expect("client request should exist")
        .expect("client request should be valid")
        .into_text()
        .expect("client request should be text");
    serde_json::from_str(&message).expect("client request should be JSON")
}

async fn send_json(socket: &mut WebSocketStream<TcpStream>, value: Value) {
    socket
        .send(WebSocketMessage::text(value.to_string()))
        .await
        .expect("server event should send");
}

struct CapturedHttpRequest {
    headers: String,
    body: Value,
}

async fn receive_http_headers(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read HTTP headers");
        assert!(read > 0, "HTTP connection ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes[..index + 4].to_vec()).expect("UTF-8 HTTP headers");
        }
    }
}

async fn receive_http_json(stream: &mut TcpStream) -> CapturedHttpRequest {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read HTTP request");
        assert!(read > 0, "HTTP request ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("UTF-8 HTTP headers");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .expect("content-length header");
    while bytes.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read HTTP body");
        assert!(read > 0, "HTTP request ended before its body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .expect("JSON HTTP body");
    CapturedHttpRequest { headers, body }
}

fn sse_data(data: impl AsRef<str>) -> String {
    format!("data: {}\n\n", data.as_ref())
}

async fn send_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: impl AsRef<[u8]>,
) {
    send_http_response_with_raw_request_id(stream, status, content_type, None, body).await;
}

async fn send_http_response_with_raw_request_id(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    request_id: Option<&[u8]>,
    body: impl AsRef<[u8]>,
) {
    let body = body.as_ref();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write HTTP response headers");
    if let Some(request_id) = request_id {
        stream
            .write_all(b"X-Request-Id: ")
            .await
            .expect("write request-ID header name");
        stream
            .write_all(request_id)
            .await
            .expect("write request-ID header value");
        stream
            .write_all(b"\r\n")
            .await
            .expect("finish request-ID header");
    }
    stream
        .write_all(b"Connection: close\r\n\r\n")
        .await
        .expect("finish HTTP response headers");
    stream
        .write_all(body)
        .await
        .expect("write HTTP response body");
}

async fn wait_for_terminal(session: &OpenAiWebSocketSession) -> OpenAiWebSocketTerminalCategory {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(terminal) = session.terminal_status() {
                return terminal;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the websocket pump should reach terminal state")
}

// The large error type is fixed by tungstenite's server Callback contract.
struct AssertOpenAiWebSocketHandshake;

#[allow(clippy::result_large_err)]
impl Callback for AssertOpenAiWebSocketHandshake {
    fn on_request(
        self,
        request: &ServerRequest,
        response: ServerResponse,
    ) -> Result<ServerResponse, ServerErrorResponse> {
        AssertSessionWebSocketHandshake(None).on_request(request, response)
    }
}

struct AssertSessionWebSocketHandshake(Option<(&'static str, &'static str)>);

#[allow(clippy::result_large_err)]
impl Callback for AssertSessionWebSocketHandshake {
    fn on_request(
        self,
        request: &ServerRequest,
        response: ServerResponse,
    ) -> Result<ServerResponse, ServerErrorResponse> {
        assert_eq!(
            request
                .headers()
                .get("openai-beta")
                .and_then(|value| value.to_str().ok()),
            None
        );
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer test-key")
        );
        if let Some((name, expected)) = self.0 {
            let values = request.headers().get_all(name).iter().collect::<Vec<_>>();
            assert_eq!(values.len(), 1, "expected exactly one {name} header");
            assert_eq!(values[0].to_str().unwrap(), expected);
        }
        for name in [
            "session-id",
            "thread-id",
            "x-client-request-id",
            "x-opencode-session",
            "x-conversation-id",
        ] {
            if !self
                .0
                .is_some_and(|(configured, _)| configured.eq_ignore_ascii_case(name))
            {
                assert!(
                    request.headers().get(name).is_none(),
                    "unexpected {name} handshake header"
                );
            }
        }
        Ok(response)
    }
}

struct AssertOpenAiWebSocketHandshakeWithRequestId(&'static str);

#[allow(clippy::result_large_err)]
impl Callback for AssertOpenAiWebSocketHandshakeWithRequestId {
    fn on_request(
        self,
        request: &ServerRequest,
        response: ServerResponse,
    ) -> Result<ServerResponse, ServerErrorResponse> {
        let mut response = AssertOpenAiWebSocketHandshake.on_request(request, response)?;
        response
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static(self.0));
        Ok(response)
    }
}

#[test]
fn responses_endpoint_conversion_changes_only_the_scheme() {
    for (configured, expected_http, expected_websocket) in [
        (
            "http://example.test/custom/responses?version=1",
            "http://example.test/custom/responses?version=1",
            "ws://example.test/custom/responses?version=1",
        ),
        (
            "https://example.test/custom/responses?version=1",
            "https://example.test/custom/responses?version=1",
            "wss://example.test/custom/responses?version=1",
        ),
        (
            "ws://example.test/custom/responses?version=1",
            "http://example.test/custom/responses?version=1",
            "ws://example.test/custom/responses?version=1",
        ),
        (
            "wss://example.test/custom/responses?version=1",
            "https://example.test/custom/responses?version=1",
            "wss://example.test/custom/responses?version=1",
        ),
    ] {
        let endpoints = OpenAiResponseEndpoints::parse(configured).expect("valid endpoint");
        assert_eq!(endpoints.http.as_str(), expected_http);
        assert_eq!(endpoints.websocket.as_str(), expected_websocket);
    }
}

#[test]
fn request_id_extraction_ignores_absent_empty_and_non_utf8_headers() {
    let mut headers = HeaderMap::new();
    assert_eq!(crate::connection::response_request_id(&headers), None);

    headers.insert("x-request-id", HeaderValue::from_static(""));
    assert_eq!(crate::connection::response_request_id(&headers), None);

    headers.insert(
        "x-request-id",
        HeaderValue::from_bytes(&[0xff]).expect("opaque header value"),
    );
    assert_eq!(crate::connection::response_request_id(&headers), None);

    headers.insert("x-request-id", HeaderValue::from_static("req_valid"));
    assert_eq!(
        crate::connection::response_request_id(&headers).as_deref(),
        Some("req_valid")
    );
}

#[test]
fn profile_cache_keys_are_bounded_deterministic_and_component_isolated() {
    fn profile(provider: &str, model: &str) -> ResolvedModelProfile {
        resolved_profile(
            provider,
            model,
            "https://example.test/v1/responses".to_string(),
            "key",
            false,
            ReasoningSummaryLevel::Detailed,
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            128_000,
        )
    }

    let base = profile("openai", "gpt-5.6-sol");
    let key = crate::router::profile_cache_key("session-123", &base);
    assert_eq!(
        key,
        "1dfc278df8539b0f358a21e5a40f1b56ead820c782d2eb1761ddaeb01f222e4c"
    );
    assert_eq!(key.len(), 64);
    assert!(
        key.bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    );
    assert_eq!(
        key,
        crate::router::profile_cache_key("session-123", &base),
        "recreating a router identity must be deterministic"
    );

    let variants = [
        crate::router::profile_cache_key("child-session-123", &base),
        crate::router::profile_cache_key("session-123", &profile("gateway", "gpt-5.6-sol")),
        crate::router::profile_cache_key("session-123", &profile("openai", "gpt-5.6-terra")),
        crate::router::profile_cache_key(&"会话🌍".repeat(100), &base),
    ];
    for variant in variants {
        assert_eq!(variant.len(), 64);
        assert_ne!(variant, key);
    }
}

#[tokio::test]
async fn enabled_cache_keys_are_validated_before_transport_setup_and_disabled_keys_are_ignored() {
    let invalid_endpoint_profile = resolved_profile(
        "openai",
        "gpt-test",
        "not a Responses URL".to_string(),
        "key",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    for invalid_key in [String::new(), "界".repeat(22)] {
        let error = OpenAiProvider::connect(
            &invalid_endpoint_profile,
            zevria_foundation::ReasoningLevel::Medium,
            "preamble",
            ToolServer::new().run(),
            "test-session-id",
            &invalid_key,
        )
        .await
        .err()
        .expect("an enabled empty or overlong cache key must fail");
        assert!(error.to_string().contains("prompt_cache_key"), "{error:#}");
        assert!(
            !error.to_string().contains("endpoint URL"),
            "cache-key validation must precede endpoint and transport setup: {error:#}"
        );
    }

    let disabled_profile = resolved_profile(
        "openai",
        "gpt-test",
        "http://127.0.0.1:1/v1/responses".to_string(),
        "key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig {
            send_prompt_cache_key: false,
            ..ResponsesCompatibilityConfig::default()
        },
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    for ignored_key in [String::new(), "界".repeat(100)] {
        let provider = OpenAiProvider::connect(
            &disabled_profile,
            zevria_foundation::ReasoningLevel::Medium,
            "preamble",
            ToolServer::new().run(),
            "test-session-id",
            &ignored_key,
        )
        .await
        .expect("disabled cache-key transmission skips validation and network activity");
        assert_eq!(provider.prompt_cache_key, ignored_key);
        assert!(
            provider
                .prepared_responses_parameters()
                .and_then(|value| value.get("prompt_cache_key").cloned())
                .is_none()
        );
    }
}

#[tokio::test]
async fn websocket_router_transmits_its_hashed_profile_cache_key_and_encrypted_reasoning_include() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let profile = resolved_profile(
        "gateway",
        "model.长",
        format!("ws://{address}/v1/responses"),
        "key",
        true,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        128_000,
    );
    let expected_key = crate::router::profile_cache_key("root-会话", &profile);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("WebSocket connection");
        let mut socket = accept_async(stream).await.expect("WebSocket upgrade");
        let request = receive_json(&mut socket).await;
        assert_eq!(request["prompt_cache_key"], expected_key);
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        send_json(
            &mut socket,
            completed_event("resp_hashed", "msg_hashed", "hashed"),
        )
        .await;
    });

    let mut router = ResponsesRouter::from_routes(
        [(
            ModelRole::Build,
            profile,
            zevria_foundation::ReasoningLevel::Medium,
        )],
        "preamble",
        ToolServer::new().run(),
        "root-会话",
    )
    .expect("single-profile router");
    let parts = RequestParts {
        prompt: Message::user("cache shape"),
        history: Vec::new(),
        instructions: "instructions".to_string(),
        allowed_tool_names: None,
    };
    router
        .complete(parts.request(), discard_updates())
        .await
        .expect("WebSocket response");
    server.await.expect("WebSocket server");
}

#[tokio::test]
async fn initial_and_replacement_handshakes_use_only_configured_headers() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let (release_server, server_released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        for request_id in ["ws_connect_initial", "ws_connect_replacement"] {
            let (stream, _) = listener.accept().await.expect("server should accept");
            let socket = accept_hdr_async(
                stream,
                AssertOpenAiWebSocketHandshakeWithRequestId(request_id),
            )
            .await
            .expect("server should upgrade websocket");
            sockets.push(socket);
        }
        let _ = server_released.await;
    });

    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer test-key"));
    let config = OpenAiWebSocketConfig {
        url: format!("ws://{address}"),
        headers,
    };
    let connection = config
        .connect()
        .await
        .expect("initial handshake should succeed");
    assert_eq!(
        connection.websocket_connection_request_id.as_deref(),
        Some("ws_connect_initial")
    );
    let mut parked = OpenAiParkedWebSocket {
        session: OpenAiWebSocketSession::new(connection.socket),
        config,
        socket_generation: 0,
        websocket_connection_request_id: connection.websocket_connection_request_id,
        continuation: None,
        pending_done_response_id: None,
        last_activity: std::time::Instant::now(),
    };

    parked
        .reconnect()
        .await
        .expect("replacement handshake should succeed");
    assert_eq!(parked.socket_generation, 1);
    assert_eq!(
        parked.websocket_connection_request_id.as_deref(),
        Some("ws_connect_replacement")
    );

    let _ = release_server.send(());
    server.await.expect("server task should finish");
}

#[test]
fn input_token_count_url_supports_derivation_override_and_disable() {
    let responses =
        reqwest::Url::parse("https://user:secret@example.test/v1/responses?gateway_token=secret")
            .expect("Responses URL");
    let derived = resolve_input_token_count_url(&responses, &InputTokenCountConfig::default())
        .expect("derived URL")
        .expect("enabled URL");
    assert_eq!(derived.path(), "/v1/responses/input_tokens");
    assert_eq!(derived.query(), Some("gateway_token=secret"));

    let explicit = InputTokenCountConfig {
        url: Some("https://counter.test/custom/count".to_string()),
        ..InputTokenCountConfig::default()
    };
    assert_eq!(
        resolve_input_token_count_url(&responses, &explicit)
            .expect("explicit URL")
            .expect("enabled")
            .as_str(),
        "https://counter.test/custom/count"
    );
    let disabled = InputTokenCountConfig {
        enabled: false,
        ..InputTokenCountConfig::default()
    };
    assert_eq!(
        resolve_input_token_count_url(&responses, &disabled).expect("disabled setting"),
        None
    );
}

#[path = "image_tests.rs"]
mod image_tests;

#[tokio::test]
async fn exact_input_count_posts_the_complete_logical_request_without_touching_continuation() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("count listener");
    let address = listener.local_addr().expect("count address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("count connection");
        let request = receive_http_request(&mut stream).await;
        let body = r#"{"object":"response.input_tokens","input_tokens":321}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("count response");
        request
    });

    let mut openai = test_session_with_pump(
        "ws://127.0.0.1:9/v1/responses",
        OpenAiWebSocketSession::disconnected(OpenAiWebSocketTerminalCategory::HttpFallback),
        None,
    );
    openai.model.model = "gpt-count".to_string();
    openai.compatibility = ResponsesCompatibilityConfig::default();
    openai.responses_parameters = Some(json!({
        "prompt_cache_key": "test-session",
        "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": "medium", "summary": "detailed"}
    }));
    openai.input_token_count_url = Some(
        reqwest::Url::parse(&format!("http://{address}/custom/input_tokens?version=1"))
            .expect("count URL"),
    );
    openai
        .additional_params
        .insert("gateway_routing".to_string(), json!("count-route"));
    openai.additional_params.insert(
        "prompt_cache_options".into(),
        json!({"comparison_response_id":"resp_synthetic_baseline", "mode":"implicit", "ttl":"30m"}),
    );
    openai.ws.continuation = Some(ContinuationState {
        socket_generation: 0,
        response_id: "resp_preserved".to_string(),
        request_properties: json!({"preserve": true}),
        request_input: vec![json!({"role": "user", "content": "old"})],
        response_output: vec![json!({"type": "message", "id": "old"})],
    });
    let image = zevria_content::PromptImage::from_rgba(1, 1, &[3, 2, 1, 255]).unwrap();
    let parts = RequestParts {
        prompt: zevria_content::UserPrompt::new(vec![
            zevria_content::PromptBlock::Text("count this request".into()),
            zevria_content::PromptBlock::Image(image.clone()),
        ])
        .unwrap()
        .to_message(),
        history: vec![Message::assistant("earlier answer")],
        instructions: "Count turn instructions".to_string(),
        allowed_tool_names: Some(Vec::new()),
    };

    let count_request =
        parts.request_with_role_and_skills(ModelRole::Review, Some("Active skill instructions"));
    let count = openai
        .count_input_tokens(count_request)
        .await
        .expect("exact count");
    assert_eq!(count, InputTokenCount::Exact(321));
    assert_eq!(
        openai
            .ws
            .continuation
            .as_ref()
            .map(|state| state.response_id.as_str()),
        Some("resp_preserved")
    );

    let request = server.await.expect("count server");
    let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
    assert!(headers.starts_with("POST /custom/input_tokens?version=1 HTTP/1.1"));
    let body: Value = serde_json::from_str(body).expect("count JSON");
    assert_eq!(body["model"], "gpt-count");
    assert_eq!(body["gateway_routing"], "count-route");
    assert_eq!(
        body["prompt_cache_options"],
        json!({"mode":"implicit", "ttl":"30m"})
    );
    for response_only in [
        "background",
        "include",
        "previous_response_id",
        "prompt_cache_key",
        "store",
        "stream",
    ] {
        assert!(
            body.get(response_only).is_none(),
            "count body retained response-only field {response_only}: {body}"
        );
    }
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(
        body["instructions"],
        rendered_test_instructions("Count turn instructions")
    );
    let input = body["input"].to_string();
    assert!(input.contains("Active skill instructions"));
    assert!(!input.contains("Count turn instructions"));
    assert!(!input.contains("skill catalog"));
    assert!(input.contains(&format!("data:image/png;base64,{}", image.base64())));
    assert!(
        body["input"]
            .as_array()
            .is_some_and(|input| input.len() >= 2)
    );
}

#[tokio::test]
async fn definitive_input_count_unsupported_is_cached_but_transient_errors_retry() {
    let unsupported_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("unsupported listener");
    let unsupported_address = unsupported_listener
        .local_addr()
        .expect("unsupported address");
    let unsupported_server = tokio::spawn(async move {
        let (mut stream, _) = unsupported_listener
            .accept()
            .await
            .expect("unsupported connection");
        let _ = receive_http_request(&mut stream).await;
        let body = "unsupported";
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.expect("404");
    });
    let mut unsupported = test_session_with_pump(
        "ws://127.0.0.1:9/v1/responses",
        OpenAiWebSocketSession::disconnected(OpenAiWebSocketTerminalCategory::HttpFallback),
        None,
    );
    unsupported.input_token_count_url = Some(
        reqwest::Url::parse(&format!("http://{unsupported_address}/input_tokens"))
            .expect("unsupported URL"),
    );
    let parts = RequestParts {
        prompt: Message::user("count"),
        history: Vec::new(),
        instructions: "instructions".to_string(),
        allowed_tool_names: None,
    };
    assert_eq!(
        unsupported
            .count_input_tokens(parts.request())
            .await
            .expect("unsupported result"),
        InputTokenCount::Unsupported
    );
    unsupported_server.await.expect("unsupported server");
    assert!(unsupported.input_token_count_unsupported);
    assert_eq!(
        unsupported
            .count_input_tokens(parts.request())
            .await
            .expect("cached unsupported"),
        InputTokenCount::Unsupported
    );

    let transient_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("transient listener");
    let transient_address = transient_listener.local_addr().expect("transient address");
    let transient_server = tokio::spawn(async move {
        for (status, body) in [
            ("500 Internal Server Error", "temporary"),
            ("200 OK", r#"{"input_tokens":77}"#),
        ] {
            let (mut stream, _) = transient_listener
                .accept()
                .await
                .expect("transient connection");
            let _ = receive_http_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("transient response");
        }
    });
    let mut transient = test_session_with_pump(
        "ws://127.0.0.1:9/v1/responses",
        OpenAiWebSocketSession::disconnected(OpenAiWebSocketTerminalCategory::HttpFallback),
        None,
    );
    transient.input_token_count_url = Some(
        reqwest::Url::parse(&format!("http://{transient_address}/input_tokens"))
            .expect("transient URL"),
    );
    assert!(transient.count_input_tokens(parts.request()).await.is_err());
    assert!(!transient.input_token_count_unsupported);
    assert_eq!(
        transient
            .count_input_tokens(parts.request())
            .await
            .expect("retry succeeds"),
        InputTokenCount::Exact(77)
    );
    transient_server.await.expect("transient server");
}

#[tokio::test]
async fn configured_remote_compaction_posts_ordered_input_and_preserves_opaque_output() {
    async fn receive_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.expect("read HTTP request");
            assert!(read > 0, "HTTP request ended before its headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("content-length header");
        while bytes.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.expect("read HTTP body");
            assert!(read > 0, "HTTP request ended before its body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8(bytes).expect("HTTP request is UTF-8")
    }

    let http_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let http_address = http_listener.local_addr().expect("HTTP address");
    let requests = Arc::new(AtomicUsize::new(0));
    let dispatched = requests.clone();
    let http_server = tokio::spawn(async move {
        let (mut stream, _) = http_listener.accept().await.expect("HTTP connection");
        let request = receive_http_request(&mut stream).await;
        dispatched.fetch_add(1, Ordering::SeqCst);
        let body = r#"{"output":[{"type":"compaction","encrypted_content":"opaque"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write HTTP response");
        request
    });

    let ws_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("WebSocket listener");
    let ws_address = ws_listener.local_addr().expect("WebSocket address");
    let ws_server = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.expect("WebSocket connection");
        let mut socket = accept_async(stream).await.expect("WebSocket upgrade");
        let _ = socket.next().await;
    });
    let (socket, _) = connect_async(format!("ws://{ws_address}"))
        .await
        .expect("WebSocket client");
    let mut openai = test_session_with_url(&format!("ws://{ws_address}"), socket, None);
    openai.model.model = "gpt-plan".to_string();
    openai.responses_parameters = Some(json!({
        "reasoning": {"effort": "medium", "summary": "detailed"}
    }));
    openai
        .additional_params
        .insert("gateway_routing".to_string(), json!("compact-route"));
    openai.additional_params.insert(
        "prompt_cache_options".into(),
        json!({"comparison_response_id":"resp_synthetic_baseline", "mode":"implicit", "ttl":"30m"}),
    );
    openai.compaction_url = Some(
        reqwest::Url::parse(&format!(
            "http://{http_address}/custom/responses/compact?version=1"
        ))
        .expect("remote URL"),
    );
    openai.api_key = "remote-secret".to_string();
    let parts = RequestParts {
        prompt: Message::user("compact this"),
        history: vec![Message::assistant("earlier answer")],
        instructions: "Remote compact instructions".to_string(),
        allowed_tool_names: Some(Vec::new()),
    };
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        openai.compact(
            parts.request_with_role_and_skills(ModelRole::Plan, Some("must not enter compaction")),
        ),
    )
    .await
    .expect("invalid maintenance input must be rejected locally")
    .expect_err("skill overlays are forbidden on compaction requests");
    assert!(error.to_string().contains("ordered developer instructions"));
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    let mut request = parts.maintenance_request();
    request.model_role = ModelRole::Plan;
    let result = openai.compact(request).await.expect("remote compaction");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let CompactResult::Replacement(replacement) = result else {
        panic!("configured endpoint must be used");
    };
    assert!(matches!(
        replacement.as_slice(),
        [zevria_model::OwnedModelRequestItem::ReplayOnly(replay)]
            if replay.items == vec![json!({
                "type": "compaction",
                "encrypted_content": "opaque"
            })]
    ));

    let request = http_server.await.expect("HTTP server");
    let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
    assert!(headers.starts_with("POST /custom/responses/compact?version=1 HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer remote-secret")
    );
    let body: Value = serde_json::from_str(body).expect("compact JSON body");
    assert_eq!(body["model"], "gpt-plan");
    assert!(
        body["input"]
            .as_array()
            .is_some_and(|input| !input.is_empty())
    );
    assert_eq!(
        body["instructions"],
        zevria_instructions::InstructionSet::maintenance("Remote compact instructions", [])
            .render()
    );
    assert!(
        !body["input"]
            .to_string()
            .contains("Remote compact instructions")
    );
    assert!(
        !body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["role"] == "developer")
    );
    assert!(body.get("stream").is_none());
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("parallel_tool_calls").is_some(), "{body}");
    assert!(body.get("reasoning").is_some(), "{body}");
    assert_eq!(body["store"], false);
    assert_eq!(body["gateway_routing"], "compact-route");
    assert_eq!(
        body["prompt_cache_options"],
        json!({"mode":"implicit", "ttl":"30m"})
    );
    assert_eq!(body["tools"], json!([]));
    ws_server.abort();
}

#[test]
fn remote_compaction_trimming_rewrites_only_complete_correlated_outputs() {
    let large = "x".repeat(1_100_000);
    let input = vec![
        json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "read",
            "arguments": "{}",
            "status": "completed"
        }),
        json!({
            "type": "function_call_output",
            "id": "out_1",
            "call_id": "call_1",
            "status": "completed",
            "output": {"body": large, "success": true}
        }),
        json!({
            "type": "function_call_output",
            "id": "orphan",
            "call_id": "missing_call",
            "status": "completed",
            "output": "must stay intact"
        }),
    ];
    let original = input.clone();

    let rewritten = crate::turn::trim_correlated_tool_outputs(&input, 0, 272_000);

    assert_eq!(
        input, original,
        "the source transcript/request is immutable"
    );
    assert_eq!(rewritten[0], input[0], "the call is preserved exactly");
    assert_eq!(rewritten[1]["id"], "out_1");
    assert_eq!(rewritten[1]["call_id"], "call_1");
    assert_eq!(rewritten[1]["status"], "completed");
    assert_eq!(rewritten[1]["output"]["success"], true);
    assert_eq!(
        rewritten[1]["output"]["body"],
        "Output exceeded the available model context and was truncated"
    );
    assert_eq!(rewritten[2], input[2], "an orphan output is never trimmed");
}

#[test]
fn remote_compaction_trimming_uses_the_selected_profile_window() {
    let input = vec![
        function_call("fc_small", "call_small", "read", json!({})),
        json!({
            "type": "function_call_output",
            "id": "out_small",
            "call_id": "call_small",
            "status": "completed",
            "output": "x".repeat(4_000)
        }),
    ];
    let large_profile = crate::turn::trim_correlated_tool_outputs(&input, 0, 10_000);
    assert_eq!(large_profile, input, "large profile keeps the output");

    let small_profile = crate::turn::trim_correlated_tool_outputs(&input, 0, 100);
    assert_eq!(
        small_profile[1]["output"],
        "Output exceeded the available model context and was truncated"
    );
}

#[tokio::test]
async fn parked_websocket_pump_answers_ping_with_matching_pong() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let payload = b"parked-ping".to_vec();
        socket
            .send(WebSocketMessage::Ping(payload.clone().into()))
            .await
            .expect("server ping should send");
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
            .await
            .expect("the parked pump should answer promptly")
            .expect("the pong should exist")
            .expect("the pong should be valid");
        assert_eq!(reply, WebSocketMessage::Pong(payload.into()));
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let session = OpenAiWebSocketSession::new(socket);

    server.await.expect("server task should finish");
    drop(session);
}

#[tokio::test]
async fn replacing_a_session_isolates_events_by_session_receiver() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let (old_event_sent, old_event_received) = oneshot::channel();
    let (release_server, server_released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut old_socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        old_socket
            .send(WebSocketMessage::text("old-generation-event"))
            .await
            .expect("old event should send");
        let _ = old_event_sent.send(());

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut new_socket = accept_async(stream)
            .await
            .expect("server should upgrade replacement");
        new_socket
            .send(WebSocketMessage::text("new-generation-event"))
            .await
            .expect("new event should send");
        let _ = server_released.await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    old_event_received
        .await
        .expect("old event should reach the network");
    tokio::task::yield_now().await;

    openai
        .ws
        .reconnect()
        .await
        .expect("replacement handshake should succeed");
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), openai.ws.session.next())
        .await
        .expect("the replacement event should arrive")
        .expect("the replacement event should exist");
    match event {
        OpenAiWebSocketInbound::Message(WebSocketMessage::Text(text)) => {
            assert_eq!(text, "new-generation-event");
        }
        _ => panic!("the replacement should yield its own text event"),
    }

    let _ = release_server.send(());
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn dropping_a_websocket_session_terminates_its_pump_and_connection() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
            .await
            .expect("dropping the session should close the connection");
        assert!(
            result.is_none()
                || result.is_some_and(|message| {
                    message.is_err() || matches!(message, Ok(WebSocketMessage::Close(_)))
                }),
            "the dropped session must not leave a live socket"
        );
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let session = OpenAiWebSocketSession::new(socket);
    drop(session);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn provider_cancellation_terminates_the_socket_and_invalidates_continuation_state() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let closed = tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
            .await
            .expect("local cancellation should close the socket promptly");
        assert!(
            closed.is_none()
                || closed.is_some_and(|message| {
                    message.is_err() || matches!(message, Ok(WebSocketMessage::Close(_)))
                })
        );
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, Some("pending-done"));
    openai.ws.continuation = Some(ContinuationState {
        socket_generation: 0,
        response_id: "response-before-cancel".to_string(),
        request_properties: json!({"model": "gpt-test"}),
        request_input: vec![json!({"role": "user"})],
        response_output: vec![json!({"type": "message"})],
    });

    <OpenAiProvider as zevria_session_api::ModelProvider>::cancel(&mut openai);

    assert_eq!(
        openai.ws.session.terminal_status(),
        Some(OpenAiWebSocketTerminalCategory::LocalCancellation)
    );
    assert!(openai.ws.continuation.is_none());
    assert!(openai.ws.pending_done_response_id.is_none());
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn overflowing_the_parked_inbound_queue_is_terminal() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        for index in 0..300 {
            if socket
                .send(WebSocketMessage::text(format!("event-{index}")))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let session = OpenAiWebSocketSession::new(socket);
    let terminal = wait_for_terminal(&session).await;
    assert_eq!(terminal, OpenAiWebSocketTerminalCategory::InboundOverflow);

    server.await.expect("server task should finish");
}

fn assert_request_policy(request: &Value, policy: &str, expected_tools: &[&str]) {
    assert!(request["instructions"].as_str().unwrap().contains(policy));
    let tools = request["tools"]
        .as_array()
        .expect("request should contain a tool array");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>(),
        expected_tools
    );
    assert!(tools.iter().all(|tool| tool["strict"] == true));
}

#[derive(Deserialize)]
struct PolicyArgs {}

struct PolicyCommandTool;

impl Tool for PolicyCommandTool {
    const NAME: &'static str = "command";
    type Error = std::convert::Infallible;
    type Args = PolicyArgs;
    type Output = String;

    fn description(&self) -> String {
        "test command policy".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok("command".to_string())
    }
}

struct PolicyWriteTool;

impl Tool for PolicyWriteTool {
    const NAME: &'static str = "write";
    type Error = std::convert::Infallible;
    type Args = PolicyArgs;
    type Output = String;

    fn description(&self) -> String {
        "test write policy".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        Ok("write".to_string())
    }
}

fn policy_tools() -> ToolServerHandle {
    ToolServer::new()
        .tool(PolicyCommandTool)
        .tool(PolicyWriteTool)
        .run()
}

#[tokio::test]
async fn default_request_keeps_current_responses_optional_fields() {
    let request = capture_http_request_with_options(
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        policy_tools(),
    )
    .await;

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/prepared-default.json");
    if std::env::var_os("UPDATE_INSTRUCTION_FIXTURES").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&request.body).unwrap()),
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(request.body, expected, "complete prepared request changed");
    let headers = request.headers.to_ascii_lowercase();
    assert!(headers.starts_with("post /v1/responses http/1.1"));
    assert!(headers.contains("authorization: bearer test-key"));
    assert_eq!(request.body["reasoning"]["effort"], "medium");
    assert_eq!(request.body["reasoning"]["summary"], "detailed");
    assert_eq!(request.body["prompt_cache_key"], "compat-session");
    assert_eq!(
        request.body["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(request.body["store"], false);
    assert_eq!(request.body["stream"], true);
    let tools = request.body["tools"].as_array().expect("tool definitions");
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|tool| tool["strict"] == true));
}

#[tokio::test]
async fn compatibility_options_omit_responses_fields_independently() {
    let no_reasoning = capture_http_request_with_options(
        ResponsesCompatibilityConfig {
            send_reasoning: false,
            ..ResponsesCompatibilityConfig::default()
        },
        BTreeMap::new(),
        policy_tools(),
    )
    .await
    .body;
    assert!(no_reasoning.get("reasoning").is_none());
    assert_eq!(
        no_reasoning["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(no_reasoning["prompt_cache_key"], "compat-session");
    assert_eq!(no_reasoning["store"], false);
    assert!(
        no_reasoning["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().all(|tool| tool["strict"] == true))
    );

    let non_strict = capture_http_request_with_options(
        ResponsesCompatibilityConfig {
            strict_tools: false,
            ..ResponsesCompatibilityConfig::default()
        },
        BTreeMap::new(),
        policy_tools(),
    )
    .await
    .body;
    let tools = non_strict["tools"].as_array().expect("non-strict tools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>(),
        ["command", "write"]
    );
    assert!(tools.iter().all(|tool| tool.get("strict").is_none()));
    assert!(tools.iter().all(|tool| tool.get("parameters").is_some()));
    assert!(non_strict.get("reasoning").is_some());
    assert_eq!(non_strict["prompt_cache_key"], "compat-session");
    assert_eq!(non_strict["store"], false);

    let no_encrypted_reasoning = capture_http_request_with_options(
        ResponsesCompatibilityConfig {
            send_reasoning_encrypted_content: false,
            ..ResponsesCompatibilityConfig::default()
        },
        BTreeMap::new(),
        policy_tools(),
    )
    .await
    .body;
    assert!(
        no_encrypted_reasoning.get("include").is_none(),
        "{no_encrypted_reasoning}"
    );
    assert!(no_encrypted_reasoning.get("reasoning").is_some());
    assert_eq!(no_encrypted_reasoning["prompt_cache_key"], "compat-session");

    let no_cache_key = capture_http_request_with_options(
        ResponsesCompatibilityConfig {
            send_prompt_cache_key: false,
            ..ResponsesCompatibilityConfig::default()
        },
        BTreeMap::new(),
        policy_tools(),
    )
    .await
    .body;
    assert!(no_cache_key.get("prompt_cache_key").is_none());
    assert_eq!(
        no_cache_key["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert!(no_cache_key.get("reasoning").is_some());
    assert_eq!(no_cache_key["store"], false);
    assert!(
        no_cache_key["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().all(|tool| tool["strict"] == true))
    );

    let no_store = capture_http_request_with_options(
        ResponsesCompatibilityConfig {
            send_store: false,
            ..ResponsesCompatibilityConfig::default()
        },
        BTreeMap::new(),
        policy_tools(),
    )
    .await
    .body;
    assert!(no_store.get("store").is_none());
    assert!(no_store.get("reasoning").is_some());
    assert_eq!(no_store["prompt_cache_key"], "compat-session");
    assert!(
        no_store["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().all(|tool| tool["strict"] == true))
    );
}

#[tokio::test]
async fn gateway_specific_responses_parameters_merge_into_the_prepared_snapshot() {
    let additional_params = BTreeMap::from([
        ("gateway_routing".to_string(), json!("deepseek-pool")),
        (
            "thinking".to_string(),
            json!({"type": "enabled", "budget_tokens": 2048}),
        ),
    ]);
    let request = capture_http_request_with_options(
        ResponsesCompatibilityConfig::default(),
        additional_params,
        ToolServer::new().run(),
    )
    .await
    .body;

    assert_eq!(request["gateway_routing"], "deepseek-pool");
    assert_eq!(
        request["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert_eq!(request["model"], "gpt-test");
    assert!(request["input"].is_array());
    assert_eq!(request["stream"], true);
}

#[tokio::test]
async fn send_store_false_omits_store_from_websocket_requests() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("WebSocket connection");
        let mut socket = accept_async(stream).await.expect("WebSocket upgrade");
        let request = receive_json(&mut socket).await;
        assert!(request.get("store").is_none(), "{request}");
        send_json(
            &mut socket,
            completed_event("resp_no_store", "msg_no_store", "done"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("WebSocket client");
    let mut openai = test_session(socket, None);
    openai.compatibility.send_store = false;
    let mut state = AttemptState::default();
    run_turn("omit store", &mut openai, &mut state, &discard_updates())
        .await
        .expect("WebSocket turn");
    server.await.expect("WebSocket server");
}

fn attempt_message(state: &AttemptState) -> Message {
    if let Some(message) = state
        .history
        .iter()
        .rev()
        .find(|message| matches!(message, Message::Assistant { .. }))
    {
        return message.clone();
    }
    state
        .result
        .assistant_message()
        .expect("attempt should contain an assistant message")
}

fn record_item(accumulator: &mut AssistantMessageAccumulator, value: Value) {
    let event = parse_server_event(&value.to_string())
        .expect("event should parse")
        .expect("event should be recognized");
    let ResponsesWebSocketEvent::Item(item) = event else {
        panic!("test event should be an item event");
    };
    accumulator
        .record_item(item)
        .expect("item should accumulate");
}

fn completed_event(response_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "type": "response.completed",
        "sequence_number": 1,
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "model": "gpt-test",
            "usage": null,
            "output": [{
                "type": "message",
                "id": message_id,
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text
                }]
            }],
            "tools": []
        }
    })
}

fn completed_output_event(response_id: &str, output: Vec<Value>) -> Value {
    json!({
        "type": "response.completed",
        "sequence_number": 1,
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "model": "gpt-test",
            "usage": null,
            "output": output,
            "tools": []
        }
    })
}

fn function_call(id: &str, call_id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "type": "function_call",
        "id": id,
        "call_id": call_id,
        "name": name,
        "arguments": arguments.to_string(),
        "status": "completed"
    })
}

fn output_text_delta(text: &str, sequence_number: u64) -> Value {
    output_text_delta_at(text, sequence_number, 0, "msg_test")
}

fn output_text_delta_at(
    text: &str,
    sequence_number: u64,
    output_index: u64,
    item_id: &str,
) -> Value {
    json!({
        "type": "response.output_text.delta",
        "item_id": item_id,
        "output_index": output_index,
        "content_index": 0,
        "sequence_number": sequence_number,
        "delta": text
    })
}

fn done_event(response_id: &str, status: &str) -> Value {
    json!({
        "type": "response.done",
        "response": {
            "id": response_id,
            "status": status
        }
    })
}

/// The completed-event fixture with a real usage object — the other fixtures
/// keep `"usage": null`, so only turns built on this one emit `UsageUpdated`.
fn completed_event_with_usage(response_id: &str, message_id: &str, text: &str) -> Value {
    let mut event = completed_event(response_id, message_id, text);
    event["response"]["usage"] = json!({
        "input_tokens": 1200,
        "input_tokens_details": {"cached_tokens": 1000},
        "output_tokens": 40,
        "total_tokens": 1240
    });
    event
}

/// The usage values [`completed_event_with_usage`] carries.
fn expected_token_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 1200,
        cached_tokens: 1000,
        output_tokens: 40,
        total_tokens: 1240,
    }
}

/// Drain every already-delivered session event down to its usage payloads.
fn usage_events(receiver: &mut SessionEventReceiver) -> Vec<TokenUsage> {
    let mut usages = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        if let SessionUpdate::Lifecycle(SessionEvent::UsageUpdated { usage, .. }) = update {
            usages.push(usage);
        }
    }
    usages
}

fn text_with_openai_extras(text: &str, extras: Value) -> Text {
    Text {
        text: text.to_string(),
        additional_params: AdditionalParams::from_entries(Some(("openai_responses", extras))),
    }
}

#[test]
fn terminal_replay_is_authoritative_over_the_streaming_preview() {
    let mut state = AttemptState::default();
    record_item(
        &mut state.result,
        output_text_delta_at("draft", 1, 0, "msg_terminal"),
    );
    let mut response =
        completed_event("resp_terminal", "msg_terminal", "final")["response"].clone();
    response["output"][0]["content"][0]["annotations"] =
        json!([{"type": "citation", "source": "terminal"}]);
    let replay = ProviderReplay::openai_responses(
        test_profile_ref(),
        response["output"]
            .as_array()
            .expect("terminal output should be an array")
            .clone(),
    );
    let actual = replay.to_message().expect("terminal replay should convert");
    let text = text_with_openai_extras(
        "final",
        json!({
            "annotations": [{"type": "citation", "source": "terminal"}]
        }),
    );
    let expected = Message::Assistant {
        id: Some("msg_terminal".to_string()),
        content: vec![AssistantContent::Text(text)],
    };

    assert_eq!(actual, expected);
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_terminal".to_string()),
            content: vec![AssistantContent::text("draft")],
        },
        "the accumulator remains only an in-flight preview"
    );
}

#[tokio::test]
async fn foreign_replay_only_checkpoint_fails_before_network() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let replay = ProviderReplay::openai_responses(
        zevria_foundation::ModelProfileRef::new("foreign-provider", "foreign-model"),
        vec![json!({
            "type": "compaction",
            "encrypted_content": "opaque-checkpoint"
        })],
    );
    let prompt = Message::user("continue");
    let request = ModelRequest {
        instructions: test_instructions(),
        input: vec![
            ModelRequestItem::replay_only(&replay),
            ModelRequestItem::message(&prompt),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let mut provider = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        provider.complete(request, discard_updates()),
    )
    .await
    .expect("foreign opaque replay must be rejected locally")
    .expect_err("foreign opaque replay must fail locally");
    let detail = format!("{error:#}");
    assert!(detail.contains("replay-only openai.responses history"));
    assert!(detail.contains("foreign target profile"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(40), listener.accept())
            .await
            .is_err(),
        "rejected replay must not connect to the provider"
    );
}

#[tokio::test]
async fn foreign_replay_uses_portable_text_and_correlated_tool_projection() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("portable replay listener");
    let address = listener.local_addr().expect("portable replay address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("portable HTTP request");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_portable", "msg_portable", "continued").to_string()),
        )
        .await;
        request.body
    });

    let replay = ProviderReplay::openai_responses(
        zevria_foundation::ModelProfileRef::new("foreign-provider", "foreign-model"),
        vec![
            json!({
                "type": "message",
                "id": "msg_native",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {"type": "output_text", "text": "visible answer"},
                    {"type": "refusal", "refusal": "visible refusal"}
                ]
            }),
            json!({
                "type": "reasoning",
                "id": "rs_native",
                "summary": [{"type": "summary_text", "text": "private summary"}],
                "content": [{"type": "reasoning_text", "text": "private reasoning"}],
                "encrypted_content": "ciphertext",
                "status": null
            }),
            function_call(
                "fc_native",
                "call_native",
                "command",
                json!({"command": "pwd"}),
            ),
            json!({"type": "future_opaque", "id": "opaque_native", "secret": true}),
        ],
    );
    let canonical = replay.to_message().expect("canonical foreign message");
    let Message::Assistant { content, .. } = &canonical else {
        panic!("canonical replay must be assistant history");
    };
    let call = content
        .iter()
        .find_map(|item| match item {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("canonical tool call");
    let result = Message::User {
        content: vec![rig_core::message::UserContent::ToolResult(
            rig_core::message::ToolResult {
                call: call.id,
                provider: call.provider,
                name: call.function.name,
                content: vec![rig_core::message::ToolResultContent::text("tool output")],
            },
        )],
    };
    let trusted = zevria_model::ReplayMessage::new(replay.clone()).unwrap();
    let prompt = Message::user("continue elsewhere");
    let request = ModelRequest {
        instructions: test_instructions(),
        input: vec![
            ModelRequestItem::replay_backed(&trusted),
            ModelRequestItem::message(&result),
            ModelRequestItem::message(&prompt),
        ],
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let mut provider = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    provider
        .complete(request, discard_updates())
        .await
        .expect("foreign portable replay request");

    let body = server.await.expect("portable replay server");
    let input = body["input"].as_array().expect("portable input array");
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("portable function call");
    let output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("portable function result");
    let portable_id = call["call_id"].as_str().expect("portable call id");
    assert!(portable_id.starts_with("zevria_call_"));
    assert_eq!(output["call_id"], portable_id);
    let wire = body.to_string();
    for retained in [
        "visible answer",
        "visible refusal",
        "command",
        "pwd",
        "tool output",
        "continue elsewhere",
    ] {
        assert!(
            wire.contains(retained),
            "missing portable content: {retained}"
        );
    }
    for omitted in [
        "msg_native",
        "rs_native",
        "private summary",
        "private reasoning",
        "ciphertext",
        "fc_native",
        "call_native",
        "opaque_native",
    ] {
        assert!(!wire.contains(omitted), "foreign replay leaked {omitted}");
    }
}

#[test]
fn native_output_done_items_reconcile_by_index_and_terminal_output_wins() {
    let arguments = r#"{"path": "x", "content": "y", "n": 1.0}"#;
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_indexed",
        "summary": [],
        "content": [],
        "encrypted_content": "opaque",
        "status": null,
        "future_field": true
    });
    let call = json!({
        "type": "function_call",
        "id": "fc_indexed",
        "call_id": "call_indexed",
        "name": "write",
        "arguments": arguments,
        "status": "completed"
    });

    let mut state = AttemptState::default();
    state.record_native_output_item_done(
        &json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": call
        })
        .to_string(),
    );
    state.record_native_output_item_done(
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": reasoning
        })
        .to_string(),
    );
    let indexed = state
        .take_native_output()
        .expect("complete indexed output should reconcile");
    assert_eq!(indexed, vec![reasoning.clone(), call.clone()]);
    assert_eq!(indexed[1]["arguments"], arguments);

    state.record_native_output_item_done(
        &json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "future_item", "stale": true}
        })
        .to_string(),
    );
    let terminal = vec![call, reasoning];
    state.record_native_terminal_output(
        &completed_output_event("resp_terminal", terminal.clone()).to_string(),
    );
    assert_eq!(
        state.take_native_output(),
        Some(terminal),
        "the terminal response output must be authoritative"
    );
}

#[test]
fn native_output_rejects_an_index_beyond_vector_limits() {
    let mut state = AttemptState::default();
    state.record_native_output_item_done(
        &json!({
            "type": "response.output_item.done",
            "output_index": u64::MAX,
            "item": {"type": "message"}
        })
        .to_string(),
    );

    assert_eq!(state.take_native_output(), None);
}

#[test]
fn reasoning_preview_matches_canonical_replay_order() {
    let mut state = AttemptState::default();

    // A relay can attach encrypted content before reasoning text and summary
    // blocks arrive. Every snapshot still projects the blocks canonically.
    record_item(
        &mut state.result,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "sequence_number": 1,
            "item": {
                "type": "reasoning",
                "id": "rs_enc",
                "summary": [],
                "content": [],
                "encrypted_content": "cipher draft",
                "status": "in_progress"
            }
        }),
    );

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(message_reasoning(
                Some("rs_enc".to_string()),
                vec![ReasoningContent::Encrypted("cipher draft".to_string())],
            ))],
        }
    );

    record_item(
        &mut state.result,
        json!({
            "type": "response.reasoning_text.delta",
            "item_id": "rs_enc",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 2,
            "delta": "streamed reasoning"
        }),
    );

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(message_reasoning(
                Some("rs_enc".to_string()),
                vec![
                    ReasoningContent::Text {
                        text: "streamed reasoning".to_string(),
                        signature: None,
                    },
                    ReasoningContent::Encrypted("cipher draft".to_string()),
                ],
            ))],
        }
    );

    record_item(
        &mut state.result,
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_enc",
            "output_index": 0,
            "summary_index": 0,
            "sequence_number": 3,
            "delta": "streamed summary"
        }),
    );

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(message_reasoning(
                Some("rs_enc".to_string()),
                vec![
                    ReasoningContent::Summary("streamed summary".to_string()),
                    ReasoningContent::Text {
                        text: "streamed reasoning".to_string(),
                        signature: None,
                    },
                    ReasoningContent::Encrypted("cipher draft".to_string()),
                ],
            ))],
        }
    );

    let reasoning_done_item = json!({
        "type": "reasoning",
        "id": "rs_enc",
        "summary": [{"type": "summary_text", "text": "final summary"}],
        "content": [{"type": "reasoning_text", "text": "final reasoning"}],
        "encrypted_content": "cipher final",
        "status": "completed"
    });
    record_item(
        &mut state.result,
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "sequence_number": 4,
            "item": reasoning_done_item.clone()
        }),
    );
    record_item(
        &mut state.result,
        output_text_delta_at("answer", 5, 1, "msg_enc"),
    );

    let expected = Message::Assistant {
        id: Some("msg_enc".to_string()),
        content: vec![
            AssistantContent::Reasoning(message_reasoning(
                Some("rs_enc".to_string()),
                vec![
                    ReasoningContent::Summary("final summary".to_string()),
                    ReasoningContent::Text {
                        text: "final reasoning".to_string(),
                        signature: None,
                    },
                    ReasoningContent::Encrypted("cipher final".to_string()),
                ],
            )),
            AssistantContent::text("answer"),
        ],
    };
    assert_eq!(attempt_message(&state), expected);

    let response = completed_output_event(
        "resp_enc",
        vec![
            reasoning_done_item,
            json!({
                "type": "message",
                "id": "msg_enc",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer"}]
            }),
        ],
    )["response"]
        .clone();
    let completed = ProviderReplay::openai_responses(
        test_profile_ref(),
        response["output"]
            .as_array()
            .expect("completed response should carry output")
            .clone(),
    )
    .to_message()
    .expect("completed replay should convert");

    assert_eq!(completed, expected);
    assert_eq!(attempt_message(&state), completed);
}

#[test]
fn malformed_streamed_tool_arguments_are_preserved_for_a_tool_failure() {
    let mut accumulator = AssistantMessageAccumulator::default();
    record_item(
        &mut accumulator,
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "sequence_number": 1,
            "item": {
                "type": "function_call",
                "id": "fc_bad",
                "call_id": "call_bad",
                "name": "broken",
                "arguments": "",
                "status": "in_progress"
            }
        }),
    );
    record_item(
        &mut accumulator,
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_bad",
            "output_index": 0,
            "sequence_number": 2,
            "delta": "{"
        }),
    );

    let message = accumulator
        .assistant_message()
        .expect("malformed arguments should remain a tool call");
    let Message::Assistant { content, .. } = message else {
        panic!("expected assistant message");
    };
    let Some(AssistantContent::ToolCall(call)) = content.first() else {
        panic!("expected tool call");
    };
    assert_eq!(call.function.arguments, json!("{"));
}

#[tokio::test]
async fn instructions_are_top_level_and_skill_directives_keep_input_order() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let request = receive_json(&mut socket).await;
        assert_eq!(
            request["instructions"],
            rendered_test_instructions("Plan overlay")
        );
        let input = request["input"]
            .as_array()
            .expect("request should contain an input array");
        assert_eq!(
            input
                .iter()
                .map(|item| item["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["developer", "user", "developer"]
        );
        assert!(input[0].to_string().contains("Skill directive: revoke"));
        assert!(input[2].to_string().contains("Active skill instructions"));
        assert!(!request.to_string().contains("Bounded skill catalog"));
        send_json(
            &mut socket,
            completed_event("resp_policy", "msg_policy", "planned"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();

    let parts = RequestParts {
        prompt: Message::user("plan this change"),
        history: Vec::new(),
        instructions: "Plan overlay".to_string(),
        allowed_tool_names: None,
    };
    let mut request =
        parts.request_with_role_and_skills(ModelRole::Build, Some("Active skill instructions"));
    let system = zevria_instructions::DirectiveContent::new(
        zevria_instructions::DirectivePayload::SkillRevocation {
            name: "review".parse().unwrap(),
            reason: "disabled earlier".into(),
        },
    )
    .unwrap();
    request
        .input
        .insert(0, ModelRequestItem::DeveloperInstruction(&system));
    run_turn_request(request, &mut openai, &mut state, &discard_updates())
        .await
        .expect("request should complete");

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn changed_mode_tools_and_instructions_force_full_replay() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let plan = receive_json(&mut socket).await;
        assert!(plan.get("previous_response_id").is_none());
        assert_eq!(plan["model"], "gpt-fixed");
        assert_request_policy(&plan, "Plan provider instructions", &["command"]);
        send_json(
            &mut socket,
            completed_event("resp_plan", "msg_plan", "approved plan"),
        )
        .await;

        let build = receive_json(&mut socket).await;
        assert!(build.get("previous_response_id").is_none());
        assert_eq!(build["model"], "gpt-fixed");
        assert_request_policy(&build, "Build provider instructions", &["command", "write"]);
        let build_wire = build.to_string();
        assert!(build_wire.contains("Implement the approved plan."));
        assert!(build_wire.contains("plan this change"));
        assert!(build_wire.contains("approved plan"));
        send_json(
            &mut socket,
            completed_event("resp_build", "msg_build", "implemented"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let tools = policy_tools();
    let mut openai = test_session(socket, None);
    openai.tools = tools;
    openai.model.model = "gpt-fixed".to_string();
    let mut state = AttemptState::default();

    let plan = RequestParts {
        prompt: Message::user("plan this change"),
        history: Vec::new(),
        instructions: "Plan provider instructions".to_string(),
        allowed_tool_names: Some(vec!["command".to_string()]),
    };
    run_turn_request(
        plan.request_with_role(ModelRole::Plan),
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("Plan request should complete");

    let build = RequestParts {
        prompt: Message::user("Implement the approved plan."),
        history: state.history.clone(),
        instructions: "Build provider instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request(
        build.request_with_role(ModelRole::Build),
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("Build request should complete on the same response chain");

    assert_eq!(state.history.len(), 4);
    assert_eq!(continuation_response_id(&openai), Some("resp_build"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn guidance_system_components_preserve_continuation_without_duplicate_prefix() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let first = receive_json(&mut socket).await;
        assert_eq!(
            first["instructions"]
                .to_string()
                .matches("GLOBAL_OPENING_BODY")
                .count(),
            1
        );
        assert_eq!(
            first["instructions"]
                .to_string()
                .matches("PROJECT_OPENING_BODY")
                .count(),
            1
        );
        assert!(!first["input"].to_string().contains("OPENING_BODY"));
        send_json(
            &mut socket,
            completed_event("guidance-first", "m1", "first answer"),
        )
        .await;
        let next = receive_json(&mut socket).await;
        assert_eq!(next["previous_response_id"], "guidance-first");
        assert_eq!(next["instructions"], first["instructions"]);
        assert!(!next["input"].to_string().contains("OPENING_BODY"));
        assert!(next["input"].to_string().contains("next prompt"));
        send_json(
            &mut socket,
            completed_event("guidance-next", "m2", "next answer"),
        )
        .await;
    });
    let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    let mut set = test_instruction_set("Build overlay");
    set.system = [
        (
            zevria_instructions::GLOBAL_GUIDANCE_COMPONENT,
            "GLOBAL_OPENING_BODY",
        ),
        (
            zevria_instructions::PROJECT_GUIDANCE_COMPONENT,
            "PROJECT_OPENING_BODY",
        ),
    ]
    .map(|(component, text)| (component.into(), text.into()))
    .into();
    let instructions = set.render();
    let first = RequestParts {
        prompt: Message::user("first prompt"),
        history: Vec::new(),
        instructions: "Build overlay".into(),
        allowed_tool_names: None,
    };
    let mut request = first.request();
    request.instructions = &instructions;
    run_turn_request(request, &mut openai, &mut state, &discard_updates())
        .await
        .unwrap();
    let history = state.history.clone();
    let replays = state.history_replays.clone();
    let prompt = Message::user("next prompt");
    let mut request = request_with_replays(&history, &replays, &prompt, "Build overlay", None);
    request.instructions = &instructions;
    run_turn_request(request, &mut openai, &mut state, &discard_updates())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn active_skill_change_continues_incrementally_on_the_very_next_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        assert!(
            !first["instructions"]
                .as_str()
                .unwrap()
                .contains("Active review body")
        );
        send_json(
            &mut socket,
            completed_event("resp_skill_1", "msg_skill_1", "first answer"),
        )
        .await;

        let activated = receive_json(&mut socket).await;
        assert_eq!(activated["previous_response_id"], "resp_skill_1");
        assert_eq!(activated["instructions"], first["instructions"]);
        let activated_wire = activated["input"].to_string();
        assert_eq!(activated_wire.matches("Active review body").count(), 1);
        assert!(!activated_wire.contains("first prompt"));
        assert!(!activated_wire.contains("first answer"));
        send_json(
            &mut socket,
            completed_event("resp_skill_2", "msg_skill_2", "second answer"),
        )
        .await;

        let stable = receive_json(&mut socket).await;
        assert_eq!(stable["previous_response_id"], "resp_skill_2");
        assert_eq!(stable["instructions"], first["instructions"]);
        assert!(!stable.to_string().contains("Active review body"));
        let input = stable["input"].as_array().expect("incremental input");
        assert!(
            input
                .iter()
                .any(|item| item.to_string().contains("third prompt"))
        );
        assert!(!stable.to_string().contains("first prompt"));
        send_json(
            &mut socket,
            completed_event("resp_skill_3", "msg_skill_3", "third answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();

    let first = RequestParts {
        prompt: Message::user("first prompt"),
        history: Vec::new(),
        instructions: "Build overlay".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request(first.request(), &mut openai, &mut state, &discard_updates())
        .await
        .expect("first request");

    let second_prompt = Message::user("second prompt");
    let second_history = state.history.clone();
    let second_replays = state.history_replays.clone();
    run_turn_request(
        request_with_replays(
            &second_history,
            &second_replays,
            &second_prompt,
            "Build overlay",
            Some("Active review body"),
        ),
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("activation replay");

    let third_prompt = Message::user("third prompt");
    let third_history = state.history.clone();
    let third_replays = state.history_replays.clone();
    let mut third = request_with_replays(
        &third_history,
        &third_replays,
        &third_prompt,
        "Build overlay",
        None,
    );
    // The activation body remains at its original position, before the second response.
    third.input.insert(
        3,
        ModelRequestItem::DeveloperInstruction(test_skill("Active review body")),
    );
    run_turn_request(third, &mut openai, &mut state, &discard_updates())
        .await
        .expect("stable incremental continuation");

    assert_eq!(continuation_response_id(&openai), Some("resp_skill_3"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn changed_history_and_model_properties_force_full_replay() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_history", "msg_history", "first answer"),
        )
        .await;

        let edited = receive_json(&mut socket).await;
        assert!(edited.get("previous_response_id").is_none());
        let edited_wire = edited.to_string();
        assert!(edited_wire.contains("edited first question"));
        assert!(edited_wire.contains("first answer"));
        assert!(edited_wire.contains("second question"));
        send_json(
            &mut socket,
            completed_event("resp_edited", "msg_edited", "second answer"),
        )
        .await;

        let changed_model = receive_json(&mut socket).await;
        assert!(changed_model.get("previous_response_id").is_none());
        assert_eq!(changed_model["model"], "gpt-test-changed");
        let changed_wire = changed_model.to_string();
        assert!(changed_wire.contains("edited first question"));
        assert!(changed_wire.contains("second answer"));
        assert!(changed_wire.contains("third question"));
        send_json(
            &mut socket,
            completed_event("resp_model", "msg_model", "third answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn(
        "first question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("first turn should complete");

    state.history[0] = Message::user("edited first question");
    run_turn(
        "second question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("edited history should replay in full");

    openai.model.model = "gpt-test-changed".to_string();
    run_turn(
        "third question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("changed model should replay in full");

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn run_turn_chains_incrementally_and_ignores_late_done() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let first = receive_json(&mut socket).await;
        assert_eq!(first["type"], "response.create");
        assert!(first.get("previous_response_id").is_none());
        assert!(first.to_string().contains("first question"));
        send_json(
            &mut socket,
            completed_event("resp_1", "msg_1", "first answer"),
        )
        .await;

        let second = receive_json(&mut socket).await;
        assert_eq!(second["previous_response_id"], "resp_1");
        let second_wire = second.to_string();
        assert!(second_wire.contains("second question"));
        assert!(!second_wire.contains("first question"));
        assert!(!second_wire.contains("first answer"));

        send_json(&mut socket, done_event("resp_1", "completed")).await;
        send_json(&mut socket, output_text_delta("second answer", 2)).await;
        send_json(
            &mut socket,
            completed_event("resp_2", "msg_test", "second answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();

    run_turn(
        "first question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("first turn should complete");
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_1".to_string()),
            content: vec![AssistantContent::text("first answer")],
        }
    );
    assert_eq!(state.history.len(), 2);
    assert_eq!(continuation_response_id(&openai), Some("resp_1"));
    assert_eq!(
        openai.ws.pending_done_response_id.as_deref(),
        Some("resp_1")
    );

    run_turn(
        "second question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("second turn should complete");
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_test".to_string()),
            content: vec![AssistantContent::text("second answer")],
        }
    );
    assert_eq!(state.history.len(), 4);
    assert_eq!(continuation_response_id(&openai), Some("resp_2"));
    assert_eq!(
        openai.ws.pending_done_response_id.as_deref(),
        Some("resp_2")
    );

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn native_output_items_survive_capture_and_full_replay_after_reconnect() {
    let arguments = r#"{"path": "x", "content": "y\n\"z\"", "n": 1.0}"#;
    let native_output = vec![
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "first summary"}],
            "content": [{"type": "reasoning_text", "text": "first private text"}],
            "encrypted_content": "encrypted-one",
            "status": null,
            "future_reasoning_field": {"kept": 1}
        }),
        json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "write",
            "arguments": arguments,
            "status": "completed",
            "future_call_field": [null, "one"]
        }),
        json!({
            "type": "reasoning",
            "id": "rs_2",
            "summary": [{"type": "summary_text", "text": "second summary"}],
            "content": [],
            "encrypted_content": "encrypted-two",
            "status": null,
            "future_reasoning_field": {"kept": 2}
        }),
        json!({
            "type": "function_call",
            "id": "fc_2",
            "call_id": "call_2",
            "name": "write",
            "arguments": r#"{"path": "second", "content": "value", "n": 2.0}"#,
            "status": "completed"
        }),
        json!({
            "type": "message",
            "id": "msg_native",
            "role": "assistant",
            "status": "completed",
            "content": [
                {"type": "output_text", "text": "first block"},
                {"type": "output_text", "text": "second block", "future_text_field": true}
            ],
            "future_message_field": {"unknown": "survives"}
        }),
    ];

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server_output = native_output.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let first = receive_json(&mut socket).await;
        assert_eq!(first["store"], false);
        assert!(first.get("previous_response_id").is_none());
        let first_input = first["input"].as_array().expect("first input").clone();
        send_json(
            &mut socket,
            completed_output_event("resp_native", server_output.clone()),
        )
        .await;

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade reconnect");
        let replayed = receive_json(&mut socket).await;
        assert_eq!(replayed["store"], false);
        assert_eq!(first["prompt_cache_key"], "test-session");
        assert_eq!(replayed["prompt_cache_key"], first["prompt_cache_key"]);
        assert_eq!(
            wire_request_properties(&replayed),
            wire_request_properties(&first)
        );
        assert!(replayed.get("previous_response_id").is_none());
        let replayed_input = replayed["input"].as_array().expect("replayed input");
        assert_eq!(&replayed_input[..first_input.len()], first_input.as_slice());
        assert_eq!(
            &replayed_input[first_input.len()..first_input.len() + server_output.len()],
            server_output.as_slice()
        );
        assert!(
            replayed_input
                .last()
                .unwrap()
                .to_string()
                .contains("next question")
        );
        let replayed_input = replayed_input.clone();
        send_json(
            &mut socket,
            completed_event("resp_after_reconnect", "msg_after", "done"),
        )
        .await;

        let (stream, _) = listener
            .accept()
            .await
            .expect("server should accept resume");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade resumed connection");
        let resumed = receive_json(&mut socket).await;
        assert_eq!(resumed["prompt_cache_key"], first["prompt_cache_key"]);
        assert_eq!(
            wire_request_properties(&resumed),
            wire_request_properties(&first)
        );
        assert!(resumed.get("previous_response_id").is_none());
        assert_eq!(
            resumed["input"].as_array().expect("resumed input"),
            &replayed_input,
            "transcript resume and in-memory reconnect must build the same full input"
        );
        send_json(
            &mut socket,
            completed_event("resp_after_resume", "msg_resume", "resumed"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    openai.compatibility.send_prompt_cache_key = true;
    openai.responses_parameters = Some(json!({"prompt_cache_key":"test-session"}));
    let mut state = AttemptState::default();
    run_turn(
        "first question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("first response should complete");

    let replay = state.history_replays[1]
        .as_ref()
        .expect("native replay should be retained")
        .replay();
    assert_eq!(
        state.history[1],
        replay.to_message().expect("captured replay should convert"),
        "the committed message must be derived from the captured replay"
    );
    assert_eq!(replay.items, native_output);
    assert_eq!(replay.items[1]["arguments"], arguments);
    assert_eq!(replay.items[4]["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        replay.items[4]["future_message_field"]["unknown"],
        "survives"
    );

    let transcript_directory = tempfile::tempdir().expect("temporary transcript directory");
    let mut writer = TranscriptWriter::create(transcript_directory.path())
        .expect("transcript writer should create");
    writer
        .append(&TranscriptItem::Message(Message::user("first question")))
        .expect("user message should persist");
    writer
        .append(
            &TranscriptItem::provider_message(replay.clone())
                .expect("native replay should derive its provider message"),
        )
        .expect("provider message should persist");
    let loaded = transcript::load(writer.path()).expect("transcript should resume");

    openai
        .ws
        .reconnect()
        .await
        .expect("reconnect should succeed");
    run_turn("next question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("full replay on the replacement socket should complete");

    let (resumed_socket, _) = connect_async(&url)
        .await
        .expect("resumed provider should connect");
    let mut resumed_openai = test_session_with_url(&url, resumed_socket, None);
    resumed_openai.compatibility.send_prompt_cache_key = true;
    resumed_openai.responses_parameters = Some(json!({"prompt_cache_key":"test-session"}));
    let resumed_prompt = Message::user("next question");
    let mut resumed_input = loaded
        .iter()
        .filter_map(TranscriptItem::model_request_item)
        .collect::<Vec<_>>();
    resumed_input.push(ModelRequestItem::message(&resumed_prompt));
    let resumed_request = ModelRequest {
        instructions: test_instructions(),
        input: resumed_input,
        model_role: ModelRole::Build,
        allowed_tool_names: None,
    };
    let mut resumed_state = AttemptState::default();
    run_turn_request(
        resumed_request,
        &mut resumed_openai,
        &mut resumed_state,
        &discard_updates(),
    )
    .await
    .expect("transcript replay should complete");

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn a_completed_response_with_usage_reports_it_once() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event_with_usage("resp_usage", "msg_usage", "answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let (events, mut receiver) = session_event_channel(256);
    let request = RequestParts {
        prompt: Message::user("question"),
        history: Vec::new(),
        instructions: "Test turn instructions".to_string(),
        allowed_tool_names: None,
    };

    let response = openai
        .complete(request.request(), ProgressReporter::new(events))
        .await
        .expect("turn should complete");

    assert_eq!(response.usage, Some(expected_token_usage()));
    assert_eq!(usage_events(&mut receiver), [expected_token_usage()]);
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn a_done_completion_with_usage_reports_it_once() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        send_json(&mut socket, output_text_delta("draft", 1)).await;
        // The `response.done` completion path parses the full response
        // payload, so the fixture rides in a done wrapper.
        let mut done = completed_event_with_usage("resp_done_usage", "msg_done_usage", "answer");
        done["type"] = json!("response.done");
        send_json(&mut socket, done).await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    let (events, mut receiver) = session_event_channel(256);

    run_turn(
        "question",
        &mut openai,
        &mut state,
        &ProgressReporter::new(events),
    )
    .await
    .expect("turn should complete");

    assert_eq!(usage_events(&mut receiver), [expected_token_usage()]);
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_done_usage".to_string()),
            content: vec![AssistantContent::text("answer")],
        }
    );
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn dropped_connection_reads_as_a_disconnect_error() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        // Drop the socket without a close handshake, mimicking a proxy that
        // resets an idle connection.
        drop(socket);
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();

    let error = run_turn("question", &mut openai, &mut state, &discard_updates())
        .await
        .expect_err("a reset connection should fail the turn");
    assert!(
        is_websocket_disconnect(&error),
        "expected a disconnect error, got: {error}"
    );

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn provider_reconnects_and_replays_prompt_after_idle_drop() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        // First connection: accept the request, then reset without a close
        // frame — the stale idle socket the next turn will trip over.
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let dropped = receive_json(&mut socket).await;
        assert!(dropped.to_string().contains("idle question"));
        assert_request_policy(&dropped, "Plan reconnect instructions", &["command"]);
        drop(socket);

        // Second connection: the provider should reconnect here and replay the
        // same prompt, which now completes normally.
        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade the reconnect");
        let replayed = receive_json(&mut socket).await;
        let replayed_wire = replayed.to_string();
        assert!(replayed_wire.contains("idle question"));
        assert_request_policy(&replayed, "Plan reconnect instructions", &["command"]);
        assert!(replayed.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_reconnect", "msg_reconnect", "recovered answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    openai.tools = policy_tools();
    let mut state = AttemptState::default();

    let parts = RequestParts {
        prompt: Message::user("idle question"),
        history: Vec::new(),
        instructions: "Plan reconnect instructions".to_string(),
        allowed_tool_names: Some(vec!["command".to_string()]),
    };
    run_turn_request_with_reconnect(parts.request(), &mut openai, &mut state, &discard_updates())
        .await
        .expect("the turn should recover after reconnecting");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_reconnect".to_string()),
            content: vec![AssistantContent::text("recovered answer")],
        }
    );
    assert_eq!(state.history.len(), 2);
    assert_eq!(continuation_response_id(&openai), Some("resp_reconnect"));

    server.await.expect("server task should finish");
}

#[test]
fn upstream_disconnect_matcher_targets_transport_failures_only() {
    for message in [
        "HTTP 200 websocket: close 1006 (abnormal closure): unexpected EOF",
        "websocket: close 1006 (abnormal closure): unexpected EOF",
        "upstream read failed: unexpected EOF",
        "Connection reset by peer",
        // The exact failures observed in a real relay session.
        "The OpenAI websocket connection closed upstream requires HTTP replay",
        "The OpenAI websocket connection was lost while reading the response: \
         WebSocket protocol error: Connection reset without closing handshake",
        "IO error: peer closed connection without sending TLS close_notify: \
         https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof",
        "upstream request timed out",
        "write: broken pipe",
    ] {
        assert!(
            message_reports_upstream_disconnect(message),
            "should classify as a transport failure: {message}"
        );
    }
    for message in [
        "context_too_large: Your input exceeds the context window of this model.",
        "previous_response_not_found: response is no longer cached",
        "invalid_api_key: Incorrect API key provided",
    ] {
        assert!(
            !message_reports_upstream_disconnect(message),
            "should classify as a rejection: {message}"
        );
    }
}

#[tokio::test]
async fn relayed_upstream_disconnect_reconnects_and_replays_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        // First connection: the relay accepts the request but its own upstream
        // websocket dies, which it reports in-band as an error event.
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "message": "HTTP 200 websocket: close 1006 (abnormal closure): unexpected EOF"
                }
            }),
        )
        .await;

        // Second connection: the provider reconnects and replays the complete
        // local request without carrying socket-scoped continuation state.
        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade the reconnect");
        let replayed = receive_json(&mut socket).await;
        assert!(replayed.get("previous_response_id").is_none());
        assert!(replayed.to_string().contains("relayed question"));
        send_json(
            &mut socket,
            completed_event("resp_relayed", "msg_relayed", "recovered answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();

    let parts = RequestParts {
        prompt: Message::user("relayed question"),
        history: Vec::new(),
        instructions: "Build provider test instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_reconnect(parts.request(), &mut openai, &mut state, &discard_updates())
        .await
        .expect("the turn should recover after the relayed upstream failure");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_relayed".to_string()),
            content: vec![AssistantContent::text("recovered answer")],
        }
    );
    assert_eq!(continuation_response_id(&openai), Some("resp_relayed"));

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn close_frame_with_transport_reason_reconnects_and_replays_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        // First connection: the relay mirrors its upstream failure as a close
        // frame instead of an error event.
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        socket
            .close(Some(CloseFrame {
                code: CloseCode::Error,
                reason: "websocket: close 1006 (abnormal closure): unexpected EOF".into(),
            }))
            .await
            .expect("server close frame should send");

        // Second connection: the provider reconnects and replays in full.
        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade the reconnect");
        let replayed = receive_json(&mut socket).await;
        assert!(replayed.get("previous_response_id").is_none());
        assert!(replayed.to_string().contains("closed question"));
        send_json(
            &mut socket,
            completed_event("resp_closed", "msg_closed", "recovered answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();

    let parts = RequestParts {
        prompt: Message::user("closed question"),
        history: Vec::new(),
        instructions: "Build provider test instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_reconnect(parts.request(), &mut openai, &mut state, &discard_updates())
        .await
        .expect("the turn should recover after the transport-reason close");

    assert_eq!(continuation_response_id(&openai), Some("resp_closed"));

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn reconnect_replays_full_history_without_a_stale_id_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        // First connection: dies mid-request.
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        assert!(first.to_string().contains("old question"));
        drop(socket);

        // Second connection: recovery immediately sends complete local
        // history, with no stale continuation ID to reject first.
        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade the reconnect");
        let replayed = receive_json(&mut socket).await;
        assert!(replayed.get("previous_response_id").is_none());
        let replayed_wire = replayed.to_string();
        assert!(replayed_wire.contains("old question"));
        assert!(replayed_wire.contains("old answer"));
        assert!(replayed_wire.contains("new question"));
        send_json(
            &mut socket,
            completed_event("resp_recovered", "msg_recovered", "recovered answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState {
        history: vec![
            Message::user("old question"),
            Message::assistant("old answer"),
        ],
        ..Default::default()
    };

    let parts = RequestParts {
        prompt: Message::user("new question"),
        history: state.history.clone(),
        instructions: "Build provider test instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_reconnect(parts.request(), &mut openai, &mut state, &discard_updates())
        .await
        .expect("the turn should recover through the reconnect and the fallback");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_recovered".to_string()),
            content: vec![AssistantContent::text("recovered answer")],
        }
    );
    assert_eq!(continuation_response_id(&openai), Some("resp_recovered"));

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn provider_rejection_event_fails_the_turn_without_reconnecting() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "code": "context_too_large",
                    "message": "Your input exceeds the context window of this model."
                }
            }),
        )
        .await;
        // No second accept: a genuine rejection must not trigger a reconnect.
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();

    let parts = RequestParts {
        prompt: Message::user("oversized question"),
        history: Vec::new(),
        instructions: "Build provider test instructions".to_string(),
        allowed_tool_names: None,
    };
    let error = run_turn_request_with_reconnect(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect_err("a provider rejection should fail the turn");

    assert!(
        !is_websocket_disconnect(&error),
        "a rejection must not be classified as a disconnect: {error}"
    );
    assert!(error.to_string().contains("context_too_large"));
    assert!(state.result.is_empty());
    assert!(openai.ws.continuation.is_none());

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn run_turn_retries_missing_response_id_with_full_history_once() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let initial = receive_json(&mut socket).await;
        assert!(initial.get("previous_response_id").is_none());
        assert_eq!(initial["gateway_routing"], "stable-route");
        assert!(initial.to_string().contains("old question"));
        send_json(
            &mut socket,
            completed_event("resp_stale", "msg_stale", "old answer"),
        )
        .await;

        let chained = receive_json(&mut socket).await;
        assert_eq!(chained["previous_response_id"], "resp_stale");
        assert_eq!(chained["gateway_routing"], "stable-route");
        let chained_wire = chained.to_string();
        assert!(chained_wire.contains("new question"));
        assert!(!chained_wire.contains("old question"));
        assert!(!chained_wire.contains("old answer"));

        send_json(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "code": "previous_response_not_found",
                    "message": "response is no longer cached"
                }
            }),
        )
        .await;

        let replayed = receive_json(&mut socket).await;
        assert!(replayed.get("previous_response_id").is_none());
        assert_eq!(replayed["gateway_routing"], "stable-route");
        let replayed_wire = replayed.to_string();
        assert!(replayed_wire.contains("old question"));
        assert!(replayed_wire.contains("old answer"));
        assert!(replayed_wire.contains("new question"));

        send_json(
            &mut socket,
            completed_event("resp_fresh", "msg_2", "recovered answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    openai
        .additional_params
        .insert("gateway_routing".to_string(), json!("stable-route"));
    let mut state = AttemptState::default();
    run_turn("old question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("the initial turn should establish a live continuation");
    run_turn("new question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("fallback turn should complete");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_2".to_string()),
            content: vec![AssistantContent::text("recovered answer")],
        }
    );
    assert_eq!(state.history.len(), 4);
    assert_eq!(continuation_response_id(&openai), Some("resp_fresh"));
    assert_eq!(
        openai.ws.pending_done_response_id.as_deref(),
        Some("resp_fresh")
    );
    let history = serde_json::to_string(&state.history).expect("history should serialize");
    assert_eq!(history.matches("new question").count(), 1);
    assert_eq!(history.matches("recovered answer").count(), 1);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn a_stale_chain_reported_without_the_dedicated_code_still_falls_back() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let initial = receive_json(&mut socket).await;
        assert!(initial.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_stale", "msg_stale", "old answer"),
        )
        .await;

        let chained = receive_json(&mut socket).await;
        assert_eq!(chained["previous_response_id"], "resp_stale");
        // A relay reporting the lost chain under a generic code; only the
        // message names the condition.
        send_json(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "code": "invalid_request_error",
                    "message": "The response is no longer cached upstream"
                }
            }),
        )
        .await;

        let replayed = receive_json(&mut socket).await;
        assert!(replayed.get("previous_response_id").is_none());
        let replayed_wire = replayed.to_string();
        assert!(replayed_wire.contains("old question"));
        assert!(replayed_wire.contains("new question"));
        send_json(
            &mut socket,
            completed_event("resp_fresh", "msg_fresh", "recovered answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn("old question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("the initial turn should establish a live continuation");
    run_turn("new question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("the message-detected fallback should complete");

    assert_eq!(continuation_response_id(&openai), Some("resp_fresh"));
    assert_eq!(state.history.len(), 4);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn run_turn_discards_partial_result_on_failed_done() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let initial = receive_json(&mut socket).await;
        assert!(initial.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_old", "msg_old", "answer"),
        )
        .await;

        let failing = receive_json(&mut socket).await;
        assert_eq!(failing["previous_response_id"], "resp_old");
        send_json(&mut socket, output_text_delta("partial", 1)).await;
        send_json(&mut socket, done_event("resp_failed", "failed")).await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn("old", &mut openai, &mut state, &discard_updates())
        .await
        .expect("initial response should establish continuation");
    assert_eq!(continuation_response_id(&openai), Some("resp_old"));

    let error = run_turn(
        "failing question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect_err("failed response should fail the turn");

    assert!(error.to_string().contains("Failed"));
    assert!(state.result.is_empty());
    assert_eq!(state.history.len(), 2);
    assert!(openai.ws.continuation.is_none());
    assert_eq!(openai.ws.pending_done_response_id, None);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn missing_native_output_fails_without_committing_or_continuing() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let _initial = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_old", "msg_old", "old answer"),
        )
        .await;

        let chained = receive_json(&mut socket).await;
        assert_eq!(chained["previous_response_id"], "resp_old");
        send_json(&mut socket, output_text_delta("preview only", 1)).await;
        send_json(
            &mut socket,
            json!({
                "type": "response.done",
                "response": {
                    "id": "resp_missing_native",
                    "status": "completed"
                }
            }),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn("old question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("initial response should establish continuation");

    let error = run_turn("new question", &mut openai, &mut state, &discard_updates())
        .await
        .expect_err("missing native output must fail the turn");

    assert!(error.to_string().contains("without complete native output"));
    assert_eq!(
        state.history.len(),
        2,
        "no new prompt or assistant was committed"
    );
    assert!(openai.ws.continuation.is_none());
    assert_eq!(openai.ws.pending_done_response_id, None);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn unconvertible_native_output_fails_without_committing_or_continuing() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let _initial = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_old", "msg_old", "old answer"),
        )
        .await;

        let chained = receive_json(&mut socket).await;
        assert_eq!(chained["previous_response_id"], "resp_old");
        send_json(&mut socket, output_text_delta("preview only", 1)).await;
        send_json(
            &mut socket,
            json!({
                "type": "response.done",
                "response": {
                    "id": "resp_invalid_native",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_invalid",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": 7}]
                    }]
                }
            }),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn("old question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("initial response should establish continuation");

    let error = run_turn("new question", &mut openai, &mut state, &discard_updates())
        .await
        .expect_err("unconvertible native output must fail the turn");

    assert!(
        error
            .to_string()
            .contains("failed to convert captured OpenAI output")
    );
    assert_eq!(
        state.history.len(),
        2,
        "no new prompt or assistant was committed"
    );
    assert!(openai.ws.continuation.is_none());
    assert_eq!(openai.ws.pending_done_response_id, None);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn engine_reset_invalidates_a_live_continuation() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_reset", "msg_reset", "answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn("question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("turn should establish continuation");
    assert_eq!(continuation_response_id(&openai), Some("resp_reset"));
    assert_eq!(
        openai.ws.pending_done_response_id.as_deref(),
        Some("resp_reset")
    );

    <OpenAiProvider as zevria_session_api::ModelProvider>::reset(&mut openai);
    assert!(openai.ws.continuation.is_none());
    assert_eq!(openai.ws.pending_done_response_id, None);

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn run_turn_rejects_completed_done_without_response_id() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let _request = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            json!({
                "type": "response.done",
                "response": {
                    "status": "completed"
                }
            }),
        )
        .await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();

    let error = run_turn("question", &mut openai, &mut state, &discard_updates())
        .await
        .expect_err("response.done without an ID should fail the turn");

    assert!(error.to_string().contains("did not include a response ID"));
    assert!(state.result.is_empty());
    assert!(openai.ws.continuation.is_none());
    assert_eq!(openai.ws.pending_done_response_id, None);

    server.await.expect("server task should finish");
}

#[derive(Deserialize)]
struct CountArgs {}

struct CountTool {
    executions: Arc<AtomicUsize>,
}

impl Tool for CountTool {
    const NAME: &'static str = "count_once";
    type Error = std::convert::Infallible;
    type Args = CountArgs;
    type Output = String;

    fn description(&self) -> String {
        "increment a test counter".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("counted".to_string())
    }
}

#[tokio::test]
async fn reconnecting_a_continuation_does_not_execute_its_tool_twice() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let initial = receive_json(&mut socket).await;
        assert_request_policy(&initial, "Plan continuation instructions", &["count_once"]);
        let advertised = initial["tools"]
            .as_array()
            .expect("request should advertise tools");
        assert_eq!(advertised.len(), 1);
        assert_eq!(advertised[0]["name"], "count_once");
        assert_eq!(advertised[0]["strict"], true);
        send_json(
            &mut socket,
            completed_output_event(
                "resp_tool",
                vec![function_call(
                    "fc_once",
                    "call_once",
                    "count_once",
                    json!({}),
                )],
            ),
        )
        .await;

        let continuation = receive_json(&mut socket).await;
        assert_request_policy(
            &continuation,
            "Plan continuation instructions",
            &["count_once"],
        );
        assert_eq!(continuation["previous_response_id"], "resp_tool");
        assert!(continuation.to_string().contains("counted"));
        drop(socket);

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade reconnect");
        let replayed = receive_json(&mut socket).await;
        assert_request_policy(&replayed, "Plan continuation instructions", &["count_once"]);
        assert!(replayed.get("previous_response_id").is_none());
        let replayed = replayed.to_string();
        assert!(replayed.contains("call_once"));
        assert!(replayed.contains("counted"));
        assert!(replayed.contains("run it once"));
        send_json(
            &mut socket,
            completed_event("resp_final", "msg_final", "finished once"),
        )
        .await;
    });

    let executions = Arc::new(AtomicUsize::new(0));
    let tools = ToolServer::new()
        .tool(CountTool {
            executions: executions.clone(),
        })
        .run();
    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let responses_url = OpenAiResponseEndpoints::parse(&url)
        .expect("test endpoint")
        .http;
    let openai = OpenAiProvider {
        #[cfg(feature = "cache-diagnostics")]
        cache_diagnostics: Default::default(),
        profile: test_profile_ref(),
        model: test_model(),
        context_window_tokens: 272_000,
        input_token_limit: 272_000,
        preamble: "Test instructions".to_string(),
        responses_parameters: None,
        reasoning_level: ReasoningEffort::Medium,
        reasoning_summary_level: ReasoningSummaryLevel::Detailed,
        web_search: WebSearchConfig::default(),
        compatibility: ResponsesCompatibilityConfig {
            send_reasoning: false,
            send_prompt_cache_key: false,
            ..ResponsesCompatibilityConfig::default()
        },
        additional_params: Default::default(),
        prompt_cache_key: "test-session".to_string(),
        tools: tools.clone(),
        ws: OpenAiParkedWebSocket {
            session: OpenAiWebSocketSession::new(socket),
            config: OpenAiWebSocketConfig {
                url: url.clone(),
                headers: HeaderMap::new(),
            },
            socket_generation: 0,
            websocket_connection_request_id: None,
            continuation: None,
            pending_done_response_id: None,
            last_activity: std::time::Instant::now(),
        },
        responses_url,
        transport: OpenAiTransport::WebSocket,
        compaction_url: None,
        compaction_timeout: std::time::Duration::from_secs(300),
        input_token_count_url: None,
        input_token_count_timeout: std::time::Duration::from_secs(30),
        input_token_count_unsupported: false,
        api_key: "test-key".to_string(),
        http: reqwest::Client::new(),
    };
    let transcript_directory = tempfile::tempdir().expect("temporary transcript directory");
    let transcript =
        TranscriptWriter::create(transcript_directory.path()).expect("transcript writer");
    let mut engine = SessionEngine::new(
        openai,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("engine should build");
    let (event_tx, mut event_rx) = session_event_channel(256);

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        engine.handle_command(
            SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                behavior: zevria_foundation::RequestBehavior::Standard,
                text: "run it once".into(),
                mode: SessionMode::Plan,
            }),
            &event_tx,
        ),
    )
    .await
    .expect("scripted reconnect and continuation must finish")
    .expect("valid replay");

    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        std::iter::from_fn(|| event_rx.try_recv().ok()).any(|update| {
            matches!(
                update,
                SessionUpdate::Lifecycle(SessionEvent::TurnCompleted { .. })
            )
        })
    );
    assert_eq!(
        engine.history().last(),
        Some(&Message::Assistant {
            id: Some("msg_final".to_string()),
            content: vec![AssistantContent::text("finished once")],
        })
    );
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn chained_continuations_resend_byte_identical_instructions() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        assert_eq!(
            first["instructions"],
            rendered_test_instructions("Build provider instructions")
        );
        let first_input = first["input"].to_string();
        assert!(first_input.contains("keep going"));
        send_json(&mut socket, completed_event("resp_1", "msg_1", "working")).await;

        // The chained continuation sends only the new prompt — unmodified,
        // with the instructions byte-identical to the first request, so the
        // cacheable prefix is never invalidated by per-request state.
        let second = receive_json(&mut socket).await;
        assert_eq!(second["previous_response_id"], "resp_1");
        assert_eq!(second["instructions"], first["instructions"]);
        let second_input = second["input"].to_string();
        assert!(second_input.contains("keep going"));
        assert!(
            !second_input.contains("runtime-context"),
            "prompts must carry no appended per-request context"
        );
        send_json(&mut socket, completed_event("resp_2", "msg_2", "done")).await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();

    for _ in 0..2 {
        let parts = RequestParts {
            prompt: Message::user("keep going"),
            history: state.history.clone(),
            instructions: "Build provider instructions".to_string(),
            allowed_tool_names: None,
        };
        let replays = state.history_replays.clone();
        run_turn_request(
            request_with_replays(
                &parts.history,
                &replays,
                &parts.prompt,
                &parts.instructions,
                None,
            ),
            &mut openai,
            &mut state,
            &discard_updates(),
        )
        .await
        .expect("request should complete");
    }

    assert_eq!(continuation_response_id(&openai), Some("resp_2"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn factory_connections_run_independent_response_chains() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (root_stream, _) = listener.accept().await.expect("root should be accepted");
        let mut root_socket = accept_hdr_async(root_stream, AssertOpenAiWebSocketHandshake)
            .await
            .expect("root websocket should upgrade");
        let (child_stream, _) = listener.accept().await.expect("child should be accepted");
        let mut child_socket = accept_hdr_async(child_stream, AssertOpenAiWebSocketHandshake)
            .await
            .expect("child websocket should upgrade");

        let root_first = receive_json(&mut root_socket).await;
        assert!(root_first.get("previous_response_id").is_none());
        send_json(
            &mut root_socket,
            completed_event("resp_root_1", "msg_root_1", "root answer"),
        )
        .await;

        // The child connection starts its own chain even though the root
        // already produced a response.
        let child_first = receive_json(&mut child_socket).await;
        assert!(child_first.get("previous_response_id").is_none());
        send_json(
            &mut child_socket,
            completed_event("resp_child_1", "msg_child_1", "child answer"),
        )
        .await;

        // The root keeps chaining onto its own responses, never the child's.
        let root_second = receive_json(&mut root_socket).await;
        assert_eq!(root_second["previous_response_id"], "resp_root_1");
        assert!(!root_second.to_string().contains("child answer"));
        send_json(
            &mut root_socket,
            completed_event("resp_root_2", "msg_root_2", "root follow-up"),
        )
        .await;
    });

    let factory = TestProviderFactory::new(
        TestProviderConfig {
            base_url: format!("ws://{address}"),
            api_key: "test-key".to_string(),
            models: test_models(),
            supports_websockets: true,
            reasoning: TestReasoningConfig::default(),
            compatibility: ResponsesCompatibilityConfig::default(),
            additional_params: Default::default(),
            compaction: RemoteCompactionConfig::default(),
        },
        "Test instructions",
        ToolServer::new().run(),
    );
    let mut root = factory
        .connect("root-session")
        .await
        .expect("root should connect");
    let mut child = factory
        .connect("child-session")
        .await
        .expect("child should connect");

    let mut root_state = AttemptState::default();
    let mut child_state = AttemptState::default();
    run_turn(
        "root question",
        &mut root,
        &mut root_state,
        &discard_updates(),
    )
    .await
    .expect("root turn should complete");
    run_turn(
        "child question",
        &mut child,
        &mut child_state,
        &discard_updates(),
    )
    .await
    .expect("child turn should complete");
    run_turn(
        "root follow-up",
        &mut root,
        &mut root_state,
        &discard_updates(),
    )
    .await
    .expect("chained root turn should complete");

    assert_eq!(continuation_response_id(&root), Some("resp_root_2"));
    assert_eq!(continuation_response_id(&child), Some("resp_child_1"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn disabled_cache_key_transmission_retains_distinct_provider_identities() {
    let factory = TestProviderFactory::new(
        TestProviderConfig {
            base_url: "http://127.0.0.1:1/v1/responses".to_string(),
            api_key: "test-key".to_string(),
            models: test_models(),
            supports_websockets: false,
            reasoning: TestReasoningConfig::default(),
            compatibility: ResponsesCompatibilityConfig {
                send_prompt_cache_key: false,
                ..ResponsesCompatibilityConfig::default()
            },
            additional_params: Default::default(),
            compaction: RemoteCompactionConfig::default(),
        },
        "Test instructions",
        ToolServer::new().run(),
    );
    let root = factory
        .connect("root-session-id")
        .await
        .expect("root provider");
    let child = factory
        .connect("child-subtask-id")
        .await
        .expect("child provider");

    assert_eq!(root.prompt_cache_key, "root-session-id");
    assert_eq!(child.prompt_cache_key, "child-subtask-id");
    assert_ne!(root.prompt_cache_key, child.prompt_cache_key);
    for provider in [&root, &child] {
        assert!(
            provider
                .prepared_responses_parameters()
                .and_then(|value| value.get("prompt_cache_key").cloned())
                .is_none()
        );
    }
}

#[tokio::test]
async fn every_request_carries_its_connections_prompt_cache_key() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = tokio::spawn(async move {
        let (root_stream, _) = listener.accept().await.expect("root should be accepted");
        let mut root_socket = accept_hdr_async(root_stream, AssertOpenAiWebSocketHandshake)
            .await
            .expect("root websocket should upgrade");
        let (child_stream, _) = listener.accept().await.expect("child should be accepted");
        let mut child_socket = accept_hdr_async(child_stream, AssertOpenAiWebSocketHandshake)
            .await
            .expect("child websocket should upgrade");

        let root_first = receive_json(&mut root_socket).await;
        assert_eq!(root_first["prompt_cache_key"], "root-session-id");
        assert_eq!(
            root_first["include"],
            json!(["reasoning.encrypted_content"])
        );
        send_json(
            &mut root_socket,
            completed_event("resp_root_1", "msg_root_1", "root answer"),
        )
        .await;

        // The child connection routes its requests under its own key.
        let child_first = receive_json(&mut child_socket).await;
        assert_eq!(child_first["prompt_cache_key"], "child-subtask-id");
        assert_eq!(
            child_first["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_ne!(
            root_first["prompt_cache_key"],
            child_first["prompt_cache_key"]
        );
        send_json(
            &mut child_socket,
            completed_event("resp_child_1", "msg_child_1", "child answer"),
        )
        .await;

        // The key also rides chained continuations.
        let root_second = receive_json(&mut root_socket).await;
        assert_eq!(root_second["prompt_cache_key"], "root-session-id");
        assert_eq!(
            root_second["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(root_second["previous_response_id"], "resp_root_1");
        send_json(
            &mut root_socket,
            completed_event("resp_root_2", "msg_root_2", "root follow-up"),
        )
        .await;
    });

    let factory = TestProviderFactory::new(
        TestProviderConfig {
            base_url: format!("ws://{address}"),
            api_key: "test-key".to_string(),
            models: test_models(),
            supports_websockets: true,
            reasoning: TestReasoningConfig::default(),
            compatibility: ResponsesCompatibilityConfig::default(),
            additional_params: Default::default(),
            compaction: RemoteCompactionConfig::default(),
        },
        "Test instructions",
        ToolServer::new().run(),
    );
    let mut root = factory
        .connect("root-session-id")
        .await
        .expect("root should connect");
    let mut child = factory
        .connect("child-subtask-id")
        .await
        .expect("child should connect");

    let mut root_state = AttemptState::default();
    let mut child_state = AttemptState::default();
    run_turn(
        "root question",
        &mut root,
        &mut root_state,
        &discard_updates(),
    )
    .await
    .expect("root turn should complete");
    run_turn(
        "child question",
        &mut child,
        &mut child_state,
        &discard_updates(),
    )
    .await
    .expect("child turn should complete");
    run_turn(
        "root follow-up",
        &mut root,
        &mut root_state,
        &discard_updates(),
    )
    .await
    .expect("chained root turn should complete");

    server.await.expect("server task should finish");
}

fn fast_recovery_policy(max_reconnect_attempts: usize) -> RecoveryPolicy {
    RecoveryPolicy {
        max_reconnect_attempts,
        backoff_base: std::time::Duration::from_millis(10),
        backoff_cap: std::time::Duration::from_millis(20),
        send_timeout: std::time::Duration::from_secs(1),
        first_event_timeout: std::time::Duration::from_secs(1),
    }
}

#[tokio::test]
async fn a_close_observed_while_parked_reconnects_before_the_next_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let initial = receive_json(&mut socket).await;
        assert!(initial.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_parked", "msg_parked", "parked answer"),
        )
        .await;
        socket
            .send(WebSocketMessage::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "parked close".into(),
            })))
            .await
            .expect("parked close should send");

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut replacement = accept_async(stream)
            .await
            .expect("server should upgrade replacement");
        let replayed = receive_json(&mut replacement).await;
        assert!(replayed.get("previous_response_id").is_none());
        let wire = replayed.to_string();
        assert!(wire.contains("parked question"));
        assert!(wire.contains("parked answer"));
        assert!(wire.contains("next question"));
        send_json(
            &mut replacement,
            completed_event("resp_replaced", "msg_replaced", "replacement answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();
    let policy = fast_recovery_policy(3);
    let initial = RequestParts {
        prompt: Message::user("parked question"),
        history: Vec::new(),
        instructions: "Parked close instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        initial.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("initial turn should complete");
    assert_eq!(continuation_response_id(&openai), Some("resp_parked"));

    let terminal = wait_for_terminal(&openai.ws.session).await;
    assert_eq!(terminal, OpenAiWebSocketTerminalCategory::CloseFrame);
    let continuation = RequestParts {
        prompt: Message::user("next question"),
        history: state.history.clone(),
        instructions: "Parked close instructions".to_string(),
        allowed_tool_names: None,
    };
    let (sender, mut events) = session_event_channel(64);
    run_turn_request_with_recovery(
        continuation.request(),
        &mut openai,
        &mut state,
        &ProgressReporter::new(sender),
        &policy,
    )
    .await
    .expect("the parked close should reconnect before sending");

    let mut retries = Vec::new();
    while let Ok(update) = events.try_recv() {
        if let SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { retry_after, .. }) = update {
            retries.push(retry_after);
        }
    }
    assert_eq!(retries, vec![std::time::Duration::ZERO]);
    assert_eq!(openai.ws.socket_generation, 1);
    assert_eq!(continuation_response_id(&openai), Some("resp_replaced"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn first_event_timeout_reconnects_after_only_a_late_prior_done() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _initial = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_timeout_old", "msg_timeout_old", "old answer"),
        )
        .await;

        let black_holed = receive_json(&mut socket).await;
        assert_eq!(black_holed["previous_response_id"], "resp_timeout_old");
        // This is the trailing duplicate for the preceding response, not the
        // first event for the request the server is now black-holing.
        send_json(&mut socket, done_event("resp_timeout_old", "completed")).await;

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut replacement = accept_async(stream)
            .await
            .expect("server should upgrade replacement");
        let replayed = receive_json(&mut replacement).await;
        assert!(replayed.get("previous_response_id").is_none());
        let wire = replayed.to_string();
        assert!(wire.contains("old question"));
        assert!(wire.contains("old answer"));
        assert!(wire.contains("black-holed question"));
        send_json(
            &mut replacement,
            completed_event("resp_timeout_new", "msg_timeout_new", "recovered"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();
    let mut policy = fast_recovery_policy(3);
    policy.first_event_timeout = std::time::Duration::from_millis(30);
    let initial = RequestParts {
        prompt: Message::user("old question"),
        history: Vec::new(),
        instructions: "First event timeout instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        initial.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("initial turn should complete");

    let continuation = RequestParts {
        prompt: Message::user("black-holed question"),
        history: state.history.clone(),
        instructions: "First event timeout instructions".to_string(),
        allowed_tool_names: None,
    };
    let replays = state.history_replays.clone();
    run_turn_request_with_recovery(
        request_with_replays(
            &continuation.history,
            &replays,
            &continuation.prompt,
            &continuation.instructions,
            None,
        ),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("the first-event timeout should reconnect and replay");

    assert_eq!(openai.ws.socket_generation, 1);
    assert_eq!(continuation_response_id(&openai), Some("resp_timeout_new"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn stale_id_full_retry_starts_a_new_first_event_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _initial = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_stale_timeout", "msg_stale_timeout", "old answer"),
        )
        .await;

        let chained = receive_json(&mut socket).await;
        assert_eq!(chained["previous_response_id"], "resp_stale_timeout");
        send_json(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "code": "previous_response_not_found",
                    "message": "Previous response not found"
                }
            }),
        )
        .await;

        let same_socket_full = receive_json(&mut socket).await;
        assert!(same_socket_full.get("previous_response_id").is_none());
        // Emit nothing for this fallback. Its own first-event deadline must
        // expire; without a reset here the turn would wait forever because
        // the stale-ID error already ended the original deadline.

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut replacement = accept_async(stream)
            .await
            .expect("server should upgrade replacement");
        let replayed = receive_json(&mut replacement).await;
        assert!(replayed.get("previous_response_id").is_none());
        let wire = replayed.to_string();
        assert!(wire.contains("old question"));
        assert!(wire.contains("old answer"));
        assert!(wire.contains("stale timeout question"));
        send_json(
            &mut replacement,
            completed_event("resp_stale_recovered", "msg_stale_recovered", "recovered"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();
    let mut policy = fast_recovery_policy(3);
    policy.first_event_timeout = std::time::Duration::from_millis(30);
    let initial = RequestParts {
        prompt: Message::user("old question"),
        history: Vec::new(),
        instructions: "Stale timeout instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        initial.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("initial turn should complete");

    let continuation = RequestParts {
        prompt: Message::user("stale timeout question"),
        history: state.history.clone(),
        instructions: "Stale timeout instructions".to_string(),
        allowed_tool_names: None,
    };
    let replays = state.history_replays.clone();
    run_turn_request_with_recovery(
        request_with_replays(
            &continuation.history,
            &replays,
            &continuation.prompt,
            &continuation.instructions,
            None,
        ),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("the stale fallback timeout should reconnect and replay");

    assert_eq!(openai.ws.socket_generation, 1);
    assert_eq!(
        continuation_response_id(&openai),
        Some("resp_stale_recovered")
    );
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn send_timeout_reconnects_and_replays_on_a_fresh_pump() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut delayed_socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut replacement = accept_async(stream)
            .await
            .expect("server should upgrade replacement");
        let replayed = receive_json(&mut replacement).await;
        assert!(replayed.get("previous_response_id").is_none());
        assert!(replayed.to_string().contains("send timeout question"));
        send_json(
            &mut replacement,
            completed_event("resp_send_timeout", "msg_send_timeout", "recovered"),
        )
        .await;

        let old_result =
            tokio::time::timeout(std::time::Duration::from_secs(1), delayed_socket.next())
                .await
                .expect("the delayed pump should be aborted by replacement");
        assert!(
            old_result.is_none()
                || old_result.is_some_and(|message| {
                    message.is_err() || matches!(message, Ok(WebSocketMessage::Close(_)))
                })
        );
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let delayed = OpenAiWebSocketSession::new_with_test_send_delay(
        socket,
        std::time::Duration::from_millis(200),
    );
    let mut openai = test_session_with_pump(&url, delayed, None);
    let mut state = AttemptState::default();
    let mut policy = fast_recovery_policy(3);
    policy.send_timeout = std::time::Duration::from_millis(20);
    let parts = RequestParts {
        prompt: Message::user("send timeout question"),
        history: Vec::new(),
        instructions: "Send timeout instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("the send timeout should reconnect and replay");

    assert_eq!(openai.ws.socket_generation, 1);
    assert_eq!(continuation_response_id(&openai), Some("resp_send_timeout"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn websocket_connection_limit_reconnects_and_replays_full_history() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _initial = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_limit_old", "msg_limit_old", "old answer"),
        )
        .await;

        let limited = receive_json(&mut socket).await;
        assert_eq!(limited["previous_response_id"], "resp_limit_old");
        send_json(
            &mut socket,
            json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "code": "websocket_connection_limit_reached",
                    "message": "Responses websocket connection limit reached (60 minutes)."
                },
                "status": 400
            }),
        )
        .await;

        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut replacement = accept_async(stream)
            .await
            .expect("server should upgrade replacement");
        let replayed = receive_json(&mut replacement).await;
        assert!(replayed.get("previous_response_id").is_none());
        let wire = replayed.to_string();
        assert!(wire.contains("old question"));
        assert!(wire.contains("old answer"));
        assert!(wire.contains("after the limit"));
        send_json(
            &mut replacement,
            completed_event("resp_limit_new", "msg_limit_new", "continued"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();
    let policy = fast_recovery_policy(3);
    let initial = RequestParts {
        prompt: Message::user("old question"),
        history: Vec::new(),
        instructions: "Connection limit instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        initial.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("initial turn should complete");

    let continuation = RequestParts {
        prompt: Message::user("after the limit"),
        history: state.history.clone(),
        instructions: "Connection limit instructions".to_string(),
        allowed_tool_names: None,
    };
    let history_replays = state.history_replays.clone();
    run_turn_request_with_recovery(
        request_with_replays(
            &continuation.history,
            &history_replays,
            &continuation.prompt,
            &continuation.instructions,
            None,
        ),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("the connection limit should reconnect and replay");

    assert_eq!(openai.ws.socket_generation, 1);
    assert_eq!(continuation_response_id(&openai), Some("resp_limit_new"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn failed_replacement_handshake_preserves_all_socket_scoped_state() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let (release_server, server_released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");
        let _request = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_preserved", "msg_preserved", "answer"),
        )
        .await;
        let _ = server_released.await;
    });

    let (socket, _) = connect_async(format!("ws://{address}"))
        .await
        .expect("client should connect");
    // `test_session` deliberately configures reconnects to port zero, which
    // fails immediately while leaving this live socket untouched.
    let mut openai = test_session(socket, None);
    let mut state = AttemptState::default();
    run_turn("question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("initial turn should establish state");
    openai.ws.websocket_connection_request_id = Some("ws_preserved".to_string());
    let generation = openai.ws.socket_generation;
    let response_id = continuation_response_id(&openai).map(ToOwned::to_owned);
    let pending_done = openai.ws.pending_done_response_id.clone();
    let last_activity = openai.ws.last_activity;
    assert!(openai.ws.session.terminal_status().is_none());

    openai
        .ws
        .reconnect()
        .await
        .expect_err("the replacement handshake should fail");

    assert_eq!(openai.ws.socket_generation, generation);
    assert_eq!(continuation_response_id(&openai), response_id.as_deref());
    assert_eq!(openai.ws.pending_done_response_id, pending_done);
    assert_eq!(
        openai.ws.websocket_connection_request_id.as_deref(),
        Some("ws_preserved")
    );
    assert_eq!(openai.ws.last_activity, last_activity);
    assert!(openai.ws.session.terminal_status().is_none());

    let _ = release_server.send(());
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn turn_survives_repeated_drops_within_the_recovery_budget() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        // Two consecutive connections die mid-request before the third one
        // finally serves the replayed prompt.
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.expect("server should accept");
            let mut socket = accept_async(stream)
                .await
                .expect("server should upgrade websocket");
            let request = receive_json(&mut socket).await;
            assert!(request.to_string().contains("flaky question"));
            drop(socket);
        }
        let (stream, _) = listener.accept().await.expect("server should re-accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade the final reconnect");
        let request = receive_json(&mut socket).await;
        assert!(request.to_string().contains("flaky question"));
        send_json(
            &mut socket,
            completed_event("resp_persist", "msg_persist", "made it through"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();
    let (sender, mut events) = session_event_channel(256);

    let parts = RequestParts {
        prompt: Message::user("flaky question"),
        history: Vec::new(),
        instructions: "Recovery instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &ProgressReporter::new(sender),
        &fast_recovery_policy(5),
    )
    .await
    .expect("the turn should recover after two drops");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_persist".to_string()),
            content: vec![AssistantContent::text("made it through")],
        }
    );

    // Each recovery cycle announced itself as a retry, never a failure.
    let mut retry_attempts = Vec::new();
    while let Ok(update) = events.try_recv() {
        if let SessionUpdate::Lifecycle(zevria_session_api::SessionEvent::TurnRetrying {
            attempt,
            max_attempts,
            retry_after,
            ..
        }) = update
        {
            assert_eq!(max_attempts, 5);
            retry_attempts.push((attempt, retry_after));
        }
    }
    assert_eq!(
        retry_attempts,
        vec![
            (1, std::time::Duration::from_millis(10)),
            (2, std::time::Duration::from_millis(20)),
        ]
    );

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn turn_fails_after_the_recovery_budget_is_exhausted() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        // Initial connection plus one per recovery cycle, all dying
        // mid-request.
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.expect("server should accept");
            let mut socket = accept_async(stream)
                .await
                .expect("server should upgrade websocket");
            let _request = receive_json(&mut socket).await;
            drop(socket);
        }
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();

    let parts = RequestParts {
        prompt: Message::user("doomed question"),
        history: Vec::new(),
        instructions: "Recovery instructions".to_string(),
        allowed_tool_names: None,
    };
    let error = run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(2),
    )
    .await
    .expect_err("a persistently dead server should fail the turn");

    assert!(
        format!("{error:#}").contains("after 2 reconnect attempts"),
        "the surfaced error should say the recovery budget was exhausted: {error:#}"
    );

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn a_healthy_socket_remains_incremental_after_more_than_sixty_seconds_idle() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let url = format!("ws://{address}");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("server should accept");
        let mut socket = accept_async(stream)
            .await
            .expect("server should upgrade websocket");

        let initial = receive_json(&mut socket).await;
        assert!(initial.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_idle_old", "msg_idle_old", "old answer"),
        )
        .await;

        // Backdating local activity must not rotate a pump that remains live.
        // The exact extension stays on this socket and carries only its suffix.
        let continuation = receive_json(&mut socket).await;
        assert_eq!(continuation["previous_response_id"], "resp_idle_old");
        let wire = continuation.to_string();
        assert!(wire.contains("after a long think"));
        assert!(!wire.contains("old question"));
        assert!(!wire.contains("old answer"));
        send_json(
            &mut socket,
            completed_event("resp_fresh", "msg_fresh", "fresh socket answer"),
        )
        .await;
    });

    let (socket, _) = connect_async(&url).await.expect("client should connect");
    let mut openai = test_session_with_url(&url, socket, None);
    let mut state = AttemptState::default();

    let initial = RequestParts {
        prompt: Message::user("old question"),
        history: Vec::new(),
        instructions: "Recovery instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        initial.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &RecoveryPolicy::default(),
    )
    .await
    .expect("the initial turn should establish a continuation");

    openai.ws.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(120);
    let original_generation = openai.ws.socket_generation;
    let continuation = RequestParts {
        prompt: Message::user("after a long think"),
        history: state.history.clone(),
        instructions: "Recovery instructions".to_string(),
        allowed_tool_names: None,
    };
    let replays = state.history_replays.clone();
    run_turn_request_with_recovery(
        request_with_replays(
            &continuation.history,
            &replays,
            &continuation.prompt,
            &continuation.instructions,
            None,
        ),
        &mut openai,
        &mut state,
        &discard_updates(),
        &RecoveryPolicy::default(),
    )
    .await
    .expect("the turn should remain on the healthy parked socket");

    assert_eq!(openai.ws.socket_generation, original_generation);
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_fresh".to_string()),
            content: vec![AssistantContent::text("fresh socket answer")],
        }
    );
    assert_eq!(
        continuation_response_id(&openai),
        Some("resp_fresh"),
        "the chain must continue from the same-socket response"
    );

    server.await.expect("server task should finish");
}

#[tokio::test]
async fn http_only_transport_posts_full_streaming_request_and_accumulates_fragmented_sse() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_http",
        "summary": [{"type": "summary_text", "text": "checked"}],
        "content": [],
        "encrypted_content": "opaque",
        "status": null
    });
    let call = function_call(
        "fc_http",
        "call_http",
        "write",
        json!({"path": "x", "content": "y"}),
    );
    let final_message = json!({
        "type": "message",
        "id": "msg_http",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": "HTTP answer"}]
    });
    let mut completed = completed_output_event(
        "resp_http",
        vec![reasoning.clone(), call.clone(), final_message.clone()],
    );
    completed["response"]["usage"] = json!({
        "input_tokens": 1200,
        "input_tokens_details": {"cached_tokens": 1000},
        "output_tokens": 40,
        "total_tokens": 1240
    });
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let request = receive_http_json(&mut stream).await;
        let body = format!(
            "{}{}{}{}",
            sse_data(""),
            sse_data("[DONE]"),
            sse_data(output_text_delta("HTTP ", 1).to_string()),
            sse_data(completed.to_string())
        );
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("SSE headers");
        for fragment in body.as_bytes().chunks(7) {
            stream.write_all(fragment).await.expect("SSE fragment");
            tokio::task::yield_now().await;
        }
        request
    });

    let endpoint = format!("http://{address}/custom/responses?version=1");
    let mut openai = connect_http_test_provider(endpoint, ToolServer::new().run()).await;
    openai.model.model = "gpt-explore".to_string();
    let mut state = AttemptState::default();
    let (sender, mut events) = session_event_channel(32);
    let parts = RequestParts {
        prompt: Message::user("HTTP question"),
        history: Vec::new(),
        instructions: "HTTP instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request_with_role(ModelRole::Explore),
        &mut openai,
        &mut state,
        &ProgressReporter::new(sender),
        &fast_recovery_policy(2),
    )
    .await
    .expect("HTTP turn should complete");

    assert_eq!(openai.transport, OpenAiTransport::Http);
    assert!(openai.ws.continuation.is_none());
    assert_eq!(usage_events(&mut events), vec![expected_token_usage()]);
    let replay = state
        .history_replays
        .last()
        .and_then(Option::as_ref)
        .expect("native HTTP replay")
        .replay();
    assert_eq!(replay.items, vec![reasoning, call, final_message]);
    assert!(format!("{:?}", attempt_message(&state)).contains("HTTP answer"));

    let request = server.await.expect("HTTP server");
    let headers = request.headers.to_ascii_lowercase();
    assert!(
        headers.starts_with("post /custom/responses?version=1 http/1.1"),
        "{}",
        request.headers
    );
    assert!(headers.contains("accept: text/event-stream"));
    assert!(headers.contains("authorization: bearer test-key"));
    assert_eq!(request.body["stream"], true);
    assert_eq!(request.body["store"], false);
    assert!(request.body.get("type").is_none());
    assert!(request.body.get("previous_response_id").is_none());
    assert_eq!(request.body["model"], "gpt-explore");
    assert!(request.body["input"].to_string().contains("HTTP question"));
}

#[tokio::test]
async fn http_response_done_completes_with_native_output_and_usage() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let mut done = completed_event_with_usage("resp_http_done", "msg_http_done", "done answer");
    done["type"] = json!("response.done");
    let expected_output = done["response"]["output"].clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(done.to_string()),
        )
        .await;
        request
    });

    let mut openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let mut state = AttemptState::default();
    let (sender, mut events) = session_event_channel(16);
    let parts = RequestParts {
        prompt: Message::user("finish with response.done"),
        history: Vec::new(),
        instructions: "HTTP response.done instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &ProgressReporter::new(sender),
        &fast_recovery_policy(1),
    )
    .await
    .expect("HTTP response.done should complete");

    assert_eq!(usage_events(&mut events), vec![expected_token_usage()]);
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_http_done".to_string()),
            content: vec![AssistantContent::text("done answer")],
        }
    );
    let replay = state
        .history_replays
        .last()
        .and_then(Option::as_ref)
        .expect("native HTTP response.done replay")
        .replay();
    assert_eq!(replay.items, expected_output.as_array().unwrap().clone());

    let request = server.await.expect("HTTP response.done server");
    assert_eq!(request.body["stream"], true);
    assert!(request.body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn http_terminal_diagnostics_include_attempt_identity_usage_and_safe_cache_fingerprint() {
    if isolate_log_test(
        "tests::http_terminal_diagnostics_include_attempt_identity_usage_and_safe_cache_fingerprint",
    ) {
        return;
    }
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let mut completed =
        completed_event_with_usage("resp_http_log", "msg_http_log", "logged answer");
    completed["response"]["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(256);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let _ = receive_http_json(&mut stream).await;
        send_http_response_with_raw_request_id(
            &mut stream,
            "200 OK",
            "text/event-stream",
            Some(b"req_http_success"),
            sse_data(completed.to_string()),
        )
        .await;
    });

    let logs = CapturedLogs::default();
    let subscriber = captured_log_subscriber(logs.clone());
    let raw_cache_key = "cache-log-key";
    let expected_fingerprint = crate::connection::prompt_cache_key_fingerprint(raw_cache_key);
    async {
        let mut openai = connect_http_test_provider_with_options(
            format!("http://{address}/v1/responses"),
            ToolServer::new().run(),
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            raw_cache_key,
        )
        .await;
        let mut state = AttemptState::default();
        let parts = RequestParts {
            prompt: Message::user("diagnostic request"),
            history: Vec::new(),
            instructions: "Diagnostic instructions".to_string(),
            allowed_tool_names: None,
        };
        run_turn_request_with_recovery(
            parts.request(),
            &mut openai,
            &mut state,
            &discard_updates(),
            &fast_recovery_policy(1),
        )
        .await
        .expect("HTTP diagnostic response");
    }
    .with_subscriber(subscriber)
    .await;
    server.await.expect("HTTP diagnostic server");

    let logs = logs.contents();
    for expected in [
        "OpenAI terminal response",
        "provider=test-provider",
        "model=gpt-test",
        "transport=\"http\"",
        "request_mode=\"full\"",
        "retry_number=Some(0)",
        "socket_generation=None",
        "replay_source=\"none\"",
        "response_id=Some(\"resp_http_log\")",
        "upstream_request_id=Some(\"req_http_success\")",
        "input_tokens=1200",
        "cached_tokens=1000",
        "cache_write_tokens=256",
        expected_fingerprint.as_str(),
    ] {
        assert!(
            logs.contains(expected),
            "missing {expected:?} in logs: {logs}"
        );
    }
    assert!(
        !logs.contains(raw_cache_key),
        "terminal logs must not expose the complete cache key: {logs}"
    );
}

#[tokio::test]
async fn websocket_terminal_diagnostics_keep_handshake_identity_connection_scoped() {
    if isolate_log_test(
        "tests::websocket_terminal_diagnostics_keep_handshake_identity_connection_scoped",
    ) {
        return;
    }
    let _log_guard = CAPTURED_LOG_TEST_LOCK.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("WebSocket connection");
        let mut socket = accept_hdr_async(
            stream,
            AssertOpenAiWebSocketHandshakeWithRequestId("req_ws_handshake"),
        )
        .await
        .expect("WebSocket upgrade");
        let request = receive_json(&mut socket).await;
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        let mut completed =
            completed_event_with_usage("resp_ws_log", "msg_ws_log", "logged answer");
        completed["response"]["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(512);
        send_json(&mut socket, completed).await;
    });

    let logs = CapturedLogs::default();
    let subscriber = captured_log_subscriber(logs.clone());
    let raw_cache_key = "ws-cache-log-key";
    let expected_fingerprint = crate::connection::prompt_cache_key_fingerprint(raw_cache_key);
    async {
        let factory = TestProviderFactory::new(
            TestProviderConfig {
                base_url: format!("ws://{address}/v1/responses"),
                api_key: "test-key".to_string(),
                models: test_models(),
                supports_websockets: true,
                reasoning: TestReasoningConfig::default(),
                compatibility: ResponsesCompatibilityConfig::default(),
                additional_params: BTreeMap::new(),
                compaction: RemoteCompactionConfig::default(),
            },
            "Test instructions",
            ToolServer::new().run(),
        );
        let mut openai = factory
            .connect(raw_cache_key)
            .await
            .expect("WebSocket provider");
        let mut state = AttemptState::default();
        run_turn(
            "diagnostic request",
            &mut openai,
            &mut state,
            &discard_updates(),
        )
        .await
        .expect("WebSocket diagnostic response");
    }
    .with_subscriber(subscriber)
    .await;
    server.await.expect("WebSocket diagnostic server");

    let logs = logs.contents();
    for expected in [
        "OpenAI terminal response",
        "transport=\"websocket\"",
        "request_mode=\"full\"",
        "retry_number=None",
        "socket_generation=Some(0)",
        "response_id=Some(\"resp_ws_log\")",
        "upstream_request_id=None",
        "websocket_connection_request_id=Some(\"req_ws_handshake\")",
        "cache_write_tokens=512",
        expected_fingerprint.as_str(),
    ] {
        assert!(
            logs.contains(expected),
            "missing {expected:?} in logs: {logs}"
        );
    }
    assert!(
        !logs.contains(raw_cache_key),
        "terminal logs must not expose the complete cache key: {logs}"
    );
}

#[tokio::test]
async fn http_retries_server_errors_and_partial_stream_loss_with_identical_requests() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();

        let (mut stream, _) = listener.accept().await.expect("first HTTP connection");
        requests.push(receive_http_json(&mut stream).await.body);
        send_http_response_with_raw_request_id(
            &mut stream,
            "503 Service Unavailable",
            "text/plain",
            Some(b"req_retry_503"),
            "temporary outage",
        )
        .await;

        let (mut stream, _) = listener.accept().await.expect("second HTTP connection");
        requests.push(receive_http_json(&mut stream).await.body);
        let partial = sse_data(output_text_delta("discard me", 1).to_string());
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nX-Request-Id: req_retry_stream\r\nConnection: close\r\n\r\n",
            partial.len() + 100
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("partial headers");
        stream
            .write_all(partial.as_bytes())
            .await
            .expect("partial SSE");
        drop(stream);

        let (mut stream, _) = listener.accept().await.expect("third HTTP connection");
        requests.push(receive_http_json(&mut stream).await.body);
        let completed =
            sse_data(completed_event("resp_retry", "msg_retry", "clean answer").to_string());
        send_http_response_with_raw_request_id(
            &mut stream,
            "200 OK",
            "text/event-stream",
            Some(b"req_retry_success"),
            completed,
        )
        .await;
        requests
    });

    let mut openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    openai
        .additional_params
        .insert("gateway_routing".to_string(), json!("stable-route"));
    let mut state = AttemptState::default();
    let (sender, mut events) = session_event_channel(64);
    let parts = RequestParts {
        prompt: Message::user("retry exactly"),
        history: Vec::new(),
        instructions: "Retry instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request_with_role_and_skills(ModelRole::Build, Some("Retry active skill body")),
        &mut openai,
        &mut state,
        &ProgressReporter::new(sender),
        &fast_recovery_policy(3),
    )
    .await
    .expect("HTTP retries should recover");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_retry".to_string()),
            content: vec![AssistantContent::text("clean answer")],
        }
    );
    let requests = server.await.expect("HTTP retry server");
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(
        requests
            .iter()
            .all(|request| request["gateway_routing"] == "stable-route")
    );
    assert!(requests.iter().all(|request| {
        request["instructions"] == rendered_test_instructions("Retry instructions")
            && request["input"]
                .to_string()
                .matches("Retry active skill body")
                .count()
                == 1
    }));

    let mut retries = Vec::new();
    while let Ok(update) = events.try_recv() {
        if let SessionUpdate::Lifecycle(SessionEvent::TurnRetrying {
            attempt,
            error,
            retry_after,
            ..
        }) = update
        {
            retries.push((attempt, error, retry_after));
        }
    }
    assert_eq!(retries.len(), 2);
    assert_eq!(retries[0].0, 1);
    assert!(retries[0].1.contains("req_retry_503"));
    assert_eq!(retries[1].0, 2);
    assert!(retries[1].1.contains("req_retry_stream"));
    assert_eq!(retries[0].2, std::time::Duration::from_millis(10));
    assert_eq!(retries[1].2, std::time::Duration::from_millis(20));
}

#[tokio::test]
async fn http_stream_retry_and_terminal_error_keep_their_per_attempt_request_ids() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        for (request_id, preview) in [
            ("req_stream_first", "discard first"),
            ("req_stream_final", "discard final"),
        ] {
            let (mut stream, _) = listener.accept().await.expect("HTTP retry connection");
            let _ = receive_http_json(&mut stream).await;
            send_http_response_with_raw_request_id(
                &mut stream,
                "200 OK",
                "text/event-stream",
                Some(request_id.as_bytes()),
                sse_data(output_text_delta(preview, 1).to_string()),
            )
            .await;
        }
    });

    let mut openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let mut state = AttemptState::default();
    let (sender, mut events) = session_event_channel(16);
    let parts = RequestParts {
        prompt: Message::user("request identity"),
        history: Vec::new(),
        instructions: "Request identity instructions".to_string(),
        allowed_tool_names: None,
    };
    let error = run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &ProgressReporter::new(sender),
        &fast_recovery_policy(1),
    )
    .await
    .expect_err("both streams end before a terminal response");
    let error = format!("{error:#}");
    assert!(error.contains("req_stream_final"), "{error}");
    assert!(!error.contains("req_stream_first"), "{error}");

    let retry_error = std::iter::from_fn(|| events.try_recv().ok()).find_map(|update| {
        let SessionUpdate::Lifecycle(SessionEvent::TurnRetrying { error, .. }) = update else {
            return None;
        };
        Some(error)
    });
    assert!(
        retry_error
            .as_deref()
            .is_some_and(|error| error.contains("req_stream_first")),
        "the retry notification must carry the first attempt's identity: {retry_error:?}"
    );
    server.await.expect("HTTP request-ID retry server");
}

#[tokio::test]
async fn http_first_event_timeout_retries_but_unknown_event_starts_stream() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stalled, _) = listener.accept().await.expect("stalled HTTP connection");
        let first = receive_http_json(&mut stalled).await.body;
        stalled
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("stalled SSE headers");
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        drop(stalled);

        let (mut stream, _) = listener.accept().await.expect("retry HTTP connection");
        let second = receive_http_json(&mut stream).await.body;
        let first_event =
            sse_data(json!({"type": "relay.metadata", "state": "started"}).to_string());
        let completed =
            sse_data(completed_event("resp_slow_http", "msg_slow_http", "finished").to_string());
        let body_len = first_event.len() + completed.len();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("slow SSE headers");
        stream
            .write_all(first_event.as_bytes())
            .await
            .expect("first SSE event");
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        stream
            .write_all(completed.as_bytes())
            .await
            .expect("terminal SSE event");
        (first, second)
    });

    let mut openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let mut state = AttemptState::default();
    let mut policy = fast_recovery_policy(2);
    policy.first_event_timeout = std::time::Duration::from_millis(20);
    let parts = RequestParts {
        prompt: Message::user("wait for it"),
        history: Vec::new(),
        instructions: "Slow stream instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &policy,
    )
    .await
    .expect("HTTP retry should tolerate midstream inactivity");

    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_slow_http".to_string()),
            content: vec![AssistantContent::text("finished")],
        }
    );
    let (first, second) = server.await.expect("slow HTTP server");
    assert_eq!(first, second, "the timeout retry must reuse its snapshot");
}

#[tokio::test]
async fn http_client_and_modeled_failures_are_terminal_and_error_bodies_are_bounded() {
    #[derive(Clone, Copy)]
    enum FailureCase {
        ClientError,
        MalformedEvent,
        ModeledErrorWithTransportWords,
        SemanticFailure,
    }

    for case in [
        FailureCase::ClientError,
        FailureCase::MalformedEvent,
        FailureCase::ModeledErrorWithTransportWords,
        FailureCase::SemanticFailure,
    ] {
        let request_id = match case {
            FailureCase::ClientError => "req_client_error",
            FailureCase::MalformedEvent => "req_malformed_event",
            FailureCase::ModeledErrorWithTransportWords => "req_modeled_error",
            FailureCase::SemanticFailure => "req_semantic_failure",
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("HTTP listener");
        let address = listener.local_addr().expect("HTTP address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("HTTP connection");
            let _request = receive_http_json(&mut stream).await;
            match case {
                FailureCase::ClientError => {
                    let mut detail = "x".repeat(5000);
                    detail.push_str("UNBOUNDED_TAIL");
                    send_http_response_with_raw_request_id(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        Some(request_id.as_bytes()),
                        detail,
                    )
                    .await;
                }
                FailureCase::MalformedEvent => {
                    send_http_response_with_raw_request_id(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        Some(request_id.as_bytes()),
                        sse_data("{not json}"),
                    )
                    .await;
                }
                FailureCase::ModeledErrorWithTransportWords => {
                    send_http_response_with_raw_request_id(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        Some(request_id.as_bytes()),
                        sse_data(
                            json!({
                                "type": "error",
                                "error": {
                                    "code": "invalid_request_error",
                                    "message": "request timed out while validating"
                                }
                            })
                            .to_string(),
                        ),
                    )
                    .await;
                }
                FailureCase::SemanticFailure => {
                    send_http_response_with_raw_request_id(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        Some(request_id.as_bytes()),
                        sse_data(done_event("resp_failed_http", "failed").to_string()),
                    )
                    .await;
                }
            }
            drop(stream);
            tokio::time::timeout(std::time::Duration::from_millis(80), listener.accept())
                .await
                .is_err()
        });

        let mut openai = connect_http_test_provider(
            format!("http://{address}/v1/responses"),
            ToolServer::new().run(),
        )
        .await;
        let mut state = AttemptState::default();
        let parts = RequestParts {
            prompt: Message::user("terminal failure"),
            history: Vec::new(),
            instructions: "Failure instructions".to_string(),
            allowed_tool_names: None,
        };
        let error = run_turn_request_with_recovery(
            parts.request(),
            &mut openai,
            &mut state,
            &discard_updates(),
            &fast_recovery_policy(2),
        )
        .await
        .expect_err("terminal HTTP failure");
        let error = format!("{error:#}");
        assert!(state.result.is_empty());
        assert!(server.await.expect("terminal HTTP server"));
        assert!(!error.contains("UNBOUNDED_TAIL"));
        assert!(
            error.contains(request_id),
            "the HTTP attempt request ID must survive the terminal error chain: {error}"
        );
        assert!(
            error.contains("400")
                || error.contains("expected ident")
                || error.contains("JsonError")
                || error.contains("timed out while validating")
                || error.contains("Failed"),
            "unexpected terminal error: {error}"
        );
    }
}

#[tokio::test]
async fn chat_completions_sse_is_rejected_without_persisting_partial_output() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(
                json!({
                    "id": "chatcmpl-incompatible",
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "must not persist"},
                        "finish_reason": null
                    }]
                })
                .to_string(),
            ),
        )
        .await;
        request
    });

    let mut openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let mut state = AttemptState::default();
    let parts = RequestParts {
        prompt: Message::user("reject chat protocol"),
        history: Vec::new(),
        instructions: "Responses only".to_string(),
        allowed_tool_names: None,
    };
    let error = run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(1),
    )
    .await
    .expect_err("Chat Completions chunks are not Responses events");

    let request = server.await.expect("Chat-shaped gateway server");
    assert!(request.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(format!("{error:#}").contains("type"), "{error:#}");
    assert!(state.result.is_empty());
    assert!(state.history.is_empty());
    assert!(state.history_replays.is_empty());
}

#[tokio::test]
async fn startup_websocket_426_immediately_selects_sticky_http() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("transport listener");
    let address = listener.local_addr().expect("transport address");
    let server = tokio::spawn(async move {
        let (mut handshake, _) = listener.accept().await.expect("WebSocket handshake");
        let handshake_headers = receive_http_headers(&mut handshake).await;
        assert!(handshake_headers.starts_with("GET /custom/responses?version=426 HTTP/1.1"));
        handshake
            .write_all(
                b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("426 response");
        drop(handshake);

        let (mut stream, _) = listener.accept().await.expect("HTTP fallback");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_426", "msg_426", "HTTP selected").to_string()),
        )
        .await;
        request
    });

    let config = TestProviderConfig {
        base_url: format!("http://{address}/custom/responses?version=426"),
        api_key: "test-key".to_string(),
        models: test_models(),
        supports_websockets: true,
        reasoning: TestReasoningConfig::default(),
        compatibility: ResponsesCompatibilityConfig::default(),
        additional_params: Default::default(),
        compaction: RemoteCompactionConfig::default(),
    };
    let mut openai = OpenAiProvider::connect(
        &config.resolved(ModelRole::Build),
        config.reasoning.effort,
        "Test instructions",
        ToolServer::new().run(),
        "test-session-id",
        "test-session",
    )
    .await
    .expect("426 should not fail provider construction");
    assert_eq!(openai.transport, OpenAiTransport::Http);

    let mut state = AttemptState::default();
    run_turn(
        "fallback question",
        &mut openai,
        &mut state,
        &discard_updates(),
    )
    .await
    .expect("HTTP fallback turn");
    assert_eq!(openai.transport, OpenAiTransport::Http);
    assert_eq!(continuation_response_id(&openai), None);
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_426".to_string()),
            content: vec![AssistantContent::text("HTTP selected")],
        }
    );
    let request = server.await.expect("426 transport server");
    assert!(request.body.get("previous_response_id").is_none());
    assert!(request.body.get("type").is_none());
}

#[tokio::test]
async fn exhausted_websocket_retries_fall_back_with_full_input_and_remain_http() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("transport listener");
    let address = listener.local_addr().expect("transport address");
    let url = format!("ws://{address}/v1/responses?transport=sticky");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("startup WebSocket");
        let mut socket = accept_async(stream).await.expect("startup upgrade");
        let initial = receive_json(&mut socket).await;
        send_json(&mut socket, output_text_delta("discard websocket", 1)).await;
        drop(socket);

        let (stream, _) = listener.accept().await.expect("replacement WebSocket");
        let mut socket = accept_async(stream).await.expect("replacement upgrade");
        let replayed = receive_json(&mut socket).await;
        drop(socket);

        let (mut stream, _) = listener.accept().await.expect("HTTP fallback");
        let fallback = receive_http_json(&mut stream).await.body;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(
                completed_event("resp_http_fallback", "msg_http_fallback", "fallback answer")
                    .to_string(),
            ),
        )
        .await;

        let (mut stream, _) = listener.accept().await.expect("sticky HTTP turn");
        let sticky = receive_http_json(&mut stream).await.body;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(
                completed_event("resp_http_sticky", "msg_http_sticky", "sticky answer").to_string(),
            ),
        )
        .await;
        (initial, replayed, fallback, sticky)
    });

    let config = TestProviderConfig {
        base_url: url,
        api_key: "test-key".to_string(),
        models: test_models(),
        supports_websockets: true,
        reasoning: TestReasoningConfig::default(),
        compatibility: ResponsesCompatibilityConfig::default(),
        additional_params: BTreeMap::from([("gateway_routing".to_string(), json!("stable-route"))]),
        compaction: RemoteCompactionConfig::default(),
    };
    let mut openai = OpenAiProvider::connect(
        &config.resolved(ModelRole::Build),
        config.reasoning.effort,
        "Test instructions",
        ToolServer::new().run(),
        "test-session-id",
        "test-session",
    )
    .await
    .expect("startup WebSocket");
    let mut state = AttemptState::default();
    let parts = RequestParts {
        prompt: Message::user("fallback question"),
        history: Vec::new(),
        instructions: "Fallback instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(1),
    )
    .await
    .expect("HTTP fallback should recover the logical request");
    assert_eq!(openai.transport, OpenAiTransport::Http);
    assert!(openai.ws.continuation.is_none());

    run_turn("next question", &mut openai, &mut state, &discard_updates())
        .await
        .expect("later turn should stay on HTTP");
    assert_eq!(openai.transport, OpenAiTransport::Http);
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_http_sticky".to_string()),
            content: vec![AssistantContent::text("sticky answer")],
        }
    );

    let (initial, replayed, fallback, sticky) = server.await.expect("sticky transport server");
    assert_eq!(
        initial, replayed,
        "WebSocket reconnect must reuse the snapshot"
    );
    assert_eq!(initial["gateway_routing"], "stable-route");
    assert_eq!(fallback["gateway_routing"], "stable-route");
    assert!(fallback.get("type").is_none());
    assert!(fallback.get("previous_response_id").is_none());
    assert_eq!(fallback["input"], initial["input"]);
    let sticky_wire = sticky["input"].to_string();
    for expected in ["fallback question", "fallback answer", "next question"] {
        assert!(
            sticky_wire.contains(expected),
            "missing {expected}: {sticky_wire}"
        );
    }
    assert!(sticky.get("previous_response_id").is_none());
}

#[tokio::test]
async fn transient_startup_preconnect_failure_is_retried_by_the_first_request() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (failed_handshake, _) = listener.accept().await.expect("startup handshake");
        drop(failed_handshake);

        let (stream, _) = listener.accept().await.expect("request reconnect");
        let mut socket = accept_async(stream).await.expect("request upgrade");
        let request = receive_json(&mut socket).await;
        send_json(
            &mut socket,
            completed_event("resp_preconnect", "msg_preconnect", "reconnected"),
        )
        .await;
        request
    });
    let config = TestProviderConfig {
        base_url: format!("ws://{address}/v1/responses"),
        api_key: "test-key".to_string(),
        models: test_models(),
        supports_websockets: true,
        reasoning: TestReasoningConfig::default(),
        compatibility: ResponsesCompatibilityConfig::default(),
        additional_params: Default::default(),
        compaction: RemoteCompactionConfig::default(),
    };
    let mut openai = OpenAiProvider::connect(
        &config.resolved(ModelRole::Build),
        config.reasoning.effort,
        "Test instructions",
        ToolServer::new().run(),
        "test-session-id",
        "test-session",
    )
    .await
    .expect("transient startup failure is best effort");
    assert_eq!(openai.transport, OpenAiTransport::WebSocket);
    assert_eq!(
        openai.ws.session.terminal_status(),
        Some(OpenAiWebSocketTerminalCategory::StartupConnectFailed)
    );

    let mut state = AttemptState::default();
    let parts = RequestParts {
        prompt: Message::user("retry startup"),
        history: Vec::new(),
        instructions: "Reconnect instructions".to_string(),
        allowed_tool_names: None,
    };
    run_turn_request_with_recovery(
        parts.request(),
        &mut openai,
        &mut state,
        &discard_updates(),
        &fast_recovery_policy(2),
    )
    .await
    .expect("first request should reconnect WebSocket");
    assert_eq!(openai.transport, OpenAiTransport::WebSocket);
    assert_eq!(continuation_response_id(&openai), Some("resp_preconnect"));
    let request = server.await.expect("preconnect server");
    assert!(request.get("previous_response_id").is_none());
}

#[tokio::test]
async fn http_tool_followup_replays_complete_native_history_without_continuation_ids() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let native_call = function_call("fc_http_tool", "call_http_tool", "count_once", json!({}));
    let server_call = native_call.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("tool-call request");
        let first = receive_http_json(&mut stream).await.body;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_output_event("resp_http_tool", vec![server_call]).to_string()),
        )
        .await;

        let (mut stream, _) = listener.accept().await.expect("tool-result follow-up");
        let second = receive_http_json(&mut stream).await.body;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(
                completed_event("resp_http_final", "msg_http_final", "tool finished").to_string(),
            ),
        )
        .await;
        (first, second)
    });

    let mut openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let mut state = AttemptState::default();
    run_turn("run the tool", &mut openai, &mut state, &discard_updates())
        .await
        .expect("HTTP tool call");
    assert_eq!(
        state
            .history_replays
            .last()
            .and_then(Option::as_ref)
            .expect("tool replay")
            .replay()
            .items,
        vec![native_call]
    );
    state.history.push(Message::tool_result(
        "call_http_tool",
        "count_once",
        "counted",
    ));
    state.history_replays.push(None);
    run_turn("finish now", &mut openai, &mut state, &discard_updates())
        .await
        .expect("HTTP tool result follow-up");

    let (first, second) = server.await.expect("HTTP tool server");
    assert!(first.get("previous_response_id").is_none());
    assert!(second.get("previous_response_id").is_none());
    assert!(second.get("type").is_none());
    let second_input = second["input"].to_string();
    for expected in [
        "run the tool",
        "function_call",
        "call_http_tool",
        "function_call_output",
        "counted",
        "finish now",
    ] {
        assert!(
            second_input.contains(expected),
            "missing {expected}: {second_input}"
        );
    }
    assert_eq!(
        attempt_message(&state),
        Message::Assistant {
            id: Some("msg_http_final".to_string()),
            content: vec![AssistantContent::text("tool finished")],
        }
    );
}

#[tokio::test]
async fn http_only_responses_gateway_supports_arbitrary_models_tools_and_replay() {
    async fn send_staged_sse(stream: &mut TcpStream, preview: Value, terminal: Value) {
        let preview = sse_data(preview.to_string());
        let terminal = sse_data(terminal.to_string());
        let body_len = preview.len() + terminal.len();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("SSE headers");
        stream
            .write_all(preview.as_bytes())
            .await
            .expect("preview SSE event");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        stream
            .write_all(terminal.as_bytes())
            .await
            .expect("terminal SSE event");
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Responses gateway listener");
    let address = listener.local_addr().expect("Responses gateway address");
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs_gateway",
        "summary": [{"type": "summary_text", "text": "checked gateway route"}],
        "content": [],
        "encrypted_content": "opaque-gateway-reasoning",
        "status": null
    });
    let call = function_call("fc_gateway", "call_gateway", "count_once", json!({}));
    let server_reasoning = reasoning.clone();
    let server_call = call.clone();
    let server = tokio::spawn(async move {
        let (mut first_stream, _) = listener.accept().await.expect("first Responses request");
        let first = receive_http_json(&mut first_stream).await;
        let first_headers = first.headers.to_ascii_lowercase();
        assert!(
            first_headers.starts_with("post /gateway/v1/responses?deployment=compat http/1.1"),
            "{}",
            first.headers
        );
        assert!(first_headers.contains("authorization: bearer gateway-secret"));
        assert_eq!(first.body["model"], "deepseek-reasoner");
        assert_eq!(first.body["gateway_routing"], "deepseek-and-glm");
        assert_eq!(first.body["thinking"], json!({"type": "enabled"}));
        assert!(first.body.get("reasoning").is_none());
        assert!(first.body.get("prompt_cache_key").is_none());
        assert!(first.body.get("store").is_none());
        assert!(
            first.body["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().all(|tool| tool.get("strict").is_none()))
        );
        let mut first_terminal =
            completed_output_event("resp_gateway_tool", vec![server_reasoning, server_call]);
        first_terminal["response"]["usage"] = json!({
            "input_tokens": 80,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 20,
            "total_tokens": 100
        });
        send_staged_sse(
            &mut first_stream,
            output_text_delta("Gateway tool preview", 2),
            first_terminal,
        )
        .await;

        let (mut second_stream, _) = listener.accept().await.expect("tool result continuation");
        let second = receive_http_json(&mut second_stream).await;
        let second_headers = second.headers.to_ascii_lowercase();
        assert!(
            second_headers.starts_with("post /gateway/v1/responses?deployment=compat http/1.1")
        );
        assert!(second_headers.contains("authorization: bearer gateway-secret"));
        assert_eq!(second.body["model"], "deepseek-reasoner");
        assert_eq!(second.body["gateway_routing"], "deepseek-and-glm");
        assert_eq!(second.body["thinking"], json!({"type": "enabled"}));
        assert!(second.body.get("previous_response_id").is_none());
        let second_input = second.body["input"].to_string();
        for expected in [
            "function_call",
            "call_gateway",
            "function_call_output",
            "counted",
        ] {
            assert!(
                second_input.contains(expected),
                "missing {expected}: {second_input}"
            );
        }
        send_staged_sse(
            &mut second_stream,
            output_text_delta("Gateway final preview", 0),
            completed_event_with_usage(
                "resp_gateway_final",
                "msg_gateway_final",
                "Gateway complete",
            ),
        )
        .await;
        (first.body, second.body)
    });

    let executions = Arc::new(AtomicUsize::new(0));
    let tools = ToolServer::new()
        .tool(CountTool {
            executions: executions.clone(),
        })
        .run();
    let config = TestProviderConfig {
        base_url: format!("http://{address}/gateway/v1/responses?deployment=compat"),
        api_key: "gateway-secret".to_string(),
        models: TestModelsConfig {
            build: "deepseek-reasoner".to_string(),
            plan: "glm-4.5".to_string(),
            explore: "deepseek-chat".to_string(),
            builder: "builder-model".to_string(),
            review: "glm-4.5-air".to_string(),
        },
        supports_websockets: false,
        reasoning: TestReasoningConfig::default(),
        compatibility: ResponsesCompatibilityConfig {
            send_reasoning: false,
            send_reasoning_encrypted_content: false,
            strict_tools: false,
            send_prompt_cache_key: false,
            send_store: false,
            developer_messages: true,
        },
        additional_params: BTreeMap::from([
            ("gateway_routing".to_string(), json!("deepseek-and-glm")),
            ("thinking".to_string(), json!({"type": "enabled"})),
        ]),
        compaction: RemoteCompactionConfig::default(),
    };
    let openai = OpenAiProvider::connect(
        &config.resolved(ModelRole::Build),
        config.reasoning.effort,
        "Gateway test instructions",
        tools.clone(),
        "gateway-conversation-id",
        "gateway-root-session",
    )
    .await
    .expect("HTTP-only Responses gateway provider");
    assert_eq!(openai.transport, OpenAiTransport::Http);
    assert!(openai.compaction_url.is_none());

    let transcript_directory = tempfile::tempdir().expect("gateway transcript directory");
    let transcript =
        TranscriptWriter::create(transcript_directory.path()).expect("gateway transcript writer");
    let transcript_path = transcript.path().to_path_buf();
    let mut engine = SessionEngine::new(
        openai,
        tools,
        test_policies(),
        transcript,
        Arc::new(SkillCatalog::default()),
    )
    .expect("gateway session engine");
    let (event_tx, mut event_rx) = session_event_channel(256);
    let command = tokio::spawn(async move {
        engine
            .handle_command(
                SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: "Use the gateway tool".into(),
                    mode: SessionMode::Build,
                }),
                &event_tx,
            )
            .await
            .expect("valid replay");
        engine
    });

    let mut saw_preview = false;
    let mut saw_tool_results = false;
    let mut usage_updates = 0usize;
    loop {
        let update = tokio::time::timeout(std::time::Duration::from_secs(2), event_rx.recv())
            .await
            .expect("gateway session event timeout")
            .expect("gateway event channel");
        match update {
            SessionUpdate::Streams(batch) => {
                if batch.root.is_some_and(|state| {
                    state
                        .message
                        .is_some_and(|message| format!("{message:?}").contains("Gateway"))
                }) {
                    saw_preview = true;
                }
            }
            SessionUpdate::Lifecycle(SessionEvent::ToolResults { .. }) => {
                saw_tool_results = true;
            }
            SessionUpdate::Lifecycle(SessionEvent::UsageUpdated { .. }) => {
                usage_updates += 1;
            }
            SessionUpdate::Lifecycle(SessionEvent::TurnCompleted { .. }) => break,
            SessionUpdate::Lifecycle(_) => {}
        }
    }

    let engine = command.await.expect("gateway session task");
    assert!(saw_preview, "Responses deltas should produce a preview");
    assert!(
        saw_tool_results,
        "the function call should execute and correlate"
    );
    assert_eq!(usage_updates, 2);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        engine.history().last(),
        Some(&Message::Assistant {
            id: Some("msg_gateway_final".to_string()),
            content: vec![AssistantContent::text("Gateway complete")],
        })
    );
    drop(engine);

    let (first, second) = server.await.expect("Responses gateway server");
    assert_eq!(first["gateway_routing"], second["gateway_routing"]);
    assert_eq!(first["thinking"], second["thinking"]);

    let loaded = transcript::load(&transcript_path).expect("gateway transcript reload");
    let replays = loaded
        .iter()
        .filter_map(TranscriptItem::provider_replay)
        .collect::<Vec<_>>();
    assert_eq!(replays.len(), 2);
    assert_eq!(replays[0].items, vec![reasoning, call]);
    assert_eq!(
        replays[1].to_message().expect("final replay projection"),
        Message::Assistant {
            id: Some("msg_gateway_final".to_string()),
            content: vec![AssistantContent::text("Gateway complete")],
        }
    );
    assert!(
        loaded
            .iter()
            .any(|item| matches!(item, TranscriptItem::ToolResults { .. }))
    );
    let raw = std::fs::read_to_string(transcript_path).expect("gateway transcript bytes");
    assert!(raw.contains(r#""provider":"openai.responses""#));
}

#[tokio::test]
async fn cancelling_an_http_completion_drops_the_live_sse_stream() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let (stream_started, started) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let _request = receive_http_json(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("SSE headers");
        let event = sse_data(output_text_delta("in progress", 1).to_string());
        let chunk = format!("{:X}\r\n{}\r\n", event.len(), event);
        stream.write_all(chunk.as_bytes()).await.expect("SSE chunk");
        let _ = stream_started.send(());

        let mut byte = [0_u8; 1];
        let closed =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.read(&mut byte))
                .await
                .expect("cancellation should close the HTTP stream promptly");
        matches!(closed, Ok(0) | Err(_))
    });

    let openai = connect_http_test_provider(
        format!("http://{address}/v1/responses"),
        ToolServer::new().run(),
    )
    .await;
    let request_task = tokio::spawn(async move {
        let mut openai = openai;
        let prompt = Message::user("cancel HTTP");
        let request = ModelRequest {
            instructions: test_instructions(),
            input: vec![ModelRequestItem::message(&prompt)],
            model_role: ModelRole::Build,
            allowed_tool_names: None,
        };
        openai.complete(request, discard_updates()).await
    });
    started.await.expect("HTTP stream should start");
    request_task.abort();
    assert!(
        request_task
            .await
            .expect_err("cancelled request task")
            .is_cancelled()
    );
    assert!(server.await.expect("HTTP cancellation server"));
}

#[tokio::test]
async fn factory_root_and_child_keep_independent_sticky_transport_state() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("transport listener");
    let address = listener.local_addr().expect("transport address");
    let server = tokio::spawn(async move {
        let (mut root_handshake, _) = listener.accept().await.expect("root handshake");
        let _headers = receive_http_headers(&mut root_handshake).await;
        root_handshake
            .write_all(
                b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("root 426");
        drop(root_handshake);

        let (child_stream, _) = listener.accept().await.expect("child handshake");
        let mut child_socket = accept_async(child_stream).await.expect("child WebSocket");

        let (mut root_http, _) = listener.accept().await.expect("root HTTP request");
        let root_request = receive_http_json(&mut root_http).await.body;
        send_http_response(
            &mut root_http,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_root_http", "msg_root_http", "root").to_string()),
        )
        .await;

        let child_request = receive_json(&mut child_socket).await;
        send_json(
            &mut child_socket,
            completed_event("resp_child_ws", "msg_child_ws", "child"),
        )
        .await;
        (root_request, child_request)
    });

    let factory = TestProviderFactory::new(
        TestProviderConfig {
            base_url: format!("http://{address}/v1/responses"),
            api_key: "test-key".to_string(),
            models: test_models(),
            supports_websockets: true,
            reasoning: TestReasoningConfig::default(),
            compatibility: ResponsesCompatibilityConfig::default(),
            additional_params: Default::default(),
            compaction: RemoteCompactionConfig::default(),
        },
        "Test instructions",
        ToolServer::new().run(),
    );
    let mut root = factory.connect("root").await.expect("root provider");
    let mut child = factory.connect("child").await.expect("child provider");
    assert_eq!(root.transport, OpenAiTransport::Http);
    assert_eq!(child.transport, OpenAiTransport::WebSocket);

    let mut root_state = AttemptState::default();
    let mut child_state = AttemptState::default();
    run_turn(
        "root request",
        &mut root,
        &mut root_state,
        &discard_updates(),
    )
    .await
    .expect("root HTTP turn");
    run_turn(
        "child request",
        &mut child,
        &mut child_state,
        &discard_updates(),
    )
    .await
    .expect("child WebSocket turn");

    assert_eq!(root.transport, OpenAiTransport::Http);
    assert_eq!(child.transport, OpenAiTransport::WebSocket);
    assert_eq!(continuation_response_id(&root), None);
    assert_eq!(continuation_response_id(&child), Some("resp_child_ws"));
    let (root_request, child_request) = server.await.expect("independent transport server");
    assert!(root_request.get("type").is_none());
    assert_eq!(child_request["type"], "response.create");
}

#[tokio::test]
async fn router_lazily_dispatches_role_specific_profiles_and_reuses_exact_matches() {
    let listener_a = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("profile A listener");
    let address_a = listener_a.local_addr().expect("profile A address");
    let server_a = tokio::spawn(async move {
        let mut requests = Vec::new();
        for index in 0..2 {
            let (mut stream, _) = listener_a.accept().await.expect("profile A request");
            let request = receive_http_json(&mut stream).await;
            send_http_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                sse_data(
                    completed_event(
                        &format!("resp_a_{index}"),
                        &format!("msg_a_{index}"),
                        "profile A answer",
                    )
                    .to_string(),
                ),
            )
            .await;
            requests.push(request);
        }
        requests
    });
    let listener_b = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("profile B listener");
    let address_b = listener_b.local_addr().expect("profile B address");
    let server_b = tokio::spawn(async move {
        let (mut stream, _) = listener_b.accept().await.expect("profile B request");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_b", "msg_b", "profile B answer").to_string()),
        )
        .await;
        request
    });

    let profile_a = resolved_profile(
        "provider-a",
        "model-a",
        format!("http://{address_a}/v1/responses"),
        "token-a",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::from([("gateway_routing".to_string(), json!("route-a"))]),
        RemoteCompactionConfig::default(),
        64_000,
    );
    let profile_b = resolved_profile(
        "provider-b",
        "model-b",
        format!("http://{address_b}/custom/responses"),
        "token-b",
        false,
        ReasoningSummaryLevel::Concise,
        ResponsesCompatibilityConfig {
            send_reasoning: false,
            send_reasoning_encrypted_content: false,
            strict_tools: false,
            send_prompt_cache_key: false,
            send_store: false,
            developer_messages: true,
        },
        BTreeMap::from([("gateway_routing".to_string(), json!("route-b"))]),
        RemoteCompactionConfig::default(),
        32_000,
    );
    let profile_a_cache_key = crate::router::profile_cache_key("root-session", &profile_a);
    let tools = policy_tools();
    let mut router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                profile_a.clone(),
                zevria_foundation::ReasoningLevel::Low,
            ),
            (
                ModelRole::Plan,
                profile_b.clone(),
                zevria_foundation::ReasoningLevel::High,
            ),
            (
                ModelRole::Review,
                profile_a,
                zevria_foundation::ReasoningLevel::Low,
            ),
        ],
        "Router preamble",
        tools,
        "root-session",
    )
    .expect("router");
    assert_eq!(router.initialized_profile_count(), 0);
    assert!(router.requires_portable_compaction());

    let build = RequestParts {
        prompt: Message::user("build request"),
        history: Vec::new(),
        instructions: "Build route instructions".to_string(),
        allowed_tool_names: Some(vec!["command".to_string()]),
    };
    router
        .complete(build.request_with_role(ModelRole::Build), discard_updates())
        .await
        .expect("Build profile");
    assert_eq!(router.initialized_profile_count(), 1);

    let plan = RequestParts {
        prompt: Message::user("plan request"),
        history: Vec::new(),
        instructions: "Plan route instructions".to_string(),
        allowed_tool_names: Some(vec!["write".to_string()]),
    };
    router
        .complete(plan.request_with_role(ModelRole::Plan), discard_updates())
        .await
        .expect("Plan profile");
    assert_eq!(router.initialized_profile_count(), 2);

    let review = RequestParts {
        prompt: Message::user("review request"),
        history: Vec::new(),
        instructions: "Review route instructions".to_string(),
        allowed_tool_names: Some(vec!["command".to_string()]),
    };
    router
        .complete(
            review.request_with_role(ModelRole::Review),
            discard_updates(),
        )
        .await
        .expect("Review reuses profile A");
    assert_eq!(router.initialized_profile_count(), 2);

    let requests_a = server_a.await.expect("profile A server");
    let request_b = server_b.await.expect("profile B server");
    for request in &requests_a {
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer token-a")
        );
        assert_eq!(request.body["model"], "model-a");
        assert_eq!(request.body["reasoning"]["effort"], "low");
        assert_eq!(request.body["reasoning"]["summary"], "detailed");
        assert_eq!(request.body["gateway_routing"], "route-a");
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["prompt_cache_key"], profile_a_cache_key);
        assert_eq!(request.body["tools"].as_array().expect("A tools").len(), 1);
        assert_eq!(request.body["tools"][0]["strict"], true);
    }
    assert_eq!(
        requests_a[0].body["instructions"],
        rendered_test_instructions("Build route instructions")
    );
    assert_eq!(
        requests_a[1].body["instructions"],
        rendered_test_instructions("Review route instructions")
    );

    assert!(
        request_b
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer token-b")
    );
    assert_eq!(request_b.body["model"], "model-b");
    assert!(request_b.body.get("reasoning").is_none());
    assert!(request_b.body.get("prompt_cache_key").is_none());
    assert!(request_b.body.get("store").is_none());
    assert_eq!(request_b.body["gateway_routing"], "route-b");
    assert_eq!(
        request_b.body["tools"].as_array().expect("B tools").len(),
        1
    );
    assert!(request_b.body["tools"][0].get("strict").is_none());
}

#[tokio::test]
async fn multi_profile_router_rejects_remote_compaction_without_connection() {
    let profile_a = resolved_profile(
        "a",
        "model-a",
        "http://127.0.0.1:1/v1/responses".to_string(),
        "a-key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig {
            url: Some("http://127.0.0.1:1/v1/responses/compact".to_string()),
            request_timeout_seconds: 1,
        },
        100_000,
    );
    let profile_b = resolved_profile(
        "b",
        "model-b",
        "http://127.0.0.1:2/v1/responses".to_string(),
        "b-key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        100_000,
    );
    let mut router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                profile_a,
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Plan,
                profile_b,
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "preamble",
        ToolServer::new().run(),
        "root",
    )
    .expect("multi-profile router");
    let parts = RequestParts {
        prompt: Message::user("compact me"),
        history: Vec::new(),
        instructions: "Build".to_string(),
        allowed_tool_names: None,
    };
    let result = router
        .compact(parts.maintenance_request())
        .await
        .expect("portable compaction policy");
    assert_eq!(result, CompactResult::Unsupported);
    assert_eq!(router.initialized_profile_count(), 0);
}

#[tokio::test]
async fn single_profile_router_forwards_configured_remote_compaction() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("compaction listener");
    let address = listener.local_addr().expect("compaction address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("compaction request");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "application/json",
            json!({
                "output": [{
                    "type": "compaction",
                    "encrypted_content": "opaque-remote-checkpoint"
                }]
            })
            .to_string(),
        )
        .await;
        request
    });
    let profile = resolved_profile(
        "single",
        "single-model",
        format!("http://{address}/v1/responses"),
        "single-key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig {
            url: Some(format!("http://{address}/v1/responses/compact")),
            request_timeout_seconds: 5,
        },
        100_000,
    );
    let mut router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                profile.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Plan,
                profile.clone(),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Review,
                profile,
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "preamble",
        ToolServer::new().run(),
        "root",
    )
    .expect("single-profile root router");
    assert!(!router.requires_portable_compaction());
    let parts = RequestParts {
        prompt: Message::user("history to compact"),
        history: Vec::new(),
        instructions: "Build".to_string(),
        allowed_tool_names: None,
    };
    let result = router
        .compact(parts.maintenance_request())
        .await
        .expect("remote compaction");
    let CompactResult::Replacement(items) = result else {
        panic!("configured remote compaction must return replacement history");
    };
    assert_eq!(router.initialized_profile_count(), 1);
    let replay = items[0].replay_ref().expect("opaque replacement replay");
    assert_eq!(
        replay.source_profile,
        zevria_foundation::ModelProfileRef::new("single", "single-model")
    );
    assert_eq!(
        replay.items[0]["encrypted_content"],
        "opaque-remote-checkpoint"
    );
    let request = server.await.expect("compaction server");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer single-key")
    );
    assert_eq!(request.body["model"], "single-model");
    assert!(request.body.get("include").is_none());
}

#[tokio::test]
async fn failed_profile_does_not_poison_another_router_slot() {
    let failing = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failing listener");
    let failing_address = failing.local_addr().expect("failing address");
    let failing_server = tokio::spawn(async move {
        let (mut stream, _) = failing.accept().await.expect("failing request");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "401 Unauthorized",
            "application/json",
            json!({"error": "bad profile"}).to_string(),
        )
        .await;
        request
    });
    let healthy = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("healthy listener");
    let healthy_address = healthy.local_addr().expect("healthy address");
    let healthy_server = tokio::spawn(async move {
        let (mut stream, _) = healthy.accept().await.expect("healthy request");
        let request = receive_http_json(&mut stream).await;
        send_http_response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            sse_data(completed_event("resp_healthy", "msg_healthy", "healthy").to_string()),
        )
        .await;
        request
    });
    let profile = |provider: &str, model: &str, address: std::net::SocketAddr, key: &str| {
        resolved_profile(
            provider,
            model,
            format!("http://{address}/v1/responses"),
            key,
            false,
            ReasoningSummaryLevel::Detailed,
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            100_000,
        )
    };
    let mut router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Plan,
                profile("bad", "bad-model", failing_address, "bad-key"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Build,
                profile("good", "good-model", healthy_address, "good-key"),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "preamble",
        ToolServer::new().run(),
        "root",
    )
    .expect("router");
    let parts = |text: &str| RequestParts {
        prompt: Message::user(text),
        history: Vec::new(),
        instructions: text.to_string(),
        allowed_tool_names: None,
    };
    let plan = parts("plan");
    let error = router
        .complete(plan.request_with_role(ModelRole::Plan), discard_updates())
        .await
        .expect_err("Plan profile should fail");
    assert!(error.to_string().contains("401"));

    let build = parts("build");
    let response = router
        .complete(build.request_with_role(ModelRole::Build), discard_updates())
        .await
        .expect("Build profile remains healthy");
    assert_eq!(
        response.message(),
        &Message::Assistant {
            id: Some("msg_healthy".to_string()),
            content: vec![AssistantContent::text("healthy")],
        }
    );
    assert!(
        failing_server
            .await
            .expect("failing server")
            .headers
            .contains("bad-key")
    );
    assert!(
        healthy_server
            .await
            .expect("healthy server")
            .headers
            .contains("good-key")
    );
}

#[tokio::test]
async fn router_keeps_profile_local_continuations_across_switches_and_active_cancel() {
    let listener_a = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("profile A websocket listener");
    let address_a = listener_a
        .local_addr()
        .expect("profile A websocket address");
    let server_a = tokio::spawn(async move {
        let (stream, _) = listener_a.accept().await.expect("profile A websocket");
        let mut socket = accept_async(stream).await.expect("profile A upgrade");
        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        send_json(
            &mut socket,
            completed_event("resp_a_1", "msg_a_1", "answer from A"),
        )
        .await;

        let second = receive_json(&mut socket).await;
        assert_eq!(second["previous_response_id"], "resp_a_1");
        let suffix = second["input"].to_string();
        for expected in ["question for B", "answer from B", "back to A"] {
            assert!(suffix.contains(expected), "missing {expected}: {suffix}");
        }
        send_json(
            &mut socket,
            completed_event("resp_a_2", "msg_a_2", "A continued"),
        )
        .await;
        (first, second)
    });
    let listener_b = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("profile B websocket listener");
    let address_b = listener_b
        .local_addr()
        .expect("profile B websocket address");
    let server_b = tokio::spawn(async move {
        let (stream, _) = listener_b.accept().await.expect("profile B websocket");
        let mut socket = accept_async(stream).await.expect("profile B upgrade");
        let first = receive_json(&mut socket).await;
        assert!(first.get("previous_response_id").is_none());
        let wire = first["input"].to_string();
        assert!(wire.contains("question for A"));
        assert!(wire.contains("answer from A"));
        assert!(wire.contains("question for B"));
        send_json(
            &mut socket,
            completed_event("resp_b_1", "msg_b_1", "answer from B"),
        )
        .await;
        first
    });

    let profile = |provider: &str, model: &str, address: std::net::SocketAddr| {
        resolved_profile(
            provider,
            model,
            format!("ws://{address}/v1/responses"),
            "test-key",
            true,
            ReasoningSummaryLevel::Detailed,
            ResponsesCompatibilityConfig::default(),
            BTreeMap::new(),
            RemoteCompactionConfig::default(),
            100_000,
        )
    };
    let mut router = ResponsesRouter::from_routes(
        [
            (
                ModelRole::Build,
                profile("provider-a", "model-a", address_a),
                zevria_foundation::ReasoningLevel::Medium,
            ),
            (
                ModelRole::Plan,
                profile("provider-b", "model-b", address_b),
                zevria_foundation::ReasoningLevel::Medium,
            ),
        ],
        "router preamble",
        ToolServer::new().run(),
        "root-session",
    )
    .expect("router");

    let prompt_a = Message::user("question for A");
    let response_a = router
        .complete(
            ModelRequest {
                instructions: test_instructions(),
                input: vec![ModelRequestItem::message(&prompt_a)],
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .expect("first A response");
    let replay_a = response_a.record().model_request_item();
    assert!(matches!(replay_a, ModelRequestItem::ReplayBacked(_)));

    let prompt_b = Message::user("question for B");
    let response_b = router
        .complete(
            ModelRequest {
                instructions: test_instructions(),
                input: vec![
                    ModelRequestItem::message(&prompt_a),
                    replay_a,
                    ModelRequestItem::message(&prompt_b),
                ],
                model_role: ModelRole::Plan,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .expect("B response");
    let replay_b = response_b.record().model_request_item();
    assert!(matches!(replay_b, ModelRequestItem::ReplayBacked(_)));
    let profile_a_ref = zevria_foundation::ModelProfileRef::new("provider-a", "model-a");
    let profile_b_ref = zevria_foundation::ModelProfileRef::new("provider-b", "model-b");
    assert_eq!(
        router.continuation_response_id(&profile_a_ref),
        Some("resp_a_1")
    );
    assert_eq!(
        router.continuation_response_id(&profile_b_ref),
        Some("resp_b_1")
    );

    // B was the most recently dispatched slot. Cancelling it must not disturb
    // A's parked continuation.
    router.cancel();
    assert_eq!(
        router.continuation_response_id(&profile_a_ref),
        Some("resp_a_1")
    );
    assert_eq!(router.continuation_response_id(&profile_b_ref), None);

    let prompt_a2 = Message::user("back to A");
    router
        .complete(
            ModelRequest {
                instructions: test_instructions(),
                input: vec![
                    ModelRequestItem::message(&prompt_a),
                    replay_a,
                    ModelRequestItem::message(&prompt_b),
                    replay_b,
                    ModelRequestItem::message(&prompt_a2),
                ],
                model_role: ModelRole::Build,
                allowed_tool_names: None,
            },
            discard_updates(),
        )
        .await
        .expect("A continuation survives B cancellation");
    assert_eq!(
        router.continuation_response_id(&profile_a_ref),
        Some("resp_a_2")
    );
    router.reset();
    assert_eq!(router.continuation_response_id(&profile_a_ref), None);
    assert_eq!(router.continuation_response_id(&profile_b_ref), None);

    let (a_first, a_second) = server_a.await.expect("profile A server");
    let b_first = server_b.await.expect("profile B server");
    assert_ne!(a_first["prompt_cache_key"], b_first["prompt_cache_key"]);
    assert_eq!(a_first["prompt_cache_key"], a_second["prompt_cache_key"]);
}

#[test]
fn explore_factory_creates_fresh_uninitialized_router_state() {
    let profile = resolved_profile(
        "explore-provider",
        "explore-model",
        "http://127.0.0.1:1/v1/responses".to_string(),
        "key",
        false,
        ReasoningSummaryLevel::Detailed,
        ResponsesCompatibilityConfig::default(),
        BTreeMap::new(),
        RemoteCompactionConfig::default(),
        100_000,
    );
    let factory = ResponsesRouterFactory::new(
        ModelRole::Explore,
        profile,
        zevria_foundation::ReasoningLevel::Medium,
        "preamble",
    );
    let first = factory
        .create("child-a", ToolServer::new().run())
        .expect("first child router");
    let second = factory
        .create("child-b", ToolServer::new().run())
        .expect("second child router");
    assert_eq!(first.initialized_profile_count(), 0);
    assert_eq!(second.initialized_profile_count(), 0);
    assert!(!first.requires_portable_compaction());
    assert!(!second.requires_portable_compaction());
}

// Permanent, direct wire comparisons: no diagnostic hashes or sidecars.
fn wire_request_properties(request: &Value) -> Value {
    let mut properties = request.as_object().unwrap().clone();
    for key in ["input", "previous_response_id", "type", "stream"] {
        properties.remove(key);
    }
    Value::Object(properties)
}

/// Ordinary fixture shared by prefix regressions. Usage is synthetic evidence,
/// not a simulation of real upstream tokenization or cache placement.
async fn incident_prefix_flow(developer_messages: bool, cached_tokens: [u64; 4]) -> Vec<Value> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let mut requests = Vec::new();
        let mut full_input = Vec::<Value>::new();
        let mut previous_output = Vec::<Value>::new();
        for (index, cached) in cached_tokens.into_iter().enumerate() {
            if index == 2 {
                let (stream, _) = listener.accept().await.unwrap();
                socket = accept_async(stream).await.unwrap();
            }
            let request = receive_json(&mut socket).await;
            let input = request["input"].as_array().unwrap();
            if index == 0 || index == 2 {
                assert!(request.get("previous_response_id").is_none());
                if index == 2 {
                    full_input.extend(previous_output);
                    assert!(
                        input.starts_with(&full_input),
                        "old input plus native output must be exact"
                    );
                    assert_eq!(
                        input.len(),
                        full_input.len() + 1,
                        "only the second ordinary prompt is appended"
                    );
                    assert_eq!(
                        input.last().unwrap()["content"][0]["text"],
                        "second ordinary prompt"
                    );
                }
                full_input = input.clone();
            } else {
                assert_eq!(
                    request["previous_response_id"],
                    format!("resp_incident_{}", index - 1)
                );
                assert_eq!(input.len(), 1);
                assert_eq!(input[0]["type"], "function_call_output");
                assert_eq!(input[0]["call_id"], format!("call_incident_{}", index - 1));
                assert!(input[0]["output"].as_str().unwrap().contains("counted"));
                full_input.extend(previous_output);
                full_input.extend(input.iter().cloned());
            }
            assert_eq!(request["prompt_cache_key"], "test-session");
            assert!(
                request["instructions"]
                    .as_str()
                    .unwrap()
                    .contains("Build provider test instructions")
            );
            if let Some(first) = requests.first() {
                assert_eq!(
                    wire_request_properties(&request),
                    wire_request_properties(first)
                );
            }
            let mut event = if index % 2 == 0 {
                completed_output_event(
                    &format!("resp_incident_{index}"),
                    vec![
                        json!({"type":"reasoning", "id":format!("rs_incident_{index}"),
                        "summary":[{"type":"summary_text", "text":"synthetic reasoning"}],
                        "encrypted_content":"SYNTHETIC_OPAQUE_REASONING", "future":{"array":[2,1]}}),
                        json!({"type":"function_call", "id":format!("fc_incident_{index}"),
                        "call_id":format!("call_incident_{index}"), "name":"count_once",
                        "arguments":"{  }", "status":"completed"}),
                    ],
                )
            } else {
                completed_event(
                    &format!("resp_incident_{index}"),
                    &format!("msg_incident_{index}"),
                    "final assistant response",
                )
            };
            previous_output = event["response"]["output"].as_array().unwrap().clone();
            event["response"]["usage"] = json!({"input_tokens":123307,
                "input_tokens_details":{"cached_tokens":cached}, "output_tokens":100, "total_tokens":123407});
            send_json(&mut socket, event).await;
            requests.push(request);
            if index == 1 {
                // An actual terminated parked pump, not an idle timeout heuristic.
                socket.close(None).await.unwrap();
            }
        }
        requests
    });
    let executions = Arc::new(AtomicUsize::new(0));
    let tools = ToolServer::new()
        .tool(CountTool {
            executions: executions.clone(),
        })
        .run();
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut provider = test_session_with_url(&url, socket, None);
    provider.tools = tools.clone();
    provider.compatibility.developer_messages = developer_messages;
    provider.compatibility.send_prompt_cache_key = true;
    provider.responses_parameters = Some(json!({"prompt_cache_key":"test-session"}));
    let directory = tempfile::tempdir().unwrap();
    let writer = TranscriptWriter::create(directory.path()).unwrap();
    let mut engine = SessionEngine::new(
        provider,
        tools,
        test_policies(),
        writer,
        Arc::new(SkillCatalog::default()),
    )
    .unwrap();
    for (index, text) in ["first ordinary prompt", "second ordinary prompt"]
        .into_iter()
        .enumerate()
    {
        if index == 1 {
            assert_eq!(
                wait_for_terminal(&engine.provider().ws.session).await,
                OpenAiWebSocketTerminalCategory::CloseFrame
            );
        }
        let (events, mut receiver) = session_event_channel(256);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.handle_command(
                SessionCommand::Turn(zevria_session_api::TurnCommand::Submit {
                    behavior: zevria_foundation::RequestBehavior::Standard,
                    text: text.into(),
                    mode: SessionMode::Build,
                }),
                &events,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let mut completed = false;
        while let Ok(update) = receiver.try_recv() {
            if let SessionUpdate::Lifecycle(event) = update {
                assert!(
                    !matches!(
                        event,
                        SessionEvent::TurnFailed { .. } | SessionEvent::TurnRejected { .. }
                    ),
                    "{event:?}"
                );
                completed |= matches!(event, SessionEvent::TurnCompleted { .. });
            }
        }
        assert!(completed);
        assert_eq!(
            executions.load(Ordering::SeqCst),
            index + 1,
            "recovery must not rerun earlier tools"
        );
    }
    assert_eq!(engine.provider().ws.socket_generation, 1);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    requests
}

#[tokio::test]
async fn second_prompt_preserves_native_prefix_after_parked_socket_termination() {
    incident_prefix_flow(true, [121472, 121472, 3968, 122112]).await;
}
