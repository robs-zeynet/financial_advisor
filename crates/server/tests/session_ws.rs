//! End-to-end session tests over a real WebSocket against a mock Messages API.
//! Covers acceptance tests 2 (server half), 8 (refusal), and the request
//! shape behind 1 (tool list) and 4 (system + memory only).

mod common;

use base64::Engine;
use common::*;
use protocol::{ClientMsg, ServerMsg};
use serde_json::{json, Value};
use server::claude::{ADVISOR_SYSTEM_PROMPT, PAUSED_TEXT, REFUSAL_APOLOGY};
use server::memory::load_memory;

const JPEG_A: &[u8] = b"\xFF\xD8\xFF\xE0fake-jpeg-A";
const JPEG_B: &[u8] = b"\xFF\xD8\xFF\xE0fake-jpeg-B";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Content blocks of the last message with `role`.
fn last_with_role<'a>(body: &'a Value, role: &str) -> &'a Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == role)
        .and_then(|m| m["content"].as_array())
        .unwrap()
}

#[tokio::test]
async fn frames_while_paused_are_dropped_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;

    // Before any call: no session means paused.
    c.send_frame(1, JPEG_A).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 1, .. }));

    c.start_call().await;
    c.send_frame(2, JPEG_A).await;
    match c.recv().await {
        ServerMsg::ProtocolViolation { total, .. } => assert_eq!(total, 1),
        other => panic!("expected violation, got {other:?}"),
    }

    // While VIEWING, frames are accepted silently.
    c.set_viewing(true).await;
    c.send_frame(3, JPEG_A).await;
    c.set_viewing(false).await; // would have seen a violation first otherwise

    c.send_frame(4, JPEG_B).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 2, .. }));
}

#[tokio::test]
async fn view_screen_returns_frames_and_request_shape_is_locked_down() {
    let dir = tempfile::tempdir().unwrap();
    // Memory the session should see...
    std::fs::create_dir_all(dir.path().join("memory/sessions")).unwrap();
    std::fs::write(dir.path().join("memory/profile.md"), "Goal: retire at 60.\n").unwrap();
    std::fs::write(dir.path().join("memory/sessions/2026-10-20.md"), "Approved summary.\n").unwrap();
    // ...and things it must never see.
    std::fs::create_dir_all(dir.path().join("records/old-session")).unwrap();
    std::fs::write(dir.path().join("records/old-session/transcript.md"), "SECRET-RECORD-TEXT").unwrap();
    std::fs::write(dir.path().join("memory/draft-unapproved.txt"), "SECRET-DRAFT-TEXT").unwrap();

    let mock = spawn_mock_claude(vec![MockReply::Sse("tool_use.sse"), MockReply::Sse("text.sse")]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.set_viewing(true).await;
    c.send_frame(1_000, JPEG_A).await;
    c.send_frame(2_000, JPEG_B).await;
    c.send(ClientMsg::UserText { text: "critique my portfolio".into() }).await;
    let seen = c.recv_until(done).await;

    assert!(seen.contains(&ServerMsg::UserTurn { text: "critique my portfolio".into() }));
    assert!(seen.contains(&ServerMsg::ToolActivity {
        tool: "view_screen".into(),
        detail: "viewed screen: 2 frames".into()
    }));
    let said: String = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, "Let me take a look.Utilities and consumer staples.");
    assert_eq!(seen.last(), Some(&ServerMsg::AssistantDone { stop_reason: "end_turn".into() }));

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let first = &reqs[0].body;

    // Headers.
    let h = &reqs[0].headers;
    assert_eq!(h["x-api-key"], "test-key");
    let betas = h["anthropic-beta"].to_str().unwrap();
    assert!(betas.contains("server-side-fallback-2026-07-01"));
    assert!(betas.contains("context-management-2025-06-27"));

    // Tools: view_screen only (test 1).
    let names: Vec<&str> = first["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["view_screen"]);

    // System = frozen prompt + load_memory(), byte for byte, cache breakpoint after memory (test 4).
    let memory = load_memory(dir.path()).unwrap();
    assert_eq!(
        first["system"],
        json!([
            {"type": "text", "text": ADVISOR_SYSTEM_PROMPT},
            {"type": "text", "text": memory, "cache_control": {"type": "ephemeral"}}
        ])
    );
    let whole = first.to_string();
    assert!(!whole.contains("SECRET-RECORD-TEXT") && !whole.contains("SECRET-DRAFT-TEXT"));

    // Other request settings.
    assert_eq!(first["model"], "claude-opus-5-5");
    assert_eq!(first["stream"], true);
    assert_eq!(first["output_config"], json!({"effort": "low"}));
    assert_eq!(first["fallbacks"], "default");
    assert!(first.get("thinking").is_none());
    let edit = &first["context_management"]["edits"][0];
    assert_eq!(edit["type"], "clear_tool_uses_20250919");
    assert_eq!(edit["keep"], json!({"type": "tool_uses", "value": 2}));

    // The first turn is the user's text followed by the viewing-state note as
    // a mid-conversation system message.
    let msgs = first["messages"].as_array().unwrap();
    assert_eq!(msgs[0], json!({"role": "user", "content": [{"type": "text", "text": "critique my portfolio"}]}));
    assert_eq!(msgs[1]["role"], "system");
    assert!(msgs[1]["content"][0]["text"].as_str().unwrap().contains("resumed screen viewing"));

    // Second request: append-only history plus the tool_result with both frames.
    let second = &reqs[1].body;
    let second_msgs = second["messages"].as_array().unwrap();
    assert_eq!(&second_msgs[..msgs.len()], &msgs[..], "history must be append-only");
    assert_eq!(second["system"], first["system"]);
    let result = &last_with_role(second, "user")[0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_01");
    let images: Vec<&Value> = result["content"].as_array().unwrap().iter().filter(|b| b["type"] == "image").collect();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0]["source"]["data"], b64(JPEG_A));
    assert_eq!(images[1]["source"]["data"], b64(JPEG_B));
    assert_eq!(images[1]["source"]["media_type"], "image/jpeg");
}

