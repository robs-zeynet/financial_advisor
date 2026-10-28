//! Acceptance test 12: the server's page-scan buffer, over a real WebSocket
//! against a mock Messages API.

mod common;

use std::sync::Arc;

use base64::Engine;
use common::*;
use protocol::{ClientMsg, FrameHeader, ServerMsg};
use serde_json::{json, Value};
use server::claude::{ClaudeClient, PAUSED_TEXT};
use server::session::{AppState, Session, Voice};

const SEG: [&[u8]; 4] = [
    b"\xFF\xD8\xFF\xE0segment-0",
    b"\xFF\xD8\xFF\xE0segment-1",
    b"\xFF\xD8\xFF\xE0segment-2",
    b"\xFF\xD8\xFF\xE0segment-3",
];

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn tool_result(body: &Value) -> &Vec<Value> {
    let msgs = body["messages"].as_array().unwrap();
    let user = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
    user["content"][0]["content"].as_array().unwrap()
}

fn texts(blocks: &[Value]) -> Vec<String> {
    blocks.iter().filter(|b| b["type"] == "text").map(|b| b["text"].as_str().unwrap().to_string()).collect()
}

fn images(blocks: &[Value]) -> Vec<String> {
    blocks
        .iter()
        .filter(|b| b["type"] == "image")
        .map(|b| b["source"]["data"].as_str().unwrap().to_string())
        .collect()
}

fn scan_state(active: bool, scan_id: u32, segments: u32, gaps: u32) -> ServerMsg {
    ServerMsg::ScanState { active, scan_id, segments, gaps }
}

#[tokio::test]
async fn segments_are_kept_in_order_and_the_scan_ends_at_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![]).await;
    let mut cfg = test_config(dir.path(), &mock.base_url);
    cfg.capture.max_scan_segments = 3;
    let (addr, _) = spawn_app(cfg).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.start_scan(1).await;

    c.send_segment(1, 0, None, false, SEG[0]).await;
    assert_eq!(c.recv().await, scan_state(true, 1, 1, 0));
    c.send_segment(1, 1, Some(700), false, SEG[1]).await;
    assert_eq!(c.recv().await, scan_state(true, 1, 2, 0));
    c.send_segment(1, 2, None, true, SEG[2]).await;
    assert_eq!(c.recv().await, scan_state(false, 1, 3, 1), "the server ends the scan at the cap");

    // The scan is over: further segments are violations.
    c.send_segment(1, 3, Some(10), false, SEG[3]).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 1, .. }));
}

#[tokio::test]
async fn stray_segments_and_oversized_frames_are_violations() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    // No scan running.
    c.send_segment(1, 0, None, false, SEG[0]).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 1, .. }));

    // Wrong scan id.
    c.start_scan(2).await;
    c.send_segment(9, 0, None, false, SEG[0]).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 2, .. }));

    // Oversized segment, then oversized live frame while viewing.
    let big = FrameHeader { scan_id: 2, ..FrameHeader::live(1, 2001, 1000, true) };
    c.send_header(big, SEG[0]).await;
    match c.recv().await {
        ServerMsg::ProtocolViolation { total, detail } => {
            assert_eq!(total, 3);
            assert!(detail.contains("2000"));
        }
        other => panic!("expected violation, got {other:?}"),
    }
    c.send(ClientMsg::EndScan { scan_id: 2 }).await;
    assert_eq!(c.recv().await, scan_state(false, 2, 0, 0));
    c.set_viewing(true).await;
    c.send_header(FrameHeader::live(2, 1200, 2400, true), SEG[0]).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 4, .. }));
}

/// Scans from PAUSED, ends the scan, then has the model look at it.
#[tokio::test]
async fn view_screen_scan_returns_labeled_segments_after_the_scan_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![
        MockReply::Sse("tool_use.sse"), // scope "current" first: paused, with a hint
        MockReply::Sse("text.sse"),
        MockReply::Sse("tool_use_scan.sse"),
        MockReply::Sse("text.sse"),
    ])
    .await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.start_scan(1).await;
    c.send_segment(1, 0, None, false, SEG[0]).await;
    c.recv().await;
    c.send_segment(1, 1, Some(700), false, SEG[1]).await;
    c.recv().await;
    c.send_segment(1, 2, None, true, SEG[2]).await;
    c.recv().await;
    c.send(ClientMsg::EndScan { scan_id: 1 }).await;
    assert_eq!(c.recv().await, scan_state(false, 1, 3, 1));

    // Back to PAUSED: "current" shows nothing but points at the scan.
    c.send(ClientMsg::UserText { text: "what's on my screen?".into() }).await;
    c.recv_until(done).await;
    c.send(ClientMsg::UserText { text: "read the whole holdings list".into() }).await;
    let seen = c.recv_until(done).await;
    assert!(seen.contains(&ServerMsg::ToolActivity {
        tool: "view_screen".into(),
        detail: "viewed page scan: 3 segments".into()
    }));

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);

    // The scan-ended note follows the user's message as a system message.
    let msgs = reqs[0].body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["role"], "system");
    let note = msgs[1]["content"][0]["text"].as_str().unwrap();
    assert!(note.contains("scanned a page: 3 segments"), "{note}");
    assert!(note.contains("possible gap between segments 2 and 3"), "{note}");
    assert!(note.contains("scope \"scan\""), "{note}");

    // "current" while paused: the paused text plus a pointer to the scan.
    let current = texts(tool_result(&reqs[1].body));
    assert_eq!(current[0], PAUSED_TEXT);
    assert!(current[1].contains("unviewed page scan with 3 segments"));
    assert!(images(tool_result(&reqs[1].body)).is_empty(), "the live ring was wiped when the scan ended");

    // "scan": every segment, top to bottom, labeled, with the gap called out.
    let blocks = tool_result(&reqs[3].body);
    let labels = texts(blocks);
    assert!(labels[0].starts_with("Page scan 1: 3 segments, top to bottom."));
    assert!(labels[1].starts_with("Segment 1 of 3, top of the scan"));
    assert!(labels[2].starts_with("Segment 2 of 3, about 700 px below segment 1"));
    assert_eq!(labels[3], "Possible gap: part of the page between segments 2 and 3 may be missing.");
    assert!(labels[4].starts_with("Segment 3 of 3, position unknown"));
    assert_eq!(images(blocks), vec![b64(SEG[0]), b64(SEG[1]), b64(SEG[2])]);

    // Append-only: every request extends the previous one byte for byte.
    for pair in reqs.windows(2) {
        let (a, b) = (pair[0].body["messages"].as_array().unwrap(), pair[1].body["messages"].as_array().unwrap());
        assert_eq!(&b[..a.len()], &a[..]);
        assert_eq!(pair[0].body["system"], pair[1].body["system"]);
        assert_eq!(pair[0].body["tools"], pair[1].body["tools"]);
    }
    // Caching and context editing settings for big scan results.
    let first = &reqs[0].body;
    assert_eq!(first["cache_control"], json!({"type": "ephemeral"}));
    assert!(first["context_management"]["edits"][0]["clear_at_least"]["value"].as_u64().unwrap() > 0);
    let scope = &first["tools"][0]["input_schema"]["properties"]["scope"];
    assert_eq!(scope["enum"], json!(["current", "scan", "document"]));
}

