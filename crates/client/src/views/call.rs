//! The call screen: start/end, viewing toggle with the big state indicator,
//! the live transcript, and typed input (M0).

use std::cell::RefCell;
use std::rc::Rc;

use protocol::{is_pause_command, ClientMsg, ServerMsg, VIEWING_HOTKEY_KEY};
use wasm_bindgen::JsCast;
use web_sys::{Element, HtmlTextAreaElement, KeyboardEvent};

use crate::capture::Capturer;
use crate::dom::{self, append, el, el_text, on, set_class, set_disabled, set_text};
use crate::ws::Ws;

pub struct CallView {
    ws: RefCell<Option<Ws>>,
    capture: RefCell<Option<Rc<Capturer>>>,
    in_call: RefCell<bool>,
    indicator: Element,
    start_btn: Element,
    end_btn: Element,
    share_btn: Element,
    view_btn: Element,
    transcript: Element,
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
        for b in [&start_btn, &end_btn, &share_btn, &view_btn] {
            append(&controls, b);
        }
        let hint = el_text(
            "p",
            "hint",
            "Share a single tab or window, not your whole screen, so notifications and other apps \
             stay out of view. Log in to your accounts while viewing is paused, then resume. \
             Ctrl+Shift+V toggles viewing; typing \u{201c}pause viewing\u{201d} pauses at once.",
        );

        let transcript = el("div", "transcript");
        let input_row = el("div", "input-row");
        let input: HtmlTextAreaElement = el("textarea", "").unchecked_into();
        input.set_placeholder("Ask the advisor… (Enter to send, Shift+Enter for a new line)");
        input.set_rows(2);
        let send_btn = el_text("button", "primary", "Send");
        append(&input_row, &input);
        append(&input_row, &send_btn);
        let status = el("div", "status");

        for e in [&header, &controls, &hint, &transcript, &input_row, &status] {
            append(root, e);
        }

        let view = Rc::new(Self {
            ws: RefCell::new(None),
            capture: RefCell::new(None),
            in_call: RefCell::new(false),
            indicator,
            start_btn,
            end_btn,
            share_btn,
            view_btn,
            transcript,
            input,
            send_btn,
            status,
            current: RefCell::new(None),
        });
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
                *me.in_call.borrow_mut() = false;
                me.render();
                me.set_status("Disconnected from the local server. Reload the page to reconnect.", true);
            }
        };
        match Ws::connect(on_msg, on_open, on_close) {
            Ok(ws) => {
                let capture = Capturer::new(ws.clone());
                let me = self.clone();
                capture.set_on_share_ended(move || {
                    me.ws_send(&ClientMsg::SetViewing { viewing: false });
                    me.render();
                    me.set_status("Screen sharing stopped.", false);
                });
                *self.capture.borrow_mut() = Some(capture);
                *self.ws.borrow_mut() = Some(ws);
            }
            Err(e) => self.set_status(&format!("Could not connect: {e:?}"), true),
        }
    }

    fn wire(self: &Rc<Self>) {
        let me = self.clone();
        on(&self.start_btn, "click", move |_: web_sys::Event| me.ws_send(&ClientMsg::StartSession));

        let me = self.clone();
        on(&self.end_btn, "click", move |_: web_sys::Event| me.hang_up());

        let me = self.clone();
        on(&self.share_btn, "click", move |_: web_sys::Event| {
            let me = me.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let Some(capture) = me.capture.borrow().clone() else { return };
                match capture.share().await {
                    Ok(()) => me.set_status("Sharing. Viewing is still paused until you resume.", false),
                    Err(e) => me.set_status(&format!("Screen share cancelled or failed: {e:?}"), true),
                }
                me.render();
            });
        });

        let me = self.clone();
        on(&self.view_btn, "click", move |_: web_sys::Event| me.toggle_viewing());

        let me = self.clone();
        on(&dom::document(), "keydown", move |e: KeyboardEvent| {
            if e.ctrl_key() && e.shift_key() && e.key().eq_ignore_ascii_case(VIEWING_HOTKEY_KEY) {
                e.prevent_default();
                me.toggle_viewing();
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

    fn viewing(&self) -> bool {
        self.capture().is_some_and(|c| c.is_viewing())
    }

    fn pause_viewing(&self) {
        if let Some(c) = self.capture() {
            c.pause(); // stop capturing first, then tell the server
        }
        self.ws_send(&ClientMsg::SetViewing { viewing: false });
        self.render();
    }

    fn toggle_viewing(&self) {
        if !*self.in_call.borrow() {
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

    fn hang_up(&self) {
        if let Some(c) = self.capture() {
            c.stop_sharing();
        }
        self.ws_send(&ClientMsg::EndSession);
        *self.in_call.borrow_mut() = false;
        self.render();
    }

    fn send_text(&self) {
        let text = self.input.value().trim().to_string();
        if text.is_empty() || !*self.in_call.borrow() {
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
            ServerMsg::SessionStarted { capture, viewing, .. } => {
                if let Some(c) = self.capture() {
                    c.state.borrow_mut().set_settings(capture);
                    if !viewing {
                        c.pause();
                    }
                }
                *self.in_call.borrow_mut() = true;
                self.transcript.set_inner_html("");
                self.set_status("Call started. Viewing is paused.", false);
            }
            ServerMsg::SessionEnded { .. } => {
                *self.in_call.borrow_mut() = false;
                self.set_status("Call ended.", false);
            }
            ServerMsg::ViewingState { viewing } => {
                if !viewing && self.viewing() {
                    // Server-requested pause: stop at once and acknowledge.
                    self.pause_viewing();
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
        let in_call = *self.in_call.borrow();
        let viewing = self.viewing();
        let sharing = self.capture().is_some_and(|c| c.has_stream());
        set_text(&self.indicator, if viewing { "🔴 VIEWING" } else { "⚪ PAUSED" });
        set_class(&self.indicator, "viewing", viewing);
        set_text(&self.view_btn, if viewing { "Pause viewing" } else { "Resume viewing" });
        set_disabled(&self.start_btn, in_call);
        set_disabled(&self.end_btn, !in_call);
        set_disabled(&self.share_btn, !in_call);
        set_disabled(&self.view_btn, !in_call || (!viewing && !sharing));
        set_disabled(&self.send_btn, !in_call);
        set_disabled(&self.input, !in_call);
    }
}
