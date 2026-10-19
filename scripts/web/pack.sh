#!/usr/bin/env bash
# Packs a deployable bundle from the last build: the same engine without its
# name section (48 MB of function names only stack traces read) plus a
# precompressed copy for nginx's gzip_static. 114 MB -> 65 MB -> 16 MB over
# the wire; the local web/pkg keeps the names for readable console traces.
#
#   pack.sh                                   target/ -> web/pkg-deploy
#   TARGET_DIR=target-threads OUT=web/pkg-exp-deploy pack.sh   (threaded build)
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
profile="${PROFILE:-web}"
target_dir="${TARGET_DIR:-target}"
out="${OUT:-web/pkg-deploy}"
rm -rf "$out"
wasm-bindgen --target web --no-typescript --remove-name-section --remove-producers-section \
  --out-dir "$out" "$target_dir/wasm32-unknown-unknown/$profile/iw4l.wasm"
PACK_OUT="$out" python - <<'PY'
import gzip, os
p = os.path.join(os.environ["PACK_OUT"], "iw4l_bg.wasm")
data = open(p, "rb").read()
open(p + ".gz", "wb").write(gzip.compress(data, 9))
PY
ls -la "$out"
