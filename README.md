# scry

A League of Legends game tracker for your Discord. It watches a list of
players, and when one of them finishes a game it posts a package to a webhook:
the stat line, the LP change, and two short replay clips — the **Highlight**
and the **Lowlight** — chosen by a deterministic analysis of the match
timeline.

## How it works

1. **Poll.** Every few minutes the poller asks the Riot API for each tracked
   player's newest match. A game is posted once, keyed by the player's PUUID,
   so renames and shared games are handled.
2. **Analyze.** The match timeline is folded into *moments* (fights, picks,
   objective secures, throws) and scored: duel intensity from the kill damage
   ledger, priority-target picks weighted by the victim's role and form,
   objective conversions, closing fights. The best and worst moments for each
   tracked player become clip windows.
3. **Post.** The stats, rank, LP delta, and captions go out as a Discord
   webhook message.
4. **Clip.** The League client downloads the replay, the replay API seeks to
   each window with the camera locked on the player, the game records it, and
   ffmpeg transcodes it. The clips are attached to the post by editing it.

All state is an append-only SQLite journal (`state/scry.sqlite`) of typed
events; everything the poller knows (what was posted, which clips are pending,
LP baselines) is a fold over that log. The archive under `archive/` holds
raw Riot data and the recorded videos and is never read as state.

## Requirements

- Rust 1.85 or newer (edition 2024).
- A Riot API key from <https://developer.riotgames.com>. Development keys
  expire every 24 hours; apply for a personal key if you run this for real.
- A Discord incoming-webhook URL (Server Settings → Integrations → Webhooks).
- For clips: the League of Legends client installed and logged in on the
  tracked players' region, `EnableReplayApi=1` in the game config, and
  `ffmpeg` on the path. Recording is real-time and needs the client idle, so
  it only runs when you are not in a game. Without a client, run with
  `--no-clips` and you still get the stats and captions.
- Clips are developed and run on macOS. Windows has a port of the recorder
  and the poll loop (`scripts/highlight.ps1`, `scripts/poll.ps1`) that has
  not been run on a Windows machine yet; a custom League install path goes in
  `SCRY_LOL_LOCKFILE`. Reports welcome.

## Quick start

```sh
git clone https://github.com/iamacoffeepot/scry && cd scry
cp .env.example .env                                  # RIOT_API_KEY and SCRY_DISCORD_WEBHOOK
cp scripts/accounts.example.txt scripts/accounts.txt  # one `RiotID#Tag | region | queues` per line
cargo build --release
scripts/poll.sh --once                                # one pass: post new games, record pending clips
```

`scripts/poll.sh` loops that pass every `SCRY_INTERVAL` seconds (default 300)
and documents the full environment surface in its header. On Windows,
`powershell -File scripts\poll.ps1 -Once` is the same pass, and without
`-Once` it is the loop.

## Managing the watch list

The watch list is `scripts/accounts.txt`, one player per line:

```
RiotID#Tag | region | queues
```

`region` is a platform code (`na1`, `euw1`, `eun1`, `kr`, `br1`, ...) and
`queues` is a comma-separated list of queue ids (`420` ranked solo, `440`
ranked flex, `710` ranked 5v5 premade, `400` normal draft, `450` ARAM) or
`all`. You can edit the file by hand, or let `cargo xtask` do it, which also
validates the Riot ID and refuses duplicates:

```sh
cargo xtask add-account "Faker#KR1" --region kr --queues 420   # defaults: --region na1, --queues 420,440,710
cargo xtask remove-account "Faker#KR1"
cargo xtask rename-account "Faker#KR1" "Faker#T1"              # after a Riot ID change
cargo xtask list-accounts
```

The running poller reads the file at the start of every pass, so a change
takes effect on the next pass with no restart. A newly added player backfills
exactly one game (their most recent), then only games that finish after that
are posted. A renamed player keeps their history through the PUUID; the new
name may backfill its newest game once.

## Running as a service (macOS)

`cargo xtask start` installs `scripts/poll.sh` as a launchd agent that keeps
the poller alive across logins, with the release binary and the `.env`
secrets baked into the job. Then:

```sh
cargo xtask status        # service state, journal summary, pending clip jobs
cargo xtask logs -n 100   # tail the poller log
cargo xtask restart       # after `cargo build --release` or editing .env
cargo xtask stop
```

The service runs the binary at `target/release/scry`, so rebuild before
restarting. Editing the watch list needs no restart.

One-shot commands:

```sh
cargo run --release -p scry -- --riot-id "Faker#KR1" --region kr --count 3      # post a player's last 3 games
cargo run --release -p scry -- --analyze archive/KR/<id> --riot-id "Faker#KR1"  # print moments and picks, post nothing
cargo run --release -p scry -- --journal-dump                                   # the journal as JSON lines
```

## Things to know

- Replays are patch-gated: once the client updates, older games can no longer
  be recorded. The post is edited to say so instead of waiting forever.
- Only one replay can run at a time, so clips are recorded one game at a time
  and every tracked player in a shared game is served from the same replay.
- Riot encrypts PUUIDs per API key. Swapping keys re-keys every account; the
  journal reconciles the old and new identities, so history is not reposted.
- Tested on NA with ranked solo, flex, and the 5v5 premade queue.

## Layout

- `crates/scry` — the binary. `cli` (args) → `tick` (poll pass + clip pass) →
  `riot` (API wrapper) → `analysis` (moments + joint clip picks) →
  `journal` (event log + fold) → `stats` / `rank` (per-game summary, LP) →
  `discord` (webhook messages).
- `scripts/highlight.sh` — loads a game's replay once and records each
  perspective's clip windows; `scripts/highlight.ps1` is its Windows port.
- `xtask` — the operational commands behind `cargo xtask`.
- `vendor/` — a snapshot of the data layer from [aether](https://github.com/iamacoffeepot/aether)
  that the journal's event kinds are written in; see `vendor/README.md`.
- `docs/game-analysis.md` — the analysis design: what a moment is, how picks
  are scored, and why.

## Contributing and privacy

The tree never carries a real player. Test fixtures and examples use pro
players' public Riot IDs or invented ones, match ids in docs are made up, and
the analysis validation game lives outside the repository (`SCRY_FIXTURE_DIR`
and `SCRY_FIXTURE_PUUID` point the tests at a local archive). A pre-commit
hook enforces the rule; enable it once per clone:

```sh
git config core.hooksPath .githooks
```

It refuses a commit that adds a Riot ID from your watch list, a value from
`.env`, a PUUID-shaped token, or a home-directory path, and it requires
commits to be stamped in UTC (`TZ=UTC git commit`).

## License

MIT.