#[tokio::test]
async fn view_screen_while_paused_returns_paused_text() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("tool_use.sse"), MockReply::Sse("text.sse")]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.send(ClientMsg::UserText { text: "what do you see?".into() }).await;
    c.recv_until(done).await;

    let reqs = mock.requests();
    let result = &last_with_role(&reqs[1].body, "user")[0];
    assert_eq!(result["content"], json!([{"type": "text", "text": PAUSED_TEXT}]));
    assert!(!reqs[1].body.to_string().contains("\"image\""));
}

#[tokio::test]
async fn saying_pause_viewing_pauses_immediately_and_wipes_frames() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("tool_use.sse"), MockReply::Sse("text.sse")]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.set_viewing(true).await;
    c.send_frame(1, JPEG_A).await;

    c.send(ClientMsg::UserText { text: "Pause viewing, I'm logging in".into() }).await;
    assert_eq!(c.recv().await, ServerMsg::ViewingState { viewing: false });
    // A frame already in flight before the client hears about the pause is
    // dropped without counting as a violation...
    c.send_frame(2, JPEG_B).await;
    let seen = c.recv_until(done).await;
    assert!(!seen.iter().any(|m| matches!(m, ServerMsg::ProtocolViolation { .. })));
    // ...and the model saw no frames, since the buffer was wiped.
    let reqs = mock.requests();
    let result = &last_with_role(&reqs[1].body, "user")[0];
    assert_eq!(result["content"], json!([{"type": "text", "text": PAUSED_TEXT}]));

    // Once the client acknowledges, stray frames are violations again.
    c.set_viewing(false).await;
    c.send_frame(3, JPEG_B).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 1, .. }));
}

#[tokio::test]
async fn refusal_ends_turn_gracefully_with_spoken_apology() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("refusal.sse"), MockReply::Sse("text.sse")]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.send(ClientMsg::UserText { text: "something declined".into() }).await;
    let seen = c.recv_until(done).await;
    assert!(seen.contains(&ServerMsg::AssistantDelta { text: REFUSAL_APOLOGY.into() }));
    assert_eq!(seen.last(), Some(&ServerMsg::AssistantDone { stop_reason: "refusal".into() }));
    assert!(!seen.iter().any(|m| matches!(m, ServerMsg::Error { .. })));

    // The conversation continues; the partial output was discarded and the
    // apology recorded as the assistant turn.
    c.send(ClientMsg::UserText { text: "ok, dividends then".into() }).await;
    let seen = c.recv_until(done).await;
    assert_eq!(seen.last(), Some(&ServerMsg::AssistantDone { stop_reason: "end_turn".into() }));
    let reqs = mock.requests();
    let assistant = last_with_role(&reqs[1].body, "assistant");
    assert_eq!(assistant, &vec![json!({"type": "text", "text": REFUSAL_APOLOGY})]);
    assert!(!reqs[1].body.to_string().contains("Partial out"));
}

#[tokio::test]
async fn retries_overloaded_with_retry_after() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![
        MockReply::Status {
            status: 529,
            retry_after: Some("0"),
            body: r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        },
        MockReply::Sse("error.sse"), // mid-stream overload before any output: retried too
        MockReply::Sse("text.sse"),
    ])
    .await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.send(ClientMsg::UserText { text: "hi".into() }).await;
    let seen = c.recv_until(done).await;
    assert_eq!(seen.last(), Some(&ServerMsg::AssistantDone { stop_reason: "end_turn".into() }));
    assert_eq!(mock.requests().len(), 3);
}

#[tokio::test]
async fn non_retryable_errors_surface_to_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Status {
        status: 400,
        retry_after: None,
        body: r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
    }])
    .await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.send(ClientMsg::UserText { text: "hi".into() }).await;
    let seen = c.recv_until(done).await;
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::Error { message } if message.contains("invalid_request_error"))));
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn cross_origin_websocket_is_refused() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let dir = tempfile::tempdir().unwrap();
    let (addr, _) = spawn_app(test_config(dir.path(), "http://127.0.0.1:9")).await;
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("origin", "https://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());

    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("origin", format!("http://{addr}").parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_ok());
}
