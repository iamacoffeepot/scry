mod analysis;
mod archive;
mod cli;
mod discord;
mod journal;
mod rank;
mod riot;
mod stats;
mod tick;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use riven::models::match_v5::Match;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        // Logs to stderr so stdout carries only data (e.g. the archive path).
        .with_writer(std::io::stderr)
        .init();

    run(Cli::parse()).await
}

async fn run(cli: Cli) -> Result<()> {
    // One full poll pass over the watch list (the poll.sh loop body).
    if cli.tick {
        return tick::run(&cli).await;
    }

    // Journal inspection / clip-job hygiene.
    if cli.journal_dump {
        return journal::Journal::open(&cli.journal)?.dump();
    }
    if !cli.abandon_clips.is_empty() {
        return tick::abandon_clips(&cli);
    }

    // Post a packaged embed from a dumped match record — no Riot API calls.
    if let Some(dir) = cli.from_archive.as_deref() {
        return post_from_archive(&cli, dir).await;
    }

    // Run the causal analysis over a dumped match and print the moments.
    if let Some(dir) = cli.analyze.as_deref() {
        return analyze_archive(cli.require_riot_id()?, dir, &tick::roster_riot_ids(&cli));
    }

    let region = cli.region.as_deref().context("--region is required (except with --from-archive)")?;
    let client = riot::Client::new(&cli.api_key, region)?;

    let riot_id = cli.require_riot_id()?;
    let puuid = client
        .resolve_puuid(riot_id)
        .await
        .with_context(|| format!("resolving Riot ID {riot_id}"))?
        .ok_or_else(|| anyhow!("no account found for `{riot_id}`"))?;

    let match_ids = client.recent_match_ids(&puuid, cli.count, cli.queue).await?;
    if match_ids.is_empty() {
        tracing::warn!("no recent matches found for {riot_id}");
        return Ok(());
    }

    // Archive mode: dump raw match + timeline data for offline analysis, no post.
    if let Some(dump_dir) = cli.dump.as_deref() {
        for match_id in match_ids.into_iter().rev() {
            let match_json = client.raw_match_json(&match_id).await?;
            let timeline = client.raw_timeline_json(&match_id).await?;
            let out = archive::write(dump_dir, &match_id, &match_json, &timeline)?;
            tracing::info!(%match_id, path = %out.display(), "archived match data");
            // Emit the archive path on stdout so a poller can consume it.
            println!("{}", out.display());
        }
        return Ok(());
    }

    // Fallback display name/tag if Riot omits them on the participant.
    let (fallback_name, fallback_tag) = riot_id.split_once('#').unwrap_or((riot_id, ""));
    let ctx = stats::RenderContext { region_slug: client.region_slug(), fallback_name, fallback_tag };

    let webhook = discord::Webhook::new(cli.webhook.clone());

    // Oldest -> newest so the channel reads chronologically.
    for match_id in match_ids.into_iter().rev() {
        let game = client.match_detail(&match_id).await?;
        let Some(match_summary) = stats::summarize(&game, &puuid, &ctx) else {
            tracing::warn!(%match_id, "player not found in match participants; skipping");
            continue;
        };
        webhook.post(&discord::stats_message(&match_summary, None, None, None), &[]).await?; // live posting doesn't track the message id
        tracing::info!(%match_id, "posted summary");
    }

    Ok(())
}

/// Read an archived match directory: the parsed `match.json` plus the raw
/// timeline-events JSONL.
fn read_archive(dir: &Path) -> Result<(Match, String)> {
    let raw = fs::read_to_string(dir.join("match.json"))
        .with_context(|| format!("reading {}", dir.join("match.json").display()))?;
    let game: Match = serde_json::from_str(&raw).context("parsing match.json into match-v5")?;
    let events = fs::read_to_string(dir.join("timeline-events.jsonl"))
        .with_context(|| format!("reading {}", dir.join("timeline-events.jsonl").display()))?;
    Ok((game, events))
}

/// The archived participant matching a Riot ID, by PUUID. Riot IDs are
/// case-insensitive; match data stores the registered casing (often
/// lowercase), so compare without case.
fn find_puuid(game: &Match, riot_id: &str) -> Option<String> {
    let (name, tag) = riot_id.split_once('#').unwrap_or((riot_id, ""));
    game.info
        .participants
        .iter()
        .find(|p| {
            p.riot_id_game_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name))
                && p.riot_id_tagline.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(tag))
        })
        .map(|p| p.puuid.clone())
}

