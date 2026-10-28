//! Wire protocol shared by the server and the WASM client.
//!
//! Control and events travel as JSON text WebSocket messages (`ClientMsg` /
//! `ServerMsg`). Media travels as binary WebSocket messages whose first byte is
//! a tag (`TAG_*`).

use serde::{Deserialize, Serialize};

/// PCM16 mic audio at 16 kHz (client -> server): `tag | pcm16 LE...`.
pub const TAG_MIC_PCM: u8 = 0x01;
/// Mic sample rate.
pub const MIC_SAMPLE_RATE: u32 = 16_000;
/// JPEG screen frame (client -> server). See [`FrameHeader`].
pub const TAG_FRAME: u8 = 0x02;
/// TTS PCM audio (server -> client). See [`TtsHeader`].
pub const TAG_TTS_PCM: u8 = 0x03;
/// An uploaded PDF (client -> server): `tag | name_len u16 BE | name UTF-8 | pdf...`.
pub const TAG_PDF: u8 = 0x04;

/// Largest PDF the app accepts, in bytes. Base64 inflates it by a third, and
/// the API caps a request at 32 MB including everything else in context.
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;

/// Encodes a PDF upload message.
pub fn encode_pdf(name: &str, pdf: &[u8]) -> Vec<u8> {
    let name: String = name.chars().take(200).collect();
    let mut out = Vec::with_capacity(3 + name.len() + pdf.len());
    out.push(TAG_PDF);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(pdf);
    out
}

/// Splits a PDF upload message into its file name and bytes.
pub fn decode_pdf(msg: &[u8]) -> Result<(String, &[u8]), FrameDecodeError> {
    if msg.len() < 3 {
        return Err(FrameDecodeError::TooShort);
    }
    if msg[0] != TAG_PDF {
        return Err(FrameDecodeError::WrongTag(msg[0]));
    }
    let n = u16::from_be_bytes([msg[1], msg[2]]) as usize;
    let name = msg.get(3..3 + n).ok_or(FrameDecodeError::TooShort)?;
    Ok((String::from_utf8_lossy(name).into_owned(), &msg[3 + n..]))
}

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

/// Spoken guidance during a page scan. The browser decides when (it sees the
/// scroll); the server speaks it in the advisor's voice. Never sent to Claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cue", rename_all = "snake_case")]
pub enum ScanCue {
    /// The scan started: go to the top and hold still.
    Begin,
    /// A segment was captured; `segments` so far. Scroll on.
    Captured { segments: u32 },
    /// Far enough for the next segment: stop scrolling.
    Stop,
    /// Scrolling faster than the capture can follow.
    TooFast,
    /// Past the last segment with no overlap: scroll back up a little.
    TooFar,
    /// Stopped, but not far enough for a new segment.
    KeepGoing,
    /// A short final step was kept; probably the bottom.
    MaybeBottom,
    /// No movement for a while after a capture.
    StillThere,
    /// A long scan (past about six screens): suggest a PDF instead, once.
    SuggestPdf,
    /// The scan hit its segment limit.
    Full,
    /// The user finished the scan.
    Finished,
}

impl ScanCue {
    /// What the advisor says (and the app shows). Polite and short; captures
    /// rotate through a few phrasings so it doesn't sound robotic.
    pub fn phrase(&self) -> &'static str {
        match self {
            Self::Begin => {
                "Let's go through this page together. Please scroll to the very top, then hold still for a moment."
            }
            Self::Captured { segments: 1 } => {
                "Thank you, I have the top. Now please scroll down slowly, and I'll tell you when to stop."
            }
            Self::Captured { segments } => match segments % 3 {
                0 => "Got it, thank you. Please scroll down again.",
                1 => "Thanks. A little further down, please.",
                _ => "Got it. Please keep scrolling slowly.",
            },
            Self::Stop => "That's good, please stop there.",
            Self::TooFast => "Could you go a little slower, please?",
            Self::TooFar => "Oops, I think we skipped a bit. Could you scroll back up a little, please?",
            Self::KeepGoing => "Just a little further down, please.",
            Self::MaybeBottom => "Thank you. If that's the bottom of the page, just say done.",
            Self::StillThere => "Whenever you're ready, please scroll down a bit more, or say done if that's the end.",
            Self::SuggestPdf => {
                "Got it, thank you. This is a long page: if it's easier, you can print it to a PDF and \
                 upload it instead. Otherwise, please keep scrolling."
            }
            Self::Full => {
                "That's as much as I can take in one scan, thank you. For a page this long, printing it to \
                 a PDF and uploading it works best."
            }
            Self::Finished => "Thank you, I've got the page.",
        }
    }
}

