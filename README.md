# scry

A League of Legends game tracker — pulls match data from the Riot API, computes
running statistics, and publishes summaries to Discord via webhook.

## Status

Pre-design. The shape of the tracker (stack, data model, hosting, what a "summary"
contains) is still being worked out. Nothing here is committed yet.

## What it will do (sketch)

- Watch one or more tracked summoners for newly completed matches.
- Pull match + timeline data from the Riot API.
- Compute per-game and rolling statistics (KDA, win rate, champion performance, …).
- Post a formatted summary to a Discord channel through an incoming webhook.

Built collaboratively with Claude.
