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
use crate::journal::{ClipAttempt, ClipsAbandoned, ClipsAttached, Journal};
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
        if let Err(error) = discover(cli, &projection, account, &mut pending).await {
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
    projection: &crate::journal::Projection,
    account: &Account,
    pending: &mut Vec<PendingPost>,
) -> Result<()> {
    let client = riot::Client::new(&cli.api_key, &account.region)?;
    let puuid =
        client.resolve_puuid(&account.riot_id).await.with_context(|| format!("resolving {}", account.riot_id))?;

    for queue in &account.queues {
        let queue_u16 = queue.map(|q| u16::try_from(q).unwrap_or(0));
        let ids = client.recent_match_ids(&puuid, CATCHUP_IDS, queue_u16).await?;

        // The floor keeps the window from dumping history: only games newer
        // than the newest already-posted game are news; a player/queue with
        // no posted history backfills the single newest game, never the
        // whole window.
        let floor = projection.newest_posted(&account.riot_id, *queue);
        let newest_in_window = ids.first().map(|id| split_match_id(id)).transpose()?.map(|(_, game_id)| game_id);
        for match_id in &ids {
            let (platform, game_id) = split_match_id(match_id)?;
            if projection.is_posted(platform, game_id, &account.riot_id) {
                continue;
            }
            let is_newest = Some(game_id) == newest_in_window;
            if !is_newest && !floor.is_some_and(|f| game_id > f) {
                continue;
            }
            // An overlapping queue spec ("420,all") may surface a game twice.
            if pending.iter().any(|p| p.game_id == game_id && p.riot_id == account.riot_id) {
                continue;
            }

            let match_json = client.raw_match_json(match_id).await?;
            let timeline = client.raw_timeline_json(match_id).await?;
            let dir = crate::archive::write(&cli.archive, match_id, &match_json, &timeline)?;
            pending.push(PendingPost {
                riot_id: account.riot_id.clone(),
                match_id: match_id.clone(),
                game_id,
                end_millis: game_end_millis(&match_json),
                dir,
            });
        }
    }
    Ok(())
}

/// Analyze and post one discovered game.
async fn post_one(cli: &Cli, post: &PendingPost) -> Result<()> {
    // analyze writes moments.md, clips.json, and the deterministic
    // overview.md (clip picks + captions).
    crate::analyze_archive(&post.riot_id, &post.dir)?;
    // The post path appends `game_posted` (+ `rank_observed`).
    crate::post_from_archive(&post_cli(cli, &post.riot_id, &post.dir, /* edit */ false), &post.dir).await?;
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

/// Record and attach clips for pending games, newest first — serialized,
/// idle-only, retry-capped (the replay client is a contended singleton), and
/// bounded by `--clips-per-pass` since each recording runs in real time.
async fn clips_pass(cli: &Cli, journal: &Journal) -> Result<()> {
    let mut recorded = 0;
    let projection = journal.fold()?;
    for (platform, game_id, job) in projection.pending_clips() {
        let dir = cli.archive.join(&platform).join(game_id.to_string());
        if projection.message_id(&platform, game_id).is_none() || !dir.join("overview.md").exists() {
            continue;
        }

        // Re-checked every iteration: recording kills the game process, so
        // only drive a replay while the client sits idle. An unreachable
        // client (League closed) ends the pass without burning a try.
        match client_phase().await.as_deref() {
            Some(phase) if phase.contains("None") || phase.contains("Lobby") => {}
            _ => return Ok(()),
        }

        if job.tries >= cli.clip_max_tries {
            tracing::info!(%platform, game_id, tries = job.tries, "giving up on clips");
            journal.append(&ClipsAbandoned { platform, game_id, tries: job.tries })?;
            continue;
        }
        journal.append(&ClipAttempt { platform: platform.clone(), game_id, try_number: job.tries + 1 })?;

        tracing::info!(%platform, game_id, try_number = job.tries + 1, "recording clips");
        let status = tokio::process::Command::new("scripts/highlight.sh")
            .arg(&dir)
            .status()
            .await
            .context("running scripts/highlight.sh")?;
        if !status.success() || (!dir.join("highlight.mp4").exists() && !dir.join("lowlight.mp4").exists()) {
            tracing::info!(%platform, game_id, "clips not ready (will retry)");
            continue;
        }

        // The edit path appends `clips_attached` on success.
        crate::post_from_archive(&post_cli(cli, &job.riot_id, &dir, /* edit */ true), &dir).await?;
        tracing::info!(%platform, game_id, "clips attached");
        recorded += 1;
        if recorded >= cli.clips_per_pass {
            return Ok(()); // Bound the pass; the next tick continues the queue.
        }
    }
    Ok(())
}

/// The League client's gameflow phase, or `None` when the client is
/// down/unreachable.
async fn client_phase() -> Option<String> {
    let lockfile = std::fs::read_to_string("/Applications/League of Legends.app/Contents/LoL/lockfile").ok()?;
    let mut fields = lockfile.trim().split(':');
    let (port, password) = (fields.nth(2)?, fields.next()?);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    client
        .get(format!("https://127.0.0.1:{port}/lol-gameflow/v1/gameflow-phase"))
        .basic_auth("riot", Some(password))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()
}

/// The `Cli` a tick-driven post/edit runs with: `--from-archive <dir>
/// --summary <dir>/overview.md --no-overview --track-lp [--edit]`.
fn post_cli(cli: &Cli, riot_id: &str, dir: &Path, edit: bool) -> Cli {
    Cli {
        riot_id: Some(riot_id.to_string()),
        region: None,
        webhook: cli.webhook.clone(),
        api_key: cli.api_key.clone(),
        count: 1,
        queue: None,
        dump: None,
        summary: Some(dir.join("overview.md")),
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
        bail!("journal {} already has events; refusing to import twice", cli.journal.display());
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

        for account in &accounts {
            if dir.join(format!(".posted-{}", slug(&account.riot_id))).exists() {
                journal.append(&crate::journal::GamePosted {
                    platform: platform.clone(),
                    game_id,
                    riot_id: account.riot_id.clone(),
                    queue_id,
                    message_id: message_id.clone(),
                })?;
                games += 1;
            }
        }

        let has_clips = dir.join("highlight.mp4").exists() || dir.join("lowlight.mp4").exists();
        let tries: u32 =
            std::fs::read_to_string(dir.join(".clips-tries")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
        if tries > 0 {
            journal.append(&ClipAttempt { platform: platform.clone(), game_id, try_number: tries })?;
        }
        if dir.join(".clips-done").exists() {
            if has_clips {
                journal.append(&ClipsAttached { platform: platform.clone(), game_id })?;
            } else {
                journal.append(&ClipsAbandoned { platform: platform.clone(), game_id, tries })?;
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

/// poll.sh's per-account dedup slug: every non-alphanumeric byte becomes `_`.
fn slug(riot_id: &str) -> String {
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
