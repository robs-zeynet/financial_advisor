//! Mic capture via an AudioWorklet (`web/audio-worklet.js` only copies
//! samples), resampling to 16 kHz PCM16 here in Rust, and TTS playback via
//! `AudioBufferSourceNode`s scheduled back to back.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use protocol::{encode_mic, TtsHeader, MIC_SAMPLE_RATE};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioBufferSourceNode, AudioContext, AudioScheduledSourceNode, AudioWorkletNode, AudioWorkletNodeOptions, MediaStream,
    MediaStreamConstraints, MediaStreamTrack, MessageEvent,
};

use crate::dom;
use crate::ws::Ws;

// ------------------------------------------------------------ pure logic

/// 20 ms of 16 kHz audio: one mic message.
pub const MIC_CHUNK: usize = MIC_SAMPLE_RATE as usize / 50;

/// Streaming linear resampler from the AudioContext rate to 16 kHz.
#[derive(Debug, Clone)]
pub struct Resampler {
    step: f64,
    /// Position of the next output sample, relative to `last`.
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(input_rate: f32) -> Self {
        Self { step: input_rate as f64 / MIC_SAMPLE_RATE as f64, pos: 0.0, last: 0.0 }
    }

    /// Resamples a block, carrying state across calls. Positions count from
    /// the previous block's last sample (index -1) through `input`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        let last_index = input.len() as f64 - 1.0;
        while self.pos < last_index {
            let i = self.pos.floor() as isize;
            let frac = (self.pos - i as f64) as f32;
            let a = if i < 0 { self.last } else { input[i as usize] };
            let b = input[(i + 1) as usize];
            out.push(((a + (b - a) * frac).clamp(-1.0, 1.0) * 32767.0) as i16);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        self.last = input[input.len() - 1];
    }
}

/// Peak level of a block in dBFS, for the meter.
pub fn peak_dbfs(pcm: &[i16]) -> f32 {
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0;
    20.0 * peak.max(1e-5).log10()
}

// ------------------------------------------------------------ mic

pub struct Mic {
    ctx: AudioContext,
    stream: MediaStream,
    _node: AudioWorkletNode,
    on: Rc<Cell<bool>>,
}

impl Mic {
    /// Asks for the mic and starts streaming 16 kHz PCM16 to the server.
    /// `on_level` gets a 0..=1 level about every 20 ms.
    pub async fn start(ws: Ws, on_level: impl Fn(f32) + 'static) -> Result<Rc<Self>, JsValue> {
        let devices = dom::window().navigator().media_devices()?;
        let audio = js_sys::Object::new();
        for key in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
            js_sys::Reflect::set(&audio, &key.into(), &JsValue::TRUE)?;
        }
        let constraints = MediaStreamConstraints::new();
        constraints.set_audio(&audio);
        let stream: MediaStream =
            JsFuture::from(devices.get_user_media_with_constraints(&constraints)?).await?.dyn_into()?;

        let ctx = AudioContext::new()?;
        JsFuture::from(ctx.audio_worklet()?.add_module("audio-worklet.js")?).await?;
        let source = ctx.create_media_stream_source(&stream)?;
        let opts = AudioWorkletNodeOptions::new();
        opts.set_number_of_inputs(1);
        opts.set_number_of_outputs(0);
        let node = AudioWorkletNode::new_with_options(&ctx, "pcm-forwarder", &opts)?;
        source.connect_with_audio_node(&node)?;

