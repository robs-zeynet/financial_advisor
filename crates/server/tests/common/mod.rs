//! Test harness: a mock Messages API, the real app on an ephemeral loopback
//! port, and a fake WebSocket client.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, FrameHeader, ServerMsg};
use serde_json::Value;
use server::claude::ClaudeClient;
use server::config::Config;
use server::session::AppState;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub enum MockReply {
    /// 200 with an SSE body from `tests/fixtures/<name>`.
    Sse(&'static str),
    /// An HTTP error with optional `retry-after`.
    Status { status: u16, retry_after: Option<&'static str>, body: &'static str },
}

#[derive(Clone)]
pub struct Captured {
    pub headers: HeaderMap,
    pub body: Value,
}

pub struct MockClaude {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<Captured>>>,
}

impl MockClaude {
    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

pub async fn spawn_mock_claude(replies: Vec<MockReply>) -> MockClaude {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
    let reqs = requests.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap, body: Bytes| {
            let replies = replies.clone();
            let reqs = reqs.clone();
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap();
                reqs.lock().unwrap().push(Captured { headers, body });
                let reply = replies.lock().unwrap().pop_front().expect("unexpected extra request");
                let resp: Response = match reply {
                    MockReply::Sse(name) => {
                        ([("content-type", "text/event-stream")], fixture(name)).into_response()
                    }
                    MockReply::Status { status, retry_after, body } => {
                        let mut r = (StatusCode::from_u16(status).unwrap(), body.to_string()).into_response();
                        if let Some(ra) = retry_after {
                            r.headers_mut().insert("retry-after", ra.parse().unwrap());
                        }
                        r
                    }
                };
                resp
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MockClaude { base_url: format!("http://{addr}"), requests }
}

pub fn test_config(data_dir: &Path, base_url: &str) -> Config {
    let mut c = Config { bind: "127.0.0.1:0".into(), data_dir: data_dir.to_path_buf(), ..Config::default() };
    c.claude.base_url = base_url.into();
    c.claude.max_retries = 2;
    c
}

pub async fn spawn_app(config: Config) -> (SocketAddr, Arc<AppState>) {
    let app = Arc::new(AppState {
        claude: ClaudeClient::new(config.claude.clone(), Some("test-key".into())),
        config,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server::web::router(app.clone(), addr);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, app)
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct FakeClient {
    socket: Socket,
}

impl FakeClient {
    pub async fn connect(addr: SocketAddr) -> Self {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
        Self { socket }
    }

    pub async fn send(&mut self, msg: ClientMsg) {
        let text = serde_json::to_string(&msg).unwrap();
        self.socket.send(WsMessage::Text(text.into())).await.unwrap();
    }

    pub async fn send_frame(&mut self, t: u64, jpeg: &[u8]) {
        let bytes = FrameHeader { captured_at_ms: t, width: 640, height: 400, changed: true }.encode(jpeg);
        self.socket.send(WsMessage::Binary(bytes.into())).await.unwrap();
    }

    pub async fn recv(&mut self) -> ServerMsg {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                .await
                .expect("timed out waiting for server message")
                .expect("socket closed")
                .unwrap();
            if let WsMessage::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    /// Receives until `pred` matches, returning everything seen (inclusive).
    pub async fn recv_until(&mut self, pred: impl Fn(&ServerMsg) -> bool) -> Vec<ServerMsg> {
        let mut seen = Vec::new();
        loop {
            let m = self.recv().await;
            let done = pred(&m);
            seen.push(m);
            if done {
                return seen;
            }
        }
    }

    /// Starts a call and checks it begins PAUSED.
    pub async fn start_call(&mut self) -> String {
        self.send(ClientMsg::StartSession).await;
        match self.recv().await {
            ServerMsg::SessionStarted { session_id, viewing, .. } => {
                assert!(!viewing, "sessions must start PAUSED");
                session_id
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
    }

    pub async fn set_viewing(&mut self, viewing: bool) {
        self.send(ClientMsg::SetViewing { viewing }).await;
        assert_eq!(self.recv().await, ServerMsg::ViewingState { viewing });
    }
}

pub fn done(m: &ServerMsg) -> bool {
    matches!(m, ServerMsg::AssistantDone { .. })
}
