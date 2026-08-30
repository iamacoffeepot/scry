#!/usr/bin/env bash
#
# scry poller — a thin loop over `scry --tick`, which runs one full pass in
# Rust: post each newly-completed game on the watch list, then record clips
# for pending games (serialized, idle-only, bounded per pass). All poller state lives in the
# SQLite journal (`state/scry.sqlite` by default) — the marker files the old
# shell pipeline scattered through the archive are retired; a pre-journal
# archive is adopted once with `scry --journal-import`.
#
# Usage:
#   scripts/poll.sh          # loop forever, polling every $SCRY_INTERVAL seconds
#   scripts/poll.sh --once   # a single pass, then exit
#
# Config (env, or a .env at the repo root):
#   RIOT_API_KEY, SCRY_DISCORD_WEBHOOK   required
#   SCRY_ACCOUNTS   watch-list file        (default: scripts/accounts.txt)
#   SCRY_ARCHIVE    archive root            (default: archive)
#   SCRY_JOURNAL    journal path            (default: state/scry.sqlite)
#   SCRY_INTERVAL   seconds between passes  (default: 300)
#   SCRY_QUEUE      default queue id        (default: 420 = ranked solo)
#   SCRY_SUMMARY_MODEL  footer attribution label (default: none)
#   SCRY_CLIPS      record replay clips 1/0  (default: 1; needs League client)
#   CLIP_MAX_TRIES  clip retries before giving up on a game (default: 15)
#   SCRY_BIN        scry invocation         (default: cargo run --quiet --)
#
# SCRY_ACCOUNTS/SCRY_ARCHIVE/SCRY_JOURNAL/SCRY_QUEUE/SCRY_SUMMARY_MODEL/
# CLIP_MAX_TRIES are read by the binary itself (clap env); only SCRY_CLIPS
# needs translating to a flag here.

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Load secrets / config from .env if present.
if [[ -f .env ]]; then set -a; source .env; set +a; fi

interval="${SCRY_INTERVAL:-300}"
scry="${SCRY_BIN:-cargo run --quiet --}"

args=(--tick)
[[ "${SCRY_CLIPS:-1}" != "1" ]] && args+=(--no-clips)

if [[ "${1:-}" == "--once" ]]; then
  exec $scry "${args[@]}"
fi

echo "[poll] ticking every ${interval}s (journal: ${SCRY_JOURNAL:-state/scry.sqlite})" >&2
while true; do
  $scry "${args[@]}"
  sleep "$interval"
done
