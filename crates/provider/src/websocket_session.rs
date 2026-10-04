//! Continuously polled OpenAI WebSocket sessions.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};

use crate::connection::OpenAiWebSocketStream;

const COMMAND_CAPACITY: usize = 8;
const INBOUND_EVENT_CAPACITY: usize = 256;

/// A bounded, log-safe explanation for why the socket pump stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAiWebSocketTerminalCategory {
    CloseFrame,
    EndOfStream,
    ReadError,
    SendError,
    SendTimeout,
    ResponseIdleTimeout,
    NotConnected,
    PongError,
    ConnectionLimit,
    UpstreamDisconnect,
    InboundOverflow,
    PumpStopped,
    StartupConnectFailed,
    HttpFallback,
    LocalCancellation,
}

impl OpenAiWebSocketTerminalCategory {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CloseFrame => "close_frame",
            Self::EndOfStream => "end_of_stream",
            Self::ReadError => "read_error",
            Self::SendError => "send_error",
            Self::SendTimeout => "send_timeout",
            Self::ResponseIdleTimeout => "response_idle_timeout",
            Self::NotConnected => "not_connected",
            Self::PongError => "pong_error",
            Self::ConnectionLimit => "connection_limit",
            Self::UpstreamDisconnect => "upstream_disconnect",
            Self::InboundOverflow => "inbound_overflow",
            Self::PumpStopped => "pump_stopped",
            Self::StartupConnectFailed => "startup_connect_failed",
            Self::HttpFallback => "http_fallback",
            Self::LocalCancellation => "local_cancellation",
        }
    }
}

pub(crate) enum OpenAiWebSocketInbound {
    Message(WebSocketMessage),
    Error(WebSocketError),
}

enum OpenAiWebSocketCommand {
    Send {
        message: WebSocketMessage,
        result: oneshot::Sender<Result<(), WebSocketError>>,
    },
}

/// Owns a raw tungstenite stream in a background pump for the stream's entire
/// lifetime. The pump remains active while the provider is parked between
/// turns, which lets it answer Ping frames and observe closure immediately.
enum OpenAiWebSocketSessionState {
    Connected {
        commands: mpsc::Sender<OpenAiWebSocketCommand>,
        events: mpsc::Receiver<OpenAiWebSocketInbound>,
        terminal: watch::Receiver<Option<OpenAiWebSocketTerminalCategory>>,
        pump: tokio::task::JoinHandle<()>,
    },
    Disconnected,
}

pub(crate) struct OpenAiWebSocketSession {
    #[cfg(feature = "cache-diagnostics")]
    pub(crate) cache_diagnostics: crate::cache_diagnostics::socket::SocketDiagnostics,
    state: OpenAiWebSocketSessionState,
    forced_terminal: Option<OpenAiWebSocketTerminalCategory>,
}

impl OpenAiWebSocketSession {
    pub(crate) fn new(socket: OpenAiWebSocketStream) -> Self {
        Self::new_with_send_delay(socket, std::time::Duration::ZERO)
    }

    fn new_with_send_delay(socket: OpenAiWebSocketStream, send_delay: std::time::Duration) -> Self {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::channel(INBOUND_EVENT_CAPACITY);
        let (terminal_tx, terminal) = watch::channel(None);
        #[cfg(feature = "cache-diagnostics")]
        let cache_diagnostics = crate::cache_diagnostics::socket::SocketDiagnostics::default();
        let pump = tokio::spawn(pump_socket(
            #[cfg(feature = "cache-diagnostics")]
            cache_diagnostics.clone(),
            socket,
            send_delay,
            command_rx,
            event_tx,
            terminal_tx,
        ));

        Self {
            #[cfg(feature = "cache-diagnostics")]
            cache_diagnostics,
            state: OpenAiWebSocketSessionState::Connected {
                commands,
                events,
                terminal,
                pump,
            },
            forced_terminal: None,
        }
    }

