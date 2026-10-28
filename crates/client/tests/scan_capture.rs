//! Acceptance test 11: the guided page-scan state machine against a
//! synthetic tall page scrolled to known offsets: segments, offsets, and the
//! spoken guidance (start, stop, slow down, go back, keep going).
//!
//! Run headless with: `wasm-pack test --headless --chrome crates/client`

use client::capture::{CaptureState, Mode, ThumbSource, THUMB_H, THUMB_W};
use client::scan::{estimate_offset, rows_to_px, Offset, SegmentInfo};
use protocol::{CaptureSettings, ScanCue};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// Page pixels per thumbnail pixel.
const SCALE: usize = 4;
const VIEW_W: usize = THUMB_W as usize * SCALE; // 260 px
const VIEW_H: usize = THUMB_H as usize * SCALE; // 1024 px
const PAGE_H: usize = 4096;
const HEADER_ROWS: usize = 16; // sticky header, in thumbnail rows
const TICK_MS: f64 = 250.0;

/// A tall page of "text lines": 14 px bands of random column blocks every
/// 28 px, on a light background. Deterministic (xorshift).
struct Page {
    px: Vec<u8>,
}

impl Page {
    fn new(seed: u64) -> Self {
        let mut s = seed;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut px = vec![240u8; VIEW_W * PAGE_H];
        for band_top in (0..PAGE_H).step_by(28) {
            let blocks: Vec<u8> = (0..VIEW_W / 4).map(|_| (rnd() % 200) as u8).collect();
            for y in band_top..(band_top + 14).min(PAGE_H) {
                for x in 0..VIEW_W {
                    px[y * VIEW_W + x] = blocks[x / 4];
                }
            }
        }
        Self { px }
    }

    /// The 65x256 grayscale thumbnail of the viewport scrolled to `y`, with a
    /// sticky header drawn over the top.
    fn thumbnail(&self, y: usize) -> Vec<u8> {
        let (tw, th) = (THUMB_W as usize, THUMB_H as usize);
        let mut out = vec![0u8; tw * th];
        for ty in 0..th {
            for tx in 0..tw {
                let mut sum = 0u32;
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        sum += self.px[(y + ty * SCALE + dy) * VIEW_W + tx * SCALE + dx] as u32;
                    }
                }
                out[ty * tw + tx] = (sum / (SCALE * SCALE) as u32) as u8;
            }
        }
        for ty in 0..HEADER_ROWS {
            for tx in 0..tw {
                out[ty * tw + tx] = if (tx / 8 + ty / 4) % 2 == 0 { 30 } else { 220 };
            }
        }
        out
    }
}

/// A source whose scroll position the test moves.
struct Scroller<'a> {
    page: &'a Page,
    y: usize,
    reads: u32,
}

impl ThumbSource for Scroller<'_> {
    fn thumbnail(&mut self) -> Option<Vec<u8>> {
        self.reads += 1;
        Some(self.page.thumbnail(self.y))
    }
}

struct Sim<'a> {
    state: CaptureState,
    src: Scroller<'a>,
    now: f64,
    segments: Vec<SegmentInfo>,
    live_frames: u32,
    cues: Vec<ScanCue>,
}

impl<'a> Sim<'a> {
    fn new(page: &'a Page, settings: CaptureSettings) -> Self {
        Self {
            state: CaptureState::new(settings),
            src: Scroller { page, y: 0, reads: 0 },
            now: 0.0,
            segments: Vec::new(),
            live_frames: 0,
            cues: Vec::new(),
        }
    }

    fn tick(&mut self) {
        self.now += TICK_MS;
        if let Some(f) = self.state.tick(self.now, &mut self.src) {
            match f.scan {
                Some(seg) => self.segments.push(seg),
                None => self.live_frames += 1,
            }
        }
        self.cues.extend(self.state.take_cue());
    }

    /// Starts a scan at the top and waits for the first capture.
    fn start_at_top(&mut self, scan_id: u32) {
        self.state.start_scan(scan_id);
        self.scroll_to(0, 100, 16); // 4 s: hears the instructions, holds still
        assert_eq!(self.segments.len(), 1, "the top is captured after a moment");
    }

    fn has_cue(&self, f: impl Fn(&ScanCue) -> bool) -> bool {
        self.cues.iter().any(f)
    }

    /// Scrolls to `target` in `step`-px moves (one per tick), then rests for
    /// `rest` ticks.
    fn scroll_to(&mut self, target: usize, step: usize, rest: u32) {
        while self.src.y != target {
            self.src.y = if self.src.y < target {
                (self.src.y + step).min(target)
            } else {
                self.src.y.saturating_sub(step).max(target)
            };
            self.tick();
        }
        for _ in 0..rest {
            self.tick();
        }
    }
}

