//! In-memory screen frame buffers (P3).
//!
//! Frames live only here, in RAM, in two buffers:
//! - [`FrameRing`]: the latest frame plus up to four recent *distinct* frames
//! - [`ScanBuffer`]: the segments of the most recent page scan
//!
//! This module must never touch the filesystem; a test
//! (`tests/invariants.rs`, `frame_ring_has_no_disk_io`) scans this source file
//! to enforce that. Callers wipe the buffers on pause and hang-up, and `Drop`
//! wipes them on exit.

use std::collections::VecDeque;

use protocol::FrameHeader;
use sha2::{Digest, Sha256};

/// Recent distinct frames kept in addition to the latest one.
pub const MAX_DISTINCT: usize = 4;
/// Frames `view_screen` returns per call: the latest plus up to 3 earlier.
pub const MAX_PER_VIEW: usize = 4;

pub struct Frame {
    pub seq: u64,
    pub captured_at_ms: u64,
    pub width: u16,
    pub height: u16,
    pub sha256: String,
    pub jpeg: Vec<u8>,
}

/// What may be logged about a frame: never the pixels.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FrameMeta {
    pub captured_at_ms: u64,
    pub width: u16,
    pub height: u16,
    pub sha256: String,
}

impl Frame {
    fn new(seq: u64, header: &FrameHeader, jpeg: Vec<u8>) -> Self {
        Self {
            seq,
            captured_at_ms: header.captured_at_ms,
            width: header.width,
            height: header.height,
            sha256: hex(&Sha256::digest(&jpeg)),
            jpeg,
        }
    }

    pub fn meta(&self) -> FrameMeta {
        FrameMeta {
            captured_at_ms: self.captured_at_ms,
            width: self.width,
            height: self.height,
            sha256: self.sha256.clone(),
        }
    }

    fn wipe(&mut self) {
        self.jpeg.fill(0);
        self.jpeg.clear();
    }
}

impl std::fmt::Debug for Frame {
    // Keep pixel bytes out of any debug output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("seq", &self.seq)
            .field("meta", &self.meta())
            .finish()
    }
}

#[derive(Default)]
pub struct FrameRing {
    next_seq: u64,
    latest: Option<Frame>,
    /// Older distinct frames, oldest first. Never contains `latest`.
    distinct: VecDeque<Frame>,
    /// Highest seq already returned by `take_for_view`.
    last_viewed_seq: u64,
}

impl FrameRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a frame and returns its metadata.
    ///
    /// A changed frame pushes the previous latest into the distinct history; a
    /// keepalive (unchanged) frame just replaces the latest.
    pub fn push(&mut self, header: FrameHeader, jpeg: Vec<u8>) -> FrameMeta {
        self.next_seq += 1;
        let frame = Frame::new(self.next_seq, &header, jpeg);
        let meta = frame.meta();
        if let Some(mut prev) = self.latest.replace(frame) {
            if header.changed {
                self.distinct.push_back(prev);
                while self.distinct.len() > MAX_DISTINCT {
                    if let Some(mut old) = self.distinct.pop_front() {
                        old.wipe();
                    }
                }
            } else {
                prev.wipe();
            }
        }
        meta
    }

    /// Frames for one `view_screen` call, oldest first: up to three distinct
    /// frames captured since the previous call, then the latest frame.
    pub fn take_for_view(&mut self) -> Vec<&Frame> {
        let Some(latest) = self.latest.as_ref() else {
            return Vec::new();
        };
        let since = self.last_viewed_seq;
        self.last_viewed_seq = latest.seq;
        let earlier: Vec<&Frame> = self.distinct.iter().filter(|f| f.seq > since).collect();
        let skip = earlier.len().saturating_sub(MAX_PER_VIEW - 1);
        earlier.into_iter().skip(skip).chain(std::iter::once(latest)).collect()
    }

    pub fn len(&self) -> usize {
        self.distinct.len() + usize::from(self.latest.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.latest.is_none()
    }

    /// Zeroes and drops every frame.
    pub fn wipe(&mut self) {
        if let Some(mut f) = self.latest.take() {
            f.wipe();
        }
        for mut f in self.distinct.drain(..) {
            f.wipe();
        }
    }
}

impl Drop for FrameRing {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// One segment of a page scan.
#[derive(Debug)]
pub struct ScanSegment {
    pub frame: Frame,
    pub segment_index: u16,
    /// Estimated scroll from the previous segment, in this frame's pixels.
    pub offset_px: Option<i32>,
    pub gap_before: bool,
}

/// Why a scan segment was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanReject {
    /// No scan is running, or the segment belongs to a different scan.
    NotActive,
    /// The scan already holds its maximum number of segments.
    Full,
}

/// The segments of the most recent page scan, ordered top to bottom.
#[derive(Default)]
pub struct ScanBuffer {
    scan_id: u32,
    active: bool,
    viewed: bool,
    next_seq: u64,
    segments: Vec<ScanSegment>,
}

impl ScanBuffer {
    /// Starts a new scan, wiping the previous one.
    pub fn start(&mut self, scan_id: u32) {
        self.wipe();
        self.scan_id = scan_id;
        self.active = true;
    }

