# Financial Advisor

A local-first, single-user web app: you "call" a Claude-powered financial
analyst from your browser and, when you choose, let it see one shared window or
tab. See `PROMPT.md` for the full spec and privacy invariants.

**Status: M0 (skeleton).** Typed chat with Claude, `view_screen` with the
capture loop, and pause/resume (P2). Next: page scans for long pages (M1),
screen notes (M2), voice (M3), records (M4), memory approval (M5), and polish
plus the full privacy write-up (M6).

## Run

```sh
cargo install trunk wasm-pack          # once
rustup target add wasm32-unknown-unknown
trunk build --release                  # builds the WASM client into dist/
cargo build --release -p server        # embeds dist/ into the binary
export ANTHROPIC_API_KEY=...
./target/release/financial-advisor     # optional arg: path to config.toml
```

Open http://127.0.0.1:8765, click **Start call**, then **Share a window or
tab…** (pick a single tab or window). Viewing stays ⚪ PAUSED until you click
**Resume viewing** or press Ctrl+Shift+V.

## Test

```sh
scripts/test.sh
```

## Notes on choices

- **Image size:** frames are downscaled to a 2000 px long edge (not ~1568).
  Opus 5.5 is a high-resolution vision model (2576 px native), so 1568 px
  would throw away legibility on dense statements; 2000 px stays under the
  API's stricter per-image limit for requests with more than 20 images.
- **API version:** requests send `anthropic-version: 2026-10-21`, the current
  version. The app sends no sampling parameters, so nothing it uses changed.

The privacy model and the Anthropic API data-retention note will be written
up here in M6.
