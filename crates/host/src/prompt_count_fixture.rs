//! Signal-controlled loopback provider for native prompt-admission tests.

use std::{net::SocketAddr, path::Path};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::{JoinHandle, JoinSet},
};

pub struct BlockedPromptCount {
    addr: SocketAddr,
    pub count_started: oneshot::Receiver<()>,
    pub completion_started: oneshot::Receiver<()>,
    pub release_count: Option<oneshot::Sender<()>>,
    server: JoinHandle<()>,
}

impl BlockedPromptCount {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (count_signal, count_started) = oneshot::channel();
        let (completion_signal, completion_started) = oneshot::channel();
        let (release_count, release) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut count = Some((count_signal, release));
            let mut completion = Some(completion_signal);
            let mut requests = JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (endpoint, mut stream) = read_request(stream).await;
                match endpoint.as_str() {
                    "POST /v1/responses/input_tokens HTTP/1.1" => {
                        let (signal, release) = count.take().expect("one admission count");
                        signal.send(()).unwrap();
                        requests.spawn(async move {
                            if release.await.is_ok() {
                                let body = r#"{"input_tokens":100}"#;
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                stream.write_all(response.as_bytes()).await.unwrap();
                            }
                        });
                    }
                    "POST /v1/responses HTTP/1.1" => {
                        completion.take().expect("one model call").send(()).unwrap();
                        // Keep the completion unsent so the test observes the
                        // accepted prompt independently of a model response.
                        requests.spawn(async move {
                            let _ = stream.read_u8().await;
                        });
                    }
                    _ => panic!("unexpected mock provider endpoint: {endpoint}"),
                }
            }
        });
        Self {
            addr,
            count_started,
            completion_started,
            release_count: Some(release_count),
            server,
        }
    }

    pub fn config(&self, workspace: &Path) -> crate::Config {
        // Push the local assessment across the count threshold even for short
        // drafts. The mocked exact count fits without needing compaction.
        std::fs::write(
            workspace.join("AGENTS.md"),
            "Test guidance for measuring the prompt.\n".repeat(512),
        )
        .unwrap();
        super::test_support::write_fixture(
            &workspace.join("config.toml"),
            &format!(
                r#"
[session.compaction]
auto_trigger_percent = 1
[providers.fixture]
base_url = "http://{}/v1/responses"
api_key = "fixture"
supports_websockets = false
[providers.fixture.input_token_count]
enabled = true
[providers.fixture.models.fixture]
context_window_tokens = 64000
retained_user_tokens = 128
reasoning_levels = ["medium"]
reasoning_summary_level = "detailed"
[modes]
build = {{ provider = "fixture", model = "fixture", reasoning_level = "medium" }}
plan = {{ provider = "fixture", model = "fixture", reasoning_level = "medium" }}
review = {{ provider = "fixture", model = "fixture", reasoning_level = "medium" }}
explore = {{ provider = "fixture", model = "fixture", reasoning_level = "medium" }}
builder = {{ provider = "fixture", model = "fixture", reasoning_level = "medium" }}
"#,
                self.addr
            ),
        )
        .unwrap()
    }
}

impl Drop for BlockedPromptCount {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn read_request(stream: TcpStream) -> (String, TcpStream) {
    let mut stream = BufReader::new(stream);
    let mut request = String::new();
    stream.read_line(&mut request).await.unwrap();
    let mut bytes = 0;
    loop {
        let mut header = String::new();
        assert!(stream.read_line(&mut header).await.unwrap() > 0);
        if header == "\r\n" {
            break;
        }
        if let Some((name, len)) = header.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            bytes = len.trim().parse().unwrap();
        }
    }
    stream.read_exact(&mut vec![0; bytes]).await.unwrap();
    (request.trim_end().to_owned(), stream.into_inner())
}
