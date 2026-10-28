//! Session lifecycle and turn management.
//!
//! One `Session` exists per call. It owns the viewing state (PAUSED at start,
//! P2), the in-memory frame ring (P3), and the append-only conversation. Each
//! user turn runs as a task; a new turn cancels the one in flight.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use protocol::{is_pause_command, FrameHeader, ServerMsg};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::claude::{
    self, run_turn, types::ContentBlock, ClaudeClient, ClaudeError, Conversation, ToolHost,
    ToolOutput, TurnEvent, PAUSED_TEXT,
};
use crate::config::Config;
use crate::frames::FrameRing;
use crate::memory;

pub struct AppState {
    pub config: Config,
    pub claude: ClaudeClient,
}

/// State touched by both the WebSocket reader and the turn task.
#[derive(Default)]
pub struct Shared {
    pub viewing: bool,
    /// The server paused viewing (e.g. "pause viewing") and the client hasn't
    /// confirmed yet; frames already in flight are dropped without counting
    /// as a violation.
    pub awaiting_pause_ack: bool,
    pub frames: FrameRing,
    pub violations: u64,
    /// Viewing-state notes for the model, appended after the next user message.
    pub pending_notes: Vec<String>,
}

pub struct Session {
    pub id: String,
    app: Arc<AppState>,
    shared: Arc<Mutex<Shared>>,
    conv: Arc<tokio::sync::Mutex<Conversation>>,
    out: mpsc::UnboundedSender<ServerMsg>,
    turn_cancel: Option<CancellationToken>,
}

const NOTE_RESUMED: &str =
    "The user resumed screen viewing. view_screen can now see their shared window or tab.";
const NOTE_PAUSED: &str =
    "The user paused screen viewing. view_screen will return nothing until they resume.";

impl Session {
    /// Starts a session. Viewing is always PAUSED at start.
    pub fn start(app: Arc<AppState>, out: mpsc::UnboundedSender<ServerMsg>) -> anyhow::Result<Self> {
        let memory = memory::load_memory(&app.config.data_dir)?;
        let conv = Conversation::new(
            claude::system_blocks(&claude::system_prompt(&app.config), &memory),
            claude::tools(&app.config),
        );
        let id = new_session_id();
        let session = Self {
            id: id.clone(),
            shared: Arc::new(Mutex::new(Shared::default())),
            conv: Arc::new(tokio::sync::Mutex::new(conv)),
            out,
            turn_cancel: None,
            app,
        };
        session.send(ServerMsg::SessionStarted {
            session_id: id,
            viewing: false,
            capture: session.app.config.capture.settings(),
        });
        Ok(session)
    }

    pub fn viewing(&self) -> bool {
        self.shared.lock().unwrap().viewing
    }

    pub fn violations(&self) -> u64 {
        self.shared.lock().unwrap().violations
    }

    pub fn frame_count(&self) -> usize {
        self.shared.lock().unwrap().frames.len()
    }

    /// A snapshot of the conversation (for tests and records).
    pub async fn conversation(&self) -> Conversation {
        self.conv.lock().await.clone()
    }

    fn send(&self, msg: ServerMsg) {
        let _ = self.out.send(msg);
    }

    /// The user toggled viewing (button or hotkey), or acknowledged a pause.
    pub fn set_viewing(&mut self, viewing: bool) {
        let mut s = self.shared.lock().unwrap();
        let changed = s.viewing != viewing;
        s.viewing = viewing;
        s.awaiting_pause_ack = false;
        if !viewing {
            s.frames.wipe();
        }
        if changed {
            s.pending_notes.push(if viewing { NOTE_RESUMED } else { NOTE_PAUSED }.into());
        }
        drop(s);
        self.send(ServerMsg::ViewingState { viewing });
    }

    /// Server-initiated pause: takes effect here at once and tells the client
    /// to stop capturing.
    pub fn pause_from_server(&mut self) {
        let mut s = self.shared.lock().unwrap();
        if s.viewing {
            s.viewing = false;
            s.awaiting_pause_ack = true;
            s.pending_notes.push(NOTE_PAUSED.into());
        }
        s.frames.wipe();
        drop(s);
        self.send(ServerMsg::ViewingState { viewing: false });
    }