#[wasm_bindgen_test]
fn guided_scan_captures_each_step_with_accurate_offsets() {
    let page = Page::new(0x5eed);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(1);
    assert_eq!(sim.cues[..2], [ScanCue::Begin, ScanCue::Captured { segments: 1 }]);
    let stops = [0usize, 700, 1400, 2100, 2800];
    for &stop in &stops[1..] {
        sim.scroll_to(stop, 100, 3);
    }
    // The last screenful is a short step: "a little further", then kept as
    // probably the bottom.
    sim.scroll_to(PAGE_H - VIEW_H, 100, 14);
    let all_stops: Vec<usize> = stops.iter().copied().chain([PAGE_H - VIEW_H]).collect();
    assert_eq!(sim.segments.len(), all_stops.len(), "one segment per stop: {:?}", sim.segments);
    assert_eq!(sim.live_frames, 0);
    assert!(sim.segments.iter().all(|s| s.scan_id == 1 && !s.gap_before));
    for (i, seg) in sim.segments.iter().enumerate().skip(1) {
        assert_eq!(seg.segment_index as usize, i);
        let truth = (all_stops[i] - all_stops[i - 1]) as f64;
        let est = rows_to_px(seg.offset_rows.expect("offset known"), VIEW_H as u32) as f64;
        assert!((est - truth).abs() <= truth * 0.05, "segment {i}: estimated {est}, true {truth}");
    }
    // Told when to stop on each long step, and thanked for each capture.
    assert_eq!(sim.cues.iter().filter(|c| **c == ScanCue::Stop).count(), 4);
    for n in 2..=5 {
        assert!(sim.has_cue(|c| *c == ScanCue::Captured { segments: n }), "thanked for segment {n}");
    }
    assert!(sim.has_cue(|c| *c == ScanCue::KeepGoing));
    assert_eq!(sim.cues.last(), Some(&ScanCue::MaybeBottom));
}

#[wasm_bindgen_test]
fn lingering_and_tiny_scrolls_make_no_duplicates() {
    let page = Page::new(11);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(1);
    // Small nudges with pauses: not enough new content for a segment.
    for y in [100, 200, 300] {
        sim.scroll_to(y, 100, 6);
    }
    assert_eq!(sim.segments.len(), 1, "no near-duplicate segments");
    assert!(sim.has_cue(|c| *c == ScanCue::KeepGoing), "asked to scroll a little further");
    sim.scroll_to(600, 100, 3);
    assert_eq!(sim.segments.len(), 2);
    // Lingering in place adds nothing, and eventually gets a gentle nudge.
    for _ in 0..40 {
        sim.tick();
    }
    assert_eq!(sim.segments.len(), 2);
    assert!(sim.has_cue(|c| *c == ScanCue::StillThere));
}

#[wasm_bindgen_test]
fn scrolling_too_far_asks_to_go_back_instead_of_leaving_a_gap() {
    let page = Page::new(42);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(3);
    sim.src.y = 2200; // more than a screen below the last capture
    sim.tick();
    sim.tick();
    sim.tick();
    assert!(sim.has_cue(|c| *c == ScanCue::TooFar));
    assert_eq!(sim.segments.len(), 1, "nothing captured past the gap");
    // Back up until it overlaps where we left off.
    sim.scroll_to(800, 100, 3);
    assert_eq!(sim.segments.len(), 2);
    assert!(!sim.segments[1].gap_before);
    let est = rows_to_px(sim.segments[1].offset_rows.unwrap(), VIEW_H as u32);
    assert!((est - 800).abs() <= 40, "offset {est}");
    assert_eq!(sim.state.scan_progress().unwrap().gaps, 0);
}

#[wasm_bindgen_test]
fn holding_still_past_a_gap_keeps_it_flagged() {
    let page = Page::new(43);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(1);
    sim.src.y = 2400;
    for _ in 0..30 {
        sim.tick(); // 7.5 s without scrolling back
    }
    assert_eq!(sim.segments.len(), 2);
    assert!(sim.segments[1].gap_before);
    assert_eq!(sim.state.scan_progress().unwrap().gaps, 1);
}

#[wasm_bindgen_test]
fn scrolling_fast_asks_to_slow_down() {
    let page = Page::new(7);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(1);
    sim.src.y = 1100;
    sim.tick();
    sim.src.y = 2200;
    sim.tick();
    assert!(sim.has_cue(|c| *c == ScanCue::TooFast), "cues: {:?}", sim.cues);
    assert_eq!(sim.segments.len(), 1);
}

