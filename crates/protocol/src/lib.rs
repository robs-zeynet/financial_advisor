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
/// Hotkey that starts or finishes a page scan: Ctrl+Shift+S.
pub const SCAN_HOTKEY_KEY: &str = "S";

/// Hard cap on segments per page scan, whatever the config says (P3).
pub const MAX_SCAN_SEGMENTS_CAP: u32 = 20;

/// Spoken or typed phrases that pause viewing immediately. The client checks
/// typed text before sending it; the server checks every user turn.
pub fn is_pause_command(text: &str) -> bool {
    let t = text.to_lowercase();
    ["pause viewing", "stop viewing", "pause the viewing", "pause screen", "stop looking"]
        .iter()
        .any(|p| t.contains(p))
}

fn normalized(text: &str) -> String {
    text.trim()
        .trim_end_matches(['.', '!'])
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// "scan this page" and close variants. Handled locally; never sent to Claude.
pub fn is_scan_command(text: &str) -> bool {
    matches!(
        normalized(text).as_str(),
        "scan this page" | "scan the page" | "scan page" | "start scan" | "start a scan" | "scan"
    )
}

/// "done" and close variants, meaning "finish the scan". Only a command while
/// a scan is running.
pub fn is_scan_done_command(text: &str) -> bool {
    matches!(
        normalized(text).as_str(),
        "done" | "i'm done" | "im done" | "finished" | "done scanning" | "stop scan" | "end scan"
    )
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
    /// client's acknowledgement of a server-requested pause, and cancels any
    /// scan in progress.
    SetViewing { viewing: bool },
    /// The user started a page scan. `scan_id` is chosen by the client and
    /// increases within a page load.
    StartScan { scan_id: u32 },
    /// The user finished the page scan.
    EndScan { scan_id: u32 },
    /// Typed user input (M0); voice turns arrive as audio in M3.
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
    /// immediately, cancel any scan, and reply with `SetViewing { viewing: false }`.
    ViewingState {
        viewing: bool,
    },
    /// Page-scan progress. `active: false` with the current `scan_id` means
    /// the scan is over (finished, hit the segment cap, or cancelled).
    ScanState {
        active: bool,
        scan_id: u32,
        segments: u32,
        gaps: u32,
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
    /// A frame arrived that the session wasn't accepting; it was dropped.
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
    /// Milliseconds between samples while SCANNING.
    pub scan_sample_interval_ms: u32,
    /// Downscale so the long edge is at most this many pixels.
    pub max_long_edge: u32,
    /// JPEG quality, 0.0..=1.0.
    pub jpeg_quality: f64,
    /// Send an unchanged frame anyway after this many milliseconds.
    pub keepalive_ms: u32,
    /// dHash bits that must differ for a frame to count as changed.
    pub change_threshold_bits: u32,
    /// A scan ends automatically after this many segments.
    pub max_scan_segments: u32,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            sample_interval_ms: 1000,
            scan_sample_interval_ms: 250,
            max_long_edge: 2000,
            jpeg_quality: 0.8,
            keepalive_ms: 10_000,
            change_threshold_bits: 8,
            max_scan_segments: 12,
        }
    }
}

/// Header of a `TAG_FRAME` binary message (big-endian):
///
/// `tag u8 | flags u8 | captured_at_ms u64 | width u16 | height u16 |
///  scan_id u32 | segment_index u16 | offset_px i32 | jpeg...`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Wall-clock capture time in milliseconds since the Unix epoch.
    pub captured_at_ms: u64,
    pub width: u16,
    pub height: u16,
    /// True when the frame differs meaningfully from the last one sent;
    /// false for a keepalive.
    pub changed: bool,
    /// 0 for a live frame; otherwise the page scan this segment belongs to.
    pub scan_id: u32,
    /// 0-based position of this segment within its scan.
    pub segment_index: u16,
    /// Estimated vertical scroll from the previous segment, in this frame's
    /// pixels (positive = scrolled down). `None` when unknown.
    pub offset_px: Option<i32>,
    /// The segment didn't overlap the previous one: part of the page between
    /// them may be missing.
    pub gap_before: bool,
}