#[tokio::test]
async fn explicit_pause_and_new_scans_wipe_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![
        MockReply::Sse("tool_use_scan.sse"),
        MockReply::Sse("text.sse"),
        MockReply::Sse("tool_use_scan.sse"),
        MockReply::Sse("text.sse"),
    ])
    .await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    // A scan started while VIEWING, then an explicit pause: everything goes.
    c.set_viewing(true).await;
    c.start_scan(1).await;
    c.send_segment(1, 0, None, false, SEG[0]).await;
    c.recv().await;
    c.send(ClientMsg::SetViewing { viewing: false }).await;
    assert_eq!(c.recv().await, scan_state(false, 1, 0, 0), "the cancelled scan is reported empty");
    assert_eq!(c.recv().await, ServerMsg::ViewingState { viewing: false });
    c.send(ClientMsg::UserText { text: "look at the scan".into() }).await;
    c.recv_until(done).await;

    // A finished scan is wiped by the next scan.
    c.start_scan(2).await;
    c.send_segment(2, 0, None, false, SEG[1]).await;
    c.recv().await;
    c.send(ClientMsg::EndScan { scan_id: 2 }).await;
    c.recv().await;
    c.start_scan(3).await;
    c.send(ClientMsg::EndScan { scan_id: 3 }).await;
    assert_eq!(c.recv().await, scan_state(false, 3, 0, 0));
    c.send(ClientMsg::UserText { text: "look at the scan again".into() }).await;
    c.recv_until(done).await;

    let reqs = mock.requests();
    for i in [1, 3] {
        let blocks = tool_result(&reqs[i].body);
        assert!(texts(blocks)[0].starts_with("No page scan is available"), "request {i}");
        assert!(images(blocks).is_empty());
    }
    assert!(!reqs[3].body.to_string().contains(&b64(SEG[1])));
}

#[tokio::test]
async fn saying_pause_viewing_cancels_a_scan_and_drops_in_flight_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![MockReply::Sse("text.sse")]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;
    c.start_scan(1).await;
    c.send_segment(1, 0, None, false, SEG[0]).await;
    c.recv().await;

    c.send(ClientMsg::UserText { text: "pause viewing".into() }).await;
    assert_eq!(c.recv().await, scan_state(false, 1, 0, 0));
    assert_eq!(c.recv().await, ServerMsg::ViewingState { viewing: false });
    // In flight before the client heard: dropped quietly.
    c.send_segment(1, 1, Some(500), false, SEG[1]).await;
    let seen = c.recv_until(done).await;
    assert!(!seen.iter().any(|m| matches!(m, ServerMsg::ProtocolViolation { .. })));
    // After the client acknowledges, stray segments are violations again.
    c.set_viewing(false).await;
    c.send_segment(1, 2, Some(500), false, SEG[2]).await;
    assert!(matches!(c.recv().await, ServerMsg::ProtocolViolation { total: 1, .. }));
}

#[tokio::test]
async fn hang_up_wipes_both_buffers() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path(), "http://127.0.0.1:9");
    let app = Arc::new(AppState { claude: ClaudeClient::new(config.claude.clone(), None), config, voice: Voice::default() });
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut s = Session::start(app, tx, events).unwrap();
    s.start_scan(1);
    let seg = FrameHeader { scan_id: 1, ..FrameHeader::live(1, 800, 600, true) };
    s.handle_frame(&seg.encode(SEG[0]));
    s.end_scan(1);
    s.set_viewing(true);
    s.handle_frame(&FrameHeader::live(2, 800, 600, true).encode(SEG[1]));
    assert_eq!((s.scan_segment_count(), s.frame_count()), (1, 1));
    s.end();
    assert_eq!((s.scan_segment_count(), s.frame_count()), (0, 0));
    assert!(!s.viewing());
}
