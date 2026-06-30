use std::path::PathBuf;

use clap::Parser;

/// Fetch a player's recent League matches and post stat summaries to a Discord webhook.
#[derive(Debug, Parser)]
#[command(name = "scry", version, about)]
pub struct Cli {
    /// Riot ID in `gameName#tagLine` form, e.g. "Faker#KR1".
    #[arg(long)]
    pub riot_id: String,

    /// Platform/region code, e.g. na1, euw1, eun1, kr, br1.
    #[arg(long)]
    pub region: String,

    /// Discord incoming-webhook URL.
    #[arg(long, env = "SCRY_DISCORD_WEBHOOK")]
    pub webhook: String,

    /// Riot API key (Development or Production).
    #[arg(long, env = "RIOT_API_KEY")]
    pub api_key: String,

    /// Number of most-recent matches to summarize.
    #[arg(long, default_value_t = 1)]
    pub count: i32,

    /// Instead of posting, archive each match's raw data (match.json +
    /// timeline JSONL) under <dir>/<matchId>/ for offline analysis.
    #[arg(long)]
    pub dump: Option<PathBuf>,
}
