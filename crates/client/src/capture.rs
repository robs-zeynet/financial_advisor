//! Screen capture: `getDisplayMedia`, canvas sampling, dHash change and settle
//! detection, page-scan segments, JPEG encoding.
//!
//! P2 is enforced here, in the client: while PAUSED the loop returns before
//! touching the video, so no frame is drawn, hashed, encoded, or sent. Every
//! async step re-checks the gate, so a pause also drops a frame mid-encode.

use std::cell::RefCell;
use std::rc::Rc;

use protocol::{CaptureSettings, FrameHeader, MAX_SCAN_SEGMENTS_CAP};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, CanvasRenderingContext2d, DisplayMediaStreamConstraints, HtmlCanvasElement, HtmlVideoElement,
    MediaStream, MediaStreamTrack,
};

use crate::dom;
use protocol::ScanCue;

use crate::scan::{rows_to_px, ScanProgress, ScanSummary, SegmentInfo};
use crate::ws::Ws;

// ------------------------------------------------------------ pure logic

/// Grayscale thumbnail taken every sample: 65 columns (64 horizontal
/// gradients for dHash) by 256 rows (enough vertical detail to estimate
/// scroll offsets during a scan).
pub const THUMB_W: u32 = 65;
pub const THUMB_H: u32 = 256;
/// Thumbnail rows averaged into one dHash row (256 / 4 = 64 rows).
const DHASH_ROW_GROUP: usize = 4;
const DHASH_ROWS: usize = THUMB_H as usize / DHASH_ROW_GROUP;

/// 64x64-bit difference hash of a thumbnail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DHash(pub [u64; DHASH_ROWS]);

impl DHash {
    /// Hashes a `THUMB_W` x `THUMB_H` grayscale thumbnail.
    pub fn of_thumb(gray: &[u8]) -> Self {
        assert_eq!(gray.len(), (THUMB_W * THUMB_H) as usize, "thumbnail size");
        let w = THUMB_W as usize;
        let mut rows = [0u64; DHASH_ROWS];
        for (hy, bits) in rows.iter_mut().enumerate() {
            let mut avg = [0u32; THUMB_W as usize];
            for y in hy * DHASH_ROW_GROUP..(hy + 1) * DHASH_ROW_GROUP {
                for (x, a) in avg.iter_mut().enumerate() {
                    *a += gray[y * w + x] as u32;
                }
            }
            for x in 0..64 {
                if avg[x] < avg[x + 1] {
                    *bits |= 1 << x;
                }
            }
        }
        Self(rows)
    }

    pub fn distance(&self, other: &DHash) -> u32 {
        self.0.iter().zip(other.0.iter()).map(|(a, b)| (a ^ b).count_ones()).sum()
    }
}

/// RGBA -> grayscale (BT.601 luma).
pub fn rgba_to_gray(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8)
        .collect()
}

/// Where the capture loop gets its grayscale thumbnail. Only called while
/// VIEWING or SCANNING.
pub trait ThumbSource {
    fn thumbnail(&mut self) -> Option<Vec<u8>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Paused,
    Viewing,
    /// A page scan; `resume_viewing` is the state to return to afterwards.
    Scanning { scan_id: u32, resume_viewing: bool },
}

/// What the loop should encode and send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendFrame {
    pub changed: bool,
    /// Set for page-scan segments.
    pub scan: Option<SegmentInfo>,
}

/// The capture state machine. Starts PAUSED.
#[derive(Debug)]
pub struct CaptureState {
    mode: Mode,
    settings: CaptureSettings,
    last_sample_ms: Option<f64>,
    /// Hash of the previous sample, for settle detection.
    prev_sample: Option<DHash>,
    /// Hash of the last live frame sent, for change detection.
    last_sent: Option<DHash>,
    last_sent_ms: f64,
    scan: Option<ScanProgress>,
    /// Guidance to speak, taken by the loop after each tick.
    pending_cue: Option<ScanCue>,
}