/// Commands recognized in speech and handled locally, never sent to Claude
/// (except "pause viewing", which pauses first and then reaches the model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpokenCommand {
    PauseViewing,
    StartScan,
    EndScan,
}

/// Recognizes a spoken command in a short transcript. Speech-to-text adds
/// fillers and punctuation ("Okay, scan this page."), so leading fillers are
/// dropped and the rest must be one of the command phrases.
pub fn spoken_command(text: &str) -> Option<SpokenCommand> {
    const FILLERS: &[&str] = &["okay", "ok", "alright", "so", "um", "uh", "please", "now", "and", "right"];
    let t: String = text
        .to_lowercase()
        .replace("all right", "alright")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect();
    let mut words: Vec<&str> = t.split_whitespace().collect();
    while let Some(first) = words.first() {
        if FILLERS.contains(first) {
            words.remove(0);
        } else {
            break;
        }
    }
    while matches!(words.last(), Some(&"please") | Some(&"now") | Some(&"thanks")) {
        words.pop();
    }
    let rest = words.join(" ");
    if is_pause_command(&rest) && words.len() <= 6 {
        Some(SpokenCommand::PauseViewing)
    } else if is_scan_command(&rest) {
        Some(SpokenCommand::StartScan)
    } else if is_scan_done_command(&rest) {
        Some(SpokenCommand::EndScan)
    } else {
        None
    }
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
    /// Speak this scan guidance to the user now.
    ScanCue {
        #[serde(flatten)]
        cue: ScanCue,
    },
    /// Discard the uploaded PDF. (Uploads arrive as `TAG_PDF` binary messages.)
    RemoveDocument,
    /// Typed user input. Spoken input arrives as `TAG_MIC_PCM` audio.
    UserText { text: String },
    /// The mic was switched on or off. Off discards any partial utterance.
    MicState { on: bool },
    /// Push-to-talk (hold Space): speech starts now, regardless of the VAD...
    PttDown,
    /// ...and ends now.
    PttUp,
}

/// What the app shows about an uploaded PDF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentInfo {
    pub name: String,
    pub bytes: u64,
    pub pages: Option<u32>,
}

/// What the server can do with voice this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VoiceCaps {
    /// Speech-to-text is available (mic audio is used).
    pub stt: bool,
    /// Replies are spoken.
    pub tts: bool,
}

/// The listening side of the voice pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenState {
    /// Waiting for speech.
    Idle,
    /// The user is speaking.
    Hearing,
    /// Turning the utterance into text.
    Transcribing,
}

/// Messages the server sends to the browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    SessionStarted {
        session_id: String,
        viewing: bool,
        capture: CaptureSettings,
        voice: VoiceCaps,
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
    /// Voice pipeline state, for the UI.
    Listen {
        state: ListenState,
    },
    /// A spoken command was recognized and handled locally. The client
    /// carries out scan commands (capture lives in the browser).
    CommandHeard {
        text: String,
        command: SpokenCommand,
    },
    /// Stop playing this utterance now (barge-in or cancelled turn).
    StopAudio {
        utterance_id: u32,
    },
    /// The uploaded PDF, or `None` once removed. `pages` is an estimate.
    DocumentState {
        document: Option<DocumentInfo>,
    },
    /// Streaming assistant text.
    AssistantDelta {
        text: String,
    },
    /// The assistant turn finished.
    AssistantDone {
        stop_reason: String,
    },
    /// The advisor recorded or updated a screen note (P9). A note with an
    /// `id` already shown replaces it.
    ScreenNote {
        id: u32,
        page: String,
        notes: String,
        revision: u32,
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

/// Header of a `TAG_TTS_PCM` binary message (big-endian):
/// `tag u8 | utterance_id u32 | sentence_index u16 | sample_rate u32 | pcm16 LE...`
///
/// One message per synthesized sentence; the client plays them back to back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtsHeader {
    pub utterance_id: u32,
    pub sentence_index: u16,
    pub sample_rate: u32,
}

