//! Request and response types for the Messages API. Only the fields this app
//! uses are modeled; unknown block, delta, and event types fall through to
//! catch-all variants instead of failing to parse.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------- request

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a [SystemBlock],
    pub messages: &'a [Message],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub tools: &'a [Value],
    pub stream: bool,
    pub output_config: OutputConfig<'a>,
    /// `"default"`: let the API pick a fallback model when a request is
    /// declined by a safety classifier (beta `server-side-fallback-2026-07-01`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Value>,
    /// Top-level automatic cache breakpoint on the last cacheable block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputConfig<'a> {
    pub effort: &'a str,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self { kind: "text".into(), text: text.into(), cache_control: None }
    }

    pub fn cached(mut self) -> Self {
        self.cache_control = Some(CacheControl::ephemeral());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self { kind: "ephemeral".into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Mid-conversation operator message (e.g. viewing-state changes).
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![ContentBlock::text(text)] }
    }

    pub fn system_text(text: impl Into<String>) -> Self {
        Self { role: Role::System, content: vec![ContentBlock::text(text)] }
    }
}

/// A content block. Assistant blocks are echoed back exactly as received
/// (thinking signatures must round-trip unchanged), so anything we don't model
/// is kept verbatim in `Other`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<Value>,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    #[serde(untagged)]
    Other(Value),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), citations: None }
    }

    pub fn jpeg_base64(data: String) -> Self {
        Self::Image {
            source: ImageSource { kind: "base64".into(), media_type: "image/jpeg".into(), data },
        }
    }

    /// The block's `type` string.
    pub fn kind(&self) -> &str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::ToolUse { .. } => "tool_use",
            Self::ToolResult { .. } => "tool_result",
            Self::Thinking { .. } => "thinking",
            Self::RedactedThinking { .. } => "redacted_thinking",
            Self::Other(v) => v.get("type").and_then(Value::as_str).unwrap_or(""),
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
    pub data: String,
}

impl std::fmt::Debug for ImageSource {
    // Never print image bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageSource({}, {} base64 bytes)", self.media_type, self.data.len())
    }
}

// ---------------------------------------------------------------- stream events

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: Value,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageStart {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
    CitationsDelta { citation: Value },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
}

impl Usage {
    /// Later usage reports carry cumulative values for the fields they include.
    pub fn merge(&mut self, other: &Usage) {
        let keep = |a: &mut Option<u64>, b: Option<u64>| {
            if b.is_some() {
                *a = b;
            }
        };
        keep(&mut self.input_tokens, other.input_tokens);
        keep(&mut self.output_tokens, other.output_tokens);
        keep(&mut self.cache_creation_input_tokens, other.cache_creation_input_tokens);
        keep(&mut self.cache_read_input_tokens, other.cache_read_input_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

/// Body of a non-2xx HTTP response.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorResponse {
    pub error: ApiError,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_blocks_round_trip_verbatim() {
        let raw = json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}});
        let block: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(block.kind(), "fallback");
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
    }

    #[test]
    fn thinking_round_trips() {
        let raw = json!({"type": "thinking", "thinking": "", "signature": "sig=="});
        let block: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert!(matches!(block, ContentBlock::Thinking { .. }));
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
    }

    #[test]
    fn tool_result_omits_false_is_error() {
        let b = ContentBlock::ToolResult {
            tool_use_id: "t".into(),
            content: vec![ContentBlock::text("x")],
            is_error: false,
        };
        assert_eq!(
            serde_json::to_value(&b).unwrap(),
            json!({"type": "tool_result", "tool_use_id": "t", "content": [{"type": "text", "text": "x"}]})
        );
    }

    #[test]
    fn unknown_event_and_delta_types_parse() {
        let e: StreamEvent = serde_json::from_str(r#"{"type":"brand_new_event","x":1}"#).unwrap();
        assert_eq!(e, StreamEvent::Unknown);
        let e: StreamEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"future_delta","y":2}}"#,
        )
        .unwrap();
        assert_eq!(e, StreamEvent::ContentBlockDelta { index: 0, delta: Delta::Unknown });
    }
}
