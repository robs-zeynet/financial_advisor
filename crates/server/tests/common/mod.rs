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
use server::session::{AppState, Voice};
use server::stt::SpeechToText;
use server::tts::{TtsEngine, TtsFuture};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub enum MockReply {
    /// 200 with an SSE body from `tests/fixtures/<name>`.
    Sse(&'static str),
    /// Like `Sse`, but each event is delayed by `gap_ms`.
    SlowSse(&'static str, u64),
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
                // Enforce the API's ordering rule on every request, as the real API does.
                let order: Vec<server::claude::types::Message> =
                    serde_json::from_value(body["messages"].clone()).expect("messages parse");
                let order_err = server::claude::check_message_order(&order).err();
                reqs.lock().unwrap().push(Captured { headers, body });
                if let Some(e) = order_err {
                    let err = serde_json::json!({"type": "error", "error": {"type": "invalid_request_error", "message": e}});
                    return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
                }
                let reply = replies.lock().unwrap().pop_front().expect("unexpected extra request");
                let resp: Response = match reply {
                    MockReply::Sse(name) => {
                        ([("content-type", "text/event-stream")], fixture(name)).into_response()
                    }
                    MockReply::SlowSse(name, gap_ms) => {
                        let events: Vec<String> = fixture(name).split("\n\n").map(|e| format!("{e}\n\n")).collect();
                        let stream = futures_util::stream::iter(events).then(move |e| async move {
                            tokio::time::sleep(Duration::from_millis(gap_ms)).await;
                            Ok::<_, std::convert::Infallible>(e)
                        });
                        ([("content-type", "text/event-stream")], axum::body::Body::from_stream(stream)).into_response()
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
    spawn_app_with_voice(config, Voice::default()).await
}

pub async fn spawn_app_with_voice(config: Config, voice: Voice) -> (SocketAddr, Arc<AppState>) {
    let app = Arc::new(AppState {
        claude: ClaudeClient::new(config.claude.clone(), Some("test-key".into())),
        config,
        voice,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server::web::router(app.clone(), addr);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (addr, app)
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Speech-to-text that returns scripted transcripts in order and records
/// the length of each utterance it was given.
#[derive(Default)]
pub struct FakeStt {
    pub transcripts: Mutex<VecDeque<String>>,
    pub heard_samples: Mutex<Vec<usize>>,
}

impl FakeStt {
    pub fn new(transcripts: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            transcripts: Mutex::new(transcripts.iter().map(|s| s.to_string()).collect()),
            heard_samples: Mutex::default(),
        })
    }
}

impl SpeechToText for FakeStt {
    fn transcribe(&self, pcm: &[i16]) -> anyhow::Result<String> {
        self.heard_samples.lock().unwrap().push(pcm.len());
        Ok(self.transcripts.lock().unwrap().pop_front().unwrap_or_default())
    }
}

/// Text-to-speech producing `secs_per_sentence` of silence per sentence at
/// 16 kHz, so playback timing is predictable.
pub struct FakeTts {
    pub secs_per_sentence: f64,
    pub spoken: Mutex<Vec<String>>,
}

impl FakeTts {
    pub fn new(secs_per_sentence: f64) -> Arc<Self> {
        Arc::new(Self { secs_per_sentence, spoken: Mutex::default() })
    }
}

impl TtsEngine for FakeTts {
    fn sample_rate(&self) -> u32 {
        16_000
    }

    fn synthesize<'a>(&'a self, text: &'a str) -> TtsFuture<'a> {
        self.spoken.lock().unwrap().push(text.to_string());
        let n = (self.secs_per_sentence * 16_000.0) as usize;
        Box::pin(async move { Ok(vec![1i16; n]) })
    }
}

/// 16 kHz mic audio: a tone at `db` dBFS (or near-silence for very low `db`).
pub fn mic_audio(ms: u32, db: f32) -> Vec<i16> {
    let amp = 10f32.powf(db / 20.0) * 32768.0 * std::f32::consts::SQRT_2;
    (0..(ms as usize * 16))
        .map(|i| (amp * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / 16_000.0).sin()) as i16)
        .collect()
}

pub struct FakeClient {
    socket: Socket,
    /// TTS audio messages received so far.
    pub audio: Vec<Vec<u8>>,
}

impl FakeClient {
    pub async fn connect(addr: SocketAddr) -> Self {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
        Self { socket, audio: Vec::new() }
    }

    /// Sends an arbitrary binary message (e.g. a PDF upload).
    pub async fn send_raw(&mut self, bytes: Vec<u8>) {
        self.socket.send(WsMessage::Binary(bytes.into())).await.unwrap();
    }

    /// Sends mic audio in 20 ms chunks, as the browser does.
    pub async fn send_mic(&mut self, pcm: &[i16]) {
        for chunk in pcm.chunks(320) {
            let bytes = protocol::encode_mic(chunk);
            self.socket.send(WsMessage::Binary(bytes.into())).await.unwrap();
        }
    }

    pub async fn send(&mut self, msg: ClientMsg) {
        let text = serde_json::to_string(&msg).unwrap();
        self.socket.send(WsMessage::Text(text.into())).await.unwrap();
    }

    pub async fn send_frame(&mut self, t: u64, jpeg: &[u8]) {
        self.send_header(FrameHeader::live(t, 640, 400, true), jpeg).await;
    }

    pub async fn send_header(&mut self, header: FrameHeader, jpeg: &[u8]) {
        let bytes = header.encode(jpeg);
        self.socket.send(WsMessage::Binary(bytes.into())).await.unwrap();
    }

    /// Sends one page-scan segment.
    pub async fn send_segment(&mut self, scan_id: u32, index: u16, offset_px: Option<i32>, gap_before: bool, jpeg: &[u8]) {
        let header = FrameHeader {
            scan_id,
            segment_index: index,
            offset_px,
            gap_before,
            ..FrameHeader::live(1_000 + index as u64, 1600, 900, true)
        };
        self.send_header(header, jpeg).await;
    }

    /// Starts a scan and checks the server acknowledged it.
    pub async fn start_scan(&mut self, scan_id: u32) {
        self.send(ClientMsg::StartScan { scan_id }).await;
        assert_eq!(self.recv().await, ServerMsg::ScanState { active: true, scan_id, segments: 0, gaps: 0 });
    }

    pub async fn recv(&mut self) -> ServerMsg {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                .await
                .expect("timed out waiting for server message")
                .expect("socket closed")
                .unwrap();
            match msg {
                WsMessage::Text(t) => return serde_json::from_str(&t).unwrap(),
                WsMessage::Binary(b) => self.audio.push(b.to_vec()),
                _ => {}
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
