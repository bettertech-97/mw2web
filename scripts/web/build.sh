#!/usr/bin/env bash
# Builds the browser bundle into web/pkg. Needs the wasm32-unknown-unknown
# target, clang (ring and zstd compile C for wasm) and wasm-bindgen-cli
# matching Cargo.lock's wasm-bindgen.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
if [ -d "/c/Program Files/LLVM/bin" ]; then export PATH="/c/Program Files/LLVM/bin:$PATH"; fi
profile="${PROFILE:-web}"
cargo build -p launcher --target wasm32-unknown-unknown --profile "$profile"
wasm-bindgen --target web --no-typescript --out-dir web/pkg \
  "target/wasm32-unknown-unknown/$profile/iw4l.wasm"
ls -la web/pkg
