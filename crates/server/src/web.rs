//! HTTP + WebSocket surface: the embedded client assets and `/ws`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, ServerMsg, TAG_FRAME, TAG_MIC_PCM};
use rust_embed::RustEmbed;
use tokio::sync::mpsc;

use crate::session::{AppState, Session};

/// The trunk build output, embedded so the app ships as one executable.
#[derive(RustEmbed)]
#[folder = "../../dist/"]
#[allow_missing = true]
struct Assets;

pub fn router(app: Arc<AppState>, addr: SocketAddr) -> Router {
    Router::new()
        .route("/ws", get(ws_upgrade))
        .fallback(get(static_asset))
        .layer(middleware::from_fn_with_state(addr, local_only))
        .with_state(app)
}

/// Rejects requests whose Host (and, if present, Origin) isn't this loopback
/// server, so other websites can't reach the API via DNS rebinding or a
/// cross-site WebSocket.
async fn local_only(State(addr): State<SocketAddr>, req: Request, next: Next) -> Response {
    let port = addr.port();
    let allowed_host = |h: &str| {
        [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]
            .iter()
            .any(|a| a == h)
    };
    let headers = req.headers();
    let host_ok = headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(allowed_host);
    let origin_ok = match headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
        None => true,
        Some(o) => o.strip_prefix("http://").is_some_and(allowed_host),
    };
    if !host_ok || !origin_ok {
        return (StatusCode::FORBIDDEN, "local access only").into_response();
    }
    next.run(req).await
}

async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, mime.as_ref().parse().unwrap());
            headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            (headers, Body::from(file.data.into_owned())).into_response()
        }
        None if path == "index.html" => (
            StatusCode::NOT_FOUND,
            "Client not built. Run `trunk build --release` in the repo root, then rebuild the server.",
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws_upgrade(State(app): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(16 * 1024 * 1024).on_upgrade(move |socket| connection(app, socket))
}

async fn connection(app: Arc<AppState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let text = serde_json::to_string(&msg).expect("ServerMsg serializes");
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut session: Option<Session> = None;
    let mut violations_without_session: u64 = 0;
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            WsMessage::Text(text) => {
                let msg: ClientMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = out_tx.send(ServerMsg::Error { message: format!("bad message: {e}") });
                        continue;
                    }
                };
                match msg {
                    ClientMsg::StartSession => {
                        if let Some(mut old) = session.take() {
                            old.end();
                        }
                        match Session::start(app.clone(), out_tx.clone()) {
                            Ok(s) => {
                                tracing::info!(session = %s.id, "session started (viewing PAUSED)");
                                session = Some(s);
                            }
                            Err(e) => {
                                let _ = out_tx.send(ServerMsg::Error { message: format!("start failed: {e}") });
                            }
                        }
                    }
                    ClientMsg::EndSession => {
                        if let Some(mut s) = session.take() {
                            tracing::info!(session = %s.id, "session ended");
                            s.end();
                        }
                    }
                    ClientMsg::SetViewing { viewing } => match session.as_mut() {
                        Some(s) => s.set_viewing(viewing),
                        None => {
                            let _ = out_tx.send(ServerMsg::ViewingState { viewing: false });
                        }
                    },
                    ClientMsg::StartScan { scan_id } => match session.as_mut() {
                        Some(s) => s.start_scan(scan_id),
                        None => {
                            let _ = out_tx.send(ServerMsg::ScanState { active: false, scan_id, segments: 0, gaps: 0 });
                        }
                    },
                    ClientMsg::EndScan { scan_id } => {
                        if let Some(s) = session.as_mut() {
                            s.end_scan(scan_id);
                        }
                    }
                    ClientMsg::UserText { text } => match session.as_mut() {
                        Some(s) => s.user_text(text),
                        None => {
                            let _ = out_tx.send(ServerMsg::Error { message: "start a call first".into() });
                        }
                    },
                }
            }
            WsMessage::Binary(data) => match (data.first().copied(), session.as_mut()) {
                (Some(TAG_FRAME), Some(s)) => s.handle_frame(&data),
                (Some(TAG_FRAME), None) => {
                    // No call means viewing is paused: drop and count.
                    violations_without_session += 1;
                    let _ = out_tx.send(ServerMsg::ProtocolViolation {
                        detail: "screen frame received with no active call; dropped".into(),
                        total: violations_without_session,
                    });
                }
                (Some(TAG_MIC_PCM), _) => {} // voice arrives in M1
                (tag, _) => tracing::warn!(?tag, "unknown binary message dropped"),
            },
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    if let Some(mut s) = session.take() {
        s.end();
    }
    drop(out_tx);
    let _ = writer.await;
}
