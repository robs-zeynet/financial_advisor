//! Acceptance test 14: PDF uploads are held in memory, announced to the
//! advisor, read through view_screen scope "document", and wiped on remove,
//! replace, and hang-up.

mod common;

use base64::Engine;
use common::*;
use protocol::{encode_pdf, ClientMsg, DocumentInfo, ServerMsg, MAX_PDF_BYTES};
use serde_json::Value;

const PDF_A: &[u8] = b"%PDF-1.7\n1 0 obj << /Type /Page >> endobj\n2 0 obj << /Type /Page >> endobj\nSTATEMENT-A";
const PDF_B: &[u8] = b"%PDF-1.4\n1 0 obj << /Type /Page >> endobj\nSTATEMENT-B";

fn tool_result(body: &Value) -> &Vec<Value> {
    let msgs = body["messages"].as_array().unwrap();
    let user = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
    user["content"][0]["content"].as_array().unwrap()
}

#[tokio::test]
async fn uploaded_pdf_is_announced_read_and_wiped() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![
        MockReply::Sse("tool_use_doc.sse"),
        MockReply::Sse("text.sse"),
        MockReply::Sse("tool_use_doc.sse"),
        MockReply::Sse("text.sse"),
        MockReply::Sse("tool_use_doc.sse"),
        MockReply::Sse("text.sse"),
    ])
    .await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;

    // No call yet: refused.
    c.send_raw(encode_pdf("early.pdf", PDF_A)).await;
    assert!(matches!(c.recv().await, ServerMsg::Error { message } if message.contains("Start a call")));

    c.start_call().await;
    c.send_raw(encode_pdf("/home/me/Fidelity positions.pdf", PDF_A)).await;
    assert_eq!(
        c.recv().await,
        ServerMsg::DocumentState {
            document: Some(DocumentInfo { name: "Fidelity positions.pdf".into(), bytes: PDF_A.len() as u64, pages: Some(2) })
        }
    );

    c.send(ClientMsg::UserText { text: "what's in my statement?".into() }).await;
    let seen = c.recv_until(done).await;
    assert!(seen.iter().any(|m| matches!(m, ServerMsg::ToolActivity { detail, .. } if detail.starts_with("read PDF"))));

    let reqs = mock.requests();
    // Announced as a mid-conversation system note...
    let msgs = reqs[0].body["messages"].as_array().unwrap();
    let note = msgs[1]["content"][0]["text"].as_str().unwrap();
    assert_eq!(msgs[1]["role"], "system");
    assert!(note.contains("\"Fidelity positions.pdf\"") && note.contains("about 2 pages") && note.contains("\"document\""));
    // ...and read back as a document block with exactly the uploaded bytes.
    let blocks = tool_result(&reqs[1].body);
    let doc = blocks.iter().find(|b| b["type"] == "document").expect("document block");
    assert_eq!(doc["source"]["media_type"], "application/pdf");
    assert_eq!(doc["source"]["data"], base64::engine::general_purpose::STANDARD.encode(PDF_A));
    assert_eq!(doc["title"], "Fidelity positions.pdf");

    // Replace: the new one is served, the old one is gone.
    c.send_raw(encode_pdf("Statement_000123456789.pdf", PDF_B)).await;
    match c.recv().await {
        ServerMsg::DocumentState { document: Some(d) } => assert_eq!(d.name, "uploaded.pdf", "flagged name replaced"),
        other => panic!("{other:?}"),
    }
    c.send(ClientMsg::UserText { text: "and now?".into() }).await;
    c.recv_until(done).await;
    let reqs = mock.requests();
    let body = reqs[3].body.to_string();
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    assert!(tool_result(&reqs[3].body).iter().any(|b| b["source"]["data"] == b64(PDF_B)));
    assert!(!body.contains("000123456789"), "the flagged file name never reaches Claude");

    // Remove: nothing to read.
    c.send(ClientMsg::RemoveDocument).await;
    assert_eq!(c.recv().await, ServerMsg::DocumentState { document: None });
    c.send(ClientMsg::UserText { text: "read it again".into() }).await;
    c.recv_until(done).await;
    let reqs = mock.requests();
    let texts: Vec<&str> = tool_result(&reqs[5].body).iter().filter_map(|b| b["text"].as_str()).collect();
    assert!(texts[0].starts_with("No PDF has been uploaded"), "{texts:?}");
    assert!(texts[0].contains("print it to PDF"));
}

#[tokio::test]
async fn non_pdfs_and_oversized_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mock = spawn_mock_claude(vec![]).await;
    let (addr, _) = spawn_app(test_config(dir.path(), &mock.base_url)).await;
    let mut c = FakeClient::connect(addr).await;
    c.start_call().await;

    c.send_raw(encode_pdf("photo.pdf", b"\x89PNG\r\n\x1a\nnot a pdf")).await;
    assert!(matches!(c.recv().await, ServerMsg::Error { message } if message.contains("isn't a PDF")));

    let big = [b"%PDF-1.7\n".as_slice(), &vec![b' '; MAX_PDF_BYTES]].concat();
    c.send_raw(encode_pdf("huge.pdf", &big)).await;
    assert!(matches!(c.recv().await, ServerMsg::Error { message } if message.contains("larger than")));

    // Nothing was kept.
    c.send(ClientMsg::RemoveDocument).await;
    assert_eq!(c.recv().await, ServerMsg::DocumentState { document: None });
}
