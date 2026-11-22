//! Transport setup, safe diagnostics, and cancellable requests.
use std::{
    collections::VecDeque,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::*,
    service::{
        NotificationContext, Peer, PeerRequestOptions, RequestContext, RunningService,
        RxJsonRpcMessage, ServiceError, TxJsonRpcMessage,
    },
    transport::{
        Transport,
        async_rw::AsyncRwTransport,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
    sync::mpsc,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use zevria_foundation::{McpServerName, process::ProcessTree};

use crate::{
    config::{McpServerConfig, McpTransportKind, direct_command},
    http_client::AuthenticatingHttpClient,
};

const STDERR_CAP: usize = 4096;

#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<u8>>>);

impl StderrTail {
    pub(crate) fn text(&self, config: &McpServerConfig) -> Option<String> {
        let bytes: Vec<u8> = self.0.lock().unwrap().iter().copied().collect();
        if bytes.is_empty() {
            return None;
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        let inherited = std::env::vars_os().filter_map(|(name, value)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            [
                "SECRET",
                "TOKEN",
                "PASSWORD",
                "API_KEY",
                "APIKEY",
                "AUTHORIZATION",
            ]
            .iter()
            .any(|s| name.contains(s))
            .then(|| value.into_string().ok())
            .flatten()
        });
        let mut secrets: Vec<String> = config
            .env
            .values()
            .chain(config.args.iter())
            .cloned()
            .chain(inherited)
            .filter(|s| !s.is_empty())
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        for secret in secrets {
            text = text.replace(&secret, "[redacted]");
        }
        Some(crate::convert::display_text(&text, STDERR_CAP))
    }

    async fn drain(&self, name: McpServerName, mut stderr: ChildStderr) {
        let mut buf = [0; STDERR_CAP];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            // Never write server-controlled bytes to a frontend's stdout or the application log.
            tracing::debug!(target: "zevria::mcp::stderr", server = %name, bytes = n, "captured server stderr");
            let mut tail = self.0.lock().unwrap();
            let discard = (tail.len() + n).saturating_sub(STDERR_CAP);
            tail.drain(..discard);
            tail.extend(&buf[..n]);
        }
    }
}

pub(crate) struct ZevriaClientHandler {
    server: McpServerName,
    root: Option<String>,
    refresh: mpsc::Sender<()>,
}

// Roots are deprecated for new MCP protocol versions but still used by legacy servers.
#[allow(deprecated)]
impl ClientHandler for ZevriaClientHandler {
    fn get_info(&self) -> ClientConfig {
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(RootsCapabilities::default());
        ClientConfig::new(
            capabilities,
            Implementation::new("zevria", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Ok(ListRootsResult::new(
            self.root
                .iter()
                .map(|uri| Root::new(uri.clone()).with_name("workspace"))
                .collect(),
        ))
    }

    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        let _ = self.refresh.try_send(());
    }
    async fn on_resource_list_changed(&self, _: NotificationContext<RoleClient>) {
        let _ = self.refresh.try_send(());
    }
    async fn on_prompt_list_changed(&self, _: NotificationContext<RoleClient>) {
        let _ = self.refresh.try_send(());
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        tracing::debug!(target: "zevria::mcp", server = %self.server, level = ?params.level, "received server log message");
    }
}

pub(crate) type Service = RunningService<RoleClient, ZevriaClientHandler>;

pub(crate) async fn connect(
    name: &McpServerName,
    config: &McpServerConfig,
    workspace: &Path,
    root: Option<String>,
    refresh: mpsc::Sender<()>,
    stderr_tail: StderrTail,
) -> anyhow::Result<Service> {
    let handler = ZevriaClientHandler {
        server: name.clone(),
        root,
        refresh,
    };
    match config.kind() {
        McpTransportKind::Stdio => {
            let cwd = config
                .cwd
                .as_ref()
                .map_or_else(|| workspace.to_path_buf(), |cwd| workspace.join(cwd));
            let mut command = direct_command(
                config.command.as_deref().expect("validated stdio config"),
                &cwd,
            )
            .map_err(|error| anyhow::anyhow!("MCP command not found ({:?})", error.kind()))?;
            command
                .current_dir(cwd)
                .args(&config.args)
                .envs(&config.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let (mut child, tree) = ProcessTree::spawn(&mut command).map_err(|error| {
                anyhow::anyhow!("could not launch MCP command ({:?})", error.kind())
            })?;
            let read = child.stdout.take().expect("piped stdout");
            let write = child.stdin.take().expect("piped stdin");
            let stderr = child.stderr.take().expect("piped stderr");
            let server = name.clone();
            tokio::spawn(async move {
                stderr_tail.drain(server, stderr).await;
            });
            let transport = StdioTransport {
                io: AsyncRwTransport::new(read, write),
                child,
                tree,
            };
            handler
                .serve(transport)
                .await
                .map_err(|_| anyhow::anyhow!("stdio MCP handshake failed"))
        }
        McpTransportKind::Http => {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let backend = AuthenticatingHttpClient::new(
                client,
                &config.headers,
                config.bearer_token.clone(),
            )?;
            let settings = StreamableHttpClientTransportConfig::with_uri(
                config.url.as_deref().expect("validated HTTP config"),
            )
            .reinit_on_expired_session(true)
            .max_error_response_size(4096);
            handler
                .serve(StreamableHttpClientTransport::with_client(
                    backend, settings,
                ))
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Streamable HTTP MCP handshake failed: {}",
                        safe_handshake_error(&error)
                    )
                })
        }
    }
}

