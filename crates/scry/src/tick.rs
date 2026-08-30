//! `--tick`: one full poll pass in Rust, journal-driven — the port of
//! poll.sh's `poll_all` + `clips_pass`. The shell keeps only the while/sleep.
//!
//! Per tracked account/queue: latest match id → skip if the journal says it
//! was posted for that player → dump the archive → analyze → Opus overview
//! (`claude -p` subprocess) → post (which appends `game_posted` +
//! `rank_observed`). Then one serialized clip pass: newest pending job,
//! client idle only, `scripts/highlight.sh`, edit the post to attach, one
//! success per pass — the same contention rules the shell version learned
//! the hard way.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::Cli;
use crate::journal::{ClipAttempt, ClipsAbandoned, ClipsAttached, Journal};
use crate::riot;

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

    for account in &accounts {
        for queue in &account.queues {
            if let Err(error) = process(cli, &journal, account, *queue).await {
                tracing::warn!(riot_id = %account.riot_id, error = %format!("{error:#}"), "account pass failed");
            }
        }
    }

    if cli.clips()
        && let Err(error) = clips_pass(cli, &journal).await
    {
        tracing::warn!(error = %format!("{error:#}"), "clip pass failed");
    }
    Ok(())
}

/// Check one (account, queue) for a new latest game and run it through the
/// dump → analyze → overview → post pipeline.
async fn process(cli: &Cli, journal: &Journal, account: &Account, queue: Option<u32>) -> Result<()> {
    let client = riot::Client::new(&cli.api_key, &account.region)?;
    let puuid =
        client.resolve_puuid(&account.riot_id).await.with_context(|| format!("resolving {}", account.riot_id))?;

    let queue_u16 = queue.map(|q| u16::try_from(q).unwrap_or(0));
    let ids = client.recent_match_ids(&puuid, 1, queue_u16).await?;
    let Some(match_id) = ids.first() else {
        tracing::info!(riot_id = %account.riot_id, ?queue, "no recent game");
        return Ok(());
    };
    let (platform, game_id) = split_match_id(match_id)?;
    if journal.fold()?.is_posted(platform, game_id, &account.riot_id) {
        return Ok(());
    }
    tracing::info!(riot_id = %account.riot_id, %match_id, "new game");

    let match_json = client.raw_match_json(match_id).await?;
    let timeline = client.raw_timeline_json(match_id).await?;
    let dir = crate::archive::write(&cli.archive, match_id, &match_json, &timeline)?;

    crate::analyze_archive(&account.riot_id, &dir)?;
    write_overview(&account.riot_id, &dir).await?;

    // The post path appends `game_posted` (+ `rank_observed`) to the journal.
    crate::post_from_archive(&post_cli(cli, &account.riot_id, &dir, /* edit */ false), &dir).await?;
    tracing::info!(riot_id = %account.riot_id, %match_id, "posted");
    Ok(())
}

/// Produce `<dir>/overview.md` with the OVERVIEW prompt over the grounded
/// moments, via the `claude` CLI.
async fn write_overview(riot_id: &str, dir: &Path) -> Result<()> {
    let system_prompt = std::fs::read_to_string("prompts/OVERVIEW.md").context("reading prompts/OVERVIEW.md")?;
    let output = tokio::process::Command::new("claude")
        .arg("-p")
        .args(["--model", "opus"])
        .arg("--add-dir")
        .arg(dir)
        .args(["--allowedTools", "Read Grep Glob"])
        .args(["--system-prompt", &system_prompt])
        .arg(format!(
            "Write the post-game overview centered on {riot_id}. Start from moments.md in the provided directory."
        ))
        .output()
        .await
        .context("running claude for the overview")?;
    if !output.status.success() {
        bail!("claude overview exited {}: {}", output.status, String::from_utf8_lossy(&output.stderr));
    }
    std::fs::write(dir.join("overview.md"), &output.stdout)
        .with_context(|| format!("writing {}", dir.join("overview.md").display()))?;
    Ok(())
}

/// Record and attach clips for at most one pending game — serialized,
/// idle-only, retry-capped (the replay client is a contended singleton).
async fn clips_pass(cli: &Cli, journal: &Journal) -> Result<()> {
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
        return Ok(()); // One success per pass keeps polling snappy.
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
    fn slug_matches_the_shell_marker_names() {
        // poll.sh: tr -c 'A-Za-z0-9' '_' — `#` and spaces both become `_`.
        assert_eq!(slug("Moon#132"), "Moon_132");
        assert_eq!(slug("Hide on bush#KR1"), "Hide_on_bush_KR1");
    }
}
