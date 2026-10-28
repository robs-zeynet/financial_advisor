//! Claude Messages API integration over raw HTTP (there is no official Rust
//! SDK): request building, streaming with retries, the `view_screen` tool loop,
//! prompt caching, and refusal fallbacks.

pub mod sse;
pub mod types;

use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{ClaudeConfig, Config};
use sse::{Accumulator, SseParser, Update};
use types::{ContentBlock, ErrorResponse, Message, MessagesRequest, OutputConfig, Role, SystemBlock};

/// Appendix A, verbatim. Frozen: never interpolate per-request values.
pub const ADVISOR_SYSTEM_PROMPT: &str = include_str!("../prompts/advisor_system.md");
/// Appendix B, used by the end-of-session summarizer (M3).
pub const SUMMARY_TEMPLATE: &str = include_str!("../prompts/summary_template.md");

/// Added to the system prompt only when web search is enabled in config (P7).
pub const WEB_SEARCH_RULE: &str = "\n\nWeb search\n\
- You have a web_search tool for general market facts, fund data, and news. \
Never put personal data in a query: no names, account details, balances, \
holdings sizes, or anything you saw on the user's screen. Search for general \
topics only (for example \"VTSAX expense ratio\", not the user's position).";

pub const VIEW_SCREEN: &str = "view_screen";
pub const PAUSED_TEXT: &str = "Viewing is paused by the user.";
pub const REFUSAL_APOLOGY: &str = "Sorry, I can't help with that one. Could we try it a different way?";

/// Betas: server-side refusal fallbacks and context editing.
pub const BETAS: &[&str] = &["server-side-fallback-2026-07-01", "context-management-2025-06-27"];

const MAX_TOOL_ROUNDS: usize = 8;

/// The frozen system prompt for this configuration.
pub fn system_prompt(cfg: &Config) -> String {
    let mut s = ADVISOR_SYSTEM_PROMPT.to_string();
    if cfg.web_search.enabled {
        s.push_str(WEB_SEARCH_RULE);
    }
    s
}

/// System prompt, then the session's memory block, with the cache breakpoint
/// after memory. Both stay byte-identical for the whole session.
pub fn system_blocks(system_prompt: &str, memory: &str) -> Vec<SystemBlock> {
    vec![SystemBlock::text(system_prompt), SystemBlock::text(memory).cached()]
}

pub fn view_screen_tool() -> Value {
    json!({
        "name": VIEW_SCREEN,
        "description": "Look at the user's shared browser window or tab. Returns the most recent \
screen frame plus up to three earlier distinct frames captured since your last look, each \
labeled with when it was captured. If the user has paused viewing, returns a note saying so \
instead. Use it whenever the answer depends on what is on their screen.",
        "input_schema": {
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Briefly, what you want to look at and why."
                }
            },
            "required": ["reason"],
            "additionalProperties": false
        },
        "strict": true
    })
}

/// The complete tool list. `view_screen` is the only client tool; web search
/// is added only when explicitly enabled.
pub fn tools(cfg: &Config) -> Vec<Value> {
    let mut tools = vec![view_screen_tool()];
    if cfg.web_search.enabled {
        tools.push(json!({
            "type": "web_search_20260209",
            "name": "web_search",
            "max_uses": cfg.web_search.max_uses
        }));
    }
    tools
}

/// Server-side context editing: keep only the most recent screen views.
pub fn context_management(cfg: &ClaudeConfig) -> Value {
    json!({
        "edits": [{
            "type": "clear_tool_uses_20250919",
            "trigger": {"type": "input_tokens", "value": cfg.clear_trigger_tokens},
            "keep": {"type": "tool_uses", "value": cfg.keep_screen_views},
            "exclude_tools": ["web_search"]
        }]
    })
}

/// An append-only conversation. Earlier turns are never edited (preserved
/// thinking validates the history), so state changes are appended as
/// mid-conversation system messages.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub system: Vec<SystemBlock>,
    pub tools: Vec<Value>,
    pub messages: Vec<Message>,
    pending_notes: Vec<String>,
}

