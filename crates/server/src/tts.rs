//! Text-to-speech: the `TtsEngine` trait with a Piper subprocess engine, the
//! sentence chunker that feeds it as text streams in, and the per-session
//! `Speaker` that sends audio to the browser and tracks what has been heard.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context};
use protocol::{ServerMsg, TtsHeader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::Outbound;

pub type TtsFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Vec<i16>>> + Send + 'a>>;

pub trait TtsEngine: Send + Sync {
    fn sample_rate(&self) -> u32;
    /// Synthesizes one sentence to mono PCM16 at `sample_rate()`.
    fn synthesize<'a>(&'a self, text: &'a str) -> TtsFuture<'a>;
}

/// Runs the local `piper` binary once per sentence, reading raw PCM from its
/// stdout. The process is killed if synthesis is cancelled.
pub struct PiperTts {
    bin: PathBuf,
    voice: PathBuf,
    sample_rate: u32,
}

impl PiperTts {
    pub fn new(bin: &Path, voice: &Path) -> anyhow::Result<Self> {
        if !bin.is_file() {
            bail!("piper binary not found at {}", bin.display());
        }
        let config_path = PathBuf::from(format!("{}.json", voice.display()));
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&config_path).with_context(|| format!("reading {}", config_path.display()))?,
        )?;
        let sample_rate = config["audio"]["sample_rate"].as_u64().context("voice config lacks audio.sample_rate")? as u32;
        Ok(Self { bin: bin.into(), voice: voice.into(), sample_rate })
    }
}

impl TtsEngine for PiperTts {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn synthesize<'a>(&'a self, text: &'a str) -> TtsFuture<'a> {
        Box::pin(async move {
            let mut child = tokio::process::Command::new(&self.bin)
                .arg("--model")
                .arg(&self.voice)
                .arg("--output-raw")
                .arg("--quiet")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("starting piper")?;
            let mut stdin = child.stdin.take().context("piper stdin")?;
            // Piper reads one utterance per line.
            let line = format!("{}\n", text.replace(['\n', '\r'], " "));
            stdin.write_all(line.as_bytes()).await?;
            drop(stdin);
            let mut raw = Vec::new();
            child.stdout.take().context("piper stdout")?.read_to_end(&mut raw).await?;
            let status = child.wait().await?;
            if !status.success() {
                bail!("piper exited with {status}");
            }
            Ok(raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
        })
    }
}

// ------------------------------------------------------------ chunking

/// Abbreviations whose period doesn't end a sentence.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "st", "vs", "etc", "e.g", "i.e", "u.s", "inc", "jr", "sr", "approx", "no", "co", "corp", "ltd",
];
/// Past this length, a sentence is split at a clause boundary.
const MAX_SENTENCE_CHARS: usize = 220;
const MIN_SPLIT_CHARS: usize = 100;

/// Splits streamed text into sentences for TTS. Offsets are byte positions
/// in the turn's full text, so what was heard can be mapped back to text.
#[derive(Default)]
pub struct SentenceChunker {
    text: String,
    /// Start of the text not yet emitted.
    emitted: usize,
}

