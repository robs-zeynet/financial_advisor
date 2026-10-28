//! Real local engines: Piper speaks a sentence, Whisper transcribes it back.
//! Skipped (passes trivially) when `scripts/fetch-models.sh` hasn't been run.

use std::path::{Path, PathBuf};
use std::time::Instant;

use server::stt::{SpeechToText, WhisperStt};
use server::tts::{PiperTts, TtsEngine};

fn models() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models")
}

/// Linear resampling to the 16 kHz mic rate.
fn to_16k(pcm: &[i16], rate: u32) -> Vec<i16> {
    let n = (pcm.len() as f64 * 16_000.0 / rate as f64) as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * rate as f64 / 16_000.0;
            let j = x as usize;
            let f = x - j as f64;
            let a = pcm[j] as f64;
            let b = *pcm.get(j + 1).unwrap_or(&pcm[j]) as f64;
            (a + (b - a) * f) as i16
        })
        .collect()
}

#[tokio::test]
async fn piper_speech_round_trips_through_whisper() {
    let m = models();
    let (bin, voice, whisper) =
        (m.join("piper/piper"), m.join("en_US-lessac-medium.onnx"), m.join("ggml-small.en.bin"));
    if !(bin.exists() && voice.exists() && whisper.exists()) {
        eprintln!("skipping: run scripts/fetch-models.sh to test the real voice engines");
        return;
    }
    let tts = PiperTts::new(&bin, &voice).unwrap();
    let t = Instant::now();
    let pcm = tts.synthesize("What is my effective expense ratio?").await.unwrap();
    let tts_ms = t.elapsed().as_millis();
    assert!(pcm.len() > tts.sample_rate() as usize, "at least a second of audio");

    let stt = WhisperStt::load(&whisper, 8, true).unwrap();
    let audio = to_16k(&pcm, tts.sample_rate());
    let t = Instant::now();
    let text = tokio::task::spawn_blocking(move || stt.transcribe(&audio)).await.unwrap().unwrap();
    let stt_ms = t.elapsed().as_millis();
    eprintln!("tts {tts_ms} ms, stt {stt_ms} ms: {text:?}");
    let lower = text.to_lowercase();
    assert!(lower.contains("expense ratio"), "transcript: {text:?}");
    assert!(stt_ms < 2_500, "speech-to-text took {stt_ms} ms");
}
