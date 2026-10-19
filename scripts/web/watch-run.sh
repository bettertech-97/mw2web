#!/usr/bin/env bash
# Dev: waits for the newest page load in web/dev.log to crash, reach the
# match, or time out, then prints the load-stage / failure summary.
log="$(cd "$(dirname "$0")/../.." && pwd)/web/dev.log"
timeout="${1:-240}"
run() { awk '/^---- page load/{buf=""} {buf=buf $0 "\n"} END{printf "%s", buf}' "$log"; }
for _ in $(seq 1 $((timeout / 2))); do
  run | grep -qE '^uncaught|panicked|wait spawn: InGame' && break
  sleep 2
done
run | grep -E 'load stage: [a-z_/ ]+ done|^uncaught|panicked|InGame|bots: add|spawn: presented|wgpu|WebGPU|Validation|^error|^warn' \
  | sed -E 's/^log //' | cut -c1-220 | tail -"${2:-40}"
