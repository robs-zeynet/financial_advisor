//! Acceptance test 11: the page-scan state machine against a synthetic tall
//! page scrolled to known offsets.
//!
//! Run headless with: `wasm-pack test --headless --chrome crates/client`

use client::capture::{CaptureState, Mode, ThumbSource, THUMB_H, THUMB_W};
use client::scan::{estimate_offset, rows_to_px, Offset, SegmentInfo};
use protocol::CaptureSettings;
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
}

impl<'a> Sim<'a> {
    fn new(page: &'a Page, settings: CaptureSettings) -> Self {
        Self {
            state: CaptureState::new(settings),
            src: Scroller { page, y: 0, reads: 0 },
            now: 0.0,
            segments: Vec::new(),
            live_frames: 0,
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
fn one_segment_per_settled_position_with_accurate_offsets() {
    let page = Page::new(0x5eed);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.state.start_scan(1);
    let stops = [0usize, 700, 1400, 2100, 2800, PAGE_H - VIEW_H];
    sim.scroll_to(stops[0], 100, 3);
    for &stop in &stops[1..] {
        sim.scroll_to(stop, 100, 3);
    }
    assert_eq!(sim.segments.len(), stops.len(), "one segment per settled position: {:?}", sim.segments);
    assert_eq!(sim.live_frames, 0);
    assert!(sim.segments.iter().all(|s| s.scan_id == 1 && !s.gap_before));
    for (i, seg) in sim.segments.iter().enumerate() {
        assert_eq!(seg.segment_index as usize, i);
        if i == 0 {
            assert_eq!(seg.offset_rows, None);
            continue;
        }
        let truth = (stops[i] - stops[i - 1]) as f64;
        let est = rows_to_px(seg.offset_rows.expect("offset known"), VIEW_H as u32) as f64;
        assert!((est - truth).abs() <= truth * 0.05, "segment {i}: estimated {est}, true {truth}");
    }
}

#[wasm_bindgen_test]
fn nothing_is_emitted_mid_scroll() {
    let page = Page::new(7);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.state.start_scan(1);
    sim.tick(); // first sample, unsettled
    // Keep scrolling without ever stopping.
    for _ in 0..25 {
        sim.src.y += 120;
        sim.tick();
    }
    assert!(sim.segments.is_empty(), "no segment while the view keeps moving");
    sim.tick();
    assert_eq!(sim.segments.len(), 1, "one segment once it settles");
}

#[wasm_bindgen_test]
fn jump_larger_than_a_screen_is_a_gap() {
    let page = Page::new(42);
    let mut sim = Sim::new(&page, CaptureSettings::default());
    sim.state.start_scan(3);
    sim.scroll_to(0, 100, 2);
    sim.src.y = 2200; // more than VIEW_H below: no overlap
    sim.tick();
    sim.tick();
    sim.scroll_to(2600, 100, 2); // normal overlap again
    let gaps: Vec<bool> = sim.segments.iter().map(|s| s.gap_before).collect();
    assert_eq!(gaps, vec![false, true, false]);
    assert_eq!(sim.segments[1].offset_rows, None);
    assert_eq!(sim.state.scan_progress().unwrap().gaps, 1);
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

    sim.state.start_scan(5);
    sim.scroll_to(0, 100, 2);
    sim.scroll_to(800, 100, 2);
    assert_eq!(sim.segments.len(), 2);

    let cancelled = sim.state.pause().expect("pause cancels the scan");
    assert_eq!(cancelled.segments, 2);
    assert_eq!(sim.state.mode(), Mode::Paused);
    assert!(sim.state.scan_progress().is_none());
    let reads = sim.src.reads;
    sim.scroll_to(1600, 100, 4);
    assert_eq!(sim.segments.len(), 2, "nothing emitted after an explicit pause");
    assert_eq!(sim.src.reads, reads, "nothing read after an explicit pause");
}

#[wasm_bindgen_test]
fn scan_returns_to_the_previous_state() {
    let page = Page::new(2);
    let mut sim = Sim::new(&page, CaptureSettings::default());

    // From PAUSED: back to PAUSED.
    sim.state.start_scan(1);
    assert!(sim.state.is_viewing() && sim.state.is_scanning());
    sim.scroll_to(0, 100, 2);
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
    sim.state.start_scan(1);
    for stop in [0, 600, 1200, 1800, 2400] {
        sim.scroll_to(stop, 100, 2);
    }
    assert_eq!(sim.segments.len(), 3);
    assert!(sim.state.scan_full());
}
