//! Session lifecycle and turn management.
//!
//! One `Session` exists per call. It owns the viewing state (PAUSED at start,
//! P2), the in-memory live ring and scan buffer (P3), and the append-only
//! conversation. Each user turn runs as a task; a new turn cancels the one in
//! flight.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use protocol::{
    decode_mic, is_pause_command, spoken_command, FrameHeader, ListenState, ServerMsg, SpokenCommand, VoiceCaps,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::claude::{
    self, run_turn, types::ContentBlock, ClaudeClient, ClaudeError, Conversation, ToolHost,
    ToolOutput, TurnEvent, PAUSED_TEXT,
};
use crate::config::Config;
use crate::frames::{FrameRing, ScanBuffer, ScanReject};
use crate::memory;
use crate::screen_notes::ScreenNotes;
use crate::stt::SpeechToText;
use crate::tts::{SentenceChunker, Speaker, TtsEngine};
use crate::vad::{Vad, VadEvent};

pub struct AppState {
    pub config: Config,
    pub claude: ClaudeClient,
    pub voice: Voice,
}

/// Local voice engines, when available.
#[derive(Clone, Default)]
pub struct Voice {
    pub stt: Option<Arc<dyn SpeechToText>>,
    pub tts: Option<Arc<dyn TtsEngine>>,
}

impl Voice {
    pub fn caps(&self) -> VoiceCaps {
        VoiceCaps { stt: self.stt.is_some(), tts: self.tts.is_some() }
    }
}

/// What the session sends to the browser: JSON events or TTS audio.
#[derive(Debug)]
pub enum Outbound {
    Msg(ServerMsg),
    Audio(Vec<u8>),
}

/// Results of background work, delivered back to the connection loop.
#[derive(Debug)]
pub enum SessionEvent {
    /// Speech-to-text finished for an utterance in session `session_id`.
    Transcript { session_id: String, text: String },
}

/// State touched by both the WebSocket reader and the turn task.
#[derive(Default)]
pub struct Shared {
    /// Live viewing (not counting a scan in progress).
    pub viewing: bool,
    /// The server paused viewing (e.g. "pause viewing") and the client hasn't
    /// confirmed yet; frames already in flight are dropped without counting
    /// as a violation.
    pub awaiting_pause_ack: bool,
    pub frames: FrameRing,
    pub scan: ScanBuffer,
    /// This session's screen notes (P9). RAM only.
    pub notes: ScreenNotes,
    pub violations: u64,
    /// Viewing-state notes for the model, appended after the next user message.
    pub pending_notes: Vec<String>,
    /// A turn task is running (its text may still be streaming).
    pub turn_running: bool,
    /// Full text of the last completed reply, for barge-in after it finished
    /// streaming but while it was still being spoken.
    pub last_reply: String,
}

impl Shared {
    /// Explicit pause: stops viewing, cancels any scan, wipes both buffers.
    /// Returns the id of a scan that was cancelled mid-way.
    fn pause_and_wipe(&mut self) -> Option<u32> {
        let cancelled = self.scan.is_active().then(|| self.scan.scan_id());
        let had_anything = self.viewing || cancelled.is_some() || !self.scan.is_empty();
        self.viewing = false;
        self.frames.wipe();
        self.scan.wipe();
        if had_anything {
            self.pending_notes.push(NOTE_PAUSED.into());
        }
        cancelled
    }

    /// Whether frames are being captured: live viewing, or a scan in progress.
    fn live_capture_on(&self) -> bool {
        self.viewing || self.scan.is_active()
    }
}

pub struct Session {
    pub id: String,
    app: Arc<AppState>,
    shared: Arc<Mutex<Shared>>,
    conv: Arc<tokio::sync::Mutex<Conversation>>,
    out: mpsc::UnboundedSender<Outbound>,
    events: mpsc::UnboundedSender<SessionEvent>,
    turn_cancel: Option<CancellationToken>,
    vad: Vad,
    mic_on: bool,
    speaker: Option<Arc<Speaker>>,
}

const NOTE_RESUMED: &str =
    "The user resumed screen viewing. view_screen can now see their shared window or tab.";
const NOTE_PAUSED: &str = "The user paused screen viewing. view_screen will return nothing until \
                           they resume, and any page scan was discarded.";

