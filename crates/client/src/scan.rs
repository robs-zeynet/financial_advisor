//! Page-scan support: scroll-offset and gap estimation between consecutive
//! scan segments, and per-scan progress. Pure Rust, no browser APIs, so it is
//! testable natively and under `wasm-bindgen-test`.
//!
//! Offsets are estimated on the 65x256 grayscale thumbnails the capture loop
//! already takes: for each candidate vertical shift, compare the overlapping
//! rows of the previous and current thumbnails and keep the best match.

use protocol::ScanCue;

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
    /// Segments kept despite a gap (the user held still after being asked to
    /// scroll back).
    pub gaps: u32,
    /// Achieved sampling rate; low when the browser throttled a background tab.
    pub samples_per_sec: f64,
}

// Guidance tuning, in thumbnail rows (256 = one screen) and milliseconds.

/// New content needed for a segment: about a third of a screen. Smaller
/// steps (or lingering) don't make near-duplicate segments.
pub const MIN_ADVANCE_ROWS: i32 = 90;
/// While scrolling, ask the user to stop at a bit over half a screen.
pub const STOP_AT_ROWS: i32 = 140;
/// Time to hear "please scroll to the top" before the first capture.
pub const FIRST_CAPTURE_DELAY_MS: f64 = 3_000.0;
/// The first capture also needs the view to have been still this long.
pub const FIRST_DWELL_MS: f64 = 1_000.0;
/// Stopped short of a full step for this long: "a little further, please".
pub const KEEP_GOING_AFTER_MS: f64 = 1_200.0;
/// Stopped short for this long: keep it anyway (usually the bottom).
pub const ACCEPT_SHORT_AFTER_MS: f64 = 3_000.0;
/// Held still past a gap this long after being asked to scroll back: keep
/// it, marked as a gap.
pub const ACCEPT_GAP_AFTER_MS: f64 = 6_000.0;
/// No movement this long after a capture: a gentle nudge.
pub const STILL_THERE_AFTER_MS: f64 = 7_000.0;
/// On a long page, suggest printing to PDF once this many segments are in.
pub const SUGGEST_PDF_AT_SEGMENTS: u32 = 6;
/// The same cue isn't repeated sooner than this...
const REPEAT_CUE_MS: f64 = 4_000.0;
/// ...and routine cues are at least this far apart.
const MIN_CUE_GAP_MS: f64 = 1_500.0;
/// Speed is only judged when samples are this close together (a throttled
/// background tab samples too slowly to tell).
const FAST_SAMPLE_MS: f64 = 400.0;

/// What one sample led to: maybe a segment, maybe something to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observation {
    pub segment: Option<SegmentInfo>,
    pub cue: Option<ScanCue>,
}