impl CaptureState {
    pub fn new(settings: CaptureSettings) -> Self {
        Self {
            mode: Mode::Paused,
            settings,
            last_sample_ms: None,
            prev_sample: None,
            last_sent: None,
            last_sent_ms: 0.0,
            scan: None,
            pending_cue: None,
        }
    }

    /// The latest guidance cue, if any, since the last call.
    pub fn take_cue(&mut self) -> Option<ScanCue> {
        self.pending_cue.take()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// True while frames may be captured (VIEWING or SCANNING).
    pub fn is_viewing(&self) -> bool {
        self.mode != Mode::Paused
    }

    pub fn is_scanning(&self) -> bool {
        matches!(self.mode, Mode::Scanning { .. })
    }

    pub fn scan_progress(&self) -> Option<&ScanProgress> {
        self.scan.as_ref()
    }

    pub fn settings(&self) -> CaptureSettings {
        self.settings
    }

    pub fn set_settings(&mut self, settings: CaptureSettings) {
        self.settings = settings;
    }

    fn reset_sampling(&mut self) {
        self.last_sample_ms = None;
        self.prev_sample = None;
        self.last_sent = None;
    }

    /// Resume live viewing. Ignored during a scan.
    pub fn resume(&mut self) {
        if self.mode == Mode::Paused {
            self.mode = Mode::Viewing;
            self.reset_sampling();
        }
    }

    /// Explicit pause: stops everything and cancels any scan.
    pub fn pause(&mut self) -> Option<ScanSummary> {
        self.mode = Mode::Paused;
        self.reset_sampling();
        self.pending_cue = None;
        self.scan.take().map(|s| s.summary())
    }

    /// Starts a page scan (an explicit, bounded period of viewing).
    pub fn start_scan(&mut self, scan_id: u32) {
        let resume_viewing = match self.mode {
            Mode::Paused => false,
            Mode::Viewing => true,
            Mode::Scanning { resume_viewing, .. } => resume_viewing,
        };
        self.mode = Mode::Scanning { scan_id, resume_viewing };
        self.scan = Some(ScanProgress::new(scan_id));
        self.reset_sampling();
        self.pending_cue = Some(ScanCue::Begin);
    }

    /// Finishes the scan and returns to the pre-scan state.
    pub fn end_scan(&mut self) -> Option<ScanSummary> {
        let Mode::Scanning { resume_viewing, .. } = self.mode else { return None };
        self.mode = if resume_viewing { Mode::Viewing } else { Mode::Paused };
        self.reset_sampling();
        self.pending_cue = None;
        self.scan.take().map(|s| s.summary())
    }

    fn max_segments(&self) -> u32 {
        self.settings.max_scan_segments.clamp(1, MAX_SCAN_SEGMENTS_CAP)
    }

    /// The scan has reached its segment cap and should be ended.
    pub fn scan_full(&self) -> bool {
        self.scan.as_ref().is_some_and(|s| s.segments >= self.max_segments())
    }

    /// Whether a frame decided earlier may still be sent now (it may have
    /// been encoding while the user paused or ended the scan).
    pub fn accepts(&self, header: &FrameHeader) -> bool {
        match self.mode {
            Mode::Paused => false,
            Mode::Viewing => !header.is_scan_segment(),
            Mode::Scanning { scan_id, .. } => header.scan_id == scan_id,
        }
    }

    /// One timer tick. While PAUSED this returns `None` without calling the
    /// source at all. Otherwise it samples at the mode's interval and decides
    /// whether to send a frame.
    pub fn tick(&mut self, now_ms: f64, source: &mut impl ThumbSource) -> Option<SendFrame> {
        if self.mode == Mode::Paused {
            return None;
        }
        let interval = match self.mode {
            Mode::Scanning { .. } => self.settings.scan_sample_interval_ms,
            _ => self.settings.sample_interval_ms,
        } as f64;
        if self.last_sample_ms.is_some_and(|t| now_ms - t < interval - 1.0) {
            return None;
        }
        if self.scan_full() {
            return None;
        }
        let thumb = source.thumbnail()?;
        self.last_sample_ms = Some(now_ms);
        let hash = DHash::of_thumb(&thumb);
        let threshold = self.settings.change_threshold_bits;
        // Settled: the screen didn't change since the previous sample.
        let settled = self.prev_sample.is_some_and(|p| p.distance(&hash) < threshold);
        self.prev_sample = Some(hash);

        if let Some(scan) = self.scan.as_mut() {
            let obs = scan.observe(now_ms, hash, &thumb, settled, threshold);
            if obs.cue.is_some() {
                self.pending_cue = obs.cue;
            }
            return obs.segment.map(|info| SendFrame { changed: true, scan: Some(info) });
        }

        let changed = self.last_sent.is_none_or(|l| l.distance(&hash) >= threshold);
        if changed {
            if !settled {
                return None; // mid-scroll or mid-animation: wait for it to settle
            }
            self.last_sent = Some(hash);
            self.last_sent_ms = now_ms;
            return Some(SendFrame { changed: true, scan: None });
        }
        if now_ms - self.last_sent_ms >= self.settings.keepalive_ms as f64 {
            self.last_sent_ms = now_ms;
            return Some(SendFrame { changed: false, scan: None });
        }
        None
    }
}

/// Output size so the long edge is at most `max_long_edge`.
pub fn scaled_size(w: u32, h: u32, max_long_edge: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_long_edge || long == 0 {
        return (w, h);
    }
    let s = max_long_edge as f64 / long as f64;
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

// ------------------------------------------------------------ browser side

/// Things the capture loop reports back to the view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureEvent {
    /// The user stopped sharing from the browser's own UI.
    ShareEnded,
    /// A scan segment was captured.
    Segment { segments: u32, gap_before: bool },
    /// The scan reached its segment cap.
    ScanFull,
    /// Guidance to speak to the user.
    Cue(ScanCue),
}

type EventSink = Rc<RefCell<Option<Box<dyn Fn(CaptureEvent)>>>>;

/// Owns the shared stream, the hidden video element, and the canvases.
pub struct Capturer {
    pub state: Rc<RefCell<CaptureState>>,
    stream: Rc<RefCell<Option<MediaStream>>>,
    video: HtmlVideoElement,
    canvas: HtmlCanvasElement,
    thumb: HtmlCanvasElement,
    encoding: Rc<RefCell<bool>>,
    ws: Ws,
    on_event: EventSink,
}

struct VideoThumb<'a> {
    video: &'a HtmlVideoElement,
    thumb: &'a HtmlCanvasElement,
}

