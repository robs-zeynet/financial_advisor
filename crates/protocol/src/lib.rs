//! Wire protocol shared by the server and the WASM client.
//!
//! Control and events travel as JSON text WebSocket messages (`ClientMsg` /
//! `ServerMsg`). Media travels as binary WebSocket messages whose first byte is
//! a tag (`TAG_*`).

use serde::{Deserialize, Serialize};

/// PCM16 mic audio at 16 kHz (client -> server).
pub const TAG_MIC_PCM: u8 = 0x01;
/// JPEG screen frame (client -> server). See [`FrameHeader`].
pub const TAG_FRAME: u8 = 0x02;
/// TTS PCM audio (server -> client).
pub const TAG_TTS_PCM: u8 = 0x03;

/// Hotkey that toggles viewing: Ctrl+Shift+V.
pub const VIEWING_HOTKEY_KEY: &str = "V";

/// Spoken or typed phrases that pause viewing immediately. The client checks
/// typed text before sending it; the server checks every user turn.
pub fn is_pause_command(text: &str) -> bool {
    let t = text.to_lowercase();
    ["pause viewing", "stop viewing", "pause the viewing", "pause screen", "stop looking"]
        .iter()
        .any(|p| t.contains(p))
}

/// Messages the browser sends to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Begin a call. The session always starts with viewing PAUSED.
    StartSession,
    /// Hang up.
    EndSession,
    /// The user toggled viewing (button or hotkey). `viewing: false` is also the
    /// client's acknowledgement of a server-requested pause.
    SetViewing { viewing: bool },
    /// Typed user input (M0); voice turns arrive as audio in M1.
    UserText { text: String },
}

/// Messages the server sends to the browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    SessionStarted {
        session_id: String,
        viewing: bool,
        capture: CaptureSettings,
    },
    SessionEnded {
        session_id: String,
    },
    /// Authoritative viewing state. When the server sends `viewing: false`
    /// (e.g. the user said "pause viewing"), the client must stop capturing
    /// immediately and reply with `SetViewing { viewing: false }`.
    ViewingState {
        viewing: bool,
    },
    /// Echo of what the server recorded as the user's turn.
    UserTurn {
        text: String,
    },
    /// Streaming assistant text.
    AssistantDelta {
        text: String,
    },
    /// The assistant turn finished.
    AssistantDone {
        stop_reason: String,
    },
    /// The advisor is looking at the screen.
    ToolActivity {
        tool: String,
        detail: String,
    },
    /// A frame arrived while the session was paused; it was dropped.
    ProtocolViolation {
        detail: String,
        total: u64,
    },
    Error {
        message: String,
    },
}

/// Capture parameters the server hands the client at session start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CaptureSettings {
    /// Milliseconds between samples while VIEWING.
    pub sample_interval_ms: u32,
    /// Downscale so the long edge is at most this many pixels.
    pub max_long_edge: u32,
    /// JPEG quality, 0.0..=1.0.
    pub jpeg_quality: f64,
    /// Send an unchanged frame anyway after this many milliseconds.
    pub keepalive_ms: u32,
    /// dHash bits that must differ for a frame to count as changed.
    pub change_threshold_bits: u32,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            sample_interval_ms: 1000,
            max_long_edge: 2000,
            jpeg_quality: 0.8,
            keepalive_ms: 10_000,
            change_threshold_bits: 8,
        }
    }
}

/// Header of a `TAG_FRAME` binary message:
/// `tag u8 | flags u8 | captured_at_ms u64 BE | width u16 BE | height u16 BE | jpeg...`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Wall-clock capture time in milliseconds since the Unix epoch.
    pub captured_at_ms: u64,
    pub width: u16,
    pub height: u16,
    /// True when the frame differs meaningfully from the last one sent;
    /// false for a keepalive.
    pub changed: bool,
}

pub const FRAME_HEADER_LEN: usize = 1 + 1 + 8 + 2 + 2;
const FLAG_CHANGED: u8 = 0b0000_0001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDecodeError {
    TooShort,
    WrongTag(u8),
}

impl FrameHeader {
    pub fn encode(&self, jpeg: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + jpeg.len());
        out.push(TAG_FRAME);
        out.push(if self.changed { FLAG_CHANGED } else { 0 });
        out.extend_from_slice(&self.captured_at_ms.to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(jpeg);
        out
    }

    /// Splits a binary frame message into its header and JPEG payload.
    pub fn decode(msg: &[u8]) -> Result<(FrameHeader, &[u8]), FrameDecodeError> {
        if msg.len() < FRAME_HEADER_LEN {
            return Err(FrameDecodeError::TooShort);
        }
        if msg[0] != TAG_FRAME {
            return Err(FrameDecodeError::WrongTag(msg[0]));
        }
        let header = FrameHeader {
            changed: msg[1] & FLAG_CHANGED != 0,
            captured_at_ms: u64::from_be_bytes(msg[2..10].try_into().unwrap()),
            width: u16::from_be_bytes(msg[10..12].try_into().unwrap()),
            height: u16::from_be_bytes(msg[12..14].try_into().unwrap()),
        };
        Ok((header, &msg[FRAME_HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let h = FrameHeader {
            captured_at_ms: 1_761_600_000_123,
            width: 1999,
            height: 1124,
            changed: true,
        };
        let msg = h.encode(b"\xFF\xD8jpeg");
        let (h2, body) = FrameHeader::decode(&msg).unwrap();
        assert_eq!(h, h2);
        assert_eq!(body, b"\xFF\xD8jpeg");
    }

    #[test]
    fn frame_decode_rejects_bad_input() {
        assert_eq!(FrameHeader::decode(&[2, 0]), Err(FrameDecodeError::TooShort));
        let mut msg = FrameHeader { captured_at_ms: 0, width: 1, height: 1, changed: false }.encode(b"");
        msg[0] = TAG_MIC_PCM;
        assert_eq!(FrameHeader::decode(&msg), Err(FrameDecodeError::WrongTag(TAG_MIC_PCM)));
    }

    #[test]
    fn pause_commands() {
        assert!(is_pause_command("Pause viewing please"));
        assert!(is_pause_command("ok, stop looking"));
        assert!(!is_pause_command("what's my expense ratio"));
    }

    #[test]
    fn json_shape() {
        let m: ClientMsg = serde_json::from_str(r#"{"type":"set_viewing","viewing":true}"#).unwrap();
        assert_eq!(m, ClientMsg::SetViewing { viewing: true });
        let s = serde_json::to_string(&ServerMsg::ViewingState { viewing: false }).unwrap();
        assert_eq!(s, r#"{"type":"viewing_state","viewing":false}"#);
    }
}
