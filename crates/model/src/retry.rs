//! Transport-neutral retry reasons reported by provider adapters.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryCategory {
    RequestTimeout,
    RequestStartTimeout,
    ResponseIdleTimeout,
    CredentialHelper,
    RequestError,
    Transport,
    Stream,
    SseTransportError,
    PrematureEof,
    ConnectError,
    Disconnect,
    CloseFrame,
    EndOfStream,
    ReadError,
    SendError,
    SendTimeout,
    PongError,
    UpstreamDisconnect,
    PumpStopped,
    StartupConnectFailed,
    NotConnected,
    HttpFallback,
    LocalCancellation,
    ServerError,
    Overloaded,
    ResponseFailedTransient,
    RequestTimeoutStatus,
    ConnectionLimit,
    RateLimited,
    RateLimit,
    RateLimitExceeded,
    Protocol,
    InvalidToolJson,
    StaleResponseId,
    WebsocketUpgradeRequired,
    ThinkingBindingMismatch,
    InboundOverflow,
}

impl RetryCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestTimeout => "request_timeout",
            Self::RequestStartTimeout => "request_start_timeout",
            Self::ResponseIdleTimeout => "response_idle_timeout",
            Self::CredentialHelper => "credential_helper",
            Self::RequestError => "request_error",
            Self::Transport => "transport",
            Self::Stream => "stream",
            Self::SseTransportError => "sse_transport_error",
            Self::PrematureEof => "premature_eof",
            Self::ConnectError => "connect_error",
            Self::Disconnect => "disconnect",
            Self::CloseFrame => "close_frame",
            Self::EndOfStream => "end_of_stream",
            Self::ReadError => "read_error",
            Self::SendError => "send_error",
            Self::SendTimeout => "send_timeout",
            Self::PongError => "pong_error",
            Self::UpstreamDisconnect => "upstream_disconnect",
            Self::PumpStopped => "pump_stopped",
            Self::StartupConnectFailed => "startup_connect_failed",
            Self::NotConnected => "not_connected",
            Self::HttpFallback => "http_fallback",
            Self::LocalCancellation => "local_cancellation",
            Self::ServerError => "server_error",
            Self::Overloaded => "overloaded",
            Self::ResponseFailedTransient => "response_failed_transient",
            Self::RequestTimeoutStatus => "request_timeout_status",
            Self::ConnectionLimit => "connection_limit",
            Self::RateLimited => "rate_limited",
            Self::RateLimit => "rate_limit",
            Self::RateLimitExceeded => "rate_limit_exceeded",
            Self::Protocol => "protocol",
            Self::InvalidToolJson => "invalid_tool_json",
            Self::StaleResponseId => "stale_response_id",
            Self::WebsocketUpgradeRequired => "websocket_upgrade_required",
            Self::ThinkingBindingMismatch => "thinking_binding_mismatch",
            Self::InboundOverflow => "inbound_overflow",
        }
    }
}

impl std::fmt::Display for RetryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
