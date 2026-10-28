//! Acceptance test 2 (client half): the capture state machine starts PAUSED
//! and emits no frames, and doesn't even read the screen, until resumed.
//!
//! Run headless with: `wasm-pack test --headless --chrome crates/client`

use client::capture::{CaptureState, SendFrame, ThumbSource, THUMB_H, THUMB_W};
use protocol::CaptureSettings;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// Counts every time the capture loop reads the screen.
struct CountingSource {
    reads: u32,
    gray: Vec<u8>,
}

impl CountingSource {
    fn new() -> Self {
        Self { reads: 0, gray: vec![100; (THUMB_W * THUMB_H) as usize] }
    }
}

impl ThumbSource for CountingSource {
    fn thumbnail(&mut self) -> Option<Vec<u8>> {
        self.reads += 1;
        Some(self.gray.clone())
    }
}

#[wasm_bindgen_test]
fn starts_paused_and_emits_nothing() {
    let mut state = CaptureState::new(CaptureSettings::default());
    let mut src = CountingSource::new();
    assert!(!state.is_viewing(), "capture must start PAUSED");
    for i in 0..100 {
        assert_eq!(state.tick(i as f64 * 1000.0, &mut src), None);
    }
    assert_eq!(src.reads, 0, "a paused capture loop must not read the screen");
}

#[wasm_bindgen_test]
fn emits_only_after_resume_and_stops_on_pause() {
    let settings = CaptureSettings::default();
    let mut state = CaptureState::new(settings);
    let mut src = CountingSource::new();

    state.resume();
    assert_eq!(state.tick(1_000.0, &mut src), Some(SendFrame { changed: true }));
    // Unchanged screen: nothing until the keepalive is due.
    assert_eq!(state.tick(2_000.0, &mut src), None);
    let keepalive_at = 1_000.0 + settings.keepalive_ms as f64;
    assert_eq!(state.tick(keepalive_at, &mut src), Some(SendFrame { changed: false }));
    // A changed screen goes out right away.
    src.gray = (0..THUMB_W * THUMB_H).map(|i| ((i * 37) % 251) as u8).collect();
    assert_eq!(state.tick(keepalive_at + 1_000.0, &mut src), Some(SendFrame { changed: true }));

    let reads_before_pause = src.reads;
    state.pause();
    for i in 0..20 {
        assert_eq!(state.tick(100_000.0 + i as f64 * 1000.0, &mut src), None);
    }
    assert_eq!(src.reads, reads_before_pause, "no reads after pause");

    // After resuming, the first frame is sent as changed.
    state.resume();
    assert_eq!(state.tick(200_000.0, &mut src), Some(SendFrame { changed: true }));
}

#[wasm_bindgen_test]
fn page_without_app_shell_does_not_capture() {
    // The wasm start function is a no-op without #app, so loading the module
    // never touches getDisplayMedia.
    let doc = web_sys::window().unwrap().document().unwrap();
    assert!(doc.get_element_by_id("app").is_none());
    assert_eq!(doc.get_elements_by_tag_name("video").length(), 0);
}
