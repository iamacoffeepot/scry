//! `--tick`: one full poll pass in Rust, journal-driven — the port of
//! poll.sh's `poll_all` + `clips_pass`. The shell keeps only the while/sleep.
//!
//! The pass runs discover-then-post so the channel reads in one global
//! chronology: every account/queue's unposted games (floor-guarded) are
//! dumped and stamped with their game-end time first, then the whole batch
//! posts sorted by when the games actually finished — regardless of which
//! tracked player they belong to. Per game: analyze (which also writes the
//! deterministic clip pick + captions into `overview.md`) → post (which
//! appends `game_posted` + `rank_observed`). Then the serialized clip pass:
//! pending jobs newest-first, client idle only (re-checked per job),
//! `scripts/highlight.sh`, edit the post to attach — bounded by
//! `--clips-per-pass`, since recording is real-time. The contention rules the
//! shell version learned the hard way still hold: one replay at a time, never
//! during a live game.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::Cli;
use crate::journal::{
    AccountRenamed, AccountResolved, ClipAttempt, ClipsAbandoned, ClipsAttached, Journal, Projection,
};
use crate::riot;

/// Match ids fetched per (account, queue) each pass — how many games played
/// between polls can still post (older ones age out of the window).
const CATCHUP_IDS: i32 = 5;

/// One parsed watch-list line: `riot_id|region|queue[,queue…]`.
struct Account {
    riot_id: String,
    region: String,
    /// Queue ids to scan; `None` scans all queues (the file's `all`).
    queues: Vec<Option<u32>>,
}

/// Run one poll pass over the watch list, then one clip pass.
pub async fn run(cli: &Cli) -> Result<()> {
    let accounts = parse_accounts(&cli.accounts, cli.queue.map(u32::from))
        .with_context(|| format!("reading watch list {}", cli.accounts.display()))?;
    let journal = Journal::open(&cli.journal)?;

    // Phase 1: discover and dump every unposted game across all accounts.
    let projection = journal.fold()?;
    let mut pending = Vec::new();
    for account in &accounts {
        if let Err(error) = discover(cli, &journal, &projection, account, &mut pending).await {
            tracing::warn!(riot_id = %account.riot_id, error = %format!("{error:#}"), "account pass failed");
        }
    }

    // Phase 2: post in one global chronology — the order the games finished,
    // regardless of account (ties: game id, then player, for determinism).
    pending.sort_by(|a, b| (a.end_millis, a.game_id, &a.riot_id).cmp(&(b.end_millis, b.game_id, &b.riot_id)));
    for post in &pending {
        tracing::info!(riot_id = %post.riot_id, match_id = %post.match_id, "new game");
        if let Err(error) = post_one(cli, post).await {
            tracing::warn!(riot_id = %post.riot_id, match_id = %post.match_id, error = %format!("{error:#}"), "post failed");
        }
    }

    if cli.clips()
        && let Err(error) = clips_pass(cli, &journal).await
    {
        tracing::warn!(error = %format!("{error:#}"), "clip pass failed");
    }
    Ok(())
}

/// One discovered game waiting to post.
struct PendingPost {
    riot_id: String,
    match_id: String,
    game_id: u64,
    /// When the game finished (unix millis) — the global posting order.
    end_millis: i64,
    dir: PathBuf,
}

/// Find one account's newly-completed games across its queues and dump each
/// into the archive, collecting them as [`PendingPost`]s for the global sort.
async fn discover(
    cli: &Cli,
    journal: &Journal,
    projection: &Projection,
    account: &Account,
    pending: &mut Vec<PendingPost>,
) -> Result<()> {
    let client = riot::Client::new(&cli.api_key, &account.region)?;
    let (riot_id, puuid) = identify(cli, journal, projection, account, &client)
        .await
        .with_context(|| format!("resolving {}", account.riot_id))?;

    for queue in &account.queues {
        let queue_u16 = queue.map(|q| u16::try_from(q).unwrap_or(0));
        let ids = client.recent_match_ids(&puuid, CATCHUP_IDS, queue_u16).await?;

        // The floor keeps the window from dumping history: only games newer
        // than the newest already-posted game are news; a player/queue with
        // no posted history backfills the single newest game, never the
        // whole window.
        let floor = projection.newest_posted(&riot_id, *queue);
        let newest_in_window = ids.first().map(|id| split_match_id(id)).transpose()?.map(|(_, game_id)| game_id);
        for match_id in &ids {
            let (platform, game_id) = split_match_id(match_id)?;
            if projection.is_posted(platform, game_id, &riot_id) {
                continue;
            }
            let is_newest = Some(game_id) == newest_in_window;
            if !is_newest && !floor.is_some_and(|f| game_id > f) {
                continue;
            }
            // An overlapping queue spec ("420,all") may surface a game twice.
            if pending.iter().any(|p| p.game_id == game_id && p.riot_id == riot_id) {
                continue;
            }

            let match_json = client.raw_match_json(match_id).await?;
            let timeline = client.raw_timeline_json(match_id).await?;
            let dir = crate::archive::write(&cli.archive, match_id, &match_json, &timeline)?;
            pending.push(PendingPost {
                riot_id: riot_id.clone(),
                match_id: match_id.clone(),
                game_id,
                end_millis: game_end_millis(&match_json),
                dir,
            });
        }
    }
    Ok(())
}

