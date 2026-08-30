//! Operational commands for the scry deployment — `cargo xtask <command>`.
//!
//! Watch-list edits rewrite `scripts/accounts.txt` (the roster's source of
//! truth; the journal only records what happened). Service control drives the
//! `com.scry.poller` launchd agent. Journal inspection shells the scry binary
//! itself (`--journal-dump`) so the TLV decoding lives in exactly one place.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "scry deployment operations")]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a tracked account to the watch list.
    AddAccount {
        /// Riot ID, `gameName#tagLine`.
        riot_id: String,
        /// Platform code (na1, euw1, kr, …).
        #[arg(long, default_value = "na1")]
        region: String,
        /// Queue ids to scan, comma-separated (or `all`).
        #[arg(long, default_value = "420,440")]
        queues: String,
    },
    /// Remove a tracked account from the watch list.
    RemoveAccount { riot_id: String },
    /// Rename a tracked account (Riot ID changes keep the PUUID but break
    /// resolution). The new name starts with a fresh journal floor, so its
    /// newest game backfills once even if it posted under the old name.
    RenameAccount { old: String, new: String },
    /// Print the watch list.
    ListAccounts,
    /// Service state + journal summary (events by kind, pending clip jobs).
    Status,
    /// Start the poller service (launchd).
    Start,
    /// Stop the poller service.
    Stop,
    /// Restart the poller service (picks up a rebuilt binary or edited config).
    Restart,
    /// Tail the poller log.
    Logs {
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
}

fn main() -> Result<()> {
    let root = repo_root();
    match Args::parse().command {
        Cmd::AddAccount { riot_id, region, queues } => add_account(&root, &riot_id, &region, &queues),
        Cmd::RemoveAccount { riot_id } => remove_account(&root, &riot_id),
        Cmd::RenameAccount { old, new } => rename_account(&root, &old, &new),
        Cmd::ListAccounts => list_accounts(&root),
        Cmd::Status => status(&root),
        Cmd::Start => launchctl(&["bootstrap"], true),
        Cmd::Stop => launchctl(&["bootout"], false),
        Cmd::Restart => {
            let _ = launchctl(&["bootout"], false);
            std::thread::sleep(std::time::Duration::from_secs(2));
            launchctl(&["bootstrap"], true)
        }
        Cmd::Logs { lines } => logs(lines),
    }
}

/// The workspace root (this crate lives at `<root>/xtask`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask has a parent").to_path_buf()
}

fn accounts_path(root: &Path) -> PathBuf {
    root.join("scripts/accounts.txt")
}

fn read_accounts(root: &Path) -> Result<String> {
    let path = accounts_path(root);
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn write_accounts(root: &Path, contents: &str) -> Result<()> {
    let path = accounts_path(root);
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
}

fn add_account(root: &Path, riot_id: &str, region: &str, queues: &str) -> Result<()> {
    if !riot_id.contains('#') {
        bail!("riot id must be `gameName#tagLine`, got `{riot_id}`");
    }
    let mut contents = read_accounts(root)?;
    if line_for(&contents, riot_id).is_some() {
        bail!("`{riot_id}` is already tracked");
    }
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(&format!("{riot_id} | {region} | {queues}\n"));
    write_accounts(root, &contents)?;
    println!("added {riot_id} ({region}, queues {queues}); the running poller picks it up next pass");
    Ok(())
}

fn remove_account(root: &Path, riot_id: &str) -> Result<()> {
    let contents = read_accounts(root)?;
    let Some(line) = line_for(&contents, riot_id) else {
        bail!("`{riot_id}` is not in the watch list");
    };
    let remaining: String = contents.lines().filter(|l| *l != line).map(|l| format!("{l}\n")).collect();
    write_accounts(root, &remaining)?;
    println!("removed {riot_id}");
    Ok(())
}

fn rename_account(root: &Path, old: &str, new: &str) -> Result<()> {
    if !new.contains('#') {
        bail!("riot id must be `gameName#tagLine`, got `{new}`");
    }
    let contents = read_accounts(root)?;
    let Some(line) = line_for(&contents, old) else {
        bail!("`{old}` is not in the watch list");
    };
    let renamed = contents.replace(&line, &line.replacen(old, new, 1));
    write_accounts(root, &renamed)?;
    println!("renamed {old} -> {new} (its newest game may backfill once under the new name)");
    Ok(())
}

/// The watch-list line whose riot-id field matches, case-insensitively.
fn line_for(contents: &str, riot_id: &str) -> Option<String> {
    contents
        .lines()
        .find(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with('#')
                && trimmed.split('|').next().is_some_and(|field| field.trim().eq_ignore_ascii_case(riot_id))
        })
        .map(str::to_string)
}