impl ThumbSource for VideoThumb<'_> {
    fn thumbnail(&mut self) -> Option<Vec<u8>> {
        if self.video.video_width() == 0 {
            return None; // stream not ready
        }
        let ctx = ctx2d(self.thumb)?;
        ctx.draw_image_with_html_video_element_and_dw_and_dh(self.video, 0.0, 0.0, THUMB_W as f64, THUMB_H as f64)
            .ok()?;
        let data = ctx.get_image_data(0.0, 0.0, THUMB_W as f64, THUMB_H as f64).ok()?;
        ctx.clear_rect(0.0, 0.0, THUMB_W as f64, THUMB_H as f64);
        Some(rgba_to_gray(&data.data()))
    }
}

fn ctx2d(canvas: &HtmlCanvasElement) -> Option<CanvasRenderingContext2d> {
    canvas.get_context("2d").ok()??.dyn_into().ok()
}

impl Capturer {
    pub fn new(ws: Ws) -> Rc<Self> {
        let video: HtmlVideoElement = dom::el("video", "hidden-capture").unchecked_into();
        video.set_muted(true);
        dom::set_attr(&video, "playsinline", "");
        let canvas: HtmlCanvasElement = dom::el("canvas", "hidden-capture").unchecked_into();
        let thumb: HtmlCanvasElement = dom::el("canvas", "hidden-capture").unchecked_into();
        thumb.set_width(THUMB_W);
        thumb.set_height(THUMB_H);
        let body = dom::document().body().expect("body");
        for e in [video.unchecked_ref::<web_sys::Element>(), canvas.unchecked_ref(), thumb.unchecked_ref()] {
            body.append_child(e).expect("append");
        }
        let me = Rc::new(Self {
            state: Rc::new(RefCell::new(CaptureState::new(CaptureSettings::default()))),
            stream: Rc::new(RefCell::new(None)),
            video,
            canvas,
            thumb,
            encoding: Rc::new(RefCell::new(false)),
            ws,
            on_event: Rc::new(RefCell::new(None)),
        });
        me.clone().start_loop();
        me
    }

