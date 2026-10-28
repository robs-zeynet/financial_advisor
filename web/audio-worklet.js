// AudioWorklet processor: copies mic samples to the main thread. Browsers
// require worklet processors to be JS modules; all processing (level meter,
// PCM16 conversion, framing) happens in Rust (crates/client/src/audio.rs).
class PcmForwarder extends AudioWorkletProcessor {
  process(inputs) {
    const channel = inputs[0] && inputs[0][0];
    if (channel) {
      this.port.postMessage(channel.slice(0));
    }
    return true;
  }
}

registerProcessor("pcm-forwarder", PcmForwarder);