impl Conversation {
    pub fn new(system: Vec<SystemBlock>, tools: Vec<Value>) -> Self {
        Self { system, tools, messages: Vec::new(), pending_notes: Vec::new() }
    }

    /// Queues an operator note. It is appended as a `role: "system"` message
    /// right after the next user message (the API requires it to follow one).
    pub fn note(&mut self, text: impl Into<String>) {
        self.pending_notes.push(text.into());
    }

    pub fn push_user(&mut self, content: Vec<ContentBlock>) {
        self.messages.push(Message { role: Role::User, content });
        if !self.pending_notes.is_empty() {
            let text = std::mem::take(&mut self.pending_notes).join("\n");
            self.messages.push(Message::system_text(text));
        }
    }

    pub fn push_assistant(&mut self, content: Vec<ContentBlock>) {
        if !content.is_empty() {
            self.messages.push(Message { role: Role::Assistant, content });
        }
    }

    /// Tool-use blocks of the last assistant message that still lack results.
    fn dangling_tool_uses(&self) -> Vec<String> {
        match self.messages.last() {
            Some(m) if m.role == Role::Assistant => m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("ANTHROPIC_API_KEY is not set")]
    MissingApiKey,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Claude API error {status}: {kind}: {message}")]
    Api { status: u16, kind: String, message: String },
    #[error("stream error: {kind}: {message}")]
    Stream { kind: String, message: String },
    #[error("cancelled")]
    Cancelled,
}

pub struct ClaudeClient {
    http: reqwest::Client,
    cfg: ClaudeConfig,
    api_key: Option<String>,
}

impl ClaudeClient {
    pub fn new(cfg: ClaudeConfig, api_key: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        Self { http, cfg, api_key }
    }

    pub fn config(&self) -> &ClaudeConfig {
        &self.cfg
    }

