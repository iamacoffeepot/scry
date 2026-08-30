#!/usr/bin/env bash
#
# Record the Highlight and Lowlight clips for an archived match and write them
# as <archivedir>/highlight<sfx>.mp4 / lowlight<sfx>.mp4 — artifacts that
# `scry --from-archive` auto-embeds under "Highlight" / "Lowlight" headers.
#
# The moments are NOT chosen here. The analysis pass journals each tracked
# perspective's picks (`picks_assigned`), and the tick's clip pass hands this
# script each perspective's clip windows directly — one spec argument per
# perspective:
#
#   <suffix>|<hl_seek>,<hl_dur>|<ll_seek>,<ll_dur>|<champion>
#
# where <suffix> selects that player's output names (e.g. `-Moon_132`) and a
# side with no pick is an empty field. <champion> locks the replay camera on
# that perspective (the render API resolves a champion name to its player);
# empty falls back to the auto-director, which starts at the fountain and can
# take seconds — or the whole clip — to find the action. This script just
# loads the replay ONCE, seeks each window, records it, and transcodes the
# game's native webm to a Discord-friendly mp4.
#
# Requires the League CLIENT running + logged in on the match's region/patch,
# with EnableReplayApi=1 in game.cfg. See memory/reference_lol_replay_recording.
# Usage: scripts/highlight.sh <archivedir> <spec>…

set -uo pipefail

dir="${1:?usage: highlight.sh <archivedir> <suffix|hl_seek,hl_dur|ll_seek,ll_dur>…}"
shift
specs=("$@")
[[ ${#specs[@]} -eq 0 ]] && { echo "no clip specs given"; exit 0; }
# The game resolves the record `path` from ITS own cwd, so it must be absolute.
dir="$(cd "$dir" && pwd)"
gid="$(basename "$dir")"        # numeric game id (e.g. 5592737881)

# PREROLL is just buffer: we seek a couple seconds before the clip start and
# let the replay stream in before recording.
PREROLL=3

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

# lock_camera <champion> — attach the camera to a champion's player. Right
# after the replay comes up the render API accepts the POST but resolves the
# selection to '' (camera attached to nothing = free camera at the fountain),
# so retry until the response echoes a non-empty selectionName. On persistent
# failure fall through unlocked — the auto-director is worse, not fatal.
lock_camera() {
  local champ="$1" sel
  for _ in $(seq 1 10); do
    sel="$(curl -sk --max-time 5 -X POST "$rp/render" -H "Content-Type: application/json" \
      -d "{\"selectionName\":\"$champ\",\"cameraAttached\":true}" \
      | python3 -c "import sys,json;print(json.load(sys.stdin).get('selectionName',''))" 2>/dev/null)"
    [[ -n "$sel" ]] && { echo "  camera locked on $champ ($sel)"; return 0; }
    sleep 2
  done
  echo "  camera lock on $champ never resolved; recording unlocked"
}

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

# record_window <seek,dur> <out.mp4> — one side's window, skipped when empty.
record_window() {
  local window="$1" out="$2" seek dur
  [[ -z "$window" ]] && return 0
  IFS=, read -r seek dur <<< "$window"
  echo "  window: start=${seek}s dur=${dur}s"
  record_clip "$seek" "$dur" "$out"
}

# One loaded replay serves every perspective: seeks are cheap, loads aren't.
for spec in "${specs[@]}"; do
  IFS='|' read -r sfx hl ll champ <<< "$spec"
  # Lock the camera on this perspective's champion before its windows.
  [[ -n "${champ:-}" ]] && lock_camera "$champ"
  if [[ -n "$hl" ]]; then echo "recording highlight$sfx"; record_window "$hl" "$dir/highlight$sfx.mp4"; fi
  if [[ -n "$ll" ]]; then echo "recording lowlight$sfx"; record_window "$ll" "$dir/lowlight$sfx.mp4"; fi
done

# Close the replay game window (leave the client up — it serves the LCU we need
# to download the next replay).
pkill -9 -f "LoL/Game/League" 2>/dev/null
echo "closed replay"
