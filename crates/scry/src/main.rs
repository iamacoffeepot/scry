mod analysis;
mod archive;
mod charts;
mod cli;
mod discord;
mod rank;
mod riot;
mod stats;
mod summary;

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
    // Post a packaged embed from a dumped match record — no Riot API calls.
    if let Some(dir) = cli.from_archive.as_deref() {
        return post_from_archive(&cli, dir).await;
    }

    // Run the causal analysis over a dumped match and print the moments.
    if let Some(dir) = cli.analyze.as_deref() {
        return analyze_archive(&cli.riot_id, dir);
    }

    let region = cli
        .region
        .as_deref()
        .context("--region is required (except with --from-archive)")?;
    let client = riot::Client::new(&cli.api_key, region)?;

    let puuid = client
        .resolve_puuid(&cli.riot_id)
        .await
        .with_context(|| format!("resolving Riot ID {}", cli.riot_id))?;

    let match_ids = client.recent_match_ids(&puuid, cli.count, cli.queue).await?;
    if match_ids.is_empty() {
        tracing::warn!("no recent matches found for {}", cli.riot_id);
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
    let (fallback_name, fallback_tag) = cli
        .riot_id
        .split_once('#')
        .unwrap_or((cli.riot_id.as_str(), ""));
    let ctx = stats::RenderContext {
        region_slug: client.region_slug(),
        fallback_name,
        fallback_tag,
    };

    let webhook = discord::Webhook::new(cli.webhook.clone());

    // Oldest -> newest so the channel reads chronologically.
    for match_id in match_ids.into_iter().rev() {
        let game = client.match_detail(&match_id).await?;
        let Some(match_summary) = stats::summarize(&game, &puuid, &ctx) else {
            tracing::warn!(%match_id, "player not found in match participants; skipping");
            continue;
        };
        webhook
            .post(&discord::stats_message(&match_summary, None, None, None), &[])
            .await?; // live posting doesn't track the message id
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
            p.riot_id_game_name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                && p.riot_id_tagline
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case(tag))
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
    let out = dir.join("moments.md");
    fs::write(&out, md).with_context(|| format!("writing {}", out.display()))?;
    println!("\n{} moments -> {}", moments.len(), out.display());
    Ok(())
}

