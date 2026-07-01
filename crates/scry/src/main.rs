mod archive;
mod charts;
mod cli;
mod discord;
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
        .init();

    run(Cli::parse()).await
}

async fn run(cli: Cli) -> Result<()> {
    // Post a packaged embed from a dumped match record — no Riot API calls.
    if let Some(dir) = cli.from_archive.as_deref() {
        return post_from_archive(&cli, dir).await;
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

    let match_ids = client.recent_match_ids(&puuid, cli.count).await?;
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
        webhook.post(&[discord::stats_embed(&match_summary)], &[]).await?;
        tracing::info!(%match_id, "posted summary");
    }

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
            p.riot_id_game_name.as_deref() == Some(name)
                && p.riot_id_tagline.as_deref() == Some(tag)
        })
        .ok_or_else(|| anyhow!("player {} not found in {}", cli.riot_id, dir.display()))?;
    let puuid = player.puuid.clone();

    let ctx = stats::RenderContext {
        region_slug: riot::web_region_slug(&game.info.platform_id),
        fallback_name: name,
        fallback_tag: tag,
    };
    let match_summary = stats::summarize(&game, &puuid, &ctx)
        .ok_or_else(|| anyhow!("could not summarize {} from {}", cli.riot_id, dir.display()))?;

    let mut embeds = vec![discord::stats_embed(&match_summary)];
    if let Some(summary_path) = cli.summary.as_deref() {
        let md = fs::read_to_string(summary_path)
            .with_context(|| format!("reading summary {}", summary_path.display()))?;
        embeds.push(discord::coach_embed(
            &summary::parse(&md),
            &cli.summary_model,
            match_summary.win,
        ));
    }

    let mut attachments: Vec<discord::Attachment> = Vec::new();
    if cli.charts {
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
        // Attach the dashboard to the Coach's Breakdown (the last embed).
        let last = embeds.len() - 1;
        embeds[last]["image"] = serde_json::json!({ "url": "attachment://dashboard.png" });
        attachments.push(discord::Attachment {
            filename: "dashboard.png".to_string(),
            bytes,
        });
    }

    discord::Webhook::new(cli.webhook.clone())
        .post(&embeds, &attachments)
        .await?;
    tracing::info!(dir = %dir.display(), "posted package from archive");
    Ok(())
}
