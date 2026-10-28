//! Page-scan support: scroll-offset and gap estimation between consecutive
//! scan segments, and per-scan progress. Pure Rust, no browser APIs, so it is
//! testable natively and under `wasm-bindgen-test`.
//!
//! Offsets are estimated on the 65x256 grayscale thumbnails the capture loop
//! already takes: for each candidate vertical shift, compare the overlapping
//! rows of the previous and current thumbnails and keep the best match.

use crate::capture::{DHash, THUMB_H, THUMB_W};

/// Shifts with fewer overlapping rows than this aren't considered.
pub const MIN_OVERLAP_ROWS: usize = 24;
/// A row counts as textured when its luminance standard deviation exceeds this.
const TEXTURE_STDDEV: f32 = 6.0;
/// Shifts need at least this many textured rows in the overlap to be scored.
const MIN_TEXTURED_ROWS: usize = 12;
/// Fraction of best-matching rows averaged per shift; the rest are dropped so
/// sticky headers and small animations don't spoil the match.
const TRIM_KEEP: f32 = 0.8;
/// Highest trimmed mean absolute difference (0..255) that counts as a match.
const MATCH_THRESHOLD: f32 = 8.0;
/// A textured row this close to the same row in the other frame didn't move:
/// it's sticky page chrome (a fixed header, footer, or toolbar).
const STATIC_ROW_DIFF: f32 = 3.0;

/// Result of comparing a segment with the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offset {
    /// Scrolled this many thumbnail rows (positive = down).
    Rows(i32),
    /// The two segments don't overlap: part of the page may be missing.
    Gap,
    /// Too little texture to tell (e.g. mostly blank).
    Unknown,
}

fn row(thumb: &[u8], y: usize) -> &[u8] {
    let w = THUMB_W as usize;
    &thumb[y * w..(y + 1) * w]
}

fn textured_rows(thumb: &[u8]) -> Vec<bool> {
    (0..THUMB_H as usize)
        .map(|y| {
            let r = row(thumb, y);
            let mean = r.iter().map(|&p| p as f32).sum::<f32>() / r.len() as f32;
            let var = r.iter().map(|&p| (p as f32 - mean).powi(2)).sum::<f32>() / r.len() as f32;
            var.sqrt() > TEXTURE_STDDEV
        })
        .collect()
}

fn row_diff(a: &[u8], b: &[u8]) -> f32 {
    a.iter().zip(b).map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs()).sum::<u32>() as f32 / a.len() as f32
}

/// Estimates how far the page scrolled between `prev` and `cur` (both 65x256
/// grayscale thumbnails).
pub fn estimate_offset(prev: &[u8], cur: &[u8]) -> Offset {
    let h = THUMB_H as usize;
    let tex_prev = textured_rows(prev);
    let tex_cur = textured_rows(cur);
    // Sticky chrome sits at the same place in both frames and would otherwise
    // vote for "no scroll"; leave it out of the matching.
    let fixed: Vec<bool> = (0..h)
        .map(|y| tex_cur[y] && tex_prev[y] && row_diff(row(cur, y), row(prev, y)) < STATIC_ROW_DIFF)
        .collect();
    let max_shift = (h - MIN_OVERLAP_ROWS) as i32;

    let mut best: Option<(f32, i32)> = None;
    let mut errs = Vec::with_capacity(h);
    for s in -max_shift..=max_shift {
        errs.clear();
        // Content that was at prev row y + s is at cur row y after scrolling s.
        for y in 0..h {
            let py = y as i32 + s;
            if py < 0 || py >= h as i32 {
                continue;
            }
            let py = py as usize;
            if fixed[y] || fixed[py] || !(tex_cur[y] || tex_prev[py]) {
                continue;
            }
            errs.push(row_diff(row(cur, y), row(prev, py)));
        }
        if errs.len() < MIN_TEXTURED_ROWS {
            continue;
        }
        errs.sort_by(|a, b| a.total_cmp(b));
        let keep = ((errs.len() as f32 * TRIM_KEEP).ceil() as usize).max(1);
        let score = errs[..keep].iter().sum::<f32>() / keep as f32;
        let better = match best {
            None => true,
            Some((b, bs)) => score < b - 0.01 || ((score - b).abs() <= 0.01 && s.abs() < bs.abs()),
        };
        if better {
            best = Some((score, s));
        }
    }
    match best {
        None => Offset::Unknown,
        Some((score, s)) if score <= MATCH_THRESHOLD => Offset::Rows(s),
        Some(_) => Offset::Gap,
    }
}

/// Converts a thumbnail-row offset into pixels of a frame `frame_h` tall.
pub fn rows_to_px(rows: i32, frame_h: u32) -> i32 {
    (rows as f64 * frame_h as f64 / THUMB_H as f64).round() as i32
}

/// Where one emitted scan segment sits relative to the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentInfo {
    pub scan_id: u32,
    pub segment_index: u16,
    /// Thumbnail rows scrolled since the previous segment, when known.
    pub offset_rows: Option<i32>,
    pub gap_before: bool,
}

/// Summary of a finished (or cancelled) scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanSummary {
    pub scan_id: u32,
    pub segments: u32,
    pub gaps: u32,
    /// Achieved sampling rate; low when the browser throttled a background tab.
    pub samples_per_sec: f64,
}

/// Progress of the scan in flight.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub scan_id: u32,
    pub segments: u32,
    pub gaps: u32,
    last_segment: Option<(DHash, Vec<u8>)>,
    samples: u32,
    first_sample_ms: Option<f64>,
    last_sample_ms: f64,
}

impl ScanProgress {
    pub fn new(scan_id: u32) -> Self {
        Self {
            scan_id,
            segments: 0,
            gaps: 0,
            last_segment: None,
            samples: 0,
            first_sample_ms: None,
            last_sample_ms: 0.0,
        }
    }

    pub fn note_sample(&mut self, now_ms: f64) {
        self.samples += 1;
        self.first_sample_ms.get_or_insert(now_ms);
        self.last_sample_ms = now_ms;
    }

    /// Decides whether a settled sample becomes the next segment.
    pub fn consider(&mut self, hash: DHash, thumb: &[u8], change_threshold_bits: u32) -> Option<SegmentInfo> {
        let (offset_rows, gap_before) = match &self.last_segment {
            None => (None, false),
            Some((last_hash, _)) if last_hash.distance(&hash) < change_threshold_bits => return None,
            Some((_, last_thumb)) => match estimate_offset(last_thumb, thumb) {
                Offset::Rows(r) => (Some(r), false),
                Offset::Gap => (None, true),
                Offset::Unknown => (None, false),
            },
        };
        let info = SegmentInfo {
            scan_id: self.scan_id,
            segment_index: self.segments as u16,
            offset_rows,
            gap_before,
        };
        self.segments += 1;
        if gap_before {
            self.gaps += 1;
        }
        self.last_segment = Some((hash, thumb.to_vec()));
        Some(info)
    }

    pub fn summary(&self) -> ScanSummary {
        let span_s = self.first_sample_ms.map_or(0.0, |f| (self.last_sample_ms - f) / 1000.0);
        let samples_per_sec = if span_s > 0.0 { (self.samples.saturating_sub(1)) as f64 / span_s } else { 0.0 };
        ScanSummary { scan_id: self.scan_id, segments: self.segments, gaps: self.gaps, samples_per_sec }
    }
}