pub const FRAME_HEADER_LEN: usize = 1 + 1 + 8 + 2 + 2 + 4 + 2 + 4;
const FLAG_CHANGED: u8 = 0b0000_0001;
const FLAG_GAP_BEFORE: u8 = 0b0000_0010;
const FLAG_OFFSET_KNOWN: u8 = 0b0000_0100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDecodeError {
    TooShort,
    WrongTag(u8),
}

impl FrameHeader {
    /// A live (non-scan) frame header.
    pub fn live(captured_at_ms: u64, width: u16, height: u16, changed: bool) -> Self {
        Self {
            captured_at_ms,
            width,
            height,
            changed,
            scan_id: 0,
            segment_index: 0,
            offset_px: None,
            gap_before: false,
        }
    }

    pub fn is_scan_segment(&self) -> bool {
        self.scan_id != 0
    }

    pub fn encode(&self, jpeg: &[u8]) -> Vec<u8> {
        let mut flags = 0;
        if self.changed {
            flags |= FLAG_CHANGED;
        }
        if self.gap_before {
            flags |= FLAG_GAP_BEFORE;
        }
        if self.offset_px.is_some() {
            flags |= FLAG_OFFSET_KNOWN;
        }
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + jpeg.len());
        out.push(TAG_FRAME);
        out.push(flags);
        out.extend_from_slice(&self.captured_at_ms.to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.scan_id.to_be_bytes());
        out.extend_from_slice(&self.segment_index.to_be_bytes());
        out.extend_from_slice(&self.offset_px.unwrap_or(0).to_be_bytes());
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
        let flags = msg[1];
        let header = FrameHeader {
            changed: flags & FLAG_CHANGED != 0,
            gap_before: flags & FLAG_GAP_BEFORE != 0,
            captured_at_ms: u64::from_be_bytes(msg[2..10].try_into().unwrap()),
            width: u16::from_be_bytes(msg[10..12].try_into().unwrap()),
            height: u16::from_be_bytes(msg[12..14].try_into().unwrap()),
            scan_id: u32::from_be_bytes(msg[14..18].try_into().unwrap()),
            segment_index: u16::from_be_bytes(msg[18..20].try_into().unwrap()),
            offset_px: (flags & FLAG_OFFSET_KNOWN != 0)
                .then(|| i32::from_be_bytes(msg[20..24].try_into().unwrap())),
        };
        Ok((header, &msg[FRAME_HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let live = FrameHeader::live(1_761_600_000_123, 1999, 1124, true);
        let msg = live.encode(b"\xFF\xD8jpeg");
        let (h2, body) = FrameHeader::decode(&msg).unwrap();
        assert_eq!(live, h2);
        assert_eq!(body, b"\xFF\xD8jpeg");

        for offset_px in [None, Some(0), Some(-340), Some(812)] {
            let seg = FrameHeader {
                scan_id: 7,
                segment_index: 3,
                offset_px,
                gap_before: offset_px.is_none(),
                ..FrameHeader::live(5, 1600, 900, true)
            };
            let (h2, _) = FrameHeader::decode(&seg.encode(b"")).unwrap();
            assert_eq!(seg, h2);
            assert!(h2.is_scan_segment());
        }
    }

    #[test]
    fn frame_decode_rejects_bad_input() {
        assert_eq!(FrameHeader::decode(&[2, 0]), Err(FrameDecodeError::TooShort));
        let mut msg = FrameHeader::live(0, 1, 1, false).encode(b"");
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
    fn scan_commands() {
        assert!(is_scan_command("Scan this page."));
        assert!(is_scan_command("  scan   the page "));
        assert!(!is_scan_command("can you scan this page and tell me the fees?"));
        assert!(is_scan_done_command("Done!"));
        assert!(is_scan_done_command("I'm done"));
        assert!(!is_scan_done_command("done with fees, now taxes?"));
    }

    #[test]
    fn json_shape() {
        let m: ClientMsg = serde_json::from_str(r#"{"type":"set_viewing","viewing":true}"#).unwrap();
        assert_eq!(m, ClientMsg::SetViewing { viewing: true });
        let s = serde_json::to_string(&ServerMsg::ViewingState { viewing: false }).unwrap();
        assert_eq!(s, r#"{"type":"viewing_state","viewing":false}"#);
        let m: ClientMsg = serde_json::from_str(r#"{"type":"start_scan","scan_id":2}"#).unwrap();
        assert_eq!(m, ClientMsg::StartScan { scan_id: 2 });
    }
}