/// The account's identity for this pass. PUUIDs are the identity; names are
/// display labels — the first resolution pins name→PUUID in the journal, and
/// a name Riot no longer knows reverse-resolves through its pin to the
/// current name (watch list rewritten, rename journaled). Dedup and floors
/// key by identity, so a rename orphans nothing. Returns (riot_id, puuid).
async fn identify(
    cli: &Cli,
    journal: &Journal,
    projection: &Projection,
    account: &Account,
    client: &riot::Client,
) -> Result<(String, String)> {
    if let Some(puuid) = client.resolve_puuid(&account.riot_id).await? {
        if projection.puuid_of(&account.riot_id) != Some(puuid.as_str()) {
            journal.append(&AccountResolved { riot_id: account.riot_id.clone(), puuid: puuid.clone() })?;
        }
        return Ok((account.riot_id.clone(), puuid));
    }

    let Some(puuid) = projection.puuid_of(&account.riot_id).map(str::to_string) else {
        bail!("no account found for `{}` and no pinned PUUID to heal from (typo in the watch list?)", account.riot_id);
    };
    let Some(new_riot_id) = client.riot_id_of(&puuid).await? else {
        bail!("no account found for `{}`; its pinned PUUID no longer resolves either", account.riot_id);
    };

    tracing::info!(old = %account.riot_id, new = %new_riot_id, "account renamed; updating watch list");
    rewrite_watch_list(&cli.accounts, &account.riot_id, &new_riot_id)?;
    journal.append(&AccountRenamed {
        old_riot_id: account.riot_id.clone(),
        new_riot_id: new_riot_id.clone(),
        puuid: puuid.clone(),
    })?;
    journal.append(&AccountResolved { riot_id: new_riot_id.clone(), puuid: puuid.clone() })?;
    Ok((new_riot_id, puuid))
}

/// Rewrite the watch-list line whose riot-id field is `old` to carry `new`.
fn rewrite_watch_list(path: &Path, old: &str, new: &str) -> Result<()> {
    let contents = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let rewritten: String = contents
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            let matches = !trimmed.starts_with('#')
                && trimmed.split('|').next().is_some_and(|field| field.trim().eq_ignore_ascii_case(old));
            if matches {
                format!(
                    "{}
",
                    line.replacen(old, new, 1)
                )
            } else {
                format!(
                    "{line}
"
                )
            }
        })
        .collect();
    std::fs::write(path, rewritten).with_context(|| format!("writing {}", path.display()))
}

/// Analyze and post one discovered game.
async fn post_one(cli: &Cli, post: &PendingPost) -> Result<()> {
    // analyze writes the per-player moments/clips/overview artifacts (clip
    // picks + captions differ per tracked perspective).
    crate::analyze_archive(&post.riot_id, &post.dir, &roster_riot_ids(cli))?;
    // The post path appends `game_posted` (+ `rank_observed`).
    let overview = post.dir.join(format!("overview-{}.md", slug(&post.riot_id)));
    crate::post_from_archive(&post_cli(cli, &post.riot_id, &overview, &post.dir, /* edit */ false), &post.dir).await?;
    tracing::info!(riot_id = %post.riot_id, match_id = %post.match_id, "posted");
    Ok(())
}