impl SentenceChunker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds streamed text; returns each sentence completed, with its end offset.
    pub fn push(&mut self, delta: &str) -> Vec<(String, usize)> {
        self.text.push_str(delta);
        let mut out = Vec::new();
        while let Some(end) = self.next_boundary() {
            if let Some(s) = self.take(end) {
                out.push(s);
            }
        }
        out
    }

    /// Flushes whatever is left at the end of the turn.
    pub fn finish(&mut self) -> Option<(String, usize)> {
        self.take(self.text.len())
    }

    fn take(&mut self, end: usize) -> Option<(String, usize)> {
        let sentence = self.text[self.emitted..end].trim().to_string();
        self.emitted = end;
        (!sentence.is_empty()).then_some((sentence, end))
    }

    fn next_boundary(&self) -> Option<usize> {
        let pending = &self.text[self.emitted..];
        let chars: Vec<(usize, char)> = pending.char_indices().collect();
        for (i, &(pos, c)) in chars.iter().enumerate() {
            let next = chars.get(i + 1).map(|&(_, n)| n);
            let end = self.emitted + pos + c.len_utf8();
            match c {
                '\n' => return Some(end),
                '.' | '!' | '?' | ';' if next.is_some_and(char::is_whitespace) => {
                    if c == '.' && self.is_abbreviation(self.emitted + pos) {
                        continue;
                    }
                    return Some(end);
                }
                _ => {}
            }
        }
        // A long run with no sentence end: split at a clause boundary.
        if pending.len() > MAX_SENTENCE_CHARS {
            let window = &pending[..MAX_SENTENCE_CHARS];
            if let Some(p) = window.rfind([',', ':', '—']).filter(|&p| p >= MIN_SPLIT_CHARS) {
                return Some(self.emitted + p + 1);
            }
        }
        None
    }

    /// Whether the period at `dot` ends an abbreviation or a single initial.
    fn is_abbreviation(&self, dot: usize) -> bool {
        let before = &self.text[..dot];
        let word = before.rsplit(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("");
        let word = word.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        (word.len() == 1 && word.chars().all(char::is_alphabetic)) || ABBREVIATIONS.contains(&word.as_str())
    }
}

// ------------------------------------------------------------ speaker

struct Spoken {
    end_offset: usize,
    start_s: f64,
}

struct Utterance {
    id: u32,
    cancel: CancellationToken,
    base: Instant,
    /// Sentences sent to the browser, with their estimated playback start.
    timeline: Vec<Spoken>,
    /// When the queued audio finishes playing, in seconds since `base`.
    playing_until: f64,
    /// Sentences queued but not yet synthesized and sent.
    pending: usize,
    /// Set when stopped: the prefix that had been heard by then.
    frozen_prefix: Option<usize>,
}

impl Utterance {
    fn now_s(&self) -> f64 {
        self.base.elapsed().as_secs_f64()
    }

    fn heard_prefix(&self) -> usize {
        let now = self.now_s();
        self.timeline.iter().filter(|s| s.start_s <= now).map(|s| s.end_offset).max().unwrap_or(0)
    }
}

/// Speaks assistant turns for one session.
pub struct Speaker {
    tts: Arc<dyn TtsEngine>,
    out: mpsc::UnboundedSender<Outbound>,
    current: Arc<Mutex<Option<Utterance>>>,
    next_id: Mutex<u32>,
}

/// Feeds sentences of one assistant turn to the speaker.
pub struct UtteranceHandle {
    id: u32,
    tx: mpsc::UnboundedSender<(String, usize)>,
    current: Arc<Mutex<Option<Utterance>>>,
}

impl UtteranceHandle {
    pub fn say(&self, sentence: String, end_offset: usize) {
        if let Some(u) = self.current.lock().unwrap().as_mut().filter(|u| u.id == self.id) {
            u.pending += 1;
        }
        let _ = self.tx.send((sentence, end_offset));
    }
}

impl Speaker {
    pub fn new(tts: Arc<dyn TtsEngine>, out: mpsc::UnboundedSender<Outbound>) -> Self {
        Self { tts, out, current: Arc::new(Mutex::new(None)), next_id: Mutex::new(0) }
    }

