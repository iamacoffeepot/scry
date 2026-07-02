#!/usr/bin/env bash
#
# scry poller — watch a list of accounts and post each newly-completed game.
#
# For each account it dumps the latest game (filtered by queue), and if that
# game hasn't been posted for that player yet (no `.posted-<player>` marker) runs the
# full pipeline: analyze -> claude -p overview -> post. Highlight/Lowlight video
# clips are recorded separately by clips_pass() — a serialized, idle-only step
# that records each game's replay one at a time and edits the already-posted
# message to attach them (a replay can't be driven mid-game and isn't available
# the instant a game ends). The archive directory is the dedup state; there is
# no separate database.
#
# Usage:
#   scripts/poll.sh          # loop forever, polling every $SCRY_INTERVAL seconds
#   scripts/poll.sh --once   # a single pass over all accounts, then exit
#
# Config (env, or a .env at the repo root):
#   RIOT_API_KEY, SCRY_DISCORD_WEBHOOK   required
#   SCRY_ACCOUNTS   watch-list file        (default: scripts/accounts.txt)
#   SCRY_ARCHIVE    archive root            (default: archive)
#   SCRY_INTERVAL   seconds between passes  (default: 600)
#   SCRY_QUEUE      default queue id        (default: 420 = ranked solo)
#   SCRY_SUMMARY_MODEL  footer model label  (default: Claude Opus 4.8)
#   SCRY_CLIPS      record replay clips 1/0  (default: 1; needs League client)
#   CLIP_MAX_TRIES  clip retries before giving up on a game (default: 15)
#   SCRY_BIN        scry invocation         (default: cargo run --quiet --)

set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Load secrets / config from .env if present.
if [[ -f .env ]]; then set -a; source .env; set +a; fi

accounts="${SCRY_ACCOUNTS:-scripts/accounts.txt}"
archive="${SCRY_ARCHIVE:-archive}"
interval="${SCRY_INTERVAL:-600}"
default_queue="${SCRY_QUEUE:-420}"
model="${SCRY_SUMMARY_MODEL:-Claude Opus 4.8}"
scry="${SCRY_BIN:-cargo run --quiet --}"

log() { echo "[$(date +%H:%M:%S)] $*" >&2; }

# process_account <riot_id> <region> <queue>
process_account() {
  local rid="$1" region="$2" queue="$3"
  # Per-account dedup marker: a game shared by two tracked players is posted
  # once per player (each gets their own centered overview).
  local slug
  slug="$(printf '%s' "$rid" | tr -c 'A-Za-z0-9' '_')"

  local queue_args=()
  [[ "$queue" != "all" ]] && queue_args=(--queue "$queue")

  # Dump the latest game; scry prints the archive dir on stdout.
  local dir
  dir="$($scry --dump "$archive" --riot-id "$rid" --region "$region" --count 1 "${queue_args[@]}" | tail -n1)"
  if [[ -z "$dir" ]]; then
    log "no game for $rid (queue $queue)"
    return
  fi
  local marker="$dir/.posted-$slug"
  if [[ -f "$marker" ]]; then
    return  # already handled for this player
  fi
  log "new game for $rid -> $dir"

  # Grounded moments -> moments.md in the archive dir.
  if ! $scry --analyze "$dir" --riot-id "$rid" >/dev/null; then
    log "analyze failed for $rid"; return
  fi
  # AI overview from the grounded facts.
  if ! claude -p --model opus \
      --add-dir "$dir" \
      --allowedTools "Read Grep Glob" \
      --system-prompt "$(cat "$root/prompts/OVERVIEW.md")" \
      "Write the post-game overview centered on $rid. Start from moments.md in the provided directory." \
      > "$dir/overview.md"; then
    log "overview failed for $rid"; return
  fi
  # Post the package now (no clips yet). scry records the message id in
  # <dir>/.message-id; the clip pass later records the clips and edits the
  # message to attach them. Clips are decoupled from posting because a replay
  # can't be driven while a game is live and isn't downloadable the instant a
  # game ends — so they must run serially, only when the client is idle.
  if $scry --from-archive "$dir" --riot-id "$rid" \
      --summary "$dir/overview.md" --summary-model "$model" --no-overview --track-lp; then
    touch "$marker"
    # Remember who this post centers on so the clip pass can rebuild it. (One
    # clip job per game: for a game shared by two tracked players, the clips
    # follow the last one posted — a rare case we accept for now.)
    printf '%s' "$rid" > "$dir/.clip-rid"
    log "posted $rid"
  else
    log "post failed for $rid"
  fi
}

