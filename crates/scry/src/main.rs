mod analysis;
mod archive;
mod charts;
mod cli;
mod discord;
mod journal;
mod rank;
mod riot;
mod stats;
mod summary;
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

    // One-shot cutover from the marker-file state to the journal.
    if cli.journal_import {
        return tick::import_legacy_state(&cli);
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
        return analyze_archive(cli.require_riot_id()?, dir);
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

/// Run the causal analysis over a dumped match directory and print the
/// classified moments. Uses only the archived files — no Riot API.
fn analyze_archive(riot_id: &str, dir: &Path) -> Result<()> {
    let raw = fs::read_to_string(dir.join("match.json"))
        .with_context(|| format!("reading {}", dir.join("match.json").display()))?;
    let game: Match = serde_json::from_str(&raw).context("parsing match.json into match-v5")?;
    let events = fs::read_to_string(dir.join("timeline-events.jsonl"))
        .with_context(|| format!("reading {}", dir.join("timeline-events.jsonl").display()))?;

    let (name, tag) = riot_id.split_once('#').unwrap_or((riot_id, ""));
    let puuid = game
        .info
        .participants
        .iter()
        .find(|p| {
            // Riot IDs are case-insensitive; match data stores the registered
            // casing (often lowercase), so compare without case.
            p.riot_id_game_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name))
                && p.riot_id_tagline.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(tag))
        })
        .map(|p| p.puuid.clone())
        .ok_or_else(|| anyhow!("player {} not found in {}", riot_id, dir.display()))?;

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

    // Write the grounded-facts brief the OVERVIEW prompt consumes, including the
    // Highlight/Lowlight clip candidates it chooses a timestamp from.
    let (highlights, lowlights) = analysis::clip_candidates(&game, &events, &puuid);
    let md = analysis::render_moments_md(&moments, &highlights, &lowlights, name);
    let suffix = format!("-{}", tick::slug(riot_id));
    let out = dir.join(format!("moments{suffix}.md"));
    fs::write(&out, md).with_context(|| format!("writing {}", out.display()))?;

    // Sidecar the per-candidate clip windows (seek + duration) keyed by m:ss, so
    // highlight.sh records a window sized to each play rather than a flat length.
    let clips = analysis::render_clips_json(&highlights, &lowlights);
    fs::write(dir.join(format!("clips{suffix}.json")), clips)
        .with_context(|| format!("writing clips{suffix}.json in {}", dir.display()))?;

    // The deterministic clip pick: best-scored highlight + lowlight become the
    // `## Highlight` / `## Lowlight` sections highlight.sh reads timestamps
    // from and the post uses as clip captions — all per player, since picks
    // differ per tracked perspective in a shared game.
    fs::write(dir.join(format!("overview{suffix}.md")), analysis::render_overview_md(&highlights, &lowlights))
        .with_context(|| format!("writing overview{suffix}.md in {}", dir.display()))?;

    println!("\n{} moments -> {}", moments.len(), out.display());
    Ok(())
}

/// Build and post the stats + coach package from a dumped match directory,
/// using only the archived `match.json` (and an optional `--summary` file).
async fn post_from_archive(cli: &Cli, dir: &Path) -> Result<()> {
    let raw = fs::read_to_string(dir.join("match.json"))
        .with_context(|| format!("reading {}", dir.join("match.json").display()))?;
    let game: Match = serde_json::from_str(&raw).context("parsing match.json into match-v5")?;

    let riot_id = cli.require_riot_id()?;
    let (name, tag) = riot_id.split_once('#').unwrap_or((riot_id, ""));
    let player = game
        .info
        .participants
        .iter()
        .find(|p| {
            // Riot IDs are case-insensitive; match data stores the registered
            // casing (often lowercase), so compare without case.
            p.riot_id_game_name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name))
                && p.riot_id_tagline.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(tag))
        })
        .ok_or_else(|| anyhow!("player {riot_id} not found in {}", dir.display()))?;
    let puuid = player.puuid.clone();

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
        match_summary.rank = state.rank_line(platform, game_id).cloned();
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

    // Render the dashboard first so the message can reference the attachment.
    let mut attachments: Vec<discord::Attachment> = Vec::new();
    let chart = if cli.charts {
        let charts_dir = dir.join("charts");
        fs::create_dir_all(&charts_dir).with_context(|| format!("creating {}", charts_dir.display()))?;
        let frames = fs::read_to_string(dir.join("timeline-frames.jsonl"))
            .with_context(|| format!("reading {}", dir.join("timeline-frames.jsonl").display()))?;
        let events = fs::read_to_string(dir.join("timeline-events.jsonl"))
            .with_context(|| format!("reading {}", dir.join("timeline-events.jsonl").display()))?;
        let png_path = charts_dir.join("dashboard.png");
        charts::dashboard(&game, &frames, &events, &puuid, &png_path)?;
        let bytes = fs::read(&png_path).with_context(|| format!("reading {}", png_path.display()))?;
        attachments.push(discord::Attachment { filename: "dashboard.png".to_string(), bytes });
        Some("dashboard.png")
    } else {
        None
    };

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
    let (highlight, lowlight) = (highlight.as_deref(), lowlight.as_deref());

    // With an overview, stats + separator + overview are one CV2 container.
    // --no-overview renders a minimal header + stats + clips embed instead
    // (the summary is still parsed, for the clip captions).
    let message = if let Some(summary_path) = cli.summary.as_deref() {
        let md =
            fs::read_to_string(summary_path).with_context(|| format!("reading summary {}", summary_path.display()))?;
        let summary = summary::parse(&md);
        let model = (!cli.summary_model.is_empty()).then_some(cli.summary_model.as_str());
        if cli.no_overview {
            discord::clips_message(&match_summary, &summary, model, highlight, lowlight)
        } else {
            discord::combined_message(&match_summary, &summary, model, chart, highlight, lowlight)
        }
    } else {
        discord::stats_message(&match_summary, chart, highlight, lowlight)
    };

    let webhook = discord::Webhook::new(cli.webhook.clone());
    if cli.edit {
        // Attach the clips to the message the journal recorded at post time.
        let message_id = state.message_id(platform, game_id).with_context(|| {
            format!("no posted message in the journal for {} (was this archive posted?)", dir.display())
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
