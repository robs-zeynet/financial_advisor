//! Acceptance test 10: the SSE parser and accumulator handle recorded
//! fixtures: text deltas, a streamed tool_use input, a refusal, an error
//! event, and unknown event/block/delta types.

use serde_json::json;
use server::claude::sse::{parse_event, Accumulator, SseParser, Update};
use server::claude::types::{ContentBlock, StreamEvent};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// Feeds the fixture in `chunk`-byte pieces and returns the accumulator plus
/// every update it produced.
fn run(name: &str, chunk: usize) -> (Accumulator, Vec<Update>) {
    let bytes = fixture(name);
    let mut parser = SseParser::new();
    let mut frames = Vec::new();
    for piece in bytes.chunks(chunk) {
        frames.extend(parser.feed(piece));
    }
    frames.extend(parser.finish());
    let mut acc = Accumulator::new();
    let mut updates = Vec::new();
    for f in frames {
        let ev = parse_event(&f).expect("fixture events are valid JSON");
        updates.push(acc.apply(ev));
    }
    (acc, updates)
}

fn text_of(updates: &[Update]) -> String {
    updates
        .iter()
        .filter_map(|u| match u {
            Update::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn text_deltas_and_thinking_signature() {
    for chunk in [1, 7, 64, usize::MAX] {
        let (acc, updates) = run("text.sse", chunk.min(1 << 20));
        assert_eq!(text_of(&updates), "Utilities and consumer staples.");
        assert_eq!(acc.stop_reason.as_deref(), Some("end_turn"));
        assert!(acc.done);
        assert_eq!(acc.usage.output_tokens, Some(42));
        assert_eq!(acc.usage.cache_read_input_tokens, Some(2048));
        let content = acc.content();
        assert_eq!(
            content[0],
            ContentBlock::Thinking { thinking: String::new(), signature: "EqQBCkYIBxgCKkA=".into() },
            "thinking must round-trip with its signature"
        );
        assert_eq!(content[1], ContentBlock::text("Utilities and consumer staples."));
    }
}

#[test]
fn tool_use_with_streamed_json_input() {
    let (acc, updates) = run("tool_use.sse", 5);
    assert!(updates.contains(&Update::ToolUseStart { name: "view_screen".into() }));
    assert_eq!(acc.stop_reason.as_deref(), Some("tool_use"));
    match &acc.content()[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_01");
            assert_eq!(name, "view_screen");
            assert_eq!(input, &json!({"reason": "see the holdings page"}));
        }
        other => panic!("expected tool_use, got {other:?}"),
    }
}

#[test]
fn refusal() {
    let (acc, _) = run("refusal.sse", 13);
    assert_eq!(acc.stop_reason.as_deref(), Some("refusal"));
    assert_eq!(acc.stop_details.as_ref().unwrap()["type"], "refusal");
}

#[test]
fn error_event() {
    let (acc, updates) = run("error.sse", 9);
    assert!(updates.contains(&Update::Error { kind: "overloaded_error".into(), message: "Overloaded".into() }));
    assert!(!acc.done);
    assert!(!acc.produced_output, "error arrived before any content, so it is retryable");
}

#[test]
fn unknown_types_are_ignored_but_unknown_blocks_kept() {
    let (acc, updates) = run("unknown.sse", 3);
    assert_eq!(text_of(&updates), "Hi.");
    assert!(acc.done, "final event without a trailing blank line is flushed");
    let content = acc.content();
    assert_eq!(content[0].kind(), "future_block");
    assert_eq!(
        serde_json::to_value(&content[0]).unwrap(),
        json!({"type": "future_block", "opaque": "keep-me"})
    );
    assert_eq!(content[1], ContentBlock::text("Hi."));
}

#[test]
fn crlf_and_multiline_data() {
    let mut p = SseParser::new();
    let frames = p.feed(b"event: ping\r\ndata: {\"type\":\r\ndata: \"ping\"}\r\n\r\n");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("ping"));
    assert_eq!(parse_event(&frames[0]).unwrap(), StreamEvent::Ping);
}
