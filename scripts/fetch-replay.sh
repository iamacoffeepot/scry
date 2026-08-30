#!/usr/bin/env bash
#
# Prototype: download a League replay (.rofl) by gameId via the local LCU
# replay API — to validate that arbitrary (non-participant) replays are
# downloadable. Requires the League CLIENT running and logged in on na1, on the
# current patch. gameId is the numeric part of the match id
# (e.g. 5592707294 from NA1_5592707294).
#
# Usage: scripts/fetch-replay.sh <gameId>
#
# Verbose on purpose: the /lol-replays payloads are undocumented and vary by
# client version, so we print every response to see the real shapes.

set -uo pipefail

game_id="${1:?usage: fetch-replay.sh <gameId>}"

# The lockfile lives in the install dir (varies by platform/install).
lockfile=""
for cand in \
  "/Applications/League of Legends.app/Contents/LoL/lockfile" \
  "$HOME/Library/Application Support/Riot Games/League of Legends/lockfile"; do
  [[ -f "$cand" ]] && { lockfile="$cand"; break; }
done
if [[ -z "$lockfile" ]]; then
  echo "No lockfile found. Is the League CLIENT (home screen) running and logged in?"
  exit 1
fi
echo "lockfile: $lockfile"

# Lockfile format: name:pid:port:password:protocol
IFS=: read -r _name _pid port password proto < "$lockfile"
base="$proto://127.0.0.1:$port"
echo "LCU: $base"

# -k: LCU uses a self-signed cert. Basic auth user is literally "riot".
lcu() { curl -sk -u "riot:$password" -w "\n[http %{http_code}]\n" "$@"; }

replays_dir="$HOME/Library/Application Support/Riot Games/League of Legends/Replays"

echo "=== 0. sanity: current summoner (confirms logged in) ==="
lcu "$base/lol-summoner/v1/current-summoner"

echo "=== 1. replay config (does this client build allow downloads?) ==="
lcu "$base/lol-replays/v1/configuration"

echo "=== 2. metadata for $game_id (state before download) ==="
lcu "$base/lol-replays/v2/metadata/$game_id"
lcu "$base/lol-replays/v1/metadata/$game_id"

echo "=== 3. trigger download ==="
lcu -X POST "$base/lol-replays/v1/rofls/$game_id/download/graceful" \
    -H "Content-Type: application/json" -d "{}"

echo "=== 4. poll download state (up to ~60s) ==="
for _ in $(seq 1 30); do
  out="$(curl -sk -u "riot:$password" "$base/lol-replays/v1/rofls/$game_id/download")"
  echo "$out"
  echo "$out" | grep -qiE '"(state|downloadStatus)"\s*:\s*"(watch|downloaded|complete)"' && { echo "-> downloaded"; break; }
  sleep 2
done

echo "=== 5. .rofl on disk? ==="
ls -la "$replays_dir" 2>/dev/null | grep -i "$game_id" || echo "no .rofl matching $game_id in $replays_dir"