fn list_accounts(root: &Path) -> Result<()> {
    for line in read_accounts(root)?.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            println!("{trimmed}");
        }
    }
    Ok(())
}

fn status(root: &Path) -> Result<()> {
    let running = Command::new("launchctl")
        .args(["print", &format!("gui/{}/com.scry.poller", uid())])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    println!(
        "service: {}",
        if running {
            "running"
        } else {
            "stopped"
        }
    );

    // The scry binary owns the TLV decoding; feed it the .env config the
    // service runs with.
    let dump = Command::new("cargo")
        .current_dir(root)
        .args(["run", "--quiet", "--release", "-p", "scry", "--", "--journal-dump"])
        .envs(dot_env(root))
        .output()
        .context("running scry --journal-dump")?;
    if !dump.status.success() {
        bail!("journal dump failed: {}", String::from_utf8_lossy(&dump.stderr));
    }

    let mut by_kind = std::collections::BTreeMap::<String, usize>::new();
    let mut posted = std::collections::HashSet::new();
    let mut terminal = std::collections::HashSet::new();
    for line in String::from_utf8_lossy(&dump.stdout).lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = event["kind"].as_str().unwrap_or("?").to_string();
        *by_kind.entry(kind.clone()).or_default() += 1;
        let game =
            (event["payload"]["platform"].as_str().unwrap_or("").to_string(), event["payload"]["game_id"].as_u64());
        match kind.as_str() {
            "scry.journal.game_posted" => {
                posted.insert(game);
            }
            "scry.journal.clips_attached" | "scry.journal.clips_abandoned" => {
                terminal.insert(game);
            }
            _ => {}
        }
    }
    for (kind, count) in &by_kind {
        println!("{kind}: {count}");
    }
    println!("pending clip jobs: {}", posted.difference(&terminal).count());
    Ok(())
}

/// KEY=VALUE pairs from the repo `.env` (the service's secrets/config).
fn dot_env(root: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(root.join(".env"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn launchctl(action: &[&str], with_plist: bool) -> Result<()> {
    let target = format!("gui/{}", uid());
    // HOME is a genuinely external var (the launchd agent's location), not cap config.
    #[allow(clippy::disallowed_methods)]
    let plist = format!("{}/Library/LaunchAgents/com.scry.poller.plist", std::env::var("HOME").unwrap_or_default());
    let mut args: Vec<String> = action.iter().map(ToString::to_string).collect();
    if with_plist {
        args.push(target.clone());
        args.push(plist);
    } else {
        args.push(format!("{target}/com.scry.poller"));
    }
    let status = Command::new("launchctl").args(&args).status().context("running launchctl")?;
    if !status.success() {
        bail!("launchctl {} failed", action.join(" "));
    }
    println!("launchctl {} ok", action.join(" "));
    Ok(())
}

fn uid() -> u32 {
    // SAFETY: getuid has no failure modes.
    unsafe { libc_getuid() }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn logs(lines: usize) -> Result<()> {
    // HOME is a genuinely external var (the service's log location), not cap config.
    #[allow(clippy::disallowed_methods)]
    let log = format!("{}/Library/Logs/scry-poller.log", std::env::var("HOME").unwrap_or_default());
    let status = Command::new("tail").args(["-n", &lines.to_string(), &log]).status().context("running tail")?;
    if !status.success() {
        bail!("tail failed");
    }
    Ok(())
}