fn scan_finished_note(segments: usize, gaps: &[usize]) -> String {
    if segments == 0 {
        return "The user tried to scan a page, but no segments were captured.".into();
    }
    let mut note = format!(
        "The user scanned a page: {segments} segment{}",
        if segments == 1 { "" } else { "s" }
    );
    if !gaps.is_empty() {
        let list: Vec<String> = gaps.iter().map(|g| format!("{} and {}", g - 1, g)).collect();
        note.push_str(&format!(", with a possible gap between segments {}", list.join("; ")));
    }
    note.push_str(". Call view_screen with scope \"scan\" to see the whole page, top to bottom.");
    note
}

impl Session {
    /// Starts a session. Viewing is always PAUSED at start.
    pub fn start(
        app: Arc<AppState>,
        out: mpsc::UnboundedSender<Outbound>,
        events: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<Self> {
        let memory = memory::load_memory(&app.config.data_dir)?;
        let conv = Conversation::new(
            claude::system_blocks(&claude::system_prompt(&app.config), &memory),
            claude::tools(&app.config),
        );
        let id = new_session_id();
        let speaker = app.voice.tts.clone().map(|tts| Arc::new(Speaker::new(tts, out.clone())));
        let session = Self {
            id: id.clone(),
            shared: Arc::new(Mutex::new(Shared::default())),
            conv: Arc::new(tokio::sync::Mutex::new(conv)),
            out,
            events,
            turn_cancel: None,
            vad: Vad::new(app.config.voice.vad()),
            mic_on: true,
            speaker,
            app,
        };
        session.send(ServerMsg::SessionStarted {
            session_id: id,
            viewing: false,
            capture: session.app.config.capture.settings(),
            voice: session.app.voice.caps(),
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

    pub fn scan_segment_count(&self) -> usize {
        self.shared.lock().unwrap().scan.len()
    }

    /// Current screen notes as (page, notes) pairs.
    pub fn screen_notes(&self) -> Vec<(String, String)> {
        self.shared.lock().unwrap().notes.current().iter().map(|n| (n.page.clone(), n.notes.clone())).collect()
    }

    /// A snapshot of the conversation (for tests and records).
    pub async fn conversation(&self) -> Conversation {
        self.conv.lock().await.clone()
    }

    fn send(&self, msg: ServerMsg) {
        let _ = self.out.send(Outbound::Msg(msg));
    }

    fn max_scan_segments(&self) -> usize {
        self.app.config.capture.max_scan_segments as usize
    }

    fn scan_state(s: &Shared) -> ServerMsg {
        ServerMsg::ScanState {
            active: s.scan.is_active(),
            scan_id: s.scan.scan_id(),
            segments: s.scan.len() as u32,
            gaps: s.scan.gaps_before().len() as u32,
        }
    }

    /// The user toggled viewing (button or hotkey), or acknowledged a pause.
    /// Pausing is explicit: it cancels any scan and wipes everything.
    pub fn set_viewing(&mut self, viewing: bool) {
        let mut s = self.shared.lock().unwrap();
        s.awaiting_pause_ack = false;
        let mut cancelled = None;
        if viewing {
            if !s.viewing {
                s.viewing = true;
                s.pending_notes.push(NOTE_RESUMED.into());
            }
        } else {
            cancelled = s.pause_and_wipe();
        }
        let scan_msg = cancelled.map(|_| Self::scan_state(&s));
        drop(s);
        if let Some(m) = scan_msg {
            self.send(m);
        }
        self.send(ServerMsg::ViewingState { viewing });
    }

    /// Server-initiated pause: takes effect here at once and tells the client
    /// to stop capturing.
    pub fn pause_from_server(&mut self) {
        let mut s = self.shared.lock().unwrap();
        if s.live_capture_on() {
            s.awaiting_pause_ack = true;
        }
        let cancelled = s.pause_and_wipe();
        let scan_msg = cancelled.map(|_| Self::scan_state(&s));
        drop(s);
        if let Some(m) = scan_msg {
            self.send(m);
        }
        self.send(ServerMsg::ViewingState { viewing: false });
    }

    /// The user started a page scan: an explicit, bounded period of viewing.
    pub fn start_scan(&mut self, scan_id: u32) {
        let mut s = self.shared.lock().unwrap();
        s.awaiting_pause_ack = false;
        s.scan.start(scan_id);
        let msg = Self::scan_state(&s);
        drop(s);
        tracing::info!(scan_id, "page scan started");
        self.send(msg);
    }

    /// The user finished the scan (or it hit the segment cap). Viewing returns
    /// to its pre-scan state; the scan's segments stay for `view_screen`.
    pub fn end_scan(&mut self, scan_id: u32) {
        let mut s = self.shared.lock().unwrap();
        if !s.scan.is_active() || s.scan.scan_id() != scan_id {
            return;
        }
        self.finish_scan_locked(&mut s);
    }

    fn finish_scan_locked(&self, s: &mut MutexGuard<'_, Shared>) {
        s.scan.finish();
        if !s.viewing {
            s.frames.wipe(); // back to PAUSED: live copies of segments go
        }
        let note = scan_finished_note(s.scan.len(), &s.scan.gaps_before());
        tracing::info!(scan_id = s.scan.scan_id(), segments = s.scan.len(), "page scan finished");
        s.pending_notes.push(note);
        self.send(Self::scan_state(s));
    }

    fn violation(&self, mut s: MutexGuard<'_, Shared>, detail: &str) {
        s.violations += 1;
        let total = s.violations;
        drop(s);
        tracing::warn!(total, detail, "frame dropped (protocol violation)");
        self.send(ServerMsg::ProtocolViolation { detail: detail.into(), total });
    }

    /// A binary frame from the client. Live frames are accepted only while
    /// VIEWING; scan segments only for the active scan. Anything else, and any
    /// oversized frame, is dropped and counted as a protocol violation.
    pub fn handle_frame(&mut self, msg: &[u8]) {
        let (header, jpeg) = match FrameHeader::decode(msg) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(?e, "malformed frame dropped");
                return;
            }
        };
        let s = self.shared.lock().unwrap();
        let max_edge = self.app.config.capture.max_long_edge;
        if u32::from(header.width.max(header.height)) > max_edge {
            return self.violation(s, &format!("frame larger than {max_edge} px; dropped"));
        }
        if header.is_scan_segment() {
            self.handle_segment(s, header, jpeg);
            return;
        }
        let mut s = s;
        if !s.viewing {
            if s.awaiting_pause_ack {
                return; // was in flight when we asked the client to pause
            }
            return self.violation(s, "screen frame received while viewing is paused; dropped");
        }
        let meta = s.frames.push(header, jpeg.to_vec());
        tracing::debug!(?meta, "frame stored in memory");
    }

    fn handle_segment(&self, mut s: MutexGuard<'_, Shared>, header: FrameHeader, jpeg: &[u8]) {
        let max = self.max_scan_segments();
        match s.scan.push(header, jpeg.to_vec(), max) {
            Ok(meta) => {
                tracing::debug!(
                    ?meta,
                    scan_id = header.scan_id,
                    segment = header.segment_index,
                    offset_px = ?header.offset_px,
                    gap_before = header.gap_before,
                    "scan segment stored in memory"
                );
                // The newest segment is also what's on screen now.
                s.frames.push(FrameHeader { changed: true, ..header }, jpeg.to_vec());
                if s.scan.len() >= max {
                    self.finish_scan_locked(&mut s);
                } else {
                    self.send(Self::scan_state(&s));
                }
            }
            Err(_) if s.awaiting_pause_ack => {}
            Err(ScanReject::NotActive) => self.violation(s, "scan segment received with no active scan; dropped"),
            Err(ScanReject::Full) => self.violation(s, "scan segment beyond the segment limit; dropped"),
        }
    }

    // ------------------------------------------------------------ voice in

    /// Mic audio from the browser (16 kHz PCM16).
    pub fn handle_mic(&mut self, msg: &[u8]) {
        if !self.mic_on || self.app.voice.stt.is_none() {
            return;
        }
        let speaking = self.speaker.as_ref().is_some_and(|s| s.is_speaking());
        self.vad.set_echo_guard(speaking);
        for event in self.vad.feed(&decode_mic(msg)) {
            self.vad_event(event);
        }
    }

    pub fn mic_state(&mut self, on: bool) {
        self.mic_on = on;
        if !on {
            self.vad.reset();
            self.send(ServerMsg::Listen { state: ListenState::Idle });
        }
    }

    pub fn ptt_down(&mut self) {
        if self.app.voice.stt.is_some() {
            if let Some(e) = self.vad.force_start() {
                self.vad_event(e);
            }
        }
    }

    pub fn ptt_up(&mut self) {
        if let Some(e) = self.vad.force_end() {
            self.vad_event(e);
        }
    }

    fn vad_event(&mut self, event: VadEvent) {
        match event {
            VadEvent::SpeechStart => {
                // Barge-in: the user talking over the advisor stops it at once.
                if self.speaker.as_ref().is_some_and(|s| s.is_speaking()) {
                    tracing::info!("barge-in");
                    self.interrupt();
                }
                self.send(ServerMsg::Listen { state: ListenState::Hearing });
            }
            VadEvent::Discarded => self.send(ServerMsg::Listen { state: ListenState::Idle }),
            VadEvent::SpeechEnd(pcm) => {
                let Some(stt) = self.app.voice.stt.clone() else { return };
                self.send(ServerMsg::Listen { state: ListenState::Transcribing });
                let events = self.events.clone();
                let session_id = self.id.clone();
                tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    let secs = pcm.len() as f64 / protocol::MIC_SAMPLE_RATE as f64;
                    let text = match tokio::task::spawn_blocking(move || stt.transcribe(&pcm)).await {
                        Ok(Ok(t)) => t,
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "transcription failed");
                            String::new()
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "transcription task failed");
                            String::new()
                        }
                    };
                    tracing::info!(audio_s = secs, took_ms = started.elapsed().as_millis() as u64, "transcribed");
                    let _ = events.send(SessionEvent::Transcript { session_id, text });
                });
            }
        }
    }

    /// A finished transcript. Scan commands are handled here and never reach
    /// Claude; everything else becomes a user turn.
    pub fn voice_text(&mut self, text: String) {
        self.send(ServerMsg::Listen { state: ListenState::Idle });
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let scanning = self.shared.lock().unwrap().scan.is_active();
        match spoken_command(&text) {
            Some(command @ SpokenCommand::StartScan) => self.send(ServerMsg::CommandHeard { text, command }),
            Some(command @ SpokenCommand::EndScan) if scanning => self.send(ServerMsg::CommandHeard { text, command }),
            _ => self.user_text(text),
        }
    }

    /// Stops the advisor: freezes what was heard, silences playback, and
    /// cancels the turn in flight.
    ///
    /// If the reply had already finished streaming (so it's in the history in
    /// full) but was still being spoken, the history can't be edited; a
    /// system note tells the model how much the user actually heard.
    pub fn interrupt(&mut self) {
        if let Some(speaker) = &self.speaker {
            let was_speaking = speaker.stop();
            let mut s = self.shared.lock().unwrap();
            if was_speaking && !s.turn_running {
                let heard = speaker.heard_prefix().unwrap_or(0).min(s.last_reply.len());
                let mut cut = heard;
                while !s.last_reply.is_char_boundary(cut) {
                    cut -= 1;
                }
                let heard_text = s.last_reply[..cut].trim().to_string();
                let note = if heard_text.is_empty() {
                    "The user interrupted before hearing any of your last reply.".to_string()
                } else {
                    format!(
                        "The user interrupted you. They heard only this much of your last reply: \"{heard_text}\". \
                         They did not hear the rest."
                    )
                };
                s.pending_notes.push(note);
            }
        }
        self.cancel_turn();
    }

    /// Starts a turn for typed or transcribed user text. Interrupts any turn
    /// in flight first.
    pub fn user_text(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if is_pause_command(&text) {
            self.pause_from_server();
        }
        self.interrupt();
        self.send(ServerMsg::UserTurn { text: text.clone() });

        let cancel = CancellationToken::new();
        self.turn_cancel = Some(cancel.clone());
        let app = self.app.clone();
        let conv = self.conv.clone();
        let speaker = self.speaker.clone();
        let host = SessionHost { shared: self.shared.clone(), out: self.out.clone(), speaker: speaker.clone() };
        let out = self.out.clone();
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let mut conv = conv.lock().await;
            if cancel.is_cancelled() {
                return;
            }
            shared.lock().unwrap().turn_running = true;
            claude::close_dangling_tool_uses(&mut conv);
            for note in host.take_notes() {
                conv.note(note);
            }
            conv.push_user(vec![ContentBlock::text(text)]);

            let (tx, mut rx) = mpsc::unbounded_channel();
            let fwd_out = out.clone();
            let speech = speaker.as_ref().map(|s| s.begin());
            let forward = tokio::spawn(async move {
                let mut chunker = SentenceChunker::new();
                while let Some(ev) = rx.recv().await {
                    let msg = match ev {
                        TurnEvent::Text(text) => {
                            if let Some(h) = &speech {
                                for (sentence, end) in chunker.push(&text) {
                                    h.say(sentence, end);
                                }
                            }
                            ServerMsg::AssistantDelta { text }
                        }
                        TurnEvent::Tool { name, detail } => ServerMsg::ToolActivity { tool: name, detail },
                    };
                    let _ = fwd_out.send(Outbound::Msg(msg));
                }
                if let (Some(h), Some((sentence, end))) = (&speech, chunker.finish()) {
                    h.say(sentence, end);
                }
            });
            let effort = app.claude.config().effort.clone();
            let result = run_turn(&app.claude, &mut conv, &host, &effort, &cancel, &tx).await;
            drop(tx);
            let _ = forward.await;
            {
                let mut s = shared.lock().unwrap();
                s.turn_running = false;
                s.last_reply = match &result {
                    Ok(outcome) => outcome.text.clone(),
                    Err(_) => String::new(),
                };
            }
            let stop_reason = match result {
                Ok(outcome) => outcome.stop_reason,
                Err(ClaudeError::Cancelled) => "cancelled".into(),
                Err(e) => {
                    tracing::error!(error = %e, "turn failed");
                    claude::close_dangling_tool_uses(&mut conv);
                    let _ = out.send(Outbound::Msg(ServerMsg::Error { message: e.to_string() }));
                    "error".into()
                }
            };
            let _ = out.send(Outbound::Msg(ServerMsg::AssistantDone { stop_reason }));
        });
    }

    pub fn cancel_turn(&mut self) {
        if let Some(c) = self.turn_cancel.take() {
            c.cancel();
        }
    }

    /// Hang up: stop the turn and speech, pause, and wipe the frame buffers,
    /// notes, and any partial utterance.
    pub fn end(&mut self) {
        self.interrupt();
        self.vad.reset();
        let mut s = self.shared.lock().unwrap();
        s.viewing = false;
        s.frames.wipe();
        s.scan.wipe();
        s.notes.wipe();
        drop(s);
        self.send(ServerMsg::SessionEnded { session_id: self.id.clone() });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.interrupt();
        if let Ok(mut s) = self.shared.lock() {
            s.viewing = false;
            s.frames.wipe();
            s.scan.wipe();
            s.notes.wipe();
        }
    }
}