/// Compute one tracked player's clip picks for an archived game — assigned
/// jointly across every tracked player in the lobby so a shared teamfight
/// doesn't become N near-identical clips — and journal them as
/// `picks_assigned`, the record the post's captions and the recorder's clip
/// windows read. Returns the appended event.
fn journal_picks(
    journal: &journal::Journal,
    riot_id: &str,
    dir: &Path,
    roster: &[String],
) -> Result<journal::PicksAssigned> {
    let (game, events) = read_archive(dir)?;
    let puuid = find_puuid(&game, riot_id).ok_or_else(|| anyhow!("player {riot_id} not found in {}", dir.display()))?;

    let mut tracked = tracked_puuids(&game, roster);
    if !tracked.contains(&puuid) {
        tracked.push(puuid.clone());
    }
    let picks = analysis::assign_picks(&game, &events, &tracked);
    let (highlight, lowlight) =
        picks.into_iter().find(|(p, _, _)| *p == puuid).map(|(_, h, l)| (h, l)).unwrap_or((None, None));

    let (platform, game_id) = tick::split_match_id(&game.metadata.match_id)
        .with_context(|| format!("archive {} has a malformed match id", dir.display()))?;
    let event = journal::PicksAssigned {
        platform: platform.to_string(),
        game_id,
        riot_id: riot_id.to_string(),
        puuid,
        highlight: highlight.as_ref().map(Into::into),
        lowlight: lowlight.as_ref().map(Into::into),
    };
    journal.append(&event)?;
    Ok(event)
}

/// Run the causal analysis over a dumped match directory and print the
/// classified moments and clip picks. Uses only the archived files — no Riot
/// API, no journal write.
fn analyze_archive(riot_id: &str, dir: &Path, roster: &[String]) -> Result<()> {
    let (game, events) = read_archive(dir)?;
    let puuid = find_puuid(&game, riot_id).ok_or_else(|| anyhow!("player {riot_id} not found in {}", dir.display()))?;

    let moments = analysis::analyze(&game, &events, &puuid);
    for m in &moments {
        use analysis::MomentKind::*;
        let secs = m.t_ms / 1000;
        let tag = match m.kind {
            FightConversion { converted: true, .. } => "fight ✓",
            FightConversion { converted: false, .. } => "fight ✗",
            Death { free: true } => "death ✗",
            Death { free: false } => "death ~",
            Nemesis => "nemesis",
            ObjectiveAbsence => "absent",
            DragonMonopoly { .. } => "dragons",
        };
        println!("{:>3}:{:02}  [{tag}]  {}", secs / 60, secs % 60, m.summary);
        for e in &m.evidence {
            println!("           · {e}");
        }
    }

    let mut tracked = tracked_puuids(&game, roster);
    if !tracked.contains(&puuid) {
        tracked.push(puuid.clone());
    }
    let picks = analysis::assign_picks(&game, &events, &tracked);
    let (highlight, lowlight) =
        picks.into_iter().find(|(p, _, _)| *p == puuid).map(|(_, h, l)| (h, l)).unwrap_or((None, None));
    for (side, pick) in [("highlight", highlight), ("lowlight", lowlight)] {
        match pick {
            Some(c) => {
                println!("{side}: {} — {} (seek {}s, {}s)", analysis::mmss(c.t_ms), c.summary, c.seek_s, c.dur_s)
            }
            None => println!("{side}: none"),
        }
    }
    Ok(())
}

/// The PUUIDs of every watch-list player appearing in this game (Riot IDs
/// compare case-insensitively; the match data stores name and tag at game
/// time).
fn tracked_puuids(game: &Match, roster: &[String]) -> Vec<String> {
    roster
        .iter()
        .filter_map(|riot_id| {
            let (name, tag) = riot_id.split_once('#')?;
            game.info
                .participants
                .iter()
                .find(|p| {
                    p.riot_id_game_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name))
                        && p.riot_id_tagline.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(tag))
                })
                .map(|p| p.puuid.clone())
        })
        .collect()
}

