#!/usr/bin/env bash
# Downloads the local voice stack into ./models (gitignored):
#   - Piper TTS binary (rhasspy/piper release) and the en_US-lessac-medium voice
#   - Whisper small.en model for whisper.cpp
# Everything runs locally afterwards; nothing here is contacted at runtime.
# Hugging Face files are checked against the SHA-256 the hub publishes.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p models
cd models

PIPER_URL="https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz"
VOICE_BASE="https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/lessac/medium"
WHISPER_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin"

# Fetches a Hugging Face LFS file and verifies it against the hub's SHA-256.
fetch_hf() {
  local url=$1 out=$2
  if [[ -s $out ]]; then echo "have $out"; return; fi
  local expected
  expected=$(curl -sIL "$url" | tr -d '\r' | awk -F': ' 'tolower($1)=="x-linked-etag"{gsub(/"/,"",$2); print $2}' | tail -1)
  echo "fetching $out"
  curl -fL --progress-bar -o "$out.part" "$url"
  if [[ $expected =~ ^[0-9a-f]{64}$ ]]; then
    local actual
    actual=$(sha256sum "$out.part" | cut -d' ' -f1)
    if [[ $actual != "$expected" ]]; then
      echo "SHA-256 mismatch for $out: expected $expected, got $actual" >&2
      rm -f "$out.part"; exit 1
    fi
    echo "  sha256 ok ($expected)"
  else
    echo "  (no published hash; sha256 $(sha256sum "$out.part" | cut -d' ' -f1))"
  fi
  mv "$out.part" "$out"
}

if [[ ! -x piper/piper ]]; then
  echo "fetching piper"
  rm -rf piper.tmp && mkdir piper.tmp
  curl -fL --progress-bar -o piper.tmp/piper.tar.gz "$PIPER_URL"
  echo "  sha256 $(sha256sum piper.tmp/piper.tar.gz | cut -d' ' -f1)"
  tar -xzf piper.tmp/piper.tar.gz -C piper.tmp
  rm -rf piper && mv piper.tmp/piper piper && rm -rf piper.tmp
else
  echo "have piper/piper"
fi

fetch_hf "$VOICE_BASE/en_US-lessac-medium.onnx" en_US-lessac-medium.onnx
fetch_hf "$VOICE_BASE/en_US-lessac-medium.onnx.json" en_US-lessac-medium.onnx.json
fetch_hf "$WHISPER_URL" ggml-small.en.bin
echo "done: $(du -sh . | cut -f1) in models/"