/// When the game finished, unix millis: `gameEndTimestamp` where Riot
/// provides it, else start + duration, else 0 (sorting by game id alone).
fn game_end_millis(match_json: &serde_json::Value) -> i64 {
    let info = &match_json["info"];
    info.get("gameEndTimestamp").and_then(serde_json::Value::as_i64).unwrap_or_else(|| {
        let start = info.get("gameStartTimestamp").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let duration_secs = info.get("gameDuration").and_then(serde_json::Value::as_i64).unwrap_or(0);
        start + duration_secs * 1000
    })
}

/// Record and attach clips for pending jobs, newest game first — one job per
/// (game, tracked player), so N tracked players in one lobby each get their
/// own perspective on their own post. Serialized, idle-only, retry-capped
/// (the replay client is a contended singleton), bounded by
/// `--clips-per-pass`; jobs whose replay hasn't finished verifying get one
/// in-pass second chance after a minute instead of waiting a whole interval.
async fn clips_pass(cli: &Cli, journal: &Journal) -> Result<()> {
    let mut recorded = 0;
    let projection = journal.fold()?;
    let client_patch = client_replay_patch().await;

    // Group jobs by game: tracked players share lobbies, and one loaded
    // replay serves every perspective — loads are the expensive part.
    let mut games: Vec<(String, u64, Vec<crate::journal::ClipJob>)> = Vec::new();
    for (platform, game_id, job) in projection.pending_clips() {
        match games.last_mut() {
            Some((p, g, jobs)) if *p == platform && *g == game_id => jobs.push(job),
            _ => games.push((platform, game_id, vec![job])),
        }
    }

    let mut deferred = Vec::new();
    for entry in games {
        match try_game_clips(cli, journal, &projection, client_patch.as_deref(), &entry).await? {
            ClipOutcome::Recorded(n) => {
                recorded += n;
                if recorded >= cli.clips_per_pass {
                    return Ok(());
                }
            }
            ClipOutcome::NotReady => deferred.push(entry),
            ClipOutcome::ClientBusy => return Ok(()),
            ClipOutcome::Skipped => {}
        }
    }

    if !deferred.is_empty() && recorded < cli.clips_per_pass {
        // A fresh replay verifies within a minute or two of the game ending.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        for entry in deferred {
            if let ClipOutcome::Recorded(n) =
                try_game_clips(cli, journal, &projection, client_patch.as_deref(), &entry).await?
            {
                recorded += n;
                if recorded >= cli.clips_per_pass {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// One game's clip batch this pass.
enum ClipOutcome {
    /// How many perspectives recorded and attached.
    Recorded(u32),
    /// The replay hasn't finished downloading/verifying — retryable cheaply,
    /// no tries burned.
    NotReady,
    /// The client left idle; the whole pass must stop.
    ClientBusy,
    Skipped,
}

/// Record every pending perspective of one game in a single replay session,
/// then edit each player's post to attach their clips.
async fn try_game_clips(
    cli: &Cli,
    journal: &Journal,
    projection: &Projection,
    client_patch: Option<&str>,
    (platform, game_id, jobs): &(String, u64, Vec<crate::journal::ClipJob>),
) -> Result<ClipOutcome> {
    let (platform, game_id) = (platform.clone(), *game_id);
    let dir = cli.archive.join(&platform).join(game_id.to_string());
    // Each perspective edits its own post; a job without a recorded message
    // can't attach anywhere.
    let jobs: Vec<_> =
        jobs.iter().filter(|job| projection.message_id(&platform, game_id, &job.riot_id).is_some()).collect();
    if jobs.is_empty() {
        return Ok(ClipOutcome::Skipped);
    }

    // Replays are patch-gated: a game from a previous patch can never play
    // again — abandon every perspective and note it on each post.
    if let (Some(client), Some(game)) = (client_patch, game_patch(&dir))
        && client != game
    {
        tracing::info!(%platform, game_id, game_patch = %game, client_patch = %client, "replay predates the current patch; abandoning clips");
        for job in jobs {
            journal.append(&ClipsAbandoned {
                platform: platform.clone(),
                game_id,
                tries: job.tries,
                riot_id: Some(job.riot_id.clone()),
            })?;
            let suffix = format!("-{}", slug(&job.riot_id));
            if !dir.join(format!("overview{suffix}.md")).exists() {
                let _ = crate::analyze_archive(&job.riot_id, &dir, &roster_riot_ids(cli));
            }
            let overview = dir.join(format!("overview{suffix}.md"));
            if overview.exists()
                && let Err(error) =
                    crate::post_from_archive(&post_cli(cli, &job.riot_id, &overview, &dir, true), &dir).await
            {
                tracing::debug!(%platform, game_id, error = %format!("{error:#}"), "expired-replay note edit failed");
            }
        }
        return Ok(ClipOutcome::Skipped);
    }

    // Re-checked per game: recording kills the game process, so only drive a
    // replay while the client sits idle. An unreachable client (League
    // closed) ends the pass without burning a try.
    match client_phase().await.as_deref() {
        Some(phase) if phase.contains("None") || phase.contains("Lobby") => {}
        _ => return Ok(ClipOutcome::ClientBusy),
    }

    // Which perspectives ride this session: budget-exhausted jobs abandon,
    // unanalyzable ones (e.g. renamed since the game) abandon, the rest
    // ensure their per-player artifacts and join the batch.
    let mut batch = Vec::new();
    for job in jobs {
        if job.tries >= cli.clip_max_tries {
            tracing::info!(%platform, game_id, riot_id = %job.riot_id, tries = job.tries, "giving up on clips");
            journal.append(&ClipsAbandoned {
                platform: platform.clone(),
                game_id,
                tries: job.tries,
                riot_id: Some(job.riot_id.clone()),
            })?;
            continue;
        }
        let suffix = format!("-{}", slug(&job.riot_id));
        if !dir.join(format!("overview{suffix}.md")).exists()
            && let Err(error) = crate::analyze_archive(&job.riot_id, &dir, &roster_riot_ids(cli))
        {
            tracing::warn!(%platform, game_id, riot_id = %job.riot_id, error = %format!("{error:#}"), "perspective analysis failed; abandoning clips");
            journal.append(&ClipsAbandoned {
                platform: platform.clone(),
                game_id,
                tries: job.tries,
                riot_id: Some(job.riot_id.clone()),
            })?;
            continue;
        }
        batch.push((suffix, job));
    }
    if batch.is_empty() {
        return Ok(ClipOutcome::Skipped);
    }

    // Cheap availability probe before committing to a full replay load: a
    // just-ended game's .rofl takes a couple minutes to verify.
    if !replay_ready(game_id).await {
        tracing::info!(%platform, game_id, "replay not yet available");
        return Ok(ClipOutcome::NotReady);
    }

    for (_, job) in &batch {
        journal.append(&ClipAttempt {
            platform: platform.clone(),
            game_id,
            try_number: job.tries + 1,
            riot_id: Some(job.riot_id.clone()),
        })?;
    }
    tracing::info!(%platform, game_id, perspectives = batch.len(), "recording clips");
    let mut command = tokio::process::Command::new("scripts/highlight.sh");
    command.arg(&dir);
    for (suffix, _) in &batch {
        command.arg(suffix);
    }
    let status = command.status().await.context("running scripts/highlight.sh")?;
    if !status.success() {
        tracing::info!(%platform, game_id, "clips not ready (will retry)");
        return Ok(ClipOutcome::Skipped);
    }

    let mut attached = 0;
    for (suffix, job) in &batch {
        let highlight = dir.join(format!("highlight{suffix}.mp4"));
        let lowlight = dir.join(format!("lowlight{suffix}.mp4"));
        if !highlight.exists() && !lowlight.exists() {
            tracing::info!(%platform, game_id, riot_id = %job.riot_id, "clips not ready (will retry)");
            continue;
        }
        // The edit path appends `clips_attached` on success.
        let overview = dir.join(format!("overview{suffix}.md"));
        crate::post_from_archive(&post_cli(cli, &job.riot_id, &overview, &dir, /* edit */ true), &dir).await?;
        tracing::info!(%platform, game_id, riot_id = %job.riot_id, "clips attached");
        attached += 1;
    }
    Ok(ClipOutcome::Recorded(attached))
}

/// Whether the replay for `game_id` is downloaded and verified (metadata
/// state `watch`): kick the download and poll briefly — seconds, not the
/// minutes a blind replay load costs when the .rofl isn't there yet.
async fn replay_ready(game_id: u64) -> bool {
    let _ = lcu_post(&format!("/lol-replays/v1/rofls/{game_id}/download/graceful"), "{}").await;
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        if let Some(body) = lcu_get(&format!("/lol-replays/v1/metadata/{game_id}")).await
            && let Ok(meta) = serde_json::from_str::<serde_json::Value>(&body)
            && meta.get("state").and_then(serde_json::Value::as_str) == Some("watch")
        {
            return true;
        }
    }
    false
}

/// The client's replay patch as `major.minor` (`/lol-replays/v1/configuration`
/// `gameVersion`), or `None` when the client is down/unreachable.
pub(crate) async fn client_replay_patch() -> Option<String> {
    let body = lcu_get("/lol-replays/v1/configuration").await?;
    let config: serde_json::Value = serde_json::from_str(&body).ok()?;
    major_minor(config.get("gameVersion")?.as_str()?)
}

/// A game's patch as `major.minor`, from its archived `match.json`.
pub(crate) fn game_patch(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("match.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    major_minor(value.pointer("/info/gameVersion")?.as_str()?)
}

/// `16.16.804.9184` -> `16.16` (replay compatibility is per patch line).
fn major_minor(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    Some(format!("{}.{}", parts.next()?, parts.next()?))
}

/// The League client's gameflow phase, or `None` when the client is
/// down/unreachable.
async fn client_phase() -> Option<String> {
    lcu_get("/lol-gameflow/v1/gameflow-phase").await
}

/// POST one LCU endpoint; `None` when the client is down or the call fails.
async fn lcu_post(path: &str, body: &'static str) -> Option<String> {
    lcu_request(path, Some(body)).await
}

/// GET one LCU endpoint through the lockfile-published local port; `None`
/// when the client is down or the request fails.
async fn lcu_get(path: &str) -> Option<String> {
    lcu_request(path, None).await
}

async fn lcu_request(path: &str, post_body: Option<&'static str>) -> Option<String> {
    let lockfile = std::fs::read_to_string("/Applications/League of Legends.app/Contents/LoL/lockfile").ok()?;
    let mut fields = lockfile.trim().split(':');
    let (port, password) = (fields.nth(2)?, fields.next()?);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let url = format!("https://127.0.0.1:{port}{path}");
    let request = match post_body {
        Some(body) => client.post(url).header("Content-Type", "application/json").body(body),
        None => client.get(url),
    };
    request.basic_auth("riot", Some(password)).send().await.ok()?.text().await.ok()
}

/// The `Cli` a tick-driven post/edit runs with: `--from-archive <dir>
/// --summary <overview> --no-overview --track-lp [--edit]`.
fn post_cli(cli: &Cli, riot_id: &str, overview: &Path, dir: &Path, edit: bool) -> Cli {
    Cli {
        riot_id: Some(riot_id.to_string()),
        region: None,
        webhook: cli.webhook.clone(),
        api_key: cli.api_key.clone(),
        count: 1,
        queue: None,
        dump: None,
        summary: Some(overview.to_path_buf()),
        summary_model: cli.summary_model.clone(),
        from_archive: Some(dir.to_path_buf()),
        charts: false,
        track_lp: true,
        state_dir: cli.state_dir.clone(),
        analyze: None,
        no_overview: true,
        edit,
        tick: false,
        journal_import: false,
        accounts: cli.accounts.clone(),
        archive: cli.archive.clone(),
        journal: cli.journal.clone(),
        no_clips: cli.no_clips,
        clip_max_tries: cli.clip_max_tries,
        clips_per_pass: cli.clips_per_pass,
        journal_dump: false,
        abandon_clips: Vec::new(),
    }
}

/// Split `NA1_5630828116` into `("NA1", 5630828116)`.
pub fn split_match_id(match_id: &str) -> Result<(&str, u64)> {
    let (platform, rest) = match_id.split_once('_').ok_or_else(|| anyhow!("malformed match id `{match_id}`"))?;
    Ok((platform, rest.parse().with_context(|| format!("malformed game id in `{match_id}`"))?))
}

/// Parse the watch list: one `riot_id|region|queue[,queue…]` per line, `#`
/// starting a line is a comment (but not inline — Riot IDs contain `#`),
/// missing queue → the default, `all` → no queue filter.
fn parse_accounts(path: &Path, default_queue: Option<u32>) -> Result<Vec<Account>> {
    let raw = std::fs::read_to_string(path)?;
    let mut accounts = Vec::new();
    for line in raw.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('|').map(str::trim);
        let (Some(riot_id), Some(region)) = (fields.next(), fields.next()) else {
            tracing::warn!(line, "skipping malformed watch-list line");
            continue;
        };
        if riot_id.is_empty() || region.is_empty() {
            tracing::warn!(line, "skipping malformed watch-list line");
            continue;
        }
        let queues = match fields.next().filter(|q| !q.is_empty()) {
            None => vec![default_queue.or(Some(420))],
            Some(spec) => spec
                .split(',')
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .map(|q| {
                    if q == "all" {
                        Ok(None)
                    } else {
                        q.parse().map(Some)
                    }
                })
                .collect::<Result<_, _>>()
                .with_context(|| format!("bad queue list in `{line}`"))?,
        };
        accounts.push(Account { riot_id: riot_id.to_string(), region: region.to_string(), queues });
    }
    Ok(accounts)
}

/// Backfill the journal from the marker files a pre-journal poller left in
/// the archive (`.posted-*`, `.message-id`, `.clip-*`) and the `state/lp`
/// snapshots — a one-shot cutover, refused once the journal has events.
pub fn import_legacy_state(cli: &Cli) -> Result<()> {
    let journal = Journal::open(&cli.journal)?;
    if !journal.is_empty()? {
        // Already imported: the remaining legacy value is post-time LP lines
        // written before game_posted carried them. Backfill just those, as
        // idempotent re-appends (last post wins in the fold).
        return backfill_rank_lines(cli, &journal);
    }
    let accounts = parse_accounts(&cli.accounts, None)?;

    let mut games = 0usize;
    for dir in archive_dirs(&cli.archive)? {
        let platform = dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_string();
        let Some(game_id) = dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let message_id = std::fs::read_to_string(dir.join(".message-id")).unwrap_or_default().trim().to_string();
        let queue_id = read_queue_id(&dir).unwrap_or(0);
        let rank: Option<crate::stats::RankInfo> =
            std::fs::read_to_string(dir.join(".rank.json")).ok().and_then(|json| serde_json::from_str(&json).ok());

        for account in &accounts {
            if dir.join(format!(".posted-{}", slug(&account.riot_id))).exists() {
                journal.append(&crate::journal::GamePosted {
                    platform: platform.clone(),
                    game_id,
                    riot_id: account.riot_id.clone(),
                    queue_id,
                    message_id: message_id.clone(),
                    rank: rank.clone(),
                    puuid: None,
                })?;
                games += 1;
            }
        }

        let has_clips = dir.join("highlight.mp4").exists() || dir.join("lowlight.mp4").exists();
        let tries: u32 =
            std::fs::read_to_string(dir.join(".clips-tries")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
        if tries > 0 {
            journal.append(&ClipAttempt { platform: platform.clone(), game_id, try_number: tries, riot_id: None })?;
        }
        if dir.join(".clips-done").exists() {
            if has_clips {
                journal.append(&ClipsAttached { platform: platform.clone(), game_id, riot_id: None })?;
            } else {
                journal.append(&ClipsAbandoned { platform: platform.clone(), game_id, tries, riot_id: None })?;
            }
        }
    }

    let mut baselines = 0usize;
    for entry in std::fs::read_dir(&cli.state_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((puuid, queue)) = stem.rsplit_once('_') else {
            continue;
        };
        let Ok(queue_id) = queue.parse::<u32>() else {
            continue;
        };
        let Some(snapshot) = crate::rank::read_snapshot(&path) else {
            continue;
        };
        journal.append(&crate::journal::RankObserved {
            puuid: puuid.to_string(),
            riot_id: String::new(),
            queue_id,
            ladder_value: snapshot.value,
            label: snapshot.label,
            lp: snapshot.lp,
        })?;
        baselines += 1;
    }

    tracing::info!(games, baselines, journal = %cli.journal.display(), "imported legacy state");
    Ok(())
}

/// Mark clip jobs abandoned for explicitly named match ids — games whose
/// replay aged out of the patch, or whose post was removed. Idempotent: a
/// finished job is skipped.
pub fn abandon_clips(cli: &Cli) -> Result<()> {
    let journal = Journal::open(&cli.journal)?;
    let projection = journal.fold()?;
    for match_id in &cli.abandon_clips {
        let (platform, game_id) = split_match_id(match_id)?;
        let jobs = projection.clip_jobs(platform, game_id);
        if !jobs.is_empty() && jobs.iter().all(|job| job.done) {
            tracing::info!(%match_id, "clip jobs already finished; skipping");
            continue;
        }
        // riot_id: None = every perspective of the game.
        let tries = jobs.iter().map(|job| job.tries).max().unwrap_or(0);
        journal.append(&ClipsAbandoned { platform: platform.to_string(), game_id, tries, riot_id: None })?;
        tracing::info!(%match_id, "clip jobs abandoned");
    }
    Ok(())
}

/// Re-append `game_posted` (same identity, plus the legacy `.rank.json` LP
/// line) for already-posted games the journal has no rank line for — so a
/// clip-attach edit reproduces the post-time LP line without the file.
fn backfill_rank_lines(cli: &Cli, journal: &Journal) -> Result<()> {
    let projection = journal.fold()?;
    let mut backfilled = 0usize;
    for dir in archive_dirs(&cli.archive)? {
        let platform = dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_string();
        let Some(game_id) = dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let Some(rank) = std::fs::read_to_string(dir.join(".rank.json"))
            .ok()
            .and_then(|json| serde_json::from_str::<crate::stats::RankInfo>(&json).ok())
        else {
            continue;
        };
        for job in projection.clip_jobs(&platform, game_id) {
            if job.done || projection.rank_line(&platform, game_id, &job.riot_id).is_some() {
                continue; // A finished job never edits again; a lined one is set.
            }
            let Some(message_id) = projection.message_id(&platform, game_id, &job.riot_id) else {
                continue;
            };
            journal.append(&crate::journal::GamePosted {
                platform: platform.clone(),
                game_id,
                riot_id: job.riot_id.clone(),
                queue_id: read_queue_id(&dir).unwrap_or(0),
                message_id: message_id.to_string(),
                rank: Some(rank.clone()),
                puuid: None,
            })?;
            backfilled += 1;
        }
    }
    tracing::info!(backfilled, "journal already imported; backfilled legacy rank lines for pending clip edits");
    Ok(())
}

/// `archive/<platform>/<game_id>` directories.
fn archive_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for platform in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if platform.path().is_dir() {
            for game in std::fs::read_dir(platform.path()).into_iter().flatten().flatten() {
                if game.path().is_dir() {
                    dirs.push(game.path());
                }
            }
        }
    }
    Ok(dirs)
}

/// The queue id of an archived game, from its `match.json`.
fn read_queue_id(dir: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(dir.join("match.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    u32::try_from(value.get("info")?.get("queueId")?.as_u64()?).ok()
}

/// The watch list's riot ids, for joint clip-pick assignment across tracked
/// players sharing a game; a missing or unreadable list degrades to solo
/// assignment.
pub(crate) fn roster_riot_ids(cli: &Cli) -> Vec<String> {
    parse_accounts(&cli.accounts, None)
        .map(|accounts| accounts.into_iter().map(|account| account.riot_id).collect())
        .unwrap_or_default()
}

/// A riot id as a filename-safe slug: every non-alphanumeric byte becomes
/// `_` (the same mapping the old shell markers used).
pub fn slug(riot_id: &str) -> String {
    riot_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_ids_split_into_platform_and_game() {
        assert_eq!(split_match_id("NA1_5630828116").unwrap(), ("NA1", 5630828116));
        assert!(split_match_id("garbage").is_err());
    }

    #[test]
    fn game_end_falls_back_from_end_timestamp_to_start_plus_duration() {
        let with_end = serde_json::json!({"info": {"gameEndTimestamp": 1700000900000i64, "gameStartTimestamp": 1700000000000i64, "gameDuration": 900}});
        let without_end = serde_json::json!({"info": {"gameStartTimestamp": 1700000000000i64, "gameDuration": 900}});
        assert_eq!(game_end_millis(&with_end), 1700000900000);
        assert_eq!(game_end_millis(&without_end), 1700000900000);
        assert_eq!(game_end_millis(&serde_json::json!({})), 0);
    }

    #[test]
    fn slug_matches_the_shell_marker_names() {
        // poll.sh: tr -c 'A-Za-z0-9' '_' — `#` and spaces both become `_`.
        assert_eq!(slug("Moon#132"), "Moon_132");
        assert_eq!(slug("Hide on bush#KR1"), "Hide_on_bush_KR1");
    }
}