pub const TTS_HEADER_LEN: usize = 1 + 4 + 2 + 4;

impl TtsHeader {
    pub fn encode(&self, pcm: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(TTS_HEADER_LEN + pcm.len() * 2);
        out.push(TAG_TTS_PCM);
        out.extend_from_slice(&self.utterance_id.to_be_bytes());
        out.extend_from_slice(&self.sentence_index.to_be_bytes());
        out.extend_from_slice(&self.sample_rate.to_be_bytes());
        for s in pcm {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Splits a TTS message into its header and PCM16 LE payload bytes.
    pub fn decode(msg: &[u8]) -> Result<(TtsHeader, &[u8]), FrameDecodeError> {
        if msg.len() < TTS_HEADER_LEN {
            return Err(FrameDecodeError::TooShort);
        }
        if msg[0] != TAG_TTS_PCM {
            return Err(FrameDecodeError::WrongTag(msg[0]));
        }
        Ok((
            TtsHeader {
                utterance_id: u32::from_be_bytes(msg[1..5].try_into().unwrap()),
                sentence_index: u16::from_be_bytes(msg[5..7].try_into().unwrap()),
                sample_rate: u32::from_be_bytes(msg[7..11].try_into().unwrap()),
            },
            &msg[TTS_HEADER_LEN..],
        ))
    }
}

/// Encodes mic samples as a `TAG_MIC_PCM` message.
pub fn encode_mic(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + pcm.len() * 2);
    out.push(TAG_MIC_PCM);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Decodes the samples of a `TAG_MIC_PCM` message (odd trailing byte dropped).
pub fn decode_mic(msg: &[u8]) -> Vec<i16> {
    msg.get(1..).unwrap_or_default().chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
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
    fn audio_round_trip() {
        let h = TtsHeader { utterance_id: 9, sentence_index: 2, sample_rate: 22_050 };
        let msg = h.encode(&[1, -2, i16::MAX]);
        let (h2, body) = TtsHeader::decode(&msg).unwrap();
        assert_eq!(h, h2);
        assert_eq!(body.len(), 6);
        assert_eq!(decode_mic(&encode_mic(&[0, -1, 300])), vec![0, -1, 300]);
    }

    #[test]
    fn pdf_round_trip() {
        let msg = encode_pdf("holdings.pdf", b"%PDF-1.7 body");
        let (name, body) = decode_pdf(&msg).unwrap();
        assert_eq!((name.as_str(), body), ("holdings.pdf", &b"%PDF-1.7 body"[..]));
        assert_eq!(decode_pdf(&[TAG_PDF, 0, 9, b'x']), Err(FrameDecodeError::TooShort));
    }

    #[test]
    fn spoken_commands() {
        use SpokenCommand::*;
        assert_eq!(spoken_command("Okay, scan this page."), Some(StartScan));
        assert_eq!(spoken_command("Done."), Some(EndScan));
        assert_eq!(spoken_command("all right, I'm done"), Some(EndScan));
        assert_eq!(spoken_command("Pause viewing, please."), Some(PauseViewing));
        assert_eq!(spoken_command("Can you scan this page and tell me the fees?"), None);
        assert_eq!(spoken_command("I'm done with my taxes, what about dividends?"), None);
    }

    #[test]
    fn scan_cues_are_polite_and_serialize() {
        let all = [
            ScanCue::Begin,
            ScanCue::Captured { segments: 1 },
            ScanCue::Captured { segments: 2 },
            ScanCue::Stop,
            ScanCue::TooFast,
            ScanCue::TooFar,
            ScanCue::KeepGoing,
            ScanCue::MaybeBottom,
            ScanCue::StillThere,
            ScanCue::SuggestPdf,
            ScanCue::Full,
            ScanCue::Finished,
        ];
        for cue in all {
            let p = cue.phrase().to_lowercase();
            assert!(p.contains("please") || p.contains("thank"), "{cue:?}: {p}");
            let msg = ClientMsg::ScanCue { cue };
            assert_eq!(serde_json::from_str::<ClientMsg>(&serde_json::to_string(&msg).unwrap()).unwrap(), msg);
        }
        assert_eq!(
            serde_json::to_string(&ClientMsg::ScanCue { cue: ScanCue::Captured { segments: 3 } }).unwrap(),
            r#"{"type":"scan_cue","cue":"captured","segments":3}"#
        );
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
