//! Screen capture: `getDisplayMedia`, canvas sampling, dHash change detection,
//! JPEG encoding.
//!
//! P2 is enforced here, in the client: while PAUSED the loop returns before
//! touching the video, so no frame is drawn, hashed, encoded, or sent. Every
//! async step re-checks the gate, so a pause also drops a frame mid-encode.

use std::cell::RefCell;
use std::rc::Rc;

use protocol::{CaptureSettings, FrameHeader};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, CanvasRenderingContext2d, DisplayMediaStreamConstraints, HtmlCanvasElement, HtmlVideoElement,
    MediaStream, MediaStreamTrack,
};

use crate::dom;
use crate::ws::Ws;

// ------------------------------------------------------------ pure logic

/// Thumbnail size for dHash: 65 columns give 64 horizontal gradients per row.
pub const THUMB_W: u32 = 65;
pub const THUMB_H: u32 = 64;

/// 64x64-bit difference hash of a grayscale `THUMB_W` x `THUMB_H` thumbnail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DHash(pub [u64; THUMB_H as usize]);

impl DHash {
    pub fn of_gray(gray: &[u8]) -> Self {
        assert_eq!(gray.len(), (THUMB_W * THUMB_H) as usize, "thumbnail size");
        let mut rows = [0u64; THUMB_H as usize];
        for (y, row) in rows.iter_mut().enumerate() {
            let base = y * THUMB_W as usize;
            for x in 0..64 {
                if gray[base + x] < gray[base + x + 1] {
                    *row |= 1 << x;
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

/// Where the capture loop gets its 65x64 grayscale thumbnail. Only called
/// while VIEWING.
pub trait ThumbSource {
    fn thumbnail(&mut self) -> Option<Vec<u8>>;
}

/// The capture state machine. Starts PAUSED.
#[derive(Debug)]
pub struct CaptureState {
    viewing: bool,
    settings: CaptureSettings,
    last_hash: Option<DHash>,
    last_sent_ms: f64,
}

/// A frame the loop should encode and send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendFrame {
    pub changed: bool,
}

impl CaptureState {
    pub fn new(settings: CaptureSettings) -> Self {
        Self { viewing: false, settings, last_hash: None, last_sent_ms: 0.0 }
    }

    pub fn is_viewing(&self) -> bool {
        self.viewing
    }

    pub fn settings(&self) -> CaptureSettings {
        self.settings
    }

    pub fn set_settings(&mut self, settings: CaptureSettings) {
        self.settings = settings;
    }

    pub fn resume(&mut self) {
        self.viewing = true;
        // The first frame after resuming is always sent as changed.
        self.last_hash = None;
    }

    pub fn pause(&mut self) {
        self.viewing = false;
        self.last_hash = None;
    }

    /// One sampling tick. While PAUSED this returns `None` without calling
    /// the source at all.
    pub fn tick(&mut self, now_ms: f64, source: &mut impl ThumbSource) -> Option<SendFrame> {
        if !self.viewing {
            return None;
        }
        let gray = source.thumbnail()?;
        let hash = DHash::of_gray(&gray);
        let changed = match &self.last_hash {
            None => true,
            Some(prev) => prev.distance(&hash) >= self.settings.change_threshold_bits,
        };
        let keepalive_due = now_ms - self.last_sent_ms >= self.settings.keepalive_ms as f64;
        if !changed && !keepalive_due {
            return None;
        }
        if changed {
            self.last_hash = Some(hash);
        }
        self.last_sent_ms = now_ms;
        Some(SendFrame { changed })
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

type Callback = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// Owns the shared stream, the hidden video element, and the canvases.
pub struct Capturer {
    pub state: Rc<RefCell<CaptureState>>,
    stream: Rc<RefCell<Option<MediaStream>>>,
    video: HtmlVideoElement,
    canvas: HtmlCanvasElement,
    thumb: HtmlCanvasElement,
    encoding: Rc<RefCell<bool>>,
    ws: Ws,
    /// Called when sharing ends from the browser's own "Stop sharing" UI.
    on_share_ended: Callback,
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
            on_share_ended: Rc::new(RefCell::new(None)),
        });
        me.clone().start_loop();
        me
    }

    pub fn set_on_share_ended(&self, f: impl Fn() + 'static) {
        *self.on_share_ended.borrow_mut() = Some(Box::new(f));
    }

    pub fn is_viewing(&self) -> bool {
        self.state.borrow().is_viewing()
    }

    pub fn has_stream(&self) -> bool {
        self.stream.borrow().is_some()
    }

    pub fn resume(&self) {
        self.state.borrow_mut().resume();
    }

    /// Pauses immediately and clears any pixels left on the canvases.
    pub fn pause(&self) {
        self.state.borrow_mut().pause();
        self.clear_canvases();
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
                if let Some(f) = me.on_share_ended.borrow().as_ref() {
                    f();
                }
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

    fn start_loop(self: Rc<Self>) {
        let interval_ms = self.state.borrow().settings().sample_interval_ms;
        let me = self.clone();
        let interval = gloo_timers::callback::Interval::new(interval_ms, move || me.tick());
        interval.forget();
    }

    fn tick(self: &Rc<Self>) {
        // P2: nothing below runs while PAUSED.
        if !self.is_viewing() || *self.encoding.borrow() {
            return;
        }
        let mut src = VideoThumb { video: &self.video, thumb: &self.thumb };
        let now = js_sys::Date::now();
        let Some(send) = self.state.borrow_mut().tick(now, &mut src) else { return };
        let settings = self.state.borrow().settings();
        let (w, h) = scaled_size(self.video.video_width(), self.video.video_height(), settings.max_long_edge);
        self.canvas.set_width(w);
        self.canvas.set_height(h);
        let Some(ctx) = ctx2d(&self.canvas) else { return };
        if ctx.draw_image_with_html_video_element_and_dw_and_dh(&self.video, 0.0, 0.0, w as f64, h as f64).is_err() {
            return;
        }
        *self.encoding.borrow_mut() = true;
        let me = self.clone();
        let header = FrameHeader { captured_at_ms: now as u64, width: w as u16, height: h as u16, changed: send.changed };
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
            .to_blob_with_type_and_encoder_options(
                cb.as_ref().unchecked_ref(),
                "image/jpeg",
                &JsValue::from_f64(settings.jpeg_quality),
            )
            .is_err()
        {
            *self.encoding.borrow_mut() = false;
            self.clear_canvases();
        }
        cb.forget();
    }

    async fn finish_frame(&self, blob: JsValue, header: FrameHeader) {
        if !self.is_viewing() {
            return; // paused while encoding: drop
        }
        let Ok(blob) = blob.dyn_into::<Blob>() else { return };
        let Ok(buf) = JsFuture::from(blob.array_buffer()).await else { return };
        if !self.is_viewing() {
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
        let a = DHash::of_gray(&flat);
        let b = DHash::of_gray(&ramp);
        assert_eq!(a.distance(&a), 0);
        assert_eq!(a.distance(&b), 64 * 64);
    }
}