    pub fn build_request<'a>(&'a self, conv: &'a Conversation, effort: &'a str) -> MessagesRequest<'a> {
        MessagesRequest {
            model: &self.cfg.model,
            max_tokens: self.cfg.max_tokens,
            system: &conv.system,
            messages: &conv.messages,
            tools: &conv.tools,
            stream: true,
            output_config: OutputConfig { effort },
            fallbacks: Some("default"),
            context_management: Some(context_management(&self.cfg)),
        }
    }

    /// Sends one streaming request, retrying 429/5xx/overloaded with backoff
    /// (honoring `retry-after`) as long as no output has been produced.
    pub async fn stream(
        &self,
        req: &MessagesRequest<'_>,
        cancel: &CancellationToken,
        on_update: &mut (impl FnMut(Update) + Send),
    ) -> Result<Accumulator, ClaudeError> {
        let key = self.api_key.as_deref().ok_or(ClaudeError::MissingApiKey)?;
        let body = serde_json::to_vec(req).expect("request serializes");
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let mut attempt: u32 = 0;
        loop {
            let send = self
                .http
                .post(&url)
                .header("x-api-key", key)
                .header("anthropic-version", &self.cfg.api_version)
                .header("anthropic-beta", BETAS.join(","))
                .header("content-type", "application/json")
                .body(body.clone())
                .send();
            let resp = tokio::select! {
                _ = cancel.cancelled() => return Err(ClaudeError::Cancelled),
                r = send => r,
            };
            let retry_delay = match resp {
                Err(e) if attempt < self.cfg.max_retries && (e.is_connect() || e.is_timeout()) => {
                    backoff(attempt, None)
                }
                Err(e) => return Err(e.into()),
                Ok(resp) if resp.status().is_success() => {
                    match self.read_stream(resp, cancel, on_update).await {
                        Err(ClaudeError::Stream { kind, message })
                            if attempt < self.cfg.max_retries && retryable_kind(&kind) =>
                        {
                            tracing::warn!(%kind, %message, "stream error before output; retrying");
                            backoff(attempt, None)
                        }
                        other => return other,
                    }
                }
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<f64>().ok());
                    let text = resp.text().await.unwrap_or_default();
                    let (kind, message) = match serde_json::from_str::<ErrorResponse>(&text) {
                        Ok(e) => (e.error.kind, e.error.message),
                        Err(_) => ("http_error".into(), text),
                    };
                    if attempt < self.cfg.max_retries && retryable_status(status) {
                        tracing::warn!(status, %kind, "Claude API error; retrying");
                        backoff(attempt, retry_after)
                    } else {
                        return Err(ClaudeError::Api { status, kind, message });
                    }
                }
            };
            attempt += 1;
            tokio::select! {
                _ = cancel.cancelled() => return Err(ClaudeError::Cancelled),
                _ = tokio::time::sleep(retry_delay) => {}
            }
        }
    }

    async fn read_stream(
        &self,
        resp: reqwest::Response,
        cancel: &CancellationToken,
        on_update: &mut (impl FnMut(Update) + Send),
    ) -> Result<Accumulator, ClaudeError> {
        use futures_util::StreamExt;
        let mut parser = SseParser::new();
        let mut acc = Accumulator::new();
        let mut body = resp.bytes_stream();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(ClaudeError::Cancelled),
                c = body.next() => c,
            };
            let ended = chunk.is_none();
            let frames = match chunk {
                Some(c) => parser.feed(&c?),
                None => parser.finish().into_iter().collect(),
            };
            for frame in frames {
                let event = match sse::parse_event(&frame) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error = %e, "skipping unparseable SSE event");
                        continue;
                    }
                };
                match acc.apply(event) {
                    Update::Error { kind, message } => {
                        // A retry after visible output would duplicate it.
                        let kind = if acc.produced_output { format!("{kind} (after output)") } else { kind };
                        return Err(ClaudeError::Stream { kind, message });
                    }
                    Update::None => {}
                    u => on_update(u),
                }
            }
            if acc.done {
                return Ok(acc);
            }
            if ended {
                return if acc.stop_reason.is_some() {
                    Ok(acc)
                } else {
                    let kind = if acc.produced_output { "incomplete_stream (after output)" } else { "incomplete_stream" };
                    Err(ClaudeError::Stream {
                        kind: kind.into(),
                        message: "stream ended before message_stop".into(),
                    })
                };
            }
        }
    }
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn retryable_kind(kind: &str) -> bool {
    matches!(kind, "overloaded_error" | "api_error" | "rate_limit_error" | "incomplete_stream")
}

fn backoff(attempt: u32, retry_after: Option<f64>) -> Duration {
    if let Some(secs) = retry_after {
        return Duration::from_secs_f64(secs.clamp(0.0, 60.0));
    }
    Duration::from_millis(500u64 << attempt.min(6)).min(Duration::from_secs(30))
}

/// After a mid-output fallback, blocks before the last `fallback` marker that
/// belong to the declined model (thinking, tool calls) must not be echoed back.
pub fn sanitize_for_echo(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let Some(last_fallback) = blocks.iter().rposition(|b| b.kind() == "fallback") else {
        return blocks;
    };
    blocks
        .into_iter()
        .enumerate()
        .filter(|(i, b)| {
            *i >= last_fallback
                || !matches!(b.kind(), "thinking" | "redacted_thinking" | "tool_use" | "server_tool_use")
        })
        .map(|(_, b)| b)
        .collect()
}

/// Result of running one tool call.
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    /// Short human-readable description for the UI and transcript.
    pub detail: String,
}

/// Executes client tools (the only one is `view_screen`) and supplies
/// operator notes, such as viewing-state changes, queued since the last turn.
pub trait ToolHost: Sync {
    fn take_notes(&self) -> Vec<String> {
        Vec::new()
    }

    fn view_screen(&self, reason: &str) -> impl Future<Output = ToolOutput> + Send;
}

/// Progress of a turn, for the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    Text(String),
    Tool { name: String, detail: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub stop_reason: String,
    /// Assistant text shown to the user this turn.
    pub text: String,
}