/// Build and post the stats + coach package from a dumped match directory,
/// using only the archived `match.json` (and an optional `--summary` file).
async fn post_from_archive(cli: &Cli, dir: &Path) -> Result<()> {
    let raw = fs::read_to_string(dir.join("match.json"))
        .with_context(|| format!("reading {}", dir.join("match.json").display()))?;
    let game: Match = serde_json::from_str(&raw).context("parsing match.json into match-v5")?;

    let (name, tag) = cli
        .riot_id
        .split_once('#')
        .unwrap_or((cli.riot_id.as_str(), ""));
    let player = game
        .info
        .participants
        .iter()
        .find(|p| {
            // Riot IDs are case-insensitive; match data stores the registered
            // casing (often lowercase), so compare without case.
            p.riot_id_game_name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
                && p.riot_id_tagline
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case(tag))
        })
        .ok_or_else(|| anyhow!("player {} not found in {}", cli.riot_id, dir.display()))?;
    let puuid = player.puuid.clone();

    let ctx = stats::RenderContext {
        region_slug: riot::web_region_slug(&game.info.platform_id),
        fallback_name: name,
        fallback_tag: tag,
    };
    let mut match_summary = stats::summarize(&game, &puuid, &ctx)
        .ok_or_else(|| anyhow!("could not summarize {} from {}", cli.riot_id, dir.display()))?;

    // LP tracking. On an --edit we must NOT re-fetch or re-diff: the snapshot
    // was already advanced at post time, so a second diff would read zero. Reuse
    // the rank/LP computed then, persisted in <dir>/.rank.json.
    let rank_path = dir.join(".rank.json");
    if cli.edit {
        if let Ok(json) = fs::read_to_string(&rank_path) {
            match_summary.rank = serde_json::from_str(&json).ok();
        }
    } else if cli.track_lp
        && let Some(queue) = rank::queue_type(game.info.queue_id)
    {
        let region = game.info.platform_id.to_lowercase();
        let client = riot::Client::new(&cli.api_key, &region)?;
        match client.rank(&puuid, queue).await {
            Ok(Some(current)) => {
                let snap = cli
                    .state_dir
                    .join(format!("{}_{}.json", puuid, game.info.queue_id.0));
                let delta = rank::read_previous(&snap).map(|prev| current.ladder_value() - prev);
                if let Err(e) = rank::write_snapshot(&snap, &current) {
                    tracing::warn!(error = %e, "writing LP snapshot failed");
                }
                let info = stats::RankInfo {
                    label: current.label(),
                    lp: current.lp,
                    delta,
                };
                // Persist so a later --edit renders the identical LP line.
                if let Ok(json) = serde_json::to_string(&info) {
                    let _ = fs::write(&rank_path, json);
                }
                match_summary.rank = Some(info);
            }
            Ok(None) => tracing::info!("player is unranked in this queue; no LP line"),
            Err(e) => tracing::warn!(error = %e, "rank fetch failed; skipping LP line"),
        }
    }

    // Render the dashboard first so the message can reference the attachment.
    let mut attachments: Vec<discord::Attachment> = Vec::new();
    let chart = if cli.charts {
        let charts_dir = dir.join("charts");
        fs::create_dir_all(&charts_dir)
            .with_context(|| format!("creating {}", charts_dir.display()))?;
        let frames = fs::read_to_string(dir.join("timeline-frames.jsonl"))
            .with_context(|| format!("reading {}", dir.join("timeline-frames.jsonl").display()))?;
        let events = fs::read_to_string(dir.join("timeline-events.jsonl"))
            .with_context(|| format!("reading {}", dir.join("timeline-events.jsonl").display()))?;
        let png_path = charts_dir.join("dashboard.png");
        charts::dashboard(&game, &frames, &events, &puuid, &png_path)?;
        let bytes = fs::read(&png_path).with_context(|| format!("reading {}", png_path.display()))?;
        attachments.push(discord::Attachment {
            filename: "dashboard.png".to_string(),
            bytes,
        });
        Some("dashboard.png")
    } else {
        None
    };

    // Embed the Highlight/Lowlight clips if they've been recorded into the
    // archive dir (kept as first-class artifacts next to match.json / moments.md).
    let mut embed_clip = |name: &'static str| -> Result<Option<&'static str>> {
        let path = dir.join(name);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        attachments.push(discord::Attachment {
            filename: name.to_string(),
            bytes,
        });
        Ok(Some(name))
    };
    let highlight = embed_clip("highlight.mp4")?;
    let lowlight = embed_clip("lowlight.mp4")?;

    // With an overview, stats + separator + overview are one CV2 container.
    let message = if let Some(summary_path) = cli.summary.as_deref() {
        let md = fs::read_to_string(summary_path)
            .with_context(|| format!("reading summary {}", summary_path.display()))?;
        discord::combined_message(
            &match_summary,
            &summary::parse(&md),
            &cli.summary_model,
            chart,
            highlight,
            lowlight,
        )
    } else {
        discord::stats_message(&match_summary, chart, highlight, lowlight)
    };

    let webhook = discord::Webhook::new(cli.webhook.clone());
    let mid_path = dir.join(".message-id");
    if cli.edit {
        // Attach the clips to the message we already posted for this archive.
        let message_id = fs::read_to_string(&mid_path)
            .with_context(|| format!("reading {} (was this archive posted?)", mid_path.display()))?;
        webhook.edit(message_id.trim(), &message, &attachments).await?;
        tracing::info!(dir = %dir.display(), "edited posted message with clips");
    } else {
        // Initial post: record the message id so the clip pass can edit it.
        if let Some(id) = webhook.post(&message, &attachments).await? {
            fs::write(&mid_path, &id)
                .with_context(|| format!("writing {}", mid_path.display()))?;
        } else {
            tracing::warn!("webhook did not return a message id; clips can't be attached later");
        }
        tracing::info!(dir = %dir.display(), "posted package from archive");
    }
    Ok(())
}
