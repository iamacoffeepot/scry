use std::path::PathBuf;

use clap::Parser;

/// Fetch a player's recent League matches and post stat summaries to a Discord webhook.
#[derive(Debug, Parser)]
#[command(name = "scry", version, about)]
pub struct Cli {
    /// Riot ID in `gameName#tagLine` form, e.g. "Faker#KR1".
    #[arg(long)]
    pub riot_id: String,

    /// Platform/region code, e.g. na1, euw1, eun1, kr, br1. Required unless
    /// posting from an archive (--from-archive), which reads the platform from
    /// the match record.
    #[arg(long)]
    pub region: Option<String>,

    /// Discord incoming-webhook URL.
    #[arg(long, env = "SCRY_DISCORD_WEBHOOK")]
    pub webhook: String,

    /// Riot API key (Development or Production).
    #[arg(long, env = "RIOT_API_KEY")]
    pub api_key: String,

    /// Number of most-recent matches to summarize.
    #[arg(long, default_value_t = 1)]
    pub count: i32,

    /// Only pull games from this queue id (e.g. 420 ranked solo, 440 ranked
    /// flex, 400 normal draft, 430 normal blind, 450 ARAM). Omit for all queues.
    #[arg(long, env = "SCRY_QUEUE")]
    pub queue: Option<u16>,

    /// Instead of posting, archive each match's raw data (match.json +
    /// timeline JSONL) under <dir>/<platform>/<id>/ for offline analysis.
    #[arg(long)]
    pub dump: Option<PathBuf>,

    /// Pair the posted match with an overview summary (Markdown from the
    /// OVERVIEW prompt): its ## sections become a second embed posted alongside
    /// the stats embed.
    #[arg(long)]
    pub summary: Option<PathBuf>,

    /// Model label shown in the coach summary's footer (attribution).
    #[arg(long, default_value = "Claude Opus")]
    pub summary_model: String,

    /// Post a packaged stats + coach embed from a dumped match directory
    /// (containing match.json), using no Riot API. Pair with --summary.
    #[arg(long)]
    pub from_archive: Option<PathBuf>,

    /// Also render and attach charts (gold lead, damage, lobby ranking) as
    /// embeds. Requires the archive to contain timeline-frames.jsonl.
    #[arg(long)]
    pub charts: bool,

    /// Instead of posting, run the causal analysis over a dumped match
    /// directory and print the classified moments. Uses no Riot API.
    #[arg(long)]
    pub analyze: Option<PathBuf>,
}
