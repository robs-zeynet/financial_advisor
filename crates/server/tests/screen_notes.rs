//! Acceptance test 13: screen notes are linted, shown to the user, kept only
//! for the session, never cleared by context editing, and never reach memory.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::*;
use protocol::{ClientMsg, ServerMsg};
use serde_json::{json, Value};

/// Every file under `dir` with its contents, to detect any change.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.display().to_string(), std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

fn tool_result(req: &Value) -> &Value {
    let msgs = req["messages"].as_array().unwrap();
    let user = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
    &user["content"][0]
}

fn notes_events(seen: &[ServerMsg]) -> Vec<(u32, String, String, u32)> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::ScreenNote { id, page, notes, revision } => Some((*id, page.clone(), notes.clone(), *revision)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn notes_are_linted_shown_superseded_and_session_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("memory/sessions")).unwrap();
    std::fs::write(dir.path().join("memory/profile.md"), "Goal: retire at 60.\n").unwrap();
    let memory_before = snapshot(&dir.path().join("memory"));

    let mock = spawn_mock_claude(vec![
        // Turn 1: a flagged note, then the rewritten one, then the answer.
        MockReply::Sse("notes_bad.sse"),
        MockReply::Sse("notes_good.sse"),
        MockReply::Sse("text.sse"),
        // Turn 2: an update to the same page.
        MockReply::Sse("notes_update.sse"),
        MockReply::Sse("text.sse"),
        // Next session.
        MockReply::Sse("text.sse"),
    ])
    .await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    c.send(ClientMsg::UserText { text: "note my Fidelity positions".into() }).await;
    let turn1 = c.recv_until(done).await;
    c.send(ClientMsg::UserText { text: "I rebalanced; update it".into() }).await;
    let turn2 = c.recv_until(done).await;

    // The flagged note never reached the UI; the clean one did, then its update
    // replaced it under the same id.
    let shown = notes_events(&turn1);
    assert_eq!(shown.len(), 1, "only the accepted note is shown: {shown:?}");
    let (id, page, notes, rev) = &shown[0];
    assert_eq!((page.as_str(), *rev), ("Fidelity brokerage ...6789 positions", 1));
    assert!(notes.starts_with("NOTE-MARKER-A"));
    assert!(turn1.iter().any(|m| matches!(m, ServerMsg::ToolActivity { detail, .. } if detail.contains("note rejected"))));
    let updated = notes_events(&turn2);
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].0, *id, "same page label, same note");
    assert_eq!(updated[0].3, 2);
    assert!(updated[0].2.starts_with("NOTE-MARKER-B"));

    let reqs = mock.requests();
    // The rejection told the model what to fix, masked, as an error result.
    let rejected = tool_result(&reqs[1].body);
    assert_eq!(rejected["type"], "tool_result");
    assert_eq!(rejected["is_error"], true);
    let msg = rejected["content"][0]["text"].as_str().unwrap();
    assert!(msg.contains("account-number-like") && msg.contains("email"), "{msg}");
    assert!(msg.contains("…6789") && !msg.contains("000123456789"), "identifiers are masked: {msg}");
    // The accepted note is acknowledged.
    let ok = tool_result(&reqs[2].body);
    assert!(ok.get("is_error").is_none());
    assert_eq!(ok["content"], json!([{"type": "text", "text": "Noted."}]));

    // Context editing never clears notes.
    let edit = &reqs[0].body["context_management"]["edits"][0];
    assert!(edit["exclude_tools"].as_array().unwrap().contains(&json!("record_screen_notes")));

    // Hang up: nothing under data/memory changed, and a new session's
    // request carries no note text.
    c.send(ClientMsg::EndSession).await;
    c.recv_until(|m| matches!(m, ServerMsg::SessionEnded { .. })).await;
    assert_eq!(snapshot(&dir.path().join("memory")), memory_before, "data/memory must not change");

    c.start_call().await;
    c.send(ClientMsg::UserText { text: "hello again".into() }).await;
    c.recv_until(done).await;
    let reqs = mock.requests();
    let next = reqs.last().unwrap().body.to_string();
    assert!(!next.contains("NOTE-MARKER"), "notes must not reach a new session");
    assert_eq!(reqs.last().unwrap().body["messages"].as_array().unwrap().len(), 1);
}