    pub fn set_on_event(&self, f: impl Fn(CaptureEvent) + 'static) {
        *self.on_event.borrow_mut() = Some(Box::new(f));
    }

    fn emit(&self, e: CaptureEvent) {
        if let Some(f) = self.on_event.borrow().as_ref() {
            f(e);
        }
    }

    pub fn is_viewing(&self) -> bool {
        self.state.borrow().is_viewing()
    }

    pub fn is_scanning(&self) -> bool {
        self.state.borrow().is_scanning()
    }

    pub fn mode(&self) -> Mode {
        self.state.borrow().mode()
    }

    pub fn has_stream(&self) -> bool {
        self.stream.borrow().is_some()
    }

    pub fn resume(&self) {
        self.state.borrow_mut().resume();
    }

    /// Pauses immediately (cancelling any scan) and clears any pixels left on
    /// the canvases.
    pub fn pause(&self) -> Option<ScanSummary> {
        let cancelled = self.state.borrow_mut().pause();
        self.clear_canvases();
        cancelled
    }

    pub fn start_scan(&self, scan_id: u32) {
        self.state.borrow_mut().start_scan(scan_id);
    }

    pub fn end_scan(&self) -> Option<ScanSummary> {
        let summary = self.state.borrow_mut().end_scan();
        if !self.is_viewing() {
            self.clear_canvases();
        }
        summary
    }

    fn clear_canvases(&self) {
        for c in [&self.canvas, &self.thumb] {
            if let Some(ctx) = ctx2d(c) {
                ctx.clear_rect(0.0, 0.0, c.width() as f64, c.height() as f64);
            }
        }
    }

    /// Asks the user to pick a window or tab to share. Viewing stays as it was
    /// (PAUSED unless the user resumes).
    pub async fn share(self: &Rc<Self>) -> Result<(), JsValue> {
        self.stop_sharing();
        let devices = dom::window().navigator().media_devices()?;
        let constraints = DisplayMediaStreamConstraints::new();
        constraints.set_video(&JsValue::TRUE);
        constraints.set_audio(&JsValue::FALSE);
        let stream: MediaStream = JsFuture::from(devices.get_display_media_with_constraints(&constraints)?)
            .await?
            .dyn_into()?;
        for track in stream.get_video_tracks().iter() {
            let track: MediaStreamTrack = track.unchecked_into();
            let me = self.clone();
            dom::on(&track, "ended", move |_: web_sys::Event| {
                me.pause();
                me.stream.borrow_mut().take();
                me.video.set_src_object(None);
                me.emit(CaptureEvent::ShareEnded);
            });
        }
        self.video.set_src_object(Some(&stream));
        *self.stream.borrow_mut() = Some(stream);
        let _ = self.video.play();
        Ok(())
    }

    /// Stops the shared stream entirely (on hang-up).
    pub fn stop_sharing(&self) {
        self.pause();
        if let Some(stream) = self.stream.borrow_mut().take() {
            for t in stream.get_tracks().iter() {
                t.unchecked_into::<MediaStreamTrack>().stop();
            }
        }
        self.video.set_src_object(None);
    }

    /// One fast base timer; `CaptureState::tick` decides when to actually
    /// sample for the current mode.
    fn start_loop(self: Rc<Self>) {
        let d = CaptureSettings::default();
        let base_ms = d.scan_sample_interval_ms.min(d.sample_interval_ms);
        let me = self.clone();
        let interval = gloo_timers::callback::Interval::new(base_ms, move || me.tick());
        interval.forget();
    }

