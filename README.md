# scry

A League of Legends game tracker — pulls match data from the Riot API, computes
running statistics, and publishes summaries to Discord via webhook.

## Status

MVP: a one-shot CLI that fetches a player's recent matches from the Riot API,
computes a per-game stat summary (including a vision/warding block), and posts a
Discord embed per game. The always-on poller and persistence layer come next.

## Usage

Requires a Riot API key and a Discord incoming-webhook URL. Copy `.env.example`
to `.env`, or pass them as flags / environment variables.

```sh
export RIOT_API_KEY=RGAPI-...
export SCRY_DISCORD_WEBHOOK=https://discord.com/api/webhooks/...

cargo run -p scry -- --riot-id "Faker#KR1" --region kr --count 3
```

Flags: `--riot-id <gameName#tagLine>`, `--region <na1|euw1|kr|…>`,
`--count <N>` (matches to summarize), `--webhook <url>` (or `SCRY_DISCORD_WEBHOOK`),
`--api-key <key>` (or `RIOT_API_KEY`).

## Layout

- `crates/scry` — the binary. `cli` (args) → `riot` (riven wrapper) → `stats`
  (per-game summary) → `discord` (webhook embed).

## Roadmap

- Long-running poller: watch tracked summoners, post only newly completed matches.
- SQLite persistence: last-seen match per summoner + match history for rolling stats.
- Timeline-derived stats: laning gold/XP/CS diffs, ward cadence.
- Derived stats: LP deltas, lobby-relative ranking, personal baselines.

Built collaboratively with Claude.