    /// A binary frame from the client. Frames are accepted only while VIEWING;
    /// anything else is dropped and counted as a protocol violation.
    pub fn handle_frame(&mut self, msg: &[u8]) {
        let (header, jpeg) = match FrameHeader::decode(msg) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(?e, "malformed frame dropped");
                return;
            }
        };
        let mut s = self.shared.lock().unwrap();
        if !s.viewing {
            if s.awaiting_pause_ack {
                return; // was in flight when we asked the client to pause
            }
            s.violations += 1;
            let total = s.violations;
            drop(s);
            tracing::warn!(total, "frame received while PAUSED; dropped (protocol violation)");
            self.send(ServerMsg::ProtocolViolation {
                detail: "screen frame received while viewing is paused; dropped".into(),
                total,
            });
            return;
        }
        let meta = s.frames.push(header, jpeg.to_vec());
        tracing::debug!(?meta, "frame stored in memory");
    }

    /// Starts a turn for typed (or, later, transcribed) user text. Cancels any
    /// turn in flight first.
    pub fn user_text(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if is_pause_command(&text) {
            self.pause_from_server();
        }
        self.cancel_turn();
        self.send(ServerMsg::UserTurn { text: text.clone() });

        let cancel = CancellationToken::new();
        self.turn_cancel = Some(cancel.clone());
        let app = self.app.clone();
        let conv = self.conv.clone();
        let host = SessionHost { shared: self.shared.clone() };
        let out = self.out.clone();
        tokio::spawn(async move {
            let mut conv = conv.lock().await;
            if cancel.is_cancelled() {
                return;
            }
            claude::close_dangling_tool_uses(&mut conv);
            for note in host.take_notes() {
                conv.note(note);
            }
            conv.push_user(vec![ContentBlock::text(text)]);

            let (tx, mut rx) = mpsc::unbounded_channel();
            let fwd_out = out.clone();
            let forward = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let msg = match ev {
                        TurnEvent::Text(text) => ServerMsg::AssistantDelta { text },
                        TurnEvent::Tool { name, detail } => ServerMsg::ToolActivity { tool: name, detail },
                    };
                    let _ = fwd_out.send(msg);
                }
            });
            let effort = app.claude.config().effort.clone();
            let result = run_turn(&app.claude, &mut conv, &host, &effort, &cancel, &tx).await;
            drop(tx);
            let _ = forward.await;
            let stop_reason = match result {
                Ok(outcome) => outcome.stop_reason,
                Err(ClaudeError::Cancelled) => "cancelled".into(),
                Err(e) => {
                    tracing::error!(error = %e, "turn failed");
                    claude::close_dangling_tool_uses(&mut conv);
                    let _ = out.send(ServerMsg::Error { message: e.to_string() });
                    "error".into()
                }
            };
            let _ = out.send(ServerMsg::AssistantDone { stop_reason });
        });
    }

    pub fn cancel_turn(&mut self) {
        if let Some(c) = self.turn_cancel.take() {
            c.cancel();
        }
    }

    /// Hang up: stop the turn, pause, and wipe frames.
    pub fn end(&mut self) {
        self.cancel_turn();
        let mut s = self.shared.lock().unwrap();
        s.viewing = false;
        s.frames.wipe();
        drop(s);
        self.send(ServerMsg::SessionEnded { session_id: self.id.clone() });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel_turn();
        if let Ok(mut s) = self.shared.lock() {
            s.viewing = false;
            s.frames.wipe();
        }
    }
}

struct SessionHost {
    shared: Arc<Mutex<Shared>>,
}

impl ToolHost for SessionHost {
    fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().unwrap().pending_notes)
    }

    fn view_screen(&self, reason: &str) -> impl std::future::Future<Output = ToolOutput> + Send {
        tracing::info!(%reason, "view_screen");
        let out = self.look();
        async move { out }
    }
}

impl SessionHost {
    fn look(&self) -> ToolOutput {
        let mut s = self.shared.lock().unwrap();
        if !s.viewing {
            return ToolOutput {
                content: vec![ContentBlock::text(PAUSED_TEXT)],
                is_error: false,
                detail: "viewing paused".into(),
            };
        }
        let frames = s.frames.take_for_view();
        if frames.is_empty() {
            return ToolOutput {
                content: vec![ContentBlock::text(
                    "Viewing is on, but no screen frame has arrived yet. The user may need to \
                     share a window or tab.",
                )],
                is_error: false,
                detail: "no frames yet".into(),
            };
        }
        let now = now_ms();
        let n = frames.len();
        let mut content = Vec::with_capacity(n * 2);
        for (i, f) in frames.iter().enumerate() {
            let age = now.saturating_sub(f.captured_at_ms) as f64 / 1000.0;
            let which = if i + 1 == n { "latest" } else { "earlier" };
            content.push(ContentBlock::text(format!(
                "Frame {} of {n} ({which}), captured {age:.0} seconds ago, {}x{} px:",
                i + 1,
                f.width,
                f.height
            )));
            content.push(ContentBlock::jpeg_base64(
                base64::engine::general_purpose::STANDARD.encode(&f.jpeg),
            ));
        }
        ToolOutput { content, is_error: false, detail: format!("viewed screen: {n} frames") }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn new_session_id() -> String {
    let secs = now_ms() / 1000;
    let short = uuid::Uuid::new_v4().simple().to_string();
    format!("{secs}-{}", &short[..8])
}
