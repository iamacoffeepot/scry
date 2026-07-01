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
# Requires the League CLIENT running + logged in on the match's region/patch,
# with EnableReplayApi=1 in game.cfg. See memory/reference_lol_replay_recording.
# Usage: scripts/highlight.sh <archivedir>

set -uo pipefail

dir="${1:?usage: highlight.sh <archivedir>}"
# The game resolves the record `path` from ITS own cwd, so it must be absolute.
dir="$(cd "$dir" && pwd)"
gid="$(basename "$dir")"        # numeric game id (e.g. 5592737881)
overview="$dir/overview.md"
[[ -f "$overview" ]] || { echo "no overview.md in $dir — nothing to clip"; exit 0; }

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

hl_ts="$(section_ts Highlight)"
ll_ts="$(section_ts Lowlight)"
if [[ -z "$hl_ts" && -z "$ll_ts" ]]; then
  echo "no Highlight/Lowlight timestamps in overview.md — skipping clips"; exit 0
fi
echo "clips: highlight=${hl_ts:-none} lowlight=${ll_ts:-none}"

# --- load THIS game's replay once (a stale one may be up) -------------------
pkill -9 -f "LoL/Game/League" 2>/dev/null; sleep 4
curl -sk --max-time 10 -u "riot:$pass" -X POST "$lcu/lol-replays/v1/rofls/$gid/download/graceful" -H "Content-Type: application/json" -d '{}' >/dev/null
curl -sk --max-time 10 -u "riot:$pass" -X POST "$lcu/lol-replays/v1/rofls/$gid/watch" -H "Content-Type: application/json" -d '{}' >/dev/null
for _ in $(seq 1 30); do [[ "$(curl -sk --max-time 3 -o /dev/null -w '%{http_code}' "$rp/playback" 2>/dev/null)" == "200" ]] && break; sleep 3; done
[[ "$(curl -sk --max-time 3 -o /dev/null -w '%{http_code}' "$rp/playback" 2>/dev/null)" == "200" ]] || { echo "replay API never came up"; exit 1; }

# Clean shot: HUD off, full vision.
curl -sk --max-time 5 -X POST "$rp/render" -H "Content-Type: application/json" -d '{"interfaceAll":false,"fogOfWar":false}' -o /dev/null

# record_clip <start_seconds> <duration_seconds> <out.mp4> — live record button,
# no offline render. Seeks PREROLL before the window so it streams in, then
# records exactly <duration> seconds of the play.
record_clip() {
  local start="$1" dur="$2" out="$3"
  local seek=$(( start - PREROLL )); (( seek < 0 )) && seek=0
  local raw="$dir/clip-raw.webm"; rm -f "$raw"
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
  [[ -f "$dir/clips.json" ]] && win="$(jq -r --arg k "$ts" '.[$k] // empty | "\(.seek) \(.dur)"' "$dir/clips.json" 2>/dev/null)"
  if [[ -n "$win" ]]; then
    read -r seek dur <<< "$win"
  else
    seek=$(( $(to_secs "$ts") - FALLBACK_LEAD )); (( seek < 0 )) && seek=0
    dur="$FALLBACK_DUR"
  fi
  echo "  window: start=${seek}s dur=${dur}s"
  record_clip "$seek" "$dur" "$out"
}

if [[ -n "$hl_ts" ]]; then echo "recording highlight @ $hl_ts"; record_ts "$hl_ts" "$dir/highlight.mp4"; fi
if [[ -n "$ll_ts" ]]; then echo "recording lowlight @ $ll_ts"; record_ts "$ll_ts" "$dir/lowlight.mp4"; fi

# Close the replay game window (leave the client up — it serves the LCU we need
# to download the next replay).
pkill -9 -f "LoL/Game/League" 2>/dev/null
echo "closed replay"