fn safe_handshake_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut current = Some(error);
    while let Some(cause) = current {
        if let Some(status) =
            cause.downcast_ref::<rmcp::transport::streamable_http_client::HttpStatusError>()
        {
            return format!("HTTP {}", status.status);
        }
        current = cause.source();
    }
    "check the endpoint, credentials, and server logs".into()
}

/// Use the SDK's framed stdio transport, but retain our cross-platform process
/// group/job guard so child and grandchild processes cannot outlive the session.
struct StdioTransport {
    io: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
    child: Child,
    tree: ProcessTree,
}

impl Transport<RoleClient> for StdioTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.io.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.io.receive().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let _ = self.io.close().await;
        #[cfg(unix)]
        {
            if timeout(Duration::from_millis(250), self.child.wait())
                .await
                .is_ok()
            {
                return Ok(());
            }
            self.tree.terminate();
            if timeout(Duration::from_millis(250), self.child.wait())
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        #[cfg(not(unix))]
        {
            if timeout(Duration::from_millis(500), self.child.wait())
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        self.tree.kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum RequestFailure {
    Cancelled,
    Timeout,
    Service(ServiceError),
}

impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("MCP request cancelled"),
            Self::Timeout => f.write_str("MCP request timed out"),
            Self::Service(ServiceError::McpError(error)) => {
                write!(f, "MCP server returned JSON-RPC error {}", error.code.0)
            }
            Self::Service(ServiceError::TransportClosed) => f.write_str("MCP server disconnected"),
            Self::Service(_) => f.write_str("MCP request failed"),
        }
    }
}
impl std::error::Error for RequestFailure {}

pub(crate) async fn request(
    peer: &Peer<RoleClient>,
    params: ClientRequest,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ServerResult, RequestFailure> {
    let mut handle = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(RequestFailure::Cancelled),
        result = timeout_at(deadline, peer.send_cancellable_request(params, PeerRequestOptions::no_options())) => {
            result.map_err(|_| RequestFailure::Timeout)?.map_err(RequestFailure::Service)?
        }
    };
    let failure = tokio::select! {
        biased;
        _ = cancellation.cancelled() => RequestFailure::Cancelled,
        _ = tokio::time::sleep_until(deadline) => RequestFailure::Timeout,
        result = &mut handle.rx => return result.map_err(|_| RequestFailure::Service(ServiceError::TransportClosed))?.map_err(RequestFailure::Service),
    };
    // Enqueue the protocol cancellation before returning, but don't block the
    // turn on a server that refuses to accept the control message.
    let _ = timeout(
        Duration::from_millis(400),
        handle.cancel(Some(failure.to_string())),
    )
    .await;
    Err(failure)
}
