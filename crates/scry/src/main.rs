mod archive;
mod cli;
mod discord;
mod riot;
mod stats;

use anyhow::{Context, Result};
use clap::Parser;
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
    let client = riot::Client::new(&cli.api_key, &cli.region)?;

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

    let webhook = discord::Webhook::new(cli.webhook);

    // Oldest -> newest so the channel reads chronologically.
    for match_id in match_ids.into_iter().rev() {
        let game = client.match_detail(&match_id).await?;
        let Some(summary) = stats::summarize(&game, &puuid, &ctx) else {
            tracing::warn!(%match_id, "player not found in match participants; skipping");
            continue;
        };
        webhook.post(&summary).await?;
        tracing::info!(%match_id, "posted summary");
    }

    Ok(())
}
