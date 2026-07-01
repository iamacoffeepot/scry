#!/usr/bin/env bash
#
# Record a "Play of the Game" highlight for an archived match and write it as
# <archivedir>/highlight.mp4 (an artifact scry --from-archive auto-embeds).
#
# The highlight = the centered player's biggest per-minute gold swing that
# contained one of their kills. We seek ~7s before that kill, hit the replay
# record button (recording:true), let it play, then stop (recording:false),
# and transcode the game's native webm to a Discord-friendly mp4.
#
# Requires the League CLIENT running + logged in on the match's region/patch.
# Usage: scripts/highlight.sh <archivedir> "<RiotID#Tag>"

set -uo pipefail

dir="${1:?usage: highlight.sh <archivedir> <RiotID#Tag>}"
rid="${2:?usage: highlight.sh <archivedir> <RiotID#Tag>}"
# The game resolves the record `path` from ITS own cwd, so it must be absolute.
dir="$(cd "$dir" && pwd)"
name="${rid%%#*}"; tag="${rid##*#}"
gid="$(basename "$dir")"   # numeric game id (e.g. 5592737881)

lockfile="/Applications/League of Legends.app/Contents/LoL/lockfile"
[[ -f "$lockfile" ]] || { echo "no League client lockfile — is it running/logged in?"; exit 1; }
IFS=: read -r _ _ port pass _ < "$lockfile"
lcu="https://127.0.0.1:$port"
rp="https://127.0.0.1:2999/replay"

# --- player participantId (case-insensitive Riot ID match) ------------------
pid="$(jq -r --arg n "$name" --arg t "$tag" '
  .info.participants[]
  | select((.riotIdGameName|ascii_downcase)==($n|ascii_downcase)
       and (.riotIdTagline|ascii_downcase)==($t|ascii_downcase))
  | .participantId' "$dir/match.json")"
[[ -n "$pid" && "$pid" != "null" ]] || { echo "player $rid not in $dir/match.json"; exit 1; }

# --- highlight kill time (ms): biggest gold-jump minute containing a kill ----
kill_ms="$(jq -n \
  --slurpfile frames "$dir/timeline-frames.jsonl" \
  --slurpfile events "$dir/timeline-events.jsonl" \
  --argjson pid "$pid" '
  ($frames | map({t:.timestamp, g:(.participants[]|select(.participantId==$pid)|.totalGold)})) as $g
  | ($events | map(select(.type=="CHAMPION_KILL" and (.killerId==$pid or ((.assistingParticipantIds//[])|index($pid)))) | .timestamp)) as $k
  | [range(1;($g|length)) as $i
     | {t0:$g[$i-1].t, t1:$g[$i].t, d:($g[$i].g - $g[$i-1].g),
        kills:[ $k[] | select(. >= $g[$i-1].t and . < $g[$i].t) ]}]
  | map(select(.kills|length>0)) | sort_by(-.d) | .[0].kills[0] // empty')"
[[ -n "$kill_ms" ]] || { echo "no gold-swing kill found for $rid — skipping highlight"; exit 0; }
kill_s=$(( kill_ms / 1000 ))
seek_s=$(( kill_s - 10 ))   # play from here so it buffers
echo "highlight kill at ${kill_s}s (game time); ~7s lead-in"

# --- always (re)load THIS game's replay (a stale one may be up) --------------
pkill -9 -f "LoL/Game/League" 2>/dev/null; sleep 4
curl -sk --max-time 10 -u "riot:$pass" -X POST "$lcu/lol-replays/v1/rofls/$gid/download/graceful" -H "Content-Type: application/json" -d '{}' >/dev/null
curl -sk --max-time 10 -u "riot:$pass" -X POST "$lcu/lol-replays/v1/rofls/$gid/watch" -H "Content-Type: application/json" -d '{}' >/dev/null
for _ in $(seq 1 30); do [[ "$(curl -sk --max-time 3 -o /dev/null -w '%{http_code}' "$rp/playback" 2>/dev/null)" == "200" ]] && break; sleep 3; done
[[ "$(curl -sk --max-time 3 -o /dev/null -w '%{http_code}' "$rp/playback" 2>/dev/null)" == "200" ]] || { echo "replay API never came up"; exit 1; }

# --- clean shot: HUD off, full vision ---------------------------------------
curl -sk --max-time 5 -X POST "$rp/render" -H "Content-Type: application/json" -d '{"interfaceAll":false,"fogOfWar":false}' -o /dev/null

# --- record button: play, start, wait, stop ---------------------------------
raw="$dir/highlight-raw.webm"; rm -f "$raw"
curl -sk --max-time 5 -X POST "$rp/playback" -H "Content-Type: application/json" -d "{\"time\":$seek_s.0,\"speed\":1.0,\"paused\":false}" -o /dev/null
sleep 3
curl -sk --max-time 6 -X POST "$rp/recording" -H "Content-Type: application/json" -d "{\"recording\":true,\"codec\":\"webm\",\"path\":\"$raw\"}" -o /dev/null
sleep 14   # ~7s lead-in + the play + a few after
curl -sk --max-time 6 -X POST "$rp/recording" -H "Content-Type: application/json" -d '{"recording":false}' -o /dev/null
sleep 4

[[ -s "$raw" ]] || { echo "recording produced no file"; exit 1; }

# --- transcode to a Discord-friendly mp4 (<10MB, faststart) -----------------
ffmpeg -hide_banner -loglevel error -y -i "$raw" -vf "scale=1280:-2,format=yuv420p" \
  -c:v libx264 -crf 26 -preset medium -c:a aac -b:a 96k -movflags +faststart \
  "$dir/highlight.mp4"
rm -f "$raw"
echo "highlight -> $dir/highlight.mp4 ($(du -h "$dir/highlight.mp4" | cut -f1))"
