# Financial Advisor

A local-first, single-user web app: you "call" a Claude-powered financial
analyst from your browser and, when you choose, let it see one shared window or
tab. See `PROMPT.md` for the full spec and privacy invariants.

**Status: M3 (voice).** Talk to the advisor and it answers out loud; talk
over it to interrupt. Plus typed chat, `view_screen` with pause/resume (P2),
page scans for long pages, and linted, session-only screen notes. Next:
records (M4), memory approval (M5), and polish plus the full privacy write-up
(M6).

## Run

Building needs cmake, a C++ compiler, and libclang (for whisper.cpp).

```sh
cargo install trunk wasm-pack          # once
rustup target add wasm32-unknown-unknown
scripts/fetch-models.sh                # once: ~580 MB of local voice models into models/
trunk build --release                  # builds the WASM client into dist/
cargo build --release -p server        # embeds dist/ into the binary
export ANTHROPIC_API_KEY=...
./target/release/financial-advisor     # optional arg: path to config.toml
```

Open http://127.0.0.1:8765, click **Start call**, then **Share a window or
tab…** (pick a single tab or window). Viewing stays ⚪ PAUSED until you click
**Resume viewing** or press Ctrl+Shift+V.

### Long pages and statements: upload a PDF

For a page much longer than a screen, or a statement, the easiest path is to
print it to PDF (Ctrl+P or Cmd+P, then **Save as PDF**) and click **Upload
PDF…** (or drop the file on the page) during a call. The advisor suggests
this itself, and reads the PDF's text and pages the same way it reads your
screen. The PDF is kept in memory only (up to 10 MB), never written to disk,
and discarded when you click **Remove**, upload another, or hang up.

### Long pages: guided scan

Click **Scan page** (or Ctrl+Shift+S, or say or type "scan this page"), then
switch to the shared page. The advisor guides you out loud: scroll to the top
and hold still, scroll down slowly, "please stop there", "a little slower",
"I think we skipped a bit, could you scroll back up?", "a little further".
It only keeps a new snapshot after about a third of a screen of new content,
so lingering doesn't make duplicates, and it won't skip ahead past content
it missed. Say "done" (or come back to the app, or click **Done**) at the
bottom. The guidance is decided locally and spoken by the local voice; it
never goes to Claude. A scan started while paused returns to paused when it
ends.

Browsers slow timers in background tabs, so if the app and the shared page
are tabs in the same window, scans can sample about once a second instead of
four times. Keeping the app in its own window avoids that.

### Screen notes

After looking at an account page or a scan, the advisor calls
`record_screen_notes` to keep the facts it needs (holdings, rough values,
fees) once the screenshots leave its context. Each note appears in the
**Screen notes** pane. A note containing anything that looks like a full
account or card number, SSN, routing number, email, street address, or
credential is rejected and the advisor rewrites it. Notes are held in memory
only and are discarded when you hang up.

### Voice

Speech never leaves the machine: Whisper (`small.en`) transcribes locally and
Piper speaks locally; audio lives only in memory and is never stored.

- **Hands-free:** just talk. About 0.7 s of silence ends your turn.
- **Interrupt:** talk over the advisor and it stops at once. Only what you
  actually heard is kept as its reply.
- **Push-to-talk:** hold Space (outside the text box).
- **Spoken commands** handled locally, never sent to Claude: "scan this page",
  "done" (during a scan), and "pause viewing".
- **Mic on/off** button; the level meter shows what the mic hears.

Use headphones or the browser's echo cancellation (on by default) so the
advisor's voice doesn't count as you interrupting. If the models are missing,
voice turns off and typing still works.

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
