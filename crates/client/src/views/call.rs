//! The call screen: start/end, viewing toggle and page scans with the big
//! state indicator, the live transcript, the screen notes pane, and typed
//! input.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use protocol::{
    is_pause_command, is_scan_command, is_scan_done_command, ClientMsg, ListenState, ScanCue, ServerMsg,
    SpokenCommand, VoiceCaps, SCAN_HOTKEY_KEY, VIEWING_HOTKEY_KEY,
};
use wasm_bindgen::JsCast;
use web_sys::{Element, HtmlTextAreaElement, KeyboardEvent};

use crate::audio::{Mic, Player};
use crate::capture::{CaptureEvent, Capturer, Mode};
use crate::dom::{self, append, el, el_text, on, set_class, set_disabled, set_text};
use crate::scan::ScanSummary;
use crate::ws::Ws;

/// Below this sampling rate a scan was probably throttled in a background tab.
const SLOW_SCAN_SAMPLES_PER_SEC: f64 = 2.0;

pub struct CallView {
    ws: RefCell<Option<Ws>>,
    capture: RefCell<Option<Rc<Capturer>>>,
    in_call: Cell<bool>,
    next_scan_id: Cell<u32>,
    /// Segments captured in the scan in progress.
    scan_segments: Cell<u32>,
    /// The app lost focus or visibility during the scan; regaining it ends the scan.
    scan_left_app: Cell<bool>,
    voice: Cell<VoiceCaps>,
    mic: RefCell<Option<Rc<Mic>>>,
    player: RefCell<Option<Rc<Player>>>,
    listen: Cell<ListenState>,
    ptt_held: Cell<bool>,
    mic_btn: Element,
    meter_fill: Element,
    voice_status: Element,
    indicator: Element,
    start_btn: Element,
    end_btn: Element,
    share_btn: Element,
    view_btn: Element,
    scan_btn: Element,
    transcript: Element,
    notes_list: Element,
    notes_count: Element,
    /// Note elements by note id, so updates replace in place.
    note_els: RefCell<HashMap<u32, Element>>,
    input: HtmlTextAreaElement,
    send_btn: Element,
    status: Element,
    /// The assistant message currently streaming in.
    current: RefCell<Option<Element>>,
}

impl CallView {
    pub fn mount(root: &Element) -> Rc<Self> {
        let header = el("header", "call-header");
        let title = el_text("h1", "", "Financial Advisor");
        let indicator = el("div", "indicator");
        append(&header, &title);
        append(&header, &indicator);

        let controls = el("div", "controls");
        let start_btn = el_text("button", "primary", "Start call");
        let end_btn = el_text("button", "danger", "End call");
        let share_btn = el_text("button", "", "Share a window or tab…");
        let view_btn = el_text("button", "", "Resume viewing");
        let scan_btn = el_text("button", "", "Scan page");
        let mic_btn = el_text("button", "", "Mic on");
        for b in [&start_btn, &end_btn, &share_btn, &view_btn, &scan_btn, &mic_btn] {
            append(&controls, b);
        }
        let voice_row = el("div", "voice-row");
        let meter = el("div", "meter");
        let meter_fill = el("div", "meter-fill");
        append(&meter, &meter_fill);
        let voice_status = el("span", "voice-status");
        append(&voice_row, &meter);
        append(&voice_row, &voice_status);
        let hint = el("div", "hint");
        append(
            &hint,
            &el_text(
                "p",
                "",
                "Share a single tab or window, not your whole screen, so notifications and other apps \
                 stay out of view. Log in to your accounts while viewing is paused, then resume. \
                 Ctrl+Shift+V toggles viewing; typing \u{201c}pause viewing\u{201d} pauses at once.",
            ),
        );
        append(
            &hint,
            &el_text(
                "p",
                "",
                "Long page? Click Scan page (or Ctrl+Shift+S, or say or type \u{201c}scan this page\u{201d}), \
                 then follow the advisor's spoken directions in the shared page: it tells you when to \
                 scroll, when to stop, and when to go back. Say \u{201c}done\u{201d} at the bottom. For \
                 smoother scans, keep this app in its own window.",
            ),
        );
        append(
            &hint,
            &el_text(
                "p",
                "",
                "Just talk; the advisor answers out loud. Talking over it stops it. Hold Space to talk \
                 push-to-talk style; audio is never stored.",
            ),
        );

        let panes = el("div", "panes");
        let transcript = el("div", "transcript");
        let notes_pane = el("aside", "notes");
        let notes_head = el("h2", "");
        append(&notes_head, &el_text("span", "", "Screen notes "));
        let notes_count = el_text("span", "count", "");
        append(&notes_head, &notes_count);
        let notes_list = el("div", "notes-list");
        append(&notes_pane, &notes_head);
        append(&notes_pane, &notes_list);
        append(&panes, &transcript);
        append(&panes, &notes_pane);
        let input_row = el("div", "input-row");
        let input: HtmlTextAreaElement = el("textarea", "").unchecked_into();
        input.set_placeholder("Ask the advisor… (Enter to send, Shift+Enter for a new line)");
        input.set_rows(2);
        let send_btn = el_text("button", "primary", "Send");
        append(&input_row, &input);
        append(&input_row, &send_btn);
        let status = el("div", "status");

        for e in [&header, &controls, &voice_row, &hint, &panes, &input_row, &status] {
            append(root, e);
        }

        let view = Rc::new(Self {
            ws: RefCell::new(None),
            capture: RefCell::new(None),
            in_call: Cell::new(false),
            next_scan_id: Cell::new(1),
            scan_segments: Cell::new(0),
            scan_left_app: Cell::new(false),
            voice: Cell::new(VoiceCaps::default()),
            mic: RefCell::new(None),
            player: RefCell::new(None),
            listen: Cell::new(ListenState::Idle),
            ptt_held: Cell::new(false),
            mic_btn,
            meter_fill,
            voice_status,
            indicator,
            start_btn,
            end_btn,
            share_btn,
            view_btn,
            scan_btn,
            transcript,
            notes_list,
            notes_count,
            note_els: RefCell::new(HashMap::new()),
            input,
            send_btn,
            status,
            current: RefCell::new(None),
        });
        view.clear_notes();
        view.connect();
        view.wire();
        view.render();
        view
    }

