#!/usr/bin/env bash
# Builds the browser bundle with WebAssembly threads enabled (shared memory,
# atomics) into web/pkg-exp, served by the page as `?engine=exp`.
#
# Needs a nightly toolchain with rust-src: the wasm32 standard library is
# rebuilt with the atomics target feature (`-Z build-std`). Everything else
# matches scripts/web/build.sh. The page must be cross-origin isolated for
# the shared memory, which play.mw2web.com is.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
if [ -d "/c/Program Files/LLVM/bin" ]; then export PATH="/c/Program Files/LLVM/bin:$PATH"; fi
profile="${PROFILE:-web}"
out="${OUT:-web/pkg-exp}"
# A separate target dir: the std rebuild and the feature flags must not mix
# with the single-threaded build's artifacts.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target-threads}"
# C objects (ring, zstd) must carry the same features or wasm-ld refuses to
# link them into a shared-memory module.
export CFLAGS_wasm32_unknown_unknown="${CFLAGS_wasm32_unknown_unknown:-} -matomics -mbulk-memory -mmutable-globals"
# 4 GiB is the most a 32-bit module may address; shared memory has to say
# its maximum up front, and the engine's heap passes 2 GiB on a big map.
# EXTRA_FEATURES adds target features (",+simd128"); give it its own
# CARGO_TARGET_DIR, since a feature change rebuilds everything.
features="+atomics,+bulk-memory,+mutable-globals${EXTRA_FEATURES:-}"
flags='["--cfg", "getrandom_backend=\"wasm_js\"", "-C", "target-feature='"$features"'", "-C", "link-arg=--max-memory=4294967296", "-C", "link-arg=--import-memory", "-C", "link-arg=--shared-memory", "-C", "link-arg=--export=__heap_base", "-C", "link-arg=--export=__data_end", "-C", "link-arg=--export=__wasm_init_tls", "-C", "link-arg=--export=__tls_size", "-C", "link-arg=--export=__tls_align", "-C", "link-arg=--export=__tls_base"]'
cargo +nightly build -p launcher --target wasm32-unknown-unknown --profile "$profile" \
  -Z build-std=std,panic_abort \
  --config "target.wasm32-unknown-unknown.rustflags=$flags"
mkdir -p "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out" \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/$profile/iw4l.wasm"
ls -la "$out"