/// Progress of the scan in flight, and the guidance state behind it.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub scan_id: u32,
    pub segments: u32,
    pub gaps: u32,
    last_segment: Option<(DHash, Vec<u8>)>,
    last_capture_ms: f64,
    started_ms: Option<f64>,
    /// The previous sample, for judging scroll speed.
    prev_sample: Option<(f64, Vec<u8>)>,
    /// When the view last became still.
    settled_since: Option<f64>,
    /// "Stop" was said since the last capture.
    stop_cued: bool,
    /// The user scrolled past the last segment and was asked to come back.
    skipped: bool,
    still_cued: bool,
    last_cue: Option<(ScanCue, f64)>,
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
            last_capture_ms: 0.0,
            started_ms: None,
            prev_sample: None,
            settled_since: None,
            stop_cued: false,
            skipped: false,
            still_cued: false,
            last_cue: None,
            samples: 0,
            first_sample_ms: None,
            last_sample_ms: 0.0,
        }
    }

    fn note_sample(&mut self, now_ms: f64) {
        self.samples += 1;
        self.first_sample_ms.get_or_insert(now_ms);
        self.last_sample_ms = now_ms;
    }

    /// Rate-limits cues. `urgent` cues (stop, skipped, captured) only skip the
    /// repeat check for a different cue.
    fn cue(&mut self, now: f64, cue: ScanCue, urgent: bool) -> Option<ScanCue> {
        if let Some((last, at)) = self.last_cue {
            let same = std::mem::discriminant(&last) == std::mem::discriminant(&cue);
            if same && now - at < REPEAT_CUE_MS {
                return None;
            }
            if !urgent && now - at < MIN_CUE_GAP_MS {
                return None;
            }
        }
        self.last_cue = Some((cue, now));
        Some(cue)
    }

    fn capture(&mut self, now: f64, hash: DHash, thumb: &[u8], offset_rows: Option<i32>, gap_before: bool) -> SegmentInfo {
        let info = SegmentInfo { scan_id: self.scan_id, segment_index: self.segments as u16, offset_rows, gap_before };
        self.segments += 1;
        if gap_before {
            self.gaps += 1;
        }
        self.last_segment = Some((hash, thumb.to_vec()));
        self.last_capture_ms = now;
        self.stop_cued = false;
        self.skipped = false;
        self.still_cued = false;
        info
    }

    /// Feeds one sample. Decides whether it becomes the next segment and
    /// what, if anything, to tell the user.
    pub fn observe(&mut self, now: f64, hash: DHash, thumb: &[u8], settled: bool, change_threshold_bits: u32) -> Observation {
        self.note_sample(now);
        let started = *self.started_ms.get_or_insert(now);
        let prev = self.prev_sample.replace((now, thumb.to_vec()));
        if settled {
            self.settled_since.get_or_insert(now);
        } else {
            self.settled_since = None;
        }
        let dwell = self.settled_since.map_or(0.0, |t| now - t);

        // First segment: give the user time to get to the top and hold still.
        let Some((last_hash, last_thumb)) = self.last_segment.clone() else {
            if settled && now - started >= FIRST_CAPTURE_DELAY_MS && dwell >= FIRST_DWELL_MS {
                let segment = self.capture(now, hash, thumb, None, false);
                let cue = self.cue(now, ScanCue::Captured { segments: self.segments }, true);
                return Observation { segment: Some(segment), cue };
            }
            return Observation::default();
        };

        if !settled {
            // Moving: slow down if consecutive samples don't even overlap; say
            // stop at about half a screen; past a full screen, ask to go back.
            let too_fast = matches!(&prev, Some((t, p)) if now - t <= FAST_SAMPLE_MS && estimate_offset(p, thumb) == Offset::Gap);
            let cue = if too_fast {
                self.cue(now, ScanCue::TooFast, true)
            } else {
                match estimate_offset(&last_thumb, thumb) {
                    Offset::Rows(r) if r >= STOP_AT_ROWS && !self.stop_cued => {
                        self.stop_cued = true;
                        self.cue(now, ScanCue::Stop, true)
                    }
                    Offset::Gap if !self.skipped => {
                        self.skipped = true;
                        self.cue(now, ScanCue::TooFar, true)
                    }
                    _ => None,
                }
            };
            return Observation { segment: None, cue };
        }

        // Still, and nothing new since the last capture.
        if last_hash.distance(&hash) < change_threshold_bits {
            let cue = if !self.still_cued && now - self.last_capture_ms >= STILL_THERE_AFTER_MS {
                self.still_cued = true;
                self.cue(now, ScanCue::StillThere, false)
            } else {
                None
            };
            return Observation { segment: None, cue };
        }

        match estimate_offset(&last_thumb, thumb) {
            Offset::Rows(r) if r >= MIN_ADVANCE_ROWS => {
                let segment = self.capture(now, hash, thumb, Some(r), false);
                let cue = if self.segments == SUGGEST_PDF_AT_SEGMENTS {
                    ScanCue::SuggestPdf
                } else {
                    ScanCue::Captured { segments: self.segments }
                };
                let cue = self.cue(now, cue, true);
                Observation { segment: Some(segment), cue }
            }
            Offset::Rows(r) if r > 0 && dwell >= ACCEPT_SHORT_AFTER_MS => {
                // A short last step, held: usually the bottom of the page.
                let segment = self.capture(now, hash, thumb, Some(r), false);
                let cue = self.cue(now, ScanCue::MaybeBottom, true);
                Observation { segment: Some(segment), cue }
            }
            Offset::Rows(_) if dwell >= KEEP_GOING_AFTER_MS => {
                Observation { segment: None, cue: self.cue(now, ScanCue::KeepGoing, false) }
            }
            Offset::Rows(_) => Observation::default(),
            Offset::Gap if self.skipped && dwell >= ACCEPT_GAP_AFTER_MS => {
                // They held still past the gap; keep it, flagged.
                let segment = self.capture(now, hash, thumb, None, true);
                let cue = self.cue(now, ScanCue::Captured { segments: self.segments }, true);
                Observation { segment: Some(segment), cue }
            }
            Offset::Gap => {
                self.skipped = true;
                Observation { segment: None, cue: self.cue(now, ScanCue::TooFar, true) }
            }
            Offset::Unknown => {
                let segment = self.capture(now, hash, thumb, None, false);
                let cue = self.cue(now, ScanCue::Captured { segments: self.segments }, true);
                Observation { segment: Some(segment), cue }
            }
        }
    }

    pub fn summary(&self) -> ScanSummary {
        let span_s = self.first_sample_ms.map_or(0.0, |f| (self.last_sample_ms - f) / 1000.0);
        let samples_per_sec = if span_s > 0.0 { (self.samples.saturating_sub(1)) as f64 / span_s } else { 0.0 };
        ScanSummary { scan_id: self.scan_id, segments: self.segments, gaps: self.gaps, samples_per_sec }
    }
}
