# scry

A League of Legends game tracker — polls the Riot API for tracked players'
newly-completed games, runs a grounded causal analysis over each match
timeline, records replay clips of the decisive moments, and publishes the
package (stats + captioned Highlight/Lowlight clips + LP delta) to Discord.

## State

The append-only SQLite journal (`state/scry.sqlite`) is the only state. Every
event is a storage-shaped kind (`scry.journal.*` — content-hashed field tags,
unknown-field bucket), and read-side state is a pure fold over the events:
posting dedup, clip jobs, Discord message ids, LP baselines, post-time rank
lines, and clip picks all come out of the fold. The archive
(`archive/<platform>/<game_id>/`) holds only raw Riot data (match + timeline
dumps) and the recorded clip videos — nothing under it is parsed as state.

## Running

Requires a Riot API key and a Discord incoming-webhook URL in `.env` (see
`scripts/poll.sh` for the full env surface). The poller is a launchd service
(`com.scry.poller`) looping `scry --tick`; `cargo xtask` wraps the ops surface
(`status`, `start`, `stop`, `logs`, account add/remove/rename).

One-shot debugging:

```sh
cargo run -p scry -- --riot-id "Faker#KR1" --region kr --count 3   # live post
cargo run -p scry -- --analyze archive/NA1/<id> --riot-id "…"      # print moments + picks
cargo run -p scry -- --journal-dump                                # journal as JSONL
```

## Layout

- `crates/scry` — the binary. `cli` (args) → `tick` (poll pass + clip pass) →
  `riot` (riven wrapper) → `analysis` (causal moments + joint clip picks) →
  `journal` (storage-kind event log + fold) → `stats`/`rank` (per-game summary,
  LP) → `discord` (Components-V2 webhook messages).
- `scripts/highlight.sh` — loads a game's replay once and records each
  perspective's clip windows (handed as arguments by the clip pass).

Built collaboratively with Claude.