        let on = Rc::new(Cell::new(true));
        let mut resampler = Resampler::new(ctx.sample_rate());
        let mut pending: Vec<i16> = Vec::with_capacity(MIC_CHUNK * 2);
        let gate = on.clone();
        let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            let block = js_sys::Float32Array::new(&e.data()).to_vec();
            resampler.process(&block, &mut pending);
            while pending.len() >= MIC_CHUNK {
                let chunk: Vec<i16> = pending.drain(..MIC_CHUNK).collect();
                if gate.get() {
                    on_level(((peak_dbfs(&chunk) + 60.0) / 60.0).clamp(0.0, 1.0));
                    ws.send_bytes(&encode_mic(&chunk));
                } else {
                    on_level(0.0);
                }
            }
        });
        node.port()?.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();
        let _ = ctx.resume();
        Ok(Rc::new(Self { ctx, stream, _node: node, on }))
    }

    pub fn is_on(&self) -> bool {
        self.on.get()
    }

    /// Mutes or unmutes: muted audio is never sent.
    pub fn set_on(&self, on: bool) {
        self.on.set(on);
        for t in self.stream.get_audio_tracks().iter() {
            t.unchecked_into::<MediaStreamTrack>().set_enabled(on);
        }
    }

    /// Releases the mic entirely (hang-up).
    pub fn stop(&self) {
        self.on.set(false);
        for t in self.stream.get_tracks().iter() {
            t.unchecked_into::<MediaStreamTrack>().stop();
        }
        let _ = self.ctx.close();
    }
}

// ------------------------------------------------------------ playback

pub struct Player {
    ctx: AudioContext,
    next_time: Cell<f64>,
    sources: RefCell<Vec<(u32, f64, AudioBufferSourceNode)>>,
}

impl Player {
    /// Create from a user gesture (Start call) so the browser lets it play.
    pub fn new() -> Result<Rc<Self>, JsValue> {
        let ctx = AudioContext::new()?;
        let _ = ctx.resume();
        Ok(Rc::new(Self { ctx, next_time: Cell::new(0.0), sources: RefCell::new(Vec::new()) }))
    }

    /// Queues one TTS sentence right after whatever is already queued.
    pub fn play(&self, msg: &[u8]) {
        let Ok((header, body)) = TtsHeader::decode(msg) else { return };
        let samples: Vec<f32> =
            body.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
        if samples.is_empty() {
            return;
        }
        let Ok(buffer) = self.ctx.create_buffer(1, samples.len() as u32, header.sample_rate as f32) else { return };
        if buffer.copy_to_channel(&samples, 0).is_err() {
            return;
        }
        let Ok(source) = self.ctx.create_buffer_source() else { return };
        source.set_buffer(Some(&buffer));
        if source.connect_with_audio_node(&self.ctx.destination()).is_err() {
            return;
        }
        let now = self.ctx.current_time();
        let start = self.next_time.get().max(now + 0.03);
        let end = start + samples.len() as f64 / header.sample_rate as f64;
        if source.start_with_when(start).is_err() {
            return;
        }
        self.next_time.set(end);
        let mut sources = self.sources.borrow_mut();
        sources.retain(|(_, e, _)| *e > now);
        sources.push((header.utterance_id, end, source));
    }

    /// Stops an utterance immediately (barge-in).
    pub fn stop(&self, utterance_id: u32) {
        let mut sources = self.sources.borrow_mut();
        sources.retain(|(id, _, s)| {
            if *id == utterance_id {
                let _ = AudioScheduledSourceNode::stop(s);
                false
            } else {
                true
            }
        });
        if sources.is_empty() {
            self.next_time.set(self.ctx.current_time());
        }
    }

    pub fn stop_all(&self) {
        for (_, _, s) in self.sources.borrow_mut().drain(..) {
            let _ = AudioScheduledSourceNode::stop(&s);
        }
        self.next_time.set(self.ctx.current_time());
    }

    pub fn is_playing(&self) -> bool {
        self.ctx.current_time() < self.next_time.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48k_to_16k_across_blocks() {
        let mut r = Resampler::new(48_000.0);
        let mut out = Vec::new();
        let ramp: Vec<f32> = (0..4800).map(|i| i as f32 / 4800.0).collect();
        for block in ramp.chunks(128) {
            r.process(block, &mut out);
        }
        assert!((1599..=1601).contains(&out.len()), "{}", out.len());
        // Every third input sample: the ramp is preserved.
        assert!((out[300] as f32 / 32767.0 - 900.0 / 4800.0).abs() < 0.01);
    }
}
