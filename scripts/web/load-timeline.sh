#!/usr/bin/env bash
# Dev: waits for the newest page load in web/dev.log to reach the match (or
# crash), then prints when each load stage finished, in seconds since load.
log="${LOG:-$(cd "$(dirname "$0")/../.." && pwd)/web/dev.log}"
timeout="${1:-400}"
run() { awk '/^---- page load/{buf=""} {buf=buf $0 "\n"} END{printf "%s", buf}' "$log"; }
for _ in $(seq 1 $((timeout / 2))); do
  run | grep -qE '^uncaught|panicked|wait playing: Playing' && break
  sleep 2
done
run | grep -E 'load stage: [a-z_/ ]+ done|^uncaught|panicked|wait playing|vfs: ' \
  | sed -E 's/^(log|warn|error) //; s/\[live [0-9]+ \/ heap ([0-9]+) MiB\] [a-z]+ +info +//' \
  | sed -E 's/load stage: //; s/ heap=.*//; s/ items=[^ ]*//' | cut -c1-150
