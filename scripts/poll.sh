#!/usr/bin/env bash
#
# scry poller — watch a list of accounts and post each newly-completed game.
#
# For each account it dumps the latest game (filtered by queue), and if that
# game hasn't been posted yet (no `.posted` marker in its archive dir) runs the
# full pipeline: analyze -> claude -p overview -> post. The archive directory is
# the dedup state; there is no separate database.
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

  local queue_args=()
  [[ "$queue" != "all" ]] && queue_args=(--queue "$queue")

  # Dump the latest game; scry prints the archive dir on stdout.
  local dir
  dir="$($scry --dump "$archive" --riot-id "$rid" --region "$region" --count 1 "${queue_args[@]}" | tail -n1)"
  if [[ -z "$dir" ]]; then
    log "no game for $rid (queue $queue)"
    return
  fi
  if [[ -f "$dir/.posted" ]]; then
    return  # already handled
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
  # Post the package; only mark posted on success.
  if $scry --from-archive "$dir" --riot-id "$rid" \
      --summary "$dir/overview.md" --summary-model "$model" --charts; then
    touch "$dir/.posted"
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
    process_account "$rid" "$region" "$queue"
  done < "$accounts"
}

if [[ "${1:-}" == "--once" ]]; then
  poll_all
  exit 0
fi

log "polling ${accounts} every ${interval}s (default queue ${default_queue})"
while true; do
  poll_all
  sleep "$interval"
done