    fn tick(self: &Rc<Self>) {
        // P2: nothing below runs while PAUSED.
        if !self.is_viewing() || *self.encoding.borrow() {
            return;
        }
        let mut src = VideoThumb { video: &self.video, thumb: &self.thumb };
        let now = js_sys::Date::now();
        let send = self.state.borrow_mut().tick(now, &mut src);
        let cue = self.state.borrow_mut().take_cue();
        if let Some(cue) = cue {
            self.emit(CaptureEvent::Cue(cue));
        }
        let Some(send) = send else { return };
        let settings = self.state.borrow().settings();
        let (w, h) = scaled_size(self.video.video_width(), self.video.video_height(), settings.max_long_edge);
        let mut header = FrameHeader::live(now as u64, w as u16, h as u16, send.changed);
        if let Some(seg) = send.scan {
            header.scan_id = seg.scan_id;
            header.segment_index = seg.segment_index;
            header.offset_px = seg.offset_rows.map(|r| rows_to_px(r, h));
            header.gap_before = seg.gap_before;
            self.emit(CaptureEvent::Segment { segments: seg.segment_index as u32 + 1, gap_before: seg.gap_before });
        }
        self.encode_and_send(header, w, h, settings.jpeg_quality);
        if send.scan.is_some() && self.state.borrow().scan_full() {
            self.emit(CaptureEvent::ScanFull);
        }
    }

    fn encode_and_send(self: &Rc<Self>, header: FrameHeader, w: u32, h: u32, quality: f64) {
        self.canvas.set_width(w);
        self.canvas.set_height(h);
        let Some(ctx) = ctx2d(&self.canvas) else { return };
        if ctx.draw_image_with_html_video_element_and_dw_and_dh(&self.video, 0.0, 0.0, w as f64, h as f64).is_err() {
            return;
        }
        *self.encoding.borrow_mut() = true;
        let me = self.clone();
        let cb = Closure::once(move |blob: JsValue| {
            me.clear_canvases();
            let me2 = me.clone();
            wasm_bindgen_futures::spawn_local(async move {
                me2.finish_frame(blob, header).await;
                *me2.encoding.borrow_mut() = false;
            });
        });
        if self
            .canvas
            .to_blob_with_type_and_encoder_options(cb.as_ref().unchecked_ref(), "image/jpeg", &JsValue::from_f64(quality))
            .is_err()
        {
            *self.encoding.borrow_mut() = false;
            self.clear_canvases();
        }
        cb.forget();
    }

    async fn finish_frame(&self, blob: JsValue, header: FrameHeader) {
        if !self.state.borrow().accepts(&header) {
            return; // paused, or the scan ended, while encoding: drop
        }
        let Ok(blob) = blob.dyn_into::<Blob>() else { return };
        let Ok(buf) = JsFuture::from(blob.array_buffer()).await else { return };
        if !self.state.borrow().accepts(&header) {
            return;
        }
        let jpeg = js_sys::Uint8Array::new(&buf).to_vec();
        self.ws.send_bytes(&header.encode(&jpeg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_size_caps_long_edge() {
        assert_eq!(scaled_size(3840, 2160, 2000), (2000, 1125));
        assert_eq!(scaled_size(1000, 800, 2000), (1000, 800));
        assert_eq!(scaled_size(1080, 1920, 2000), (1080, 1920));
    }

    #[test]
    fn dhash_distance() {
        let flat = vec![128u8; (THUMB_W * THUMB_H) as usize];
        let ramp: Vec<u8> = (0..THUMB_W * THUMB_H).map(|i| (i % THUMB_W) as u8).collect();
        let a = DHash::of_thumb(&flat);
        let b = DHash::of_thumb(&ramp);
        assert_eq!(a.distance(&a), 0);
        assert_eq!(a.distance(&b), 64 * 64);
    }
}