    /// Represent a failed best-effort startup preconnect without creating a
    /// background task or a synthetic socket. A later recovery cycle can
    /// replace this state with a real session through `OpenAiParkedWebSocket`.
    pub(crate) fn disconnected(category: OpenAiWebSocketTerminalCategory) -> Self {
        Self {
            #[cfg(feature = "cache-diagnostics")]
            cache_diagnostics: crate::cache_diagnostics::socket::SocketDiagnostics::disconnected(
                category,
            ),
            state: OpenAiWebSocketSessionState::Disconnected,
            forced_terminal: Some(category),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_test_send_delay(
        socket: OpenAiWebSocketStream,
        send_delay: std::time::Duration,
    ) -> Self {
        Self::new_with_send_delay(socket, send_delay)
    }

    /// Queue one write and wait until the pump acknowledges the actual socket
    /// send. Closing either side of the command path is a connection loss.
    pub(crate) async fn send(&self, message: WebSocketMessage) -> Result<(), WebSocketError> {
        let OpenAiWebSocketSessionState::Connected { commands, .. } = &self.state else {
            return Err(WebSocketError::ConnectionClosed);
        };
        let (result, acknowledged) = oneshot::channel();
        commands
            .send(OpenAiWebSocketCommand::Send { message, result })
            .await
            .map_err(|_| WebSocketError::ConnectionClosed)?;
        acknowledged
            .await
            .unwrap_or(Err(WebSocketError::ConnectionClosed))
    }

    /// Receive the next event. Queue overflow is checked before yielding any
    /// buffered data because once an event is lost the response can no longer
    /// be accumulated safely.
    pub(crate) async fn next(&mut self) -> Option<OpenAiWebSocketInbound> {
        let forced_terminal = self.forced_terminal;
        let OpenAiWebSocketSessionState::Connected {
            events,
            terminal,
            pump,
            ..
        } = &mut self.state
        else {
            return None;
        };

        loop {
            if session_terminal_status(forced_terminal, terminal, pump)
                == Some(OpenAiWebSocketTerminalCategory::InboundOverflow)
            {
                return None;
            }

            match events.try_recv() {
                Ok(event) => return Some(event),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }

            if session_terminal_status(forced_terminal, terminal, pump).is_some() {
                return None;
            }

            tokio::select! {
                biased;
                event = events.recv() => {
                    if session_terminal_status(forced_terminal, terminal, pump)
                        == Some(OpenAiWebSocketTerminalCategory::InboundOverflow)
                    {
                        return None;
                    }
                    return event;
                }
                changed = terminal.changed() => {
                    if changed.is_err() && events.is_empty() {
                        return None;
                    }
                }
            }
        }
    }

    pub(crate) fn terminal_status(&self) -> Option<OpenAiWebSocketTerminalCategory> {
        if let Some(terminal) = self.forced_terminal {
            return Some(terminal);
        }
        match &self.state {
            OpenAiWebSocketSessionState::Connected { terminal, pump, .. } => {
                session_terminal_status(None, terminal, pump)
            }
            OpenAiWebSocketSessionState::Disconnected => {
                Some(OpenAiWebSocketTerminalCategory::PumpStopped)
            }
        }
    }

    /// Stop an ambiguous or server-declared unusable connection immediately.
    /// This makes a timeout or in-band terminal error durable across failed
    /// replacement handshakes and later turns.
    pub(crate) fn terminate(&mut self, category: OpenAiWebSocketTerminalCategory) {
        let observed = match &self.state {
            OpenAiWebSocketSessionState::Connected { terminal, .. } => *terminal.borrow(),
            OpenAiWebSocketSessionState::Disconnected => None,
        };
        let terminal = self.forced_terminal.or(observed).unwrap_or(category);
        self.forced_terminal = Some(terminal);
        #[cfg(feature = "cache-diagnostics")]
        self.cache_diagnostics.observed(terminal, "local");
        if let OpenAiWebSocketSessionState::Connected { pump, .. } = &self.state {
            pump.abort();
        }
    }
}

impl Drop for OpenAiWebSocketSession {
    fn drop(&mut self) {
        // Aborting drops the task-owned raw stream, closing the underlying
        // connection even if the pump is blocked in a write.
        if let OpenAiWebSocketSessionState::Connected { pump, .. } = &self.state {
            pump.abort();
        }
    }
}

fn session_terminal_status(
    forced_terminal: Option<OpenAiWebSocketTerminalCategory>,
    terminal: &watch::Receiver<Option<OpenAiWebSocketTerminalCategory>>,
    pump: &tokio::task::JoinHandle<()>,
) -> Option<OpenAiWebSocketTerminalCategory> {
    forced_terminal.or(*terminal.borrow()).or_else(|| {
        pump.is_finished()
            .then_some(OpenAiWebSocketTerminalCategory::PumpStopped)
    })
}

async fn pump_socket(
    #[cfg(feature = "cache-diagnostics")]
    cache_diagnostics: crate::cache_diagnostics::socket::SocketDiagnostics,
    mut socket: OpenAiWebSocketStream,
    send_delay: std::time::Duration,
    mut commands: mpsc::Receiver<OpenAiWebSocketCommand>,
    events: mpsc::Sender<OpenAiWebSocketInbound>,
    terminal: watch::Sender<Option<OpenAiWebSocketTerminalCategory>>,
) {
    let category = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    break OpenAiWebSocketTerminalCategory::PumpStopped;
                };
                match command {
                    OpenAiWebSocketCommand::Send { message, result } => {
                        if !send_delay.is_zero() {
                            tokio::time::sleep(send_delay).await;
                        }
                        match socket.send(message).await {
                            Ok(()) => {
                                let _ = result.send(Ok(()));
                            }
                            Err(error) => {
                                #[cfg(feature = "cache-diagnostics")]
                                cache_diagnostics.error(OpenAiWebSocketTerminalCategory::SendError, &error);
                                let _ = result.send(Err(error));
                                break OpenAiWebSocketTerminalCategory::SendError;
                            }
                        }
                    }
                }
            }
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(WebSocketMessage::Ping(payload))) => {
                        if let Err(error) = socket.send(WebSocketMessage::Pong(payload)).await {
                            #[cfg(feature = "cache-diagnostics")]
                            cache_diagnostics.error(OpenAiWebSocketTerminalCategory::PongError, &error);
                            let category = forward_event(
                                &events,
                                OpenAiWebSocketInbound::Error(error),
                            )
                            .err()
                            .unwrap_or(OpenAiWebSocketTerminalCategory::PongError);
                            break category;
                        }
                    }
                    Some(Ok(WebSocketMessage::Pong(_))) => {}
                    Some(Ok(message @ (WebSocketMessage::Text(_)
                        | WebSocketMessage::Binary(_)
                        | WebSocketMessage::Close(_)))) => {
                        let is_close = matches!(message, WebSocketMessage::Close(_));
                        #[cfg(feature = "cache-diagnostics")]
                        if is_close {
                            cache_diagnostics.observed(OpenAiWebSocketTerminalCategory::CloseFrame, "pump");
                        }
                        if let Err(category) = forward_event(
                            &events,
                            OpenAiWebSocketInbound::Message(message),
                        ) {
                            break category;
                        }
                        if is_close {
                            break OpenAiWebSocketTerminalCategory::CloseFrame;
                        }
                    }
                    Some(Ok(WebSocketMessage::Frame(_))) => {}
                    Some(Err(error)) => {
                        #[cfg(feature = "cache-diagnostics")]
                        cache_diagnostics.error(OpenAiWebSocketTerminalCategory::ReadError, &error);
                        let category = forward_event(
                            &events,
                            OpenAiWebSocketInbound::Error(error),
                        )
                        .err()
                        .unwrap_or(OpenAiWebSocketTerminalCategory::ReadError);
                        break category;
                    }
                    None => break OpenAiWebSocketTerminalCategory::EndOfStream,
                }
            }
        }
    };

    #[cfg(feature = "cache-diagnostics")]
    cache_diagnostics.finished(category);
    terminal.send_replace(Some(category));
}

fn forward_event(
    events: &mpsc::Sender<OpenAiWebSocketInbound>,
    event: OpenAiWebSocketInbound,
) -> Result<(), OpenAiWebSocketTerminalCategory> {
    events.try_send(event).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => OpenAiWebSocketTerminalCategory::InboundOverflow,
        mpsc::error::TrySendError::Closed(_) => OpenAiWebSocketTerminalCategory::PumpStopped,
    })
}