    /// Stores a segment of the active scan.
    pub fn push(&mut self, header: FrameHeader, jpeg: Vec<u8>, max_segments: usize) -> Result<FrameMeta, ScanReject> {
        if !self.active || header.scan_id != self.scan_id {
            return Err(ScanReject::NotActive);
        }
        if self.segments.len() >= max_segments {
            return Err(ScanReject::Full);
        }
        self.next_seq += 1;
        let segment = ScanSegment {
            frame: Frame::new(self.next_seq, &header, jpeg),
            segment_index: header.segment_index,
            offset_px: header.offset_px,
            gap_before: header.gap_before,
        };
        let meta = segment.frame.meta();
        let at = self.segments.partition_point(|s| s.segment_index <= segment.segment_index);
        self.segments.insert(at, segment);
        self.viewed = false;
        Ok(meta)
    }

    /// Ends the scan; its segments stay available until wiped.
    pub fn finish(&mut self) {
        self.active = false;
    }

    pub fn scan_id(&self) -> u32 {
        self.scan_id
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// 1-based numbers of segments preceded by a possible gap.
    pub fn gaps_before(&self) -> Vec<usize> {
        self.segments.iter().enumerate().filter(|(_, s)| s.gap_before).map(|(i, _)| i + 1).collect()
    }

    /// Holds segments the model hasn't looked at yet.
    pub fn has_unviewed(&self) -> bool {
        !self.viewed && !self.segments.is_empty()
    }

    /// All segments, top to bottom, marking the scan as viewed.
    pub fn take_for_view(&mut self) -> &[ScanSegment] {
        self.viewed = true;
        &self.segments
    }

    /// Zeroes and drops every segment and ends any active scan.
    pub fn wipe(&mut self) {
        for mut s in self.segments.drain(..) {
            s.frame.wipe();
        }
        self.active = false;
        self.viewed = false;
    }
}

impl Drop for ScanBuffer {
    fn drop(&mut self) {
        self.wipe();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(t: u64, changed: bool) -> FrameHeader {
        FrameHeader::live(t, 10, 10, changed)
    }

    fn times(frames: &[&Frame]) -> Vec<u64> {
        frames.iter().map(|f| f.captured_at_ms).collect()
    }

    #[test]
    fn keeps_latest_plus_four_distinct() {
        let mut r = FrameRing::new();
        for t in 1..=10 {
            r.push(h(t, true), vec![t as u8]);
        }
        assert_eq!(r.len(), 1 + MAX_DISTINCT);
    }

    #[test]
    fn keepalive_replaces_latest_without_growing_history() {
        let mut r = FrameRing::new();
        r.push(h(1, true), vec![1]);
        r.push(h(2, false), vec![1]);
        r.push(h(3, false), vec![1]);
        assert_eq!(r.len(), 1);
        assert_eq!(times(&r.take_for_view()), vec![3]);
    }

    #[test]
    fn view_returns_new_distinct_frames_then_latest() {
        let mut r = FrameRing::new();
        for t in 1..=6 {
            r.push(h(t, true), vec![t as u8]);
        }
        // Distinct history holds 2..=5; at most 3 earlier frames plus latest.
        assert_eq!(times(&r.take_for_view()), vec![3, 4, 5, 6]);
        // Nothing new since: just the latest again.
        assert_eq!(times(&r.take_for_view()), vec![6]);
        r.push(h(7, true), vec![7]);
        r.push(h(8, true), vec![8]);
        // 6 was already shown; 7 is new and distinct.
        assert_eq!(times(&r.take_for_view()), vec![7, 8]);
    }

    #[test]
    fn wipe_empties_everything() {
        let mut r = FrameRing::new();
        r.push(h(1, true), vec![0xFF, 0xD8]);
        r.push(h(2, true), vec![0xFF, 0xD8]);
        r.wipe();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.take_for_view().is_empty());
    }

    fn seg(scan_id: u32, index: u16, offset_px: Option<i32>, gap_before: bool) -> FrameHeader {
        FrameHeader { scan_id, segment_index: index, offset_px, gap_before, ..h(index as u64, true) }
    }

    #[test]
    fn scan_buffer_orders_caps_and_wipes() {
        let mut b = ScanBuffer::default();
        assert_eq!(b.push(seg(1, 0, None, false), vec![1], 3), Err(ScanReject::NotActive));
        b.start(1);
        b.push(seg(1, 1, Some(500), false), vec![2], 3).unwrap();
        b.push(seg(1, 0, None, false), vec![1], 3).unwrap();
        assert_eq!(b.push(seg(2, 2, None, false), vec![3], 3), Err(ScanReject::NotActive));
        b.push(seg(1, 2, None, true), vec![3], 3).unwrap();
        assert_eq!(b.push(seg(1, 3, Some(1), false), vec![4], 3), Err(ScanReject::Full));
        assert!(b.has_unviewed());
        let order: Vec<u16> = b.take_for_view().iter().map(|s| s.segment_index).collect();
        assert_eq!(order, vec![0, 1, 2]);
        assert!(!b.has_unviewed());
        assert_eq!(b.gaps_before(), vec![3]);

        b.finish();
        assert!(!b.is_active());
        assert_eq!(b.len(), 3, "finishing keeps the segments");
        assert_eq!(b.push(seg(1, 3, None, false), vec![4], 9), Err(ScanReject::NotActive));

        b.start(2);
        assert!(b.is_empty(), "a new scan wipes the old one");
        b.push(seg(2, 0, None, false), vec![9], 3).unwrap();
        b.wipe();
        assert!(b.is_empty() && !b.is_active());
    }

    #[test]
    fn meta_has_hash_not_pixels() {
        let mut r = FrameRing::new();
        let m = r.push(h(1, true), b"abc".to_vec());
        assert_eq!(
            m.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("jpeg"));
    }
}