struct SessionHost {
    shared: Arc<Mutex<Shared>>,
    out: mpsc::UnboundedSender<Outbound>,
    speaker: Option<Arc<Speaker>>,
}

impl ToolHost for SessionHost {
    fn heard_prefix(&self) -> Option<usize> {
        self.speaker.as_ref().and_then(|s| s.heard_prefix())
    }

    fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().unwrap().pending_notes)
    }

    fn view_screen(&self, input: &Value) -> impl std::future::Future<Output = ToolOutput> + Send {
        let reason = input.get("reason").and_then(Value::as_str).unwrap_or("");
        let scope = input.get("scope").and_then(Value::as_str).unwrap_or("current");
        tracing::info!(%reason, %scope, "view_screen");
        let out = if scope == "scan" { self.look_at_scan() } else { self.look_current() };
        async move { out }
    }

    fn record_screen_notes(&self, input: &Value) -> ToolOutput {
        let page = input.get("page").and_then(Value::as_str).unwrap_or("");
        let notes = input.get("notes").and_then(Value::as_str).unwrap_or("");
        let result = self.shared.lock().unwrap().notes.record(page, notes);
        match result {
            Ok(note) => {
                tracing::info!(id = note.id, revision = note.revision, chars = note.notes.len(), "screen note recorded");
                let detail = if note.revision > 1 {
                    format!("updated note: {}", note.page)
                } else {
                    format!("recorded note: {}", note.page)
                };
                let _ = self.out.send(Outbound::Msg(ServerMsg::ScreenNote {
                    id: note.id,
                    page: note.page,
                    notes: note.notes,
                    revision: note.revision,
                }));
                ToolOutput { content: vec![ContentBlock::text("Noted.")], is_error: false, detail }
            }
            Err(e) => {
                tracing::info!(reason = e.reason(), "screen note rejected");
                ToolOutput {
                    content: vec![ContentBlock::text(e.message())],
                    is_error: true,
                    detail: format!("note rejected ({}); the advisor will rewrite it", e.reason()),
                }
            }
        }
    }
}