/// Build and post the stats + clips package from a dumped match directory,
/// using the archived `match.json` and the journal's picks for the captions.
async fn post_from_archive(cli: &Cli, dir: &Path) -> Result<()> {
    let raw = fs::read_to_string(dir.join("match.json"))
        .with_context(|| format!("reading {}", dir.join("match.json").display()))?;
    let game: Match = serde_json::from_str(&raw).context("parsing match.json into match-v5")?;

    let riot_id = cli.require_riot_id()?;
    let (name, tag) = riot_id.split_once('#').unwrap_or((riot_id, ""));
    let puuid = find_puuid(&game, riot_id).ok_or_else(|| anyhow!("player {riot_id} not found in {}", dir.display()))?;

    let ctx = stats::RenderContext {
        region_slug: riot::web_region_slug(&game.info.platform_id),
        fallback_name: name,
        fallback_tag: tag,
    };
    let mut match_summary = stats::summarize(&game, &puuid, &ctx)
        .ok_or_else(|| anyhow!("could not summarize {riot_id} from {}", dir.display()))?;

    // The journal records this post/edit and is the only poller state: dedup,
    // clip jobs, message ids, LP baselines, and post-time LP lines all fold
    // out of it.
    let journal = journal::Journal::open(&cli.journal)?;
    let state = journal.fold()?;
    let (platform, game_id) = tick::split_match_id(&game.metadata.match_id)
        .with_context(|| format!("archive {} has a malformed match id", dir.display()))?;

    // LP tracking. On an --edit we must NOT re-fetch or re-diff: the baseline
    // already advanced at post time, so a second diff would read zero. Reuse
    // the LP line the post's game_posted event recorded.
    if cli.edit {
        match_summary.rank = state.rank_line(platform, game_id, riot_id).cloned();
    } else if cli.track_lp
        && let Some(queue) = rank::queue_type(game.info.queue_id)
    {
        let region = game.info.platform_id.to_lowercase();
        let client = riot::Client::new(&cli.api_key, &region)?;
        match client.rank(&puuid, queue).await {
            Ok(Some(current)) => {
                // The journal's last rank_observed for this (puuid, queue) is
                // the baseline; this observation becomes the next one.
                let queue_id = u32::from(game.info.queue_id.0);
                let delta = state.previous_ladder(&puuid, queue_id).map(|prev| current.ladder_value() - prev);
                journal.append(&journal::RankObserved {
                    puuid: puuid.clone(),
                    riot_id: riot_id.to_string(),
                    queue_id,
                    ladder_value: current.ladder_value(),
                    label: current.label(),
                    lp: current.lp,
                })?;
                match_summary.rank = Some(stats::RankInfo { label: current.label(), lp: current.lp, delta });
            }
            Ok(None) => tracing::info!("player is unranked in this queue; no LP line"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "rank fetch failed; skipping LP line"),
        }
    }

    let mut attachments: Vec<discord::Attachment> = Vec::new();

    // Embed this player's Highlight/Lowlight clips if they've been recorded
    // into the archive dir; the unsuffixed names are the legacy fallback for
    // games posted before perspectives split.
    let clip_suffix = format!("-{}", tick::slug(riot_id));
    let mut embed_clip = |base: &str| -> Result<Option<String>> {
        let suffixed = format!("{base}{clip_suffix}.mp4");
        let name = if dir.join(&suffixed).exists() {
            suffixed
        } else if dir.join(format!("{base}.mp4")).exists() {
            format!("{base}.mp4")
        } else {
            return Ok(None);
        };
        let bytes = fs::read(dir.join(&name)).with_context(|| format!("reading {} in {}", name, dir.display()))?;
        attachments.push(discord::Attachment { filename: name.clone(), bytes });
        Ok(Some(name))
    };
    let highlight = embed_clip("highlight")?;
    let lowlight = embed_clip("lowlight")?;

    // An old game's replay dies at Riot's patch boundary — say so on the post
    // instead of leaving the clips forever pending (backfills of a player's
    // stale newest game hit this constantly).
    let clip_note = if highlight.is_none() && lowlight.is_none() {
        match (tick::game_patch(dir), tick::client_replay_patch().await) {
            (Some(game), Some(client)) if game != client => {
                Some("Replay expired (older patch) — no clips for this game")
            }
            _ => None,
        }
    } else {
        None
    };

    // Captions come from the journaled picks for this perspective (absent for
    // a clip recorded before the picks event existed — the clip still embeds,
    // just uncaptioned).
    let picks = state.picks(platform, game_id, riot_id);
    let clip = |filename: Option<String>, pick: Option<&journal::ClipPick>| {
        filename.map(|filename| discord::Clip { filename, caption: pick.map(journal::ClipPick::caption) })
    };
    let highlight = clip(highlight, picks.and_then(|p| p.highlight.as_ref()));
    let lowlight = clip(lowlight, picks.and_then(|p| p.lowlight.as_ref()));
    let message = discord::stats_message(&match_summary, highlight.as_ref(), lowlight.as_ref(), clip_note);

    let webhook = discord::Webhook::new(cli.webhook.clone());
    if cli.edit {
        // Attach the clips to the message the journal recorded at post time.
        let message_id = state.message_id(platform, game_id, riot_id).with_context(|| {
            format!("no posted message in the journal for {riot_id} in {} (was this archive posted?)", dir.display())
        })?;
        webhook.edit(message_id, &message, &attachments).await?;
        journal.append(&journal::ClipsAttached {
            platform: platform.to_string(),
            game_id,
            riot_id: Some(riot_id.to_string()),
        })?;
        tracing::info!(dir = %dir.display(), "edited posted message with clips");
    } else {
        // Initial post: the game_posted event carries the message id the clip
        // pass edits later, plus the rendered LP line the edit reproduces.
        let message_id = webhook.post(&message, &attachments).await?.unwrap_or_default();
        if message_id.is_empty() {
            tracing::warn!("webhook did not return a message id; clips can't be attached later");
        }
        journal.append(&journal::GamePosted {
            platform: platform.to_string(),
            game_id,
            riot_id: riot_id.to_string(),
            queue_id: u32::from(game.info.queue_id.0),
            message_id,
            rank: match_summary.rank.clone(),
            puuid: Some(puuid.clone()),
        })?;
        tracing::info!(dir = %dir.display(), "posted package from archive");
    }
    Ok(())
}
