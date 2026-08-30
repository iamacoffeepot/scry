use std::path::PathBuf;

use clap::Parser;

/// Fetch a player's recent League matches and post stat summaries to a Discord webhook.
#[derive(Debug, Parser)]
#[command(name = "scry", version, about)]
pub struct Cli {
    /// Riot ID in `gameName#tagLine` form, e.g. "Faker#KR1". Required except
    /// with --tick / --journal-import, which read the watch list instead.
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

    /// Pair the posted match with an overview summary (Markdown from the
    /// OVERVIEW prompt): its ## sections become a second embed posted alongside
    /// the stats embed.
    #[arg(long)]
    pub summary: Option<PathBuf>,

    /// Footer attribution label for generated prose; empty (the default) means
    /// no attribution — the pipeline is deterministic.
    #[arg(long, env = "SCRY_SUMMARY_MODEL", default_value = "")]
    pub summary_model: String,

    /// Post a packaged stats + coach embed from a dumped match directory
    /// (containing match.json), using no Riot API. Pair with --summary.
    #[arg(long)]
    pub from_archive: Option<PathBuf>,

    /// Also render and attach charts (gold lead, damage, lobby ranking) as
    /// embeds. Requires the archive to contain timeline-frames.jsonl.
    #[arg(long)]
    pub charts: bool,

    /// With --from-archive on a ranked game: fetch the player's current rank
    /// (league-v4) and show the LP delta since the last snapshot. Needs the API.
    #[arg(long)]
    pub track_lp: bool,

    /// Directory holding per-account LP snapshots (used with --track-lp).
    #[arg(long, default_value = "state/lp")]
    pub state_dir: PathBuf,

    /// Instead of posting, run the causal analysis over a dumped match
    /// directory and print the classified moments. Uses no Riot API.
    #[arg(long)]
    pub analyze: Option<PathBuf>,

    /// With --from-archive + --summary: render a minimal embed (header + stats +
    /// clips) that omits the AI overview prose. The summary is still read for the
    /// Highlight/Lowlight clip captions.
    #[arg(long)]
    pub no_overview: bool,

    /// With --from-archive: edit the message already posted for this archive
    /// (id read from <dir>/.message-id) instead of posting a new one — used to
    /// attach the Highlight/Lowlight clips once they've been recorded. Reuses
    /// the post-time rank/LP from <dir>/.rank.json (no Riot API call).
    #[arg(long)]
    pub edit: bool,

    /// Run one full poll pass over the watch list (--accounts): post each
    /// newly-completed game, then record clips for one pending game. The
    /// journal (--journal) is the dedup and clip-job state.
    #[arg(long)]
    pub tick: bool,

    /// One-shot cutover: backfill the journal from a pre-journal archive's
    /// marker files (.posted-*, .message-id, .clip-*) and the state/lp
    /// snapshots. Refused once the journal has events.
    #[arg(long)]
    pub journal_import: bool,

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