fn text_only(text: &str, detail: &str) -> ToolOutput {
    ToolOutput { content: vec![ContentBlock::text(text)], is_error: false, detail: detail.into() }
}

fn jpeg_block(jpeg: &[u8]) -> ContentBlock {
    ContentBlock::jpeg_base64(base64::engine::general_purpose::STANDARD.encode(jpeg))
}

impl SessionHost {
    fn look_current(&self) -> ToolOutput {
        let mut s = self.shared.lock().unwrap();
        let scan_hint = (s.scan.has_unviewed() && !s.scan.is_active()).then(|| {
            format!(
                "An unviewed page scan with {} segments is available: call view_screen with scope \"scan\".",
                s.scan.len()
            )
        });
        if !s.live_capture_on() {
            let mut out = text_only(PAUSED_TEXT, "viewing paused");
            if let Some(h) = scan_hint {
                out.content.push(ContentBlock::text(h));
            }
            return out;
        }
        let frames = s.frames.take_for_view();
        if frames.is_empty() {
            return text_only(
                "Viewing is on, but no screen frame has arrived yet. The user may need to share a \
                 window or tab.",
                "no frames yet",
            );
        }
        let now = now_ms();
        let n = frames.len();
        let mut content = Vec::with_capacity(n * 2 + 1);
        for (i, f) in frames.iter().enumerate() {
            let age = now.saturating_sub(f.captured_at_ms) as f64 / 1000.0;
            let which = if i + 1 == n { "latest" } else { "earlier" };
            content.push(ContentBlock::text(format!(
                "Frame {} of {n} ({which}), captured {age:.0} seconds ago, {}x{} px:",
                i + 1,
                f.width,
                f.height
            )));
            content.push(jpeg_block(&f.jpeg));
        }
        if let Some(h) = scan_hint {
            content.push(ContentBlock::text(h));
        }
        ToolOutput { content, is_error: false, detail: format!("viewed screen: {n} frames") }
    }

