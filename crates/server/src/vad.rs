//! Voice activity detection: decides when the user starts and stops talking.
//!
//! An energy detector with an adaptive noise floor, which the spec allows as
//! a stopgap for Silero VAD. Audio arrives as 16 kHz PCM16 in arbitrary chunk
//! sizes; it's analyzed in 20 ms frames. Audio lives only in RAM and is
//! handed to speech-to-text when an utterance ends; nothing is stored.

use std::collections::VecDeque;

use protocol::MIC_SAMPLE_RATE;

const FRAME: usize = (MIC_SAMPLE_RATE as usize) / 50; // 20 ms

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    /// Speech must exceed the noise floor by this much...
    pub margin_db: f32,
    /// ...and be at least this loud (dBFS).
    pub min_speech_dbfs: f32,
    /// Extra margin while the advisor is talking, so its own voice leaking
    /// past echo cancellation doesn't count as the user barging in.
    pub echo_guard_db: f32,
    /// Consecutive speech frames before an utterance starts.
    pub start_frames: usize,
    /// Silence that ends an utterance.
    pub end_silence_ms: u32,
    /// Shorter utterances are dropped as clicks and bumps.
    pub min_utterance_ms: u32,
    /// Longer utterances are cut here.
    pub max_utterance_ms: u32,
    /// Audio kept from just before speech was detected.
    pub preroll_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            margin_db: 12.0,
            min_speech_dbfs: -45.0,
            echo_guard_db: 8.0,
            start_frames: 4,
            end_silence_ms: 700,
            min_utterance_ms: 250,
            max_utterance_ms: 30_000,
            preroll_ms: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VadEvent {
    SpeechStart,
    /// The finished utterance (pre-roll included).
    SpeechEnd(Vec<i16>),
    /// Speech started but was too short to transcribe.
    Discarded,
}

pub struct Vad {
    cfg: VadConfig,
    noise_db: f32,
    partial: Vec<i16>,
    preroll: VecDeque<i16>,
    utterance: Vec<i16>,
    in_speech: bool,
    /// Push-to-talk holds the utterance open regardless of silence.
    forced: bool,
    speech_run: usize,
    silence_frames: usize,
    /// Pre-roll samples at the start of the current utterance.
    preroll_used: usize,
    echo_guard: bool,
}

fn ms_to_samples(ms: u32) -> usize {
    (ms as usize * MIC_SAMPLE_RATE as usize) / 1000
}

/// RMS level of a frame in dBFS.
pub fn dbfs(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return -120.0;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
    let rms = (sum / frame.len() as f64).sqrt();
    (20.0 * rms.max(1e-6).log10()) as f32
}