/// Runs one user turn to completion: stream, execute `view_screen` calls, and
/// loop until the model stops. The user message must already be appended.
pub async fn run_turn(
    client: &ClaudeClient,
    conv: &mut Conversation,
    host: &impl ToolHost,
    effort: &str,
    cancel: &CancellationToken,
    events: &mpsc::UnboundedSender<TurnEvent>,
) -> Result<TurnOutcome, ClaudeError> {
    let mut spoken = String::new();
    for _ in 0..MAX_TOOL_ROUNDS {
        let mut round_text = String::new();
        let result = {
            let req = client.build_request(conv, effort);
            client
                .stream(&req, cancel, &mut |u| match u {
                    Update::Text(t) => {
                        round_text.push_str(&t);
                        let _ = events.send(TurnEvent::Text(t));
                    }
                    Update::ToolUseStart { name } if name != VIEW_SCREEN => {
                        let _ = events.send(TurnEvent::Tool { name, detail: String::new() });
                    }
                    _ => {}
                })
                .await
        };
        let acc = match result {
            Ok(acc) => acc,
            Err(ClaudeError::Cancelled) => {
                // Record only what was already said; drop partial thinking and tool calls.
                spoken.push_str(&round_text);
                if !round_text.is_empty() {
                    conv.push_assistant(vec![ContentBlock::text(round_text)]);
                }
                return Err(ClaudeError::Cancelled);
            }
            Err(e) => return Err(e),
        };
        let stop_reason = acc.stop_reason.clone().unwrap_or_else(|| "unknown".into());
        tracing::info!(
            model = %acc.model,
            %stop_reason,
            usage = ?acc.usage,
            "Claude response"
        );

        // Check stop_reason before reading content.
        if stop_reason == "refusal" {
            tracing::warn!(details = ?acc.stop_details, "turn ended in refusal");
            let _ = events.send(TurnEvent::Text(REFUSAL_APOLOGY.into()));
            conv.push_assistant(vec![ContentBlock::text(REFUSAL_APOLOGY)]);
            return Ok(TurnOutcome { stop_reason, text: REFUSAL_APOLOGY.into() });
        }

        spoken.push_str(&round_text);
        let content = sanitize_for_echo(acc.content());
        let tool_calls: Vec<(String, String, Value)> = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect();
        conv.push_assistant(content);

        match stop_reason.as_str() {
            "tool_use" if !tool_calls.is_empty() => {
                let mut results = Vec::new();
                for (id, name, input) in tool_calls {
                    let out = if name == VIEW_SCREEN {
                        let reason = input.get("reason").and_then(Value::as_str).unwrap_or("");
                        host.view_screen(reason).await
                    } else {
                        ToolOutput {
                            content: vec![ContentBlock::text(format!("Unknown tool: {name}"))],
                            is_error: true,
                            detail: format!("unknown tool {name}"),
                        }
                    };
                    let _ = events.send(TurnEvent::Tool { name, detail: out.detail });
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: out.content,
                        is_error: out.is_error,
                    });
                }
                for note in host.take_notes() {
                    conv.note(note);
                }
                conv.push_user(results);
                if cancel.is_cancelled() {
                    return Err(ClaudeError::Cancelled);
                }
            }
            // A server tool (web search) paused a long turn; resend to continue.
            "pause_turn" => {}
            _ => return Ok(TurnOutcome { stop_reason, text: spoken }),
        }
    }
    Ok(TurnOutcome { stop_reason: "max_tool_rounds".into(), text: spoken })
}

/// Closes any tool calls left without results (e.g. after an error), so the
/// next request is valid. Appends; never edits earlier turns.
pub fn close_dangling_tool_uses(conv: &mut Conversation) {
    let ids = conv.dangling_tool_uses();
    if ids.is_empty() {
        return;
    }
    let results = ids
        .into_iter()
        .map(|id| ContentBlock::ToolResult {
            tool_use_id: id,
            content: vec![ContentBlock::text("Interrupted before the tool ran.")],
            is_error: true,
        })
        .collect();
    conv.push_user(results);
}
