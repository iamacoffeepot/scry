#!/usr/bin/env bash
#
# Record the Highlight and Lowlight clips for an archived match and write them as
# <archivedir>/highlight.mp4 and <archivedir>/lowlight.mp4 — artifacts that
# `scry --from-archive` auto-embeds under "Highlight" / "Lowlight" headers.
#
# The two moments are NOT chosen here. The OVERVIEW pass picks them from the
# grounded candidate lists in moments.md and writes them into overview.md as
# `## Highlight` / `## Lowlight` sections, each opening with an `m:ss` timestamp.
# This script just reads those timestamps, loads the replay ONCE, and records a
# clip centered on each (with a lead-in so the setup plays before the payoff),
# then transcodes the game's native webm to a Discord-friendly mp4.
#
# The optional <suffix> selects one tracked player's artifacts in a shared
# game: overview<sfx>.md / clips<sfx>.json in, highlight<sfx>.mp4 /
# lowlight<sfx>.mp4 out (empty = the legacy unsuffixed names).
#
# Requires the League CLIENT running + logged in on the match's region/patch,
# with EnableReplayApi=1 in game.cfg. See memory/reference_lol_replay_recording.
# Usage: scripts/highlight.sh <archivedir> [suffix]

set -uo pipefail

dir="${1:?usage: highlight.sh <archivedir> [suffix…]}"
shift
# One or more per-player artifact suffixes; the replay loads ONCE and every
# perspective's clips record in that session (tracked players share games).
suffixes=("$@")
[[ ${#suffixes[@]} -eq 0 ]] && suffixes=("")
# The game resolves the record `path` from ITS own cwd, so it must be absolute.
dir="$(cd "$dir" && pwd)"
gid="$(basename "$dir")"        # numeric game id (e.g. 5592737881)

# The per-clip seek + duration come from clips.json (written by --analyze, sized
# to each fight so the whole play fits). PREROLL is just buffer: we seek a couple
# seconds before the clip start and let the replay stream in before recording.
PREROLL=3
# Fallback window (seconds) if clips.json has no entry for the chosen timestamp.
FALLBACK_LEAD=7
FALLBACK_DUR=18

lockfile="/Applications/League of Legends.app/Contents/LoL/lockfile"
[[ -f "$lockfile" ]] || { echo "no League client lockfile — is it running/logged in?"; exit 1; }
IFS=: read -r _ _ port pass _ < "$lockfile"
lcu="https://127.0.0.1:$port"
rp="https://127.0.0.1:2999/replay"

# SAFETY: recording kills the "LoL/Game/League" process — which also matches a
# LIVE match. If a game / champ select is in progress, bail so we never kill a
# real game the user is playing. The clip is skipped; the post still goes out.
phase="$(curl -sk --max-time 5 -u "riot:$pass" "$lcu/lol-gameflow/v1/gameflow-phase" 2>/dev/null)"
case "$phase" in
  *InProgress*|*ChampSelect*|*Matchmaking*|*ReadyCheck*|*GameStart*|*Reconnect*)
    echo "live game in progress ($phase) — skipping clip recording"; exit 0;;
esac

# First m:ss under a `## <section>` heading in overview.md (empty if none/absent).
section_ts() {
  awk -v sec="## $1" '
    $0==sec {grab=1; next}
    /^## / {grab=0}
    grab && match($0, /[0-9]+:[0-9][0-9]/) { print substr($0,RSTART,RLENGTH); exit }
  ' "$overview"
}
to_secs() { local t="$1"; echo $(( ${t%%:*} * 60 + 10#${t##*:} )); }

# Gather each perspective's timestamps before touching the replay, so a game
# with nothing to clip never loads it.
work=()
for sfx in "${suffixes[@]}"; do
  overview="$dir/overview$sfx.md"
  [[ -f "$overview" ]] || { echo "no overview$sfx.md — skipping that perspective"; continue; }
  hl_ts="$(section_ts Highlight)"
  ll_ts="$(section_ts Lowlight)"
  if [[ -z "$hl_ts" && -z "$ll_ts" ]]; then
    echo "no timestamps in overview$sfx.md — skipping that perspective"; continue
  fi
  echo "clips$sfx: highlight=${hl_ts:-none} lowlight=${ll_ts:-none}"
  work+=("$sfx|$hl_ts|$ll_ts")
done
if [[ ${#work[@]} -eq 0 ]]; then
  echo "nothing to clip"; exit 0
fi

# --- load THIS game's replay (a stale one may be up) ------------------------
# The .rofl download verifies asynchronously (metadata state: checking -> watch);
# launching while still "checking" no-ops the watch and the replay never comes
# up. And the replay subsystem occasionally wedges. So: download, wait for the
# state to reach "watch", launch, poll playback — and retry the whole load once.
replay_state() {
  curl -sk --max-time 5 -u "riot:$pass" "$lcu/lol-replays/v1/metadata/$gid" \
    | python3 -c "import sys,json;print(json.load(sys.stdin).get('state',''))" 2>/dev/null
}
playback_up() { [[ "$(curl -sk --max-time 3 -o /dev/null -w '%{http_code}' "$rp/playback" 2>/dev/null)" == "200" ]]; }

load_replay() {
  pkill -9 -f "LoL/Game/League" 2>/dev/null; sleep 4
  curl -sk --max-time 10 -u "riot:$pass" -X POST "$lcu/lol-replays/v1/rofls/$gid/download/graceful" -H "Content-Type: application/json" -d '{}' >/dev/null
  # Wait for the download to finish verifying (up to ~60s) before launching.
  for _ in $(seq 1 20); do [[ "$(replay_state)" == "watch" ]] && break; sleep 3; done
  curl -sk --max-time 10 -u "riot:$pass" -X POST "$lcu/lol-replays/v1/rofls/$gid/watch" -H "Content-Type: application/json" -d '{}' >/dev/null
  # Poll for the in-game replay API to come up (up to ~45s).
  for _ in $(seq 1 15); do playback_up && return 0; sleep 3; done
  playback_up
}

if ! load_replay; then
  echo "replay didn't come up; retrying load once"
  load_replay || { echo "replay API never came up"; exit 1; }
fi

# Clean shot: HUD off, full vision.
curl -sk --max-time 5 -X POST "$rp/render" -H "Content-Type: application/json" -d '{"interfaceAll":false,"fogOfWar":false}' -o /dev/null

# record_clip <start_seconds> <duration_seconds> <out.mp4> — live record button,
# no offline render. Seeks PREROLL before the window so it streams in, then
# records exactly <duration> seconds of the play.
record_clip() {
  local start="$1" dur="$2" out="$3"
  local seek=$(( start - PREROLL )); (( seek < 0 )) && seek=0
  local raw="$dir/clip-raw$sfx.webm"; rm -f "$raw"
  curl -sk --max-time 5 -X POST "$rp/playback" -H "Content-Type: application/json" -d "{\"time\":$seek.0,\"speed\":1.0,\"paused\":false}" -o /dev/null
  sleep "$PREROLL"
  curl -sk --max-time 6 -X POST "$rp/recording" -H "Content-Type: application/json" -d "{\"recording\":true,\"codec\":\"webm\",\"path\":\"$raw\"}" -o /dev/null
  sleep "$dur"
  curl -sk --max-time 6 -X POST "$rp/recording" -H "Content-Type: application/json" -d '{"recording":false}' -o /dev/null
  sleep 4
  if [[ ! -s "$raw" ]]; then echo "  recording produced no file for $out"; return 1; fi
  # crf 28 keeps a ~30s teamfight clip under Discord's 10MB attachment limit.
  ffmpeg -hide_banner -loglevel error -y -i "$raw" -vf "scale=1280:-2,format=yuv420p" \
    -c:v libx264 -crf 28 -preset medium -c:a aac -b:a 96k -movflags +faststart "$out"
  rm -f "$raw"
  echo "  $out ($(du -h "$out" | cut -f1))"
}

# record_ts <m:ss> <out.mp4> — resolve the clip window for this timestamp from
# clips.json (seek + duration sized to the fight), falling back to a fixed
# window if there's no matching entry.
record_ts() {
  local ts="$1" out="$2" seek dur win=""
  [[ -f "$dir/clips$sfx.json" ]] && win="$(jq -r --arg k "$ts" '.[$k] // empty | "\(.seek) \(.dur)"' "$dir/clips$sfx.json" 2>/dev/null)"
  if [[ -n "$win" ]]; then
    read -r seek dur <<< "$win"
  else
    seek=$(( $(to_secs "$ts") - FALLBACK_LEAD )); (( seek < 0 )) && seek=0
    dur="$FALLBACK_DUR"
  fi
  echo "  window: start=${seek}s dur=${dur}s"
  record_clip "$seek" "$dur" "$out"
}

# One loaded replay serves every perspective: seeks are cheap, loads aren't.
for entry in "${work[@]}"; do
  IFS='|' read -r sfx hl_ts ll_ts <<< "$entry"
  if [[ -n "$hl_ts" ]]; then echo "recording highlight$sfx @ $hl_ts"; record_ts "$hl_ts" "$dir/highlight$sfx.mp4"; fi
  if [[ -n "$ll_ts" ]]; then echo "recording lowlight$sfx @ $ll_ts"; record_ts "$ll_ts" "$dir/lowlight$sfx.mp4"; fi
done

# Close the replay game window (leave the client up — it serves the LCU we need
# to download the next replay).
pkill -9 -f "LoL/Game/League" 2>/dev/null
echo "closed replay"