    /// Starts a new utterance (stopping any previous one).
    pub fn begin(&self) -> UtteranceHandle {
        self.stop();
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let cancel = CancellationToken::new();
        *self.current.lock().unwrap() = Some(Utterance {
            id,
            cancel: cancel.clone(),
            base: Instant::now(),
            timeline: Vec::new(),
            playing_until: 0.0,
            pending: 0,
            frozen_prefix: None,
        });
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, usize)>();
        let tts = self.tts.clone();
        let out = self.out.clone();
        let current = self.current.clone();
        tokio::spawn(async move {
            let mut index: u16 = 0;
            while let Some((sentence, end_offset)) = rx.recv().await {
                let pcm = tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = tts.synthesize(&sentence) => r,
                };
                let mut guard = current.lock().unwrap();
                let Some(u) = guard.as_mut().filter(|u| u.id == id && !cancel.is_cancelled()) else { break };
                u.pending = u.pending.saturating_sub(1);
                let pcm = match pcm {
                    Ok(p) if !p.is_empty() => p,
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "speech synthesis failed; skipping sentence");
                        continue;
                    }
                };
                let dur = pcm.len() as f64 / tts.sample_rate() as f64;
                let start = u.now_s().max(u.playing_until);
                u.playing_until = start + dur;
                u.timeline.push(Spoken { end_offset, start_s: start });
                drop(guard);
                let header = TtsHeader { utterance_id: id, sentence_index: index, sample_rate: tts.sample_rate() };
                index = index.saturating_add(1);
                let _ = out.send(Outbound::Audio(header.encode(&pcm)));
            }
        });
        UtteranceHandle { id, tx, current: self.current.clone() }
    }

    /// The advisor is (or is about to be) audibly speaking.
    pub fn is_speaking(&self) -> bool {
        self.current.lock().unwrap().as_ref().is_some_and(|u| {
            u.frozen_prefix.is_none() && (u.pending > 0 || u.now_s() < u.playing_until)
        })
    }

    /// Byte length of the current turn's text the user has heard (at least
    /// begun hearing). After `stop`, the value at the moment it stopped.
    pub fn heard_prefix(&self) -> Option<usize> {
        self.current.lock().unwrap().as_ref().map(|u| u.frozen_prefix.unwrap_or_else(|| u.heard_prefix()))
    }

    /// Stops playback now. Returns whether anything was playing or queued.
    pub fn stop(&self) -> bool {
        let mut guard = self.current.lock().unwrap();
        let Some(u) = guard.as_mut() else { return false };
        if u.frozen_prefix.is_some() {
            return false;
        }
        let active = u.pending > 0 || u.now_s() < u.playing_until;
        u.frozen_prefix = Some(u.heard_prefix());
        u.cancel.cancel();
        let id = u.id;
        drop(guard);
        let _ = self.out.send(Outbound::Msg(ServerMsg::StopAudio { utterance_id: id }));
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentences(chunks: &[&str]) -> Vec<String> {
        let mut c = SentenceChunker::new();
        let mut out: Vec<String> = chunks.iter().flat_map(|d| c.push(d)).map(|(s, _)| s).collect();
        out.extend(c.finish().map(|(s, _)| s));
        out
    }

    #[test]
    fn splits_at_sentence_ends_across_deltas() {
        assert_eq!(
            sentences(&["Your fees look ", "fine. The expense", " ratio is about zero point zero four percent! Want", " more?"]),
            vec![
                "Your fees look fine.",
                "The expense ratio is about zero point zero four percent!",
                "Want more?"
            ]
        );
    }

    #[test]
    fn keeps_abbreviations_decimals_and_initials_together() {
        assert_eq!(
            sentences(&["U.S. stocks are 62.5% of it, e.g. VTI. Mr. T. Rowe Price funds too. Done"]),
            vec!["U.S. stocks are 62.5% of it, e.g. VTI.", "Mr. T. Rowe Price funds too.", "Done"]
        );
    }

    #[test]
    fn offsets_point_into_the_turn_text() {
        let mut c = SentenceChunker::new();
        let text = "One. Two. Three";
        let mut ends: Vec<usize> = c.push(text).into_iter().map(|(_, e)| e).collect();
        ends.extend(c.finish().map(|(_, e)| e));
        assert_eq!(ends, vec![4, 9, text.len()]);
        assert_eq!(&text[..ends[1]], "One. Two.");
    }

    #[test]
    fn splits_very_long_sentences_at_a_clause() {
        let long = format!("{}, and then {}", "a".repeat(150), "b".repeat(150));
        let out = sentences(&[&long]);
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with(','));
    }
}