    fn connect(self: &Rc<Self>) {
        let on_msg = {
            let me = self.clone();
            move |msg| me.on_server(msg)
        };
        let on_binary = {
            let me = self.clone();
            move |bytes: Vec<u8>| {
                if let Some(p) = me.player.borrow().as_ref() {
                    p.play(&bytes);
                }
            }
        };
        let on_open = {
            let me = self.clone();
            move || me.set_status("Connected.", false)
        };
        let on_close = {
            let me = self.clone();
            move || {
                if let Some(c) = me.capture.borrow().as_ref() {
                    c.stop_sharing();
                }
                me.in_call.set(false);
                me.render();
                me.set_status("Disconnected from the local server. Reload the page to reconnect.", true);
            }
        };
        match Ws::connect(on_msg, on_binary, on_open, on_close) {
            Ok(ws) => {
                let capture = Capturer::new(ws.clone());
                let me = self.clone();
                capture.set_on_event(move |e| me.on_capture(e));
                *self.capture.borrow_mut() = Some(capture);
                *self.ws.borrow_mut() = Some(ws);
            }
            Err(e) => self.set_status(&format!("Could not connect: {e:?}"), true),
        }
    }

    fn wire(self: &Rc<Self>) {
        let me = self.clone();
        on(&self.start_btn, "click", move |_: web_sys::Event| {
            // Audio must start from a click for the browser to allow it.
            me.start_audio();
            me.ws_send(&ClientMsg::StartSession);
        });

        let me = self.clone();
        on(&self.mic_btn, "click", move |_: web_sys::Event| me.toggle_mic());

        // Push-to-talk: hold Space (outside text fields).
        let me = self.clone();
        on(&dom::document(), "keydown", move |e: KeyboardEvent| {
            if e.code() == "Space" && !typing_target(&e) && me.in_call.get() && me.voice.get().stt {
                e.prevent_default();
                if !e.repeat() && !me.ptt_held.get() {
                    me.ptt_held.set(true);
                    me.ws_send(&ClientMsg::PttDown);
                    me.render();
                }
            }
        });
        let me = self.clone();
        on(&dom::document(), "keyup", move |e: KeyboardEvent| {
            if e.code() == "Space" && me.ptt_held.get() {
                e.prevent_default();
                me.ptt_held.set(false);
                me.ws_send(&ClientMsg::PttUp);
                me.render();
            }
        });

        // Keep the "Speaking" status current while audio plays.
        let me = self.clone();
        gloo_timers::callback::Interval::new(250, move || me.render_voice()).forget();

        let me = self.clone();
        on(&self.end_btn, "click", move |_: web_sys::Event| me.hang_up());

        let me = self.clone();
        on(&self.share_btn, "click", move |_: web_sys::Event| {
            let me = me.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let Some(capture) = me.capture.borrow().clone() else { return };
                match capture.share().await {
                    Ok(()) => me.set_status("Sharing. Viewing is still paused until you resume or scan.", false),
                    Err(e) => me.set_status(&format!("Screen share cancelled or failed: {e:?}"), true),
                }
                me.render();
            });
        });

        let me = self.clone();
        on(&self.view_btn, "click", move |_: web_sys::Event| me.toggle_viewing());

        let me = self.clone();
        on(&self.scan_btn, "click", move |_: web_sys::Event| me.toggle_scan());

        let me = self.clone();
        on(&dom::document(), "keydown", move |e: KeyboardEvent| {
            if !(e.ctrl_key() && e.shift_key()) {
                return;
            }
            let key = e.key();
            if key.eq_ignore_ascii_case(VIEWING_HOTKEY_KEY) {
                e.prevent_default();
                me.toggle_viewing();
            } else if key.eq_ignore_ascii_case(SCAN_HOTKEY_KEY) {
                e.prevent_default();
                me.toggle_scan();
            }
        });

        // Leaving the app and coming back ends a scan.
        let me = self.clone();
        on(&dom::window(), "blur", move |_: web_sys::Event| me.note_left_app());
        let me = self.clone();
        on(&dom::window(), "focus", move |_: web_sys::Event| me.note_returned());
        let me = self.clone();
        on(&dom::document(), "visibilitychange", move |_: web_sys::Event| {
            if dom::document().hidden() {
                me.note_left_app();
            } else {
                me.note_returned();
            }
        });

        let me = self.clone();
        on(&self.send_btn, "click", move |_: web_sys::Event| me.send_text());

        let me = self.clone();
        on(&self.input, "keydown", move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.shift_key() {
                e.prevent_default();
                me.send_text();
            }
        });
    }

    fn ws_send(&self, msg: &ClientMsg) {
        if let Some(ws) = self.ws.borrow().as_ref() {
            ws.send(msg);
        }
    }

    fn capture(&self) -> Option<Rc<Capturer>> {
        self.capture.borrow().clone()
    }

    fn mode(&self) -> Mode {
        self.capture().map_or(Mode::Paused, |c| c.mode())
    }

    fn viewing(&self) -> bool {
        self.mode() != Mode::Paused
    }

    fn scanning(&self) -> bool {
        matches!(self.mode(), Mode::Scanning { .. })
    }

    /// Explicit pause: stops capture (cancelling any scan) before telling the server.
    fn pause_viewing(&self) {
        if let Some(c) = self.capture() {
            if c.pause().is_some() {
                self.set_status("Scan cancelled.", false);
            }
        }
        self.ws_send(&ClientMsg::SetViewing { viewing: false });
        self.render();
    }

    fn toggle_viewing(&self) {
        if !self.in_call.get() {
            return;
        }
        let Some(c) = self.capture() else { return };
        if c.is_viewing() {
            self.pause_viewing();
        } else if !c.has_stream() {
            self.set_status("Share a window or tab first.", true);
        } else {
            c.resume();
            self.ws_send(&ClientMsg::SetViewing { viewing: true });
            self.render();
        }
    }

    fn toggle_scan(&self) {
        if self.scanning() {
            self.finish_scan("");
        } else {
            self.start_scan();
        }
    }

    fn start_scan(&self) {
        if !self.in_call.get() {
            return;
        }
        let Some(c) = self.capture() else { return };
        if !c.has_stream() {
            self.set_status("Share a window or tab first, then scan.", true);
            return;
        }
        let scan_id = self.next_scan_id.get();
        self.next_scan_id.set(scan_id + 1);
        self.scan_segments.set(0);
        self.scan_left_app.set(false);
        c.start_scan(scan_id); // capture starts locally at once...
        self.ws_send(&ClientMsg::StartScan { scan_id }); // ...and the server is told
        self.set_status(
            "Scanning. Switch to the shared page and follow the advisor's spoken directions; come back \
             here, say \u{201c}done,\u{201d} or click Done to finish.",
            false,
        );
        self.render();
    }

    /// Speaks scan guidance (in the advisor's voice) and shows it here too.
    fn say_cue(&self, cue: ScanCue) {
        self.ws_send(&ClientMsg::ScanCue { cue });
        let warn = matches!(cue, ScanCue::TooFar | ScanCue::TooFast);
        self.set_status(cue.phrase(), warn);
    }

    /// Ends the scan normally, thanks the user, and returns to the pre-scan
    /// viewing state.
    fn finish_scan(&self, why: &str) {
        if self.scanning() {
            self.say_cue(ScanCue::Finished);
        }
        self.finish_scan_quietly(why);
    }

    fn finish_scan_quietly(&self, why: &str) {
        let Some(c) = self.capture() else { return };
        let Some(summary) = c.end_scan() else { return };
        self.ws_send(&ClientMsg::EndScan { scan_id: summary.scan_id });
        self.set_status(&scan_report(&summary, why), summary.gaps > 0 || summary.segments == 0);
        self.render();
    }

    fn note_left_app(&self) {
        if self.scanning() {
            self.scan_left_app.set(true);
        }
    }

    fn note_returned(&self) {
        if self.scanning() && self.scan_left_app.get() {
            self.finish_scan("you came back to the app");
        }
    }

    fn on_capture(&self, e: CaptureEvent) {
        match e {
            CaptureEvent::ShareEnded => {
                self.ws_send(&ClientMsg::SetViewing { viewing: false });
                self.set_status("Screen sharing stopped.", false);
            }
            CaptureEvent::Segment { segments, .. } => self.scan_segments.set(segments),
            CaptureEvent::ScanFull => {
                self.say_cue(ScanCue::Full);
                self.finish_scan_quietly("it reached the segment limit");
            }
            CaptureEvent::Cue(cue) => self.say_cue(cue),
        }
        self.render();
    }

    /// Creates the player and starts the mic (from the Start call click).
    fn start_audio(self: &Rc<Self>) {
        if self.player.borrow().is_none() {
            match Player::new() {
                Ok(p) => *self.player.borrow_mut() = Some(p),
                Err(e) => dom::log(&format!("audio playback unavailable: {e:?}")),
            }
        }
        if self.mic.borrow().is_some() {
            return;
        }
        let Some(ws) = self.ws.borrow().clone() else { return };
        let me = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let level = {
                let fill = me.meter_fill.clone();
                move |l: f32| {
                    let _ = fill.set_attribute("style", &format!("width: {:.0}%", l * 100.0));
                }
            };
            match Mic::start(ws, level).await {
                Ok(mic) => {
                    if me.in_call.get() && !me.voice.get().stt {
                        mic.stop(); // the server can't transcribe; don't hold the mic
                    } else {
                        *me.mic.borrow_mut() = Some(mic);
                    }
                }
                Err(e) => me.set_status(&format!("Microphone unavailable ({e:?}); typing still works."), true),
            }
            me.render();
        });
    }

    fn toggle_mic(&self) {
        let Some(mic) = self.mic.borrow().clone() else { return };
        let on = !mic.is_on();
        mic.set_on(on);
        if !on {
            let _ = self.meter_fill.set_attribute("style", "width: 0%");
        }
        self.ws_send(&ClientMsg::MicState { on });
        self.render();
    }

    fn stop_audio(&self) {
        if let Some(mic) = self.mic.borrow_mut().take() {
            mic.stop();
        }
        if let Some(p) = self.player.borrow().as_ref() {
            p.stop_all();
        }
        let _ = self.meter_fill.set_attribute("style", "width: 0%");
    }

    fn hang_up(&self) {
        if let Some(c) = self.capture() {
            c.stop_sharing();
        }
        self.stop_audio();
        self.ws_send(&ClientMsg::EndSession);
        self.in_call.set(false);
        self.render();
    }

    fn send_text(&self) {
        let text = self.input.value().trim().to_string();
        if text.is_empty() || !self.in_call.get() {
            return;
        }
        // Scan commands are handled here and never sent to Claude.
        if self.scanning() && is_scan_done_command(&text) {
            self.input.set_value("");
            self.finish_scan("");
            return;
        }
        if is_scan_command(&text) {
            self.input.set_value("");
            self.start_scan();
            return;
        }
        if is_pause_command(&text) && self.viewing() {
            self.pause_viewing();
        }
        self.input.set_value("");
        self.ws_send(&ClientMsg::UserText { text });
    }

    fn on_server(&self, msg: ServerMsg) {
        match msg {
            ServerMsg::SessionStarted { capture, viewing, voice, .. } => {
                self.voice.set(voice);
                if !voice.stt {
                    if let Some(mic) = self.mic.borrow_mut().take() {
                        mic.stop();
                    }
                }
                if let Some(c) = self.capture() {
                    c.state.borrow_mut().set_settings(capture);
                    if !viewing {
                        c.pause();
                    }
                }
                self.in_call.set(true);
                self.transcript.set_inner_html("");
                self.clear_notes();
                self.set_status("Call started. Viewing is paused.", false);
            }
            ServerMsg::SessionEnded { .. } => {
                self.in_call.set(false);
                self.set_status("Call ended.", false);
            }
            ServerMsg::ViewingState { viewing } => {
                if !viewing && self.viewing() {
                    // Server-requested pause: stop at once and acknowledge.
                    self.pause_viewing();
                }
            }
            ServerMsg::ScanState { active, scan_id, .. } => {
                // The server ended our scan (e.g. at its segment cap).
                if !active && matches!(self.mode(), Mode::Scanning { scan_id: id, .. } if id == scan_id) {
                    self.finish_scan("the server ended it");
                }
            }
            ServerMsg::UserTurn { text } => {
                self.close_current();
                self.add_line("you", "You", &text);
            }
            ServerMsg::AssistantDelta { text } => {
                let mut cur = self.current.borrow_mut();
                let body = cur.get_or_insert_with(|| self.add_line("advisor", "Advisor", ""));
                let prev = body.text_content().unwrap_or_default();
                set_text(body, &(prev + &text));
                self.scroll_to_end();
            }
            ServerMsg::AssistantDone { stop_reason } => {
                if let Some(cur) = self.current.borrow().as_ref() {
                    set_class(cur, "refusal", stop_reason == "refusal");
                    set_class(cur, "interrupted", stop_reason == "cancelled");
                }
                self.close_current();
            }
            ServerMsg::ScreenNote { id, page, notes, revision } => self.show_note(id, &page, &notes, revision),
            ServerMsg::Listen { state } => self.listen.set(state),
            ServerMsg::StopAudio { utterance_id } => {
                if let Some(p) = self.player.borrow().as_ref() {
                    p.stop(utterance_id);
                }
            }
            ServerMsg::CommandHeard { text, command } => {
                let what = match command {
                    SpokenCommand::StartScan => "start a page scan",
                    SpokenCommand::EndScan => "finish the page scan",
                    SpokenCommand::PauseViewing => "pause viewing",
                };
                let line = el_text("div", "tool", &format!("[heard \u{201c}{text}\u{201d}: {what}]"));
                append(&self.transcript, &line);
                self.scroll_to_end();
                match command {
                    SpokenCommand::StartScan => self.start_scan(),
                    SpokenCommand::EndScan => self.finish_scan("you said done"),
                    SpokenCommand::PauseViewing => self.pause_viewing(),
                }
            }
            ServerMsg::ToolActivity { tool, detail } => {
                let line = el_text("div", "tool", &format!("[{tool}: {detail}]"));
                append(&self.transcript, &line);
                self.scroll_to_end();
            }
            ServerMsg::ProtocolViolation { detail, total } => {
                self.set_status(&format!("Protocol violation #{total}: {detail}"), true);
            }
            ServerMsg::Error { message } => self.set_status(&message, true),
        }
        self.render();
    }

    /// Empties the notes pane (new call). Notes last only for one call.
    fn clear_notes(&self) {
        self.note_els.borrow_mut().clear();
        self.notes_list.set_inner_html("");
        append(
            &self.notes_list,
            &el_text(
                "p",
                "notes-empty",
                "What the advisor writes down from your screen appears here. Notes last only for this call.",
            ),
        );
        set_text(&self.notes_count, "");
    }

    /// Adds a note, or replaces the note with the same id.
    fn show_note(&self, id: u32, page: &str, notes: &str, revision: u32) {
        let card = el("div", "note");
        let title = el("h3", "");
        append(&title, &el_text("span", "", page));
        if revision > 1 {
            append(&title, &el_text("span", "badge", &format!("updated ×{}", revision - 1)));
        }
        append(&card, &title);
        append(&card, &el_text("div", "note-body", notes));
        let mut els = self.note_els.borrow_mut();
        if els.is_empty() {
            self.notes_list.set_inner_html("");
        }
        match els.get(&id) {
            Some(old) => {
                let _ = old.replace_with_with_node_1(&card);
            }
            None => append(&self.notes_list, &card),
        }
        els.insert(id, card);
        set_text(&self.notes_count, &format!("({})", els.len()));
    }

    fn close_current(&self) {
        self.current.borrow_mut().take();
    }

    /// Appends a transcript line and returns its body element.
    fn add_line(&self, class: &str, who: &str, text: &str) -> Element {
        let line = el("div", &format!("line {class}"));
        append(&line, &el_text("span", "who", who));
        let body = el_text("span", "body", text);
        append(&line, &body);
        append(&self.transcript, &line);
        self.scroll_to_end();
        body
    }

    fn scroll_to_end(&self) {
        self.transcript.set_scroll_top(self.transcript.scroll_height());
    }

    fn set_status(&self, text: &str, error: bool) {
        set_text(&self.status, text);
        set_class(&self.status, "error", error);
    }

    fn render(&self) {
        let in_call = self.in_call.get();
        let mode = self.mode();
        let viewing = mode != Mode::Paused;
        let scanning = matches!(mode, Mode::Scanning { .. });
        let sharing = self.capture().is_some_and(|c| c.has_stream());
        let label = match mode {
            Mode::Paused => "⚪ PAUSED".to_string(),
            Mode::Viewing => "🔴 VIEWING".to_string(),
            Mode::Scanning { .. } => format!("🔴 SCANNING ({})", self.scan_segments.get()),
        };
        set_text(&self.indicator, &label);
        set_class(&self.indicator, "viewing", viewing);
        set_text(&self.view_btn, if viewing { "Pause viewing" } else { "Resume viewing" });
        set_text(
            &self.scan_btn,
            &if scanning { format!("Done ({} segments)", self.scan_segments.get()) } else { "Scan page".into() },
        );
        set_class(&self.scan_btn, "primary", scanning);
        set_disabled(&self.start_btn, in_call);
        set_disabled(&self.end_btn, !in_call);
        set_disabled(&self.share_btn, !in_call);
        set_disabled(&self.view_btn, !in_call || (!viewing && !sharing));
        set_disabled(&self.scan_btn, !in_call || !sharing);
        set_disabled(&self.send_btn, !in_call);
        set_disabled(&self.input, !in_call);
        let mic_on = self.mic.borrow().as_ref().map(|m| m.is_on());
        set_text(&self.mic_btn, if mic_on == Some(false) { "Mic off" } else { "Mic on" });
        set_class(&self.mic_btn, "muted", mic_on == Some(false));
        set_disabled(&self.mic_btn, !in_call || mic_on.is_none());
        self.render_voice();
    }

    /// The voice status line: what the voice pipeline is doing right now.
    fn render_voice(&self) {
        let speaking = self.player.borrow().as_ref().is_some_and(|p| p.is_playing());
        let mic_on = self.mic.borrow().as_ref().map(|m| m.is_on());
        let text = if !self.in_call.get() {
            ""
        } else if self.ptt_held.get() {
            "Push-to-talk: listening…"
        } else if self.listen.get() == ListenState::Hearing {
            "Hearing you…"
        } else if self.listen.get() == ListenState::Transcribing {
            "Transcribing…"
        } else if speaking {
            "Speaking (talk to interrupt)"
        } else if !self.voice.get().stt {
            "Voice unavailable: type instead"
        } else if mic_on == Some(false) {
            "Mic off"
        } else if mic_on == Some(true) {
            "Listening"
        } else {
            ""
        };
        set_text(&self.voice_status, text);
    }
}

fn scan_report(s: &ScanSummary, why: &str) -> String {
    let mut msg = format!("Scan finished: {} segment{}", s.segments, if s.segments == 1 { "" } else { "s" });
    if !why.is_empty() {
        msg.push_str(&format!(" ({why})"));
    }
    msg.push('.');
    if s.segments == 0 {
        msg.push_str(" Nothing was captured; make sure the shared page is visible and scroll a little slower.");
    } else if s.gaps > 0 {
        msg.push_str(&format!(" {} possible gap{}; the advisor will be told.", s.gaps, if s.gaps == 1 { "" } else { "s" }));
    }
    if s.segments > 0 && s.samples_per_sec < SLOW_SCAN_SAMPLES_PER_SEC {
        msg.push_str(
            " Tip: the browser slowed sampling because this tab was in the background. \
             Keeping the app in its own window gives smoother scans.",
        );
    }
    msg
}

/// Whether a key event is aimed at a text field (so Space types a space).
fn typing_target(e: &KeyboardEvent) -> bool {
    e.target()
        .and_then(|t| t.dyn_into::<Element>().ok())
        .is_some_and(|el| matches!(el.tag_name().as_str(), "TEXTAREA" | "INPUT" | "BUTTON"))
}
