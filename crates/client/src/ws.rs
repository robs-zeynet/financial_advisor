//! WebSocket wrapper over `web_sys::WebSocket`.

use protocol::{ClientMsg, ServerMsg};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};

use crate::dom;

#[derive(Clone)]
pub struct Ws {
    socket: WebSocket,
}

impl Ws {
    /// Connects to `/ws` on the page's own host.
    pub fn connect(
        on_msg: impl FnMut(ServerMsg) + 'static,
        on_binary: impl FnMut(Vec<u8>) + 'static,
        on_open: impl FnMut() + 'static,
        on_close: impl FnMut() + 'static,
    ) -> Result<Self, JsValue> {
        let loc = dom::window().location();
        let url = format!("ws://{}/ws", loc.host()?);
        let socket = WebSocket::new(&url)?;
        socket.set_binary_type(BinaryType::Arraybuffer);

        let mut on_msg = on_msg;
        let mut on_binary = on_binary;
        dom::on(&socket, "message", move |e: MessageEvent| {
            let data = e.data();
            if let Some(buf) = data.dyn_ref::<js_sys::ArrayBuffer>() {
                on_binary(js_sys::Uint8Array::new(buf).to_vec());
                return;
            }
            let Some(text) = data.as_string() else { return };
            match serde_json::from_str::<ServerMsg>(&text) {
                Ok(msg) => on_msg(msg),
                Err(err) => dom::log(&format!("bad server message: {err}")),
            }
        });
        let mut on_open = on_open;
        dom::on(&socket, "open", move |_: web_sys::Event| on_open());
        let mut on_close = on_close;
        dom::on(&socket, "close", move |_: web_sys::Event| on_close());
        Ok(Self { socket })
    }

    pub fn is_open(&self) -> bool {
        self.socket.ready_state() == WebSocket::OPEN
    }

    pub fn send(&self, msg: &ClientMsg) {
        if !self.is_open() {
            return;
        }
        let text = serde_json::to_string(msg).expect("ClientMsg serializes");
        if let Err(e) = self.socket.send_with_str(&text) {
            dom::log(&format!("ws send failed: {e:?}"));
        }
    }

    pub fn send_bytes(&self, bytes: &[u8]) {
        if !self.is_open() {
            return;
        }
        if let Err(e) = self.socket.send_with_u8_array(bytes) {
            dom::log(&format!("ws send failed: {e:?}"));
        }
    }
}
