# Financial Advisor

A local-first, single-user web app: you "call" a Claude-powered financial
analyst from your browser and, when you choose, let it see one shared window or
tab. See `PROMPT.md` for the full spec and privacy invariants.

**Status: M2 (screen notes).** Typed chat with Claude, `view_screen` with the
capture loop, pause/resume (P2), page scans for pages longer than one screen,
and screen notes: the advisor writes down what it read off your screen, you
see every note live, and notes are linted for identifiers and last only for
the call. Next: voice (M3), records (M4), memory approval (M5), and polish
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

### Long pages

Scroll the shared page to the top, click **Scan page** (or Ctrl+Shift+S, or
type "scan this page"), then switch to the shared page and scroll slowly to
the bottom, pausing briefly at each screenful. Come back to the app (or click
**Done**, or type "done") to finish. The advisor is told a scan is available
and looks at it with `view_screen` (scope `scan`). A scan started while paused
returns to paused when it ends.

Browsers slow timers in background tabs, so if the app and the shared page
are tabs in the same window, scans sample about once a second instead of four
times. Keeping the app in its own window avoids that.

### Screen notes

After looking at an account page or a scan, the advisor calls
`record_screen_notes` to keep the facts it needs (holdings, rough values,
fees) once the screenshots leave its context. Each note appears in the
**Screen notes** pane. A note containing anything that looks like a full
account or card number, SSN, routing number, email, street address, or
credential is rejected and the advisor rewrites it. Notes are held in memory
only and are discarded when you hang up.

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
