//! Speech-to-text, run locally. The default engine is whisper.cpp through
//! `whisper-rs`; tests substitute a fake through [`SpeechToText`].

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use protocol::MIC_SAMPLE_RATE;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

pub trait SpeechToText: Send + Sync {
    /// Transcribes 16 kHz mono PCM. Blocking; call from `spawn_blocking`.
    fn transcribe(&self, pcm: &[i16]) -> anyhow::Result<String>;
}

pub struct WhisperStt {
    state: Mutex<WhisperState>,
    threads: i32,
    /// Size the encoder's audio context to the utterance instead of the full
    /// 30 s window: about 6x faster for short turns with the same text.
    fit_audio_ctx: bool,
}

impl WhisperStt {
    pub fn load(model: &Path, threads: i32, fit_audio_ctx: bool) -> anyhow::Result<Self> {
        whisper_rs::install_logging_hooks();
        let path = model.to_str().context("model path is not UTF-8")?;
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .with_context(|| format!("loading whisper model {}", model.display()))?;
        let state = ctx.create_state().context("creating whisper state")?;
        Ok(Self { state: Mutex::new(state), threads, fit_audio_ctx })
    }
}

/// Encoder audio context for `samples` of audio: 50 per second plus margin.
fn audio_ctx(samples: usize) -> i32 {
    let secs = samples as f64 / MIC_SAMPLE_RATE as f64;
    ((secs * 50.0).ceil() as i32 + 64).clamp(128, 1500)
}

impl SpeechToText for WhisperStt {
    fn transcribe(&self, pcm: &[i16]) -> anyhow::Result<String> {
        let audio: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_n_threads(self.threads);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_single_segment(true);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        if self.fit_audio_ctx {
            params.set_audio_ctx(audio_ctx(pcm.len()));
        }
        let mut state = self.state.lock().unwrap();
        state.full(params, &audio).context("whisper transcription")?;
        let text: String = state.as_iter().map(|seg| seg.to_string()).collect();
        Ok(clean_transcript(&text))
    }
}

/// Strips non-speech annotations whisper emits for noise and silence
/// (`[BLANK_AUDIO]`, `(keyboard clicking)`, `*cough*`) and tidies spacing.
pub fn clean_transcript(text: &str) -> String {
    static ANNOTATION: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[[^\]]*\]|\([^)]*\)|\*[^*]*\*").expect("valid regex"));
    ANNOTATION.replace_all(text, " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_annotations() {
        assert_eq!(clean_transcript(" [BLANK_AUDIO]"), "");
        assert_eq!(clean_transcript("(keyboard clicking) What's my  expense ratio?"), "What's my expense ratio?");
        assert_eq!(clean_transcript("*cough* Scan this page."), "Scan this page.");
    }

    #[test]
    fn audio_ctx_scales_with_length() {
        assert_eq!(audio_ctx(16_000), 128);
        assert_eq!(audio_ctx(16_000 * 10), 564);
        assert_eq!(audio_ctx(16_000 * 60), 1500);
    }
}