poll_all() {
  if [[ ! -f "$accounts" ]]; then
    log "no accounts file at $accounts (see scripts/accounts.example.txt)"
    return
  fi
  while IFS= read -r line; do
    line="${line#"${line%%[![:space:]]*}"}"  # ltrim
    line="${line%"${line##*[![:space:]]}"}"  # rtrim
    # Skip blank and full-line comments. `#` is NOT an inline comment here —
    # Riot IDs contain it (gameName#tagLine).
    [[ -z "$line" || "$line" == \#* ]] && continue

    IFS='|' read -r rid region queue <<< "$line"
    rid="$(echo "$rid" | xargs)"
    region="$(echo "$region" | xargs)"
    queue="$(echo "${queue:-}" | xargs)"
    queue="${queue:-$default_queue}"
    if [[ -z "$rid" || -z "$region" ]]; then
      log "skipping malformed line: $line"; continue
    fi
    # A line may list several queues (e.g. "420,440") — scan each.
    IFS=',' read -ra queues <<< "$queue"
    for q in "${queues[@]}"; do
      q="$(echo "$q" | xargs)"
      [[ -n "$q" ]] && process_account "$rid" "$region" "$q"
    done
  done < "$accounts"
}

# The League client's gameflow phase (empty if the client is down/unreachable).
client_phase() {
  local lf="/Applications/League of Legends.app/Contents/LoL/lockfile"
  [[ -f "$lf" ]] || return
  local port pass
  IFS=: read -r _ _ port pass _ < "$lf"
  curl -sk --max-time 5 -u "riot:$pass" \
    "https://127.0.0.1:$port/lol-gameflow/v1/gameflow-phase" 2>/dev/null
}

# Record + attach the Highlight/Lowlight clips for posted games that don't have
# them yet. Runs ONLY when the client is idle (never mid-game) and drives ONE
# game per pass, so the single replay is never contended — the cause of the
# earlier wrong-clip/missing-clip bugs. A failure (replay not yet available)
# leaves the job pending to retry next pass, up to CLIP_MAX_TRIES.
CLIP_MAX_TRIES="${CLIP_MAX_TRIES:-15}"
clips_pass() {
  [[ "${SCRY_CLIPS:-1}" == "1" ]] || return
  local ridf dir rid tries
  # Newest game first (sort -r on the paths = descending game id): the latest
  # post gets its clips promptly, and a stuck/broken old replay can't starve the
  # newer ones behind it. Attempt each; a failure `continue`s to the next rather
  # than blocking the whole pass. Stop after one SUCCESS (keeps polling snappy).
  for ridf in $(ls -1 "$archive"/*/*/.clip-rid 2>/dev/null | sort -r); do
    dir="$(dirname "$ridf")"
    [[ -f "$dir/.clips-done" ]] && continue
    [[ -f "$dir/.message-id" && -f "$dir/overview.md" ]] || continue
    rid="$(cat "$ridf")"; [[ -n "$rid" ]] || continue

    # Only drive a replay when the client is idle (None/Lobby). Re-checked every
    # iteration — a pass can take minutes and the user may start a game. If the
    # client is down/unreachable client_phase is empty, so we bail here WITHOUT
    # burning a try (that's the common "League is closed" case).
    case "$(client_phase)" in *None*|*Lobby*) : ;; *) return ;; esac

    tries="$(cat "$dir/.clips-tries" 2>/dev/null || echo 0)"
    if (( tries >= CLIP_MAX_TRIES )); then
      log "giving up on clips for $rid after $tries tries -> $dir"
      touch "$dir/.clips-done"; continue
    fi
    printf '%s' "$((tries + 1))" > "$dir/.clips-tries"

    log "recording clips for $rid -> $dir (try $((tries + 1)))"
    if ! scripts/highlight.sh "$dir"; then
      log "clips not ready for $rid (will retry)"; continue
    fi
    if [[ ! -f "$dir/highlight.mp4" && ! -f "$dir/lowlight.mp4" ]]; then
      log "no clips produced for $rid (will retry)"; continue
    fi
    # Edit the existing message to attach the clips (reuses post-time LP; no API).
    if $scry --from-archive "$dir" --riot-id "$rid" \
        --summary "$dir/overview.md" --summary-model "$model" --no-overview --edit; then
      touch "$dir/.clips-done"
      log "clips attached for $rid"
      return  # one successful clip per pass; the next pass takes the next game
    else
      log "clip edit failed for $rid (will retry)"; continue
    fi
  done
}

if [[ "${1:-}" == "--once" ]]; then
  poll_all
  clips_pass
  exit 0
fi

log "polling ${accounts} every ${interval}s (default queue ${default_queue})"
while true; do
  poll_all
  clips_pass
  sleep "$interval"
done
