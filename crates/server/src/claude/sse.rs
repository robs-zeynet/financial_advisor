//! Server-sent-events parsing for the streaming Messages API, plus the
//! accumulator that folds stream events into a finished assistant message.

use serde_json::{Map, Value};

use super::types::{ContentBlock, Delta, StreamEvent, Usage};

/// One raw SSE event: the optional `event:` name and the joined `data:` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE line parser. Feed it arbitrary byte chunks; it returns each
/// complete event (terminated by a blank line).
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if let Some(frame) = self.line(&line) {
                out.push(frame);
            }
        }
        out
    }

    /// Flushes a final event that wasn't followed by a blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            let rest = rest.trim_end_matches('\r').to_string();
            if let Some(f) = self.line(&rest) {
                return Some(f);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {} // id, retry: unused
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() {
            self.event = None;
            return None;
        }
        Some(SseFrame { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") })
    }
}

/// Parses an SSE frame's data as a stream event. Malformed JSON is an error;
/// well-formed JSON of an unknown type is `StreamEvent::Unknown`.
pub fn parse_event(frame: &SseFrame) -> Result<StreamEvent, serde_json::Error> {
    serde_json::from_str(&frame.data)
}

/// Folds stream events into the final assistant message.
#[derive(Default, Debug)]
pub struct Accumulator {
    pub message_id: String,
    pub model: String,
    blocks: Vec<Value>,
    /// Raw `partial_json` per tool_use block index.
    tool_json: Vec<(usize, String)>,
    pub stop_reason: Option<String>,
    pub stop_details: Option<Value>,
    pub usage: Usage,
    pub done: bool,
    /// Whether any content has started; retrying after that would duplicate output.
    pub produced_output: bool,
}

/// What a single event means for the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    None,
    /// Visible assistant text.
    Text(String),
    /// A tool_use block began.
    ToolUseStart { name: String },
    /// A `fallback` block: another model took over.
    Fallback,
    /// An `error` event arrived mid-stream.
    Error { kind: String, message: String },
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: StreamEvent) -> Update {
        match event {
            StreamEvent::MessageStart { message } => {
                self.message_id = message.id;
                self.model = message.model;
                if let Some(u) = message.usage {
                    self.usage.merge(&u);
                }
                Update::None
            }
            StreamEvent::ContentBlockStart { index, content_block } => {
                self.produced_output = true;
                let kind = content_block.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                while self.blocks.len() <= index {
                    self.blocks.push(Value::Null);
                }
                self.blocks[index] = content_block.clone();
                match kind.as_str() {
                    "tool_use" | "server_tool_use" => {
                        self.tool_json.push((index, String::new()));
                        let name = content_block.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        Update::ToolUseStart { name }
                    }
                    "fallback" => Update::Fallback,
                    "text" => match content_block.get("text").and_then(Value::as_str) {
                        Some(t) if !t.is_empty() => Update::Text(t.to_string()),
                        _ => Update::None,
                    },
                    _ => Update::None,
                }
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                let Some(block) = self.blocks.get_mut(index).and_then(Value::as_object_mut) else {
                    return Update::None;
                };
                match delta {
                    Delta::TextDelta { text } => {
                        append_str(block, "text", &text);
                        Update::Text(text)
                    }
                    Delta::ThinkingDelta { thinking } => {
                        append_str(block, "thinking", &thinking);
                        Update::None
                    }
                    Delta::SignatureDelta { signature } => {
                        append_str(block, "signature", &signature);
                        Update::None
                    }
                    Delta::CitationsDelta { citation } => {
                        let list = block.entry("citations").or_insert_with(|| Value::Array(vec![]));
                        if !list.is_array() {
                            *list = Value::Array(vec![]);
                        }
                        list.as_array_mut().unwrap().push(citation);
                        Update::None
                    }
                    Delta::InputJsonDelta { partial_json } => {
                        if let Some((_, buf)) = self.tool_json.iter_mut().find(|(i, _)| *i == index) {
                            buf.push_str(&partial_json);
                        }
                        Update::None
                    }
                    Delta::Unknown => Update::None,
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                if let Some(pos) = self.tool_json.iter().position(|(i, _)| *i == index) {
                    let (_, json) = self.tool_json.remove(pos);
                    let input = if json.trim().is_empty() {
                        Value::Object(Map::new())
                    } else {
                        // Invalid JSON is kept as a string so the tool loop can report it.
                        serde_json::from_str(&json).unwrap_or(Value::String(json))
                    };
                    if let Some(block) = self.blocks.get_mut(index).and_then(Value::as_object_mut) {
                        block.insert("input".into(), input);
                    }
                }
                Update::None
            }
            StreamEvent::MessageDelta { delta, usage } => {
                if delta.stop_reason.is_some() {
                    self.stop_reason = delta.stop_reason;
                }
                if delta.stop_details.is_some() {
                    self.stop_details = delta.stop_details;
                }
                if let Some(u) = usage {
                    self.usage.merge(&u);
                }
                Update::None
            }
            StreamEvent::MessageStop => {
                self.done = true;
                Update::None
            }
            StreamEvent::Ping | StreamEvent::Unknown => Update::None,
            StreamEvent::Error { error } => Update::Error { kind: error.kind, message: error.message },
        }
    }

    /// The finished content blocks, in index order.
    pub fn content(&self) -> Vec<ContentBlock> {
        self.blocks
            .iter()
            .filter(|b| !b.is_null())
            .map(|b| serde_json::from_value(b.clone()).unwrap_or_else(|_| ContentBlock::Other(b.clone())))
            .collect()
    }
}

fn append_str(block: &mut Map<String, Value>, key: &str, more: &str) {
    match block.get_mut(key) {
        Some(Value::String(s)) => s.push_str(more),
        _ => {
            block.insert(key.into(), Value::String(more.to_string()));
        }
    }
}
