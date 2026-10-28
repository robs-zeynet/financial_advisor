#!/usr/bin/env bash
# Runs every test: server + protocol natively, the client in headless Chrome.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test --workspace
wasm-pack test --headless --chrome crates/client