impl Vad {
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            noise_db: -60.0,
            partial: Vec::with_capacity(FRAME),
            preroll: VecDeque::new(),
            utterance: Vec::new(),
            in_speech: false,
            forced: false,
            speech_run: 0,
            silence_frames: 0,
            preroll_used: 0,
            echo_guard: false,
        }
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// Raises the speech threshold while the advisor is speaking.
    pub fn set_echo_guard(&mut self, on: bool) {
        self.echo_guard = on;
    }

    /// Drops everything (mic off, hang-up).
    pub fn reset(&mut self) {
        self.partial.clear();
        self.preroll.clear();
        self.utterance.clear();
        self.in_speech = false;
        self.forced = false;
        self.speech_run = 0;
        self.silence_frames = 0;
    }

    /// Push-to-talk pressed: start an utterance now.
    pub fn force_start(&mut self) -> Option<VadEvent> {
        self.forced = true;
        if self.in_speech {
            return None;
        }
        self.begin();
        Some(VadEvent::SpeechStart)
    }

    /// Push-to-talk released: end the utterance now.
    pub fn force_end(&mut self) -> Option<VadEvent> {
        self.forced = false;
        if !self.in_speech {
            return None;
        }
        // Include any sub-frame remainder.
        let rest = std::mem::take(&mut self.partial);
        self.utterance.extend_from_slice(&rest);
        Some(self.finish())
    }

    pub fn feed(&mut self, samples: &[i16]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        self.partial.extend_from_slice(samples);
        while self.partial.len() >= FRAME {
            let frame: Vec<i16> = self.partial.drain(..FRAME).collect();
            if let Some(e) = self.frame(&frame) {
                events.push(e);
            }
        }
        events
    }

    fn threshold(&self) -> f32 {
        let guard = if self.echo_guard { self.cfg.echo_guard_db } else { 0.0 };
        (self.noise_db + self.cfg.margin_db).max(self.cfg.min_speech_dbfs) + guard
    }

    fn begin(&mut self) {
        self.in_speech = true;
        self.silence_frames = 0;
        self.utterance.clear();
        self.preroll_used = self.preroll.len();
        self.utterance.extend(self.preroll.drain(..));
    }

    fn finish(&mut self) -> VadEvent {
        let trailing = self.silence_frames * FRAME;
        self.in_speech = false;
        self.speech_run = 0;
        self.silence_frames = 0;
        let utterance = std::mem::take(&mut self.utterance);
        // Pre-roll and trailing silence don't count toward the minimum length.
        let voiced = utterance.len().saturating_sub(self.preroll_used + trailing);
        if voiced < ms_to_samples(self.cfg.min_utterance_ms) {
            VadEvent::Discarded
        } else {
            VadEvent::SpeechEnd(utterance)
        }
    }

    fn frame(&mut self, frame: &[i16]) -> Option<VadEvent> {
        let db = dbfs(frame);
        let loud = db > self.threshold();
        if !self.in_speech {
            // Track the noise floor between utterances: fall fast, rise slowly.
            if db < self.noise_db {
                self.noise_db = 0.8 * self.noise_db + 0.2 * db;
            } else if !loud {
                self.noise_db = 0.98 * self.noise_db + 0.02 * db;
            }
            self.noise_db = self.noise_db.clamp(-90.0, -25.0);

            self.preroll.extend(frame.iter().copied());
            let max_pre = ms_to_samples(self.cfg.preroll_ms);
            while self.preroll.len() > max_pre {
                self.preroll.pop_front();
            }
            self.speech_run = if loud { self.speech_run + 1 } else { 0 };
            if self.speech_run >= self.cfg.start_frames {
                self.begin();
                return Some(VadEvent::SpeechStart);
            }
            return None;
        }

        self.utterance.extend_from_slice(frame);
        if self.utterance.len() >= ms_to_samples(self.cfg.max_utterance_ms) {
            self.forced = false;
            return Some(self.finish());
        }
        if self.forced {
            return None;
        }
        self.silence_frames = if loud { 0 } else { self.silence_frames + 1 };
        if self.silence_frames * 20 >= self.cfg.end_silence_ms as usize {
            return Some(self.finish());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ms` of a 220 Hz tone at `db` dBFS plus faint noise.
    fn tone(ms: u32, db: f32, seed: &mut u32) -> Vec<i16> {
        let amp = 10f32.powf(db / 20.0) * 32768.0 * std::f32::consts::SQRT_2;
        (0..ms_to_samples(ms))
            .map(|i| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let noise = ((*seed >> 16) as f32 / 32768.0 - 1.0) * 30.0;
                let t = i as f32 / MIC_SAMPLE_RATE as f32;
                (amp * (2.0 * std::f32::consts::PI * 220.0 * t).sin() + noise) as i16
            })
            .collect()
    }

    fn silence(ms: u32, seed: &mut u32) -> Vec<i16> {
        tone(ms, -120.0, seed)
    }

    #[test]
    fn detects_an_utterance_with_preroll() {
        let mut s = 1;
        let mut vad = Vad::new(VadConfig::default());
        assert!(vad.feed(&silence(1000, &mut s)).is_empty());
        let start = vad.feed(&tone(1500, -20.0, &mut s));
        assert_eq!(start, vec![VadEvent::SpeechStart]);
        let mut events = Vec::new();
        // Feed silence in odd-sized chunks.
        for chunk in silence(1000, &mut s).chunks(137) {
            events.extend(vad.feed(chunk));
        }
        match events.as_slice() {
            [VadEvent::SpeechEnd(u)] => {
                let ms = u.len() * 1000 / MIC_SAMPLE_RATE as usize;
                // 1500 ms of speech + up to 300 ms pre-roll + ~700 ms trailing silence.
                assert!((2000..=2600).contains(&ms), "utterance length {ms} ms");
            }
            other => panic!("expected one SpeechEnd, got {other:?}"),
        }
    }

    #[test]
    fn short_blips_are_discarded_and_quiet_noise_ignored() {
        let mut s = 2;
        let mut vad = Vad::new(VadConfig::default());
        let mut events = vad.feed(&silence(500, &mut s));
        events.extend(vad.feed(&tone(120, -15.0, &mut s)));
        events.extend(vad.feed(&silence(1000, &mut s)));
        assert_eq!(events, vec![VadEvent::SpeechStart, VadEvent::Discarded]);
        // Steady low noise never triggers.
        assert!(vad.feed(&tone(3000, -55.0, &mut s)).is_empty());
    }

    #[test]
    fn echo_guard_raises_the_threshold() {
        let mut s = 3;
        let mut vad = Vad::new(VadConfig::default());
        vad.feed(&silence(500, &mut s));
        vad.set_echo_guard(true);
        assert!(vad.feed(&tone(600, -42.0, &mut s)).is_empty(), "quiet leak ignored while speaking");
        assert_eq!(vad.feed(&tone(300, -12.0, &mut s)), vec![VadEvent::SpeechStart], "real barge-in still heard");
    }

    #[test]
    fn push_to_talk_holds_through_silence() {
        let mut s = 4;
        let mut vad = Vad::new(VadConfig::default());
        assert_eq!(vad.force_start(), Some(VadEvent::SpeechStart));
        assert!(vad.feed(&tone(400, -20.0, &mut s)).is_empty());
        assert!(vad.feed(&silence(2000, &mut s)).is_empty(), "silence doesn't end a PTT utterance");
        assert!(matches!(vad.force_end(), Some(VadEvent::SpeechEnd(_))));
        assert_eq!(vad.force_end(), None);
    }
}