    fn look_at_scan(&self) -> ToolOutput {
        let mut s = self.shared.lock().unwrap();
        if s.scan.is_empty() {
            let text = if s.scan.is_active() {
                "A page scan is in progress but no segments have been captured yet. Ask the user to \
                 scroll slowly and pause briefly at each screenful."
            } else {
                "No page scan is available. If the page is longer than one screen, ask the user to \
                 scan it: scroll to the top, press Scan page, scroll slowly to the bottom, then come \
                 back to the call."
            };
            return text_only(text, "no page scan");
        }
        let active = s.scan.is_active();
        let scan_id = s.scan.scan_id();
        let segments = s.scan.take_for_view();
        let n = segments.len();
        let mut content = Vec::with_capacity(n * 3 + 1);
        content.push(ContentBlock::text(format!(
            "Page scan {scan_id}: {n} segment{}, top to bottom{}. Positions are estimates in screenshot pixels.",
            if n == 1 { "" } else { "s" },
            if active { " (the user is still scanning)" } else { "" }
        )));
        // Cumulative position below segment 1, while every offset is known.
        let mut below: Option<i64> = Some(0);
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                below = match (below, seg.offset_px, seg.gap_before) {
                    (Some(b), Some(o), false) => Some(b + o as i64),
                    _ => None,
                };
            }
            if seg.gap_before {
                content.push(ContentBlock::text(format!(
                    "Possible gap: part of the page between segments {} and {} may be missing.",
                    i,
                    i + 1
                )));
            }
            let position = match (i, below) {
                (0, _) => "top of the scan".to_string(),
                (_, Some(b)) if b >= 0 => format!("about {b} px below segment 1"),
                (_, Some(b)) => format!("about {} px above segment 1", -b),
                (_, None) => "position unknown".to_string(),
            };
            content.push(ContentBlock::text(format!(
                "Segment {} of {n}, {position}, {}x{} px:",
                i + 1,
                seg.frame.width,
                seg.frame.height
            )));
            content.push(jpeg_block(&seg.frame.jpeg));
        }
        ToolOutput { content, is_error: false, detail: format!("viewed page scan: {n} segments") }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_notes() {
        assert_eq!(
            scan_finished_note(7, &[5]),
            "The user scanned a page: 7 segments, with a possible gap between segments 4 and 5. \
             Call view_screen with scope \"scan\" to see the whole page, top to bottom."
        );
        assert!(scan_finished_note(0, &[]).contains("no segments"));
    }
}
