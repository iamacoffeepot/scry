use std::path::PathBuf;

use clap::Parser;

/// Fetch a player's recent League matches and post stat summaries to a Discord webhook.
#[derive(Debug, Parser)]
#[command(name = "scry", version, about)]
pub struct Cli {
    /// Riot ID in `gameName#tagLine` form, e.g. "Faker#KR1". Required except
    /// with --tick, which reads the watch list instead.
    #[arg(long)]
    pub riot_id: Option<String>,

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

    /// Post a packaged stats + clips embed from a dumped match directory
    /// (containing match.json), using no Riot API. Clip captions come from
    /// the journal's picks for this perspective.
    #[arg(long)]
    pub from_archive: Option<PathBuf>,

    /// With --from-archive on a ranked game: fetch the player's current rank
    /// (league-v4) and show the LP delta since the journal's last observation.
    /// Needs the API.
    #[arg(long)]
    pub track_lp: bool,

    /// Instead of posting, run the causal analysis over a dumped match
    /// directory and print the classified moments and clip picks. Uses no
    /// Riot API and writes nothing.
    #[arg(long)]
    pub analyze: Option<PathBuf>,

    /// With --from-archive: edit the message already posted for this archive
    /// (id from the journal's game_posted event) instead of posting a new
    /// one — used to attach the Highlight/Lowlight clips once they've been
    /// recorded. Reuses the journaled post-time rank/LP (no Riot API call).
    #[arg(long)]
    pub edit: bool,

    /// Run one full poll pass over the watch list (--accounts): post each
    /// newly-completed game, then record clips for one pending game. The
    /// journal (--journal) is the dedup and clip-job state.
    #[arg(long)]
    pub tick: bool,

    /// Watch-list file: one `riot_id|region|queue[,queue…]` per line.
    #[arg(long, env = "SCRY_ACCOUNTS", default_value = "scripts/accounts.txt")]
    pub accounts: PathBuf,

    /// Archive root the poller dumps matches under.
    #[arg(long, env = "SCRY_ARCHIVE", default_value = "archive")]
    pub archive: PathBuf,

    /// The append-only SQLite journal holding posted/clip/rank state.
    #[arg(long, env = "SCRY_JOURNAL", default_value = "state/scry.sqlite")]
    pub journal: PathBuf,

    /// With --tick: skip the clip pass (no League client on this host).
    #[arg(long)]
    pub no_clips: bool,

    /// Clip attempts per game before the job is abandoned.
    #[arg(long, env = "CLIP_MAX_TRIES", default_value_t = 15)]
    pub clip_max_tries: u32,

    /// Clip recordings per pass; the pass also ends when the client leaves
    /// idle. Recording is serial and real-time, so this bounds a pass's length.
    #[arg(long, env = "SCRY_CLIPS_PER_PASS", default_value_t = 5)]
    pub clips_per_pass: u32,

    /// Print every journal event as one JSON line (seq, at_millis, kind,
    /// payload) — the journal's inspection surface.
    #[arg(long)]
    pub journal_dump: bool,

    /// Mark clip jobs abandoned for these match ids (e.g. NA1_5592463865):
    /// operational hygiene for games whose replay aged out of the patch or
    /// whose post was removed.
    #[arg(long, num_args = 1.., value_name = "MATCH_ID")]
    pub abandon_clips: Vec<String>,
}

impl Cli {
    /// Whether the tick's clip pass runs.
    pub fn clips(&self) -> bool {
        !self.no_clips
    }

    /// The Riot ID, in modes that operate on a single player.
    pub fn require_riot_id(&self) -> anyhow::Result<&str> {
        self.riot_id.as_deref().ok_or_else(|| anyhow::anyhow!("--riot-id is required in this mode"))
    }
}