#[wasm_bindgen_test]
fn nothing_is_emitted_mid_scroll() {
    let page = Page::new(8);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(1);
    // Keep scrolling without ever stopping.
    for _ in 0..6 {
        sim.src.y += 100;
        sim.tick();
    }
    assert_eq!(sim.segments.len(), 1, "no segment while the view keeps moving");
    assert!(sim.has_cue(|c| *c == ScanCue::Stop), "told to stop once far enough");
    sim.tick();
    sim.tick();
    assert_eq!(sim.segments.len(), 2, "one segment once it settles");
}

#[wasm_bindgen_test]
fn scrolling_back_up_gives_a_negative_offset() {
    let page = Page::new(9);
    let a = page.thumbnail(1500);
    let b = page.thumbnail(1100);
    match estimate_offset(&a, &b) {
        Offset::Rows(r) => {
            let px = rows_to_px(r, VIEW_H as u32);
            assert!((px + 400).abs() <= 20, "expected about -400 px, got {px}");
        }
        other => panic!("expected an offset, got {other:?}"),
    }
}

#[wasm_bindgen_test]
fn paused_reads_nothing_and_pause_cancels_a_scan() {
    let page = Page::new(1);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    for _ in 0..20 {
        sim.tick();
    }
    assert_eq!(sim.src.reads, 0, "PAUSED: the screen is never read");

    sim.start_at_top(5);
    sim.scroll_to(800, 100, 2);
    assert_eq!(sim.segments.len(), 2);

    let cancelled = sim.state.pause().expect("pause cancels the scan");
    assert_eq!(cancelled.segments, 2);
    assert_eq!(sim.state.mode(), Mode::Paused);
    assert!(sim.state.scan_progress().is_none());
    let (reads, cues) = (sim.src.reads, sim.cues.len());
    sim.scroll_to(1600, 100, 4);
    assert_eq!(sim.segments.len(), 2, "nothing emitted after an explicit pause");
    assert_eq!(sim.src.reads, reads, "nothing read after an explicit pause");
    assert_eq!(sim.cues.len(), cues, "no guidance after an explicit pause");
}

#[wasm_bindgen_test]
fn scan_returns_to_the_previous_state() {
    let page = Page::new(2);
    let mut sim = Sim::new(&page, CaptureSettings::default());

    // From PAUSED: back to PAUSED.
    sim.state.start_scan(1);
    assert!(sim.state.is_viewing() && sim.state.is_scanning());
    sim.scroll_to(0, 100, 16);
    let summary = sim.state.end_scan().unwrap();
    assert_eq!((summary.scan_id, summary.segments), (1, 1));
    assert_eq!(sim.state.mode(), Mode::Paused);
    let reads = sim.src.reads;
    sim.scroll_to(500, 100, 4);
    assert_eq!(sim.src.reads, reads, "PAUSED again after the scan");

    // From VIEWING: back to VIEWING.
    sim.state.resume();
    sim.state.start_scan(2);
    sim.state.end_scan();
    assert_eq!(sim.state.mode(), Mode::Viewing);
}

#[wasm_bindgen_test]
fn scan_stops_at_the_segment_cap() {
    let page = Page::new(3);
    let settings = CaptureSettings { max_scan_segments: 3, ..CaptureSettings::default() };
    let mut sim = Sim::new(&page, settings);
    sim.start_at_top(1);
    for stop in [600, 1200, 1800, 2400] {
        sim.scroll_to(stop, 100, 2);
    }
    assert_eq!(sim.segments.len(), 3);
    assert!(sim.state.scan_full());
}

#[wasm_bindgen_test]
fn a_long_scan_suggests_printing_to_pdf_once() {
    let page = Page::new(5);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.start_at_top(1);
    for stop in [500, 1000, 1500, 2000, 2500, 3000] {
        sim.scroll_to(stop, 100, 3);
    }
    assert_eq!(sim.segments.len(), 7);
    let suggestions = sim.cues.iter().filter(|c| **c == ScanCue::SuggestPdf).count();
    assert_eq!(suggestions, 1, "cues: {:?}", sim.cues);
    let at = sim.cues.iter().position(|c| *c == ScanCue::SuggestPdf).unwrap();
    assert_eq!(sim.cues[at - 1], ScanCue::Captured { segments: 5 }, "right after the sixth segment");
    assert!(ScanCue::SuggestPdf.phrase().contains("PDF"));
}
