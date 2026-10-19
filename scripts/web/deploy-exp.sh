#!/usr/bin/env bash
# Deploys the experimental bundle and the page scripts to the relay box.
#
# Where to: scripts/web/deploy.env (machine-local, not in git) sets
#   DEPLOY_HOST  ssh target, user@host
#   DEPLOY_SITE  the site's host name, for the "is a match running" check
#   DEPLOY_IP    optional: pins DEPLOY_SITE to this address (flaky DNS)
#   DEPLOY_KEY   optional: ssh key (default ~/.ssh/id_ed25519)
#   DEPLOY_ROOT  optional: the page's folder on the host (default /srv/iw4l-web)
set -euo pipefail
cd "$(dirname "$0")/../.."
if [ -f scripts/web/deploy.env ]; then . scripts/web/deploy.env; fi
: "${DEPLOY_HOST:?set DEPLOY_HOST in scripts/web/deploy.env}"
: "${DEPLOY_SITE:?set DEPLOY_SITE in scripts/web/deploy.env}"
KEY=${DEPLOY_KEY:-~/.ssh/id_ed25519}
H=$DEPLOY_HOST
ROOT=${DEPLOY_ROOT:-/srv/iw4l-web}
SSH="ssh -i $KEY -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes"
SCP="scp -i $KEY -o IdentitiesOnly=yes -o StrictHostKeyChecking=no"
RESOLVE=()
if [ -n "${DEPLOY_IP:-}" ]; then RESOLVE=(--resolve "$DEPLOY_SITE:443:$DEPLOY_IP"); fi
rooms=$(curl -s --max-time 10 "${RESOLVE[@]}" "https://$DEPLOY_SITE/lan/rooms" || true)
echo "rooms: $rooms"
if [ "$rooms" != "[]" ] && [ "${FORCE:-0}" != "1" ]; then echo "a match is running; not deploying the page"; exit 1; fi
if [ "${PKG_ONLY:-0}" != "1" ]; then
  $SSH $H "cd $ROOT && cp index.html index.html.prev && cp vfs.js vfs.js.prev && cp vfs-worker.js vfs-worker.js.prev && mkdir -p docs"
  $SCP web/index.html web/vfs.js web/vfs-worker.js web/engine-worker.js web/engine-thread.js web/engine-shared.js web/audio-worklet.js $H:$ROOT/
  $SCP web/docs/index.html $H:$ROOT/docs/index.html
fi
if [ "${PAGE_ONLY:-0}" != "1" ]; then
  $SCP web/pkg-exp-deploy/iw4l.js web/pkg-exp-deploy/iw4l_bg.wasm web/pkg-exp-deploy/iw4l_bg.wasm.gz $H:$ROOT/pkg-exp/
fi
if [ "${STABLE:-0}" = "1" ]; then
  # The stable bundle too (worker mode is opt-in there with ?worker=1); the old one is kept in pkg-prev.
  $SSH $H "rm -rf $ROOT/pkg-prev && cp -r $ROOT/pkg $ROOT/pkg-prev"
  $SCP web/pkg-deploy/iw4l.js web/pkg-deploy/iw4l_bg.wasm web/pkg-deploy/iw4l_bg.wasm.gz $H:$ROOT/pkg/
fi
$SSH $H "chmod 644 $ROOT/*.js $ROOT/index.html $ROOT/docs/index.html $ROOT/pkg-exp/* $ROOT/pkg/*; chmod 755 $ROOT/docs; ls -la $ROOT/engine-worker.js $ROOT/pkg-exp/ $ROOT/pkg/"
