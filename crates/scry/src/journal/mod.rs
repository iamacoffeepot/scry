//! The scry journal: an append-only SQLite event log (WAL, single writer)
//! whose payloads are ADR-0059 storage-shaped kinds — the durable home for
//! everything the poller used to scatter across filesystem markers
//! (`.posted-*`, `.clip-*`, `.message-id`, `state/lp/*.json`).
//!
//! The journal is the truth; read-side state is a pure fold over the events
//! ([`Projection`]), rebuilt on open — the ADR-0149 boundary, scaled down.
//! The `kind` column stamps each row with the writing kind's `NAME`, the
//! identity the fold dispatches on; a row whose kind this binary doesn't know
//! is skipped (a newer writer's event, not an error), and unknown *fields*
//! inside a known kind ride the storage shape's unknown-field bucket.

mod kinds;

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_data::{Kind, Storage, StorageData};
use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use kinds::{
    AccountRenamed, AccountResolved, ClipAttempt, ClipPick, ClipsAbandoned, ClipsAttached, GamePosted, PicksAssigned,
    RankObserved,
};

/// The append-only event log.
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Open (creating if absent) the journal at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening journal {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 seq       INTEGER PRIMARY KEY AUTOINCREMENT,
                 at_millis INTEGER NOT NULL,
                 kind      TEXT    NOT NULL,
                 payload   BLOB    NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    /// Append one event, stamped with its kind name and the current time.
    pub fn append<E: Storage + Serialize + Clone>(&self, event: &E) -> Result<()> {
        let payload = E::encode_storage(&StorageData::from_value(event.clone()))
            .map_err(|e| anyhow!("encoding {}: {e}", E::NAME))?;
        self.conn
            .execute(
                "INSERT INTO events (at_millis, kind, payload) VALUES (?1, ?2, ?3)",
                rusqlite::params![now_millis(), E::NAME, payload],
            )
            .with_context(|| format!("appending {}", E::NAME))?;
        Ok(())
    }

    /// Print every event as one JSON line: `{seq, at_millis, kind, payload}`.
    /// An unknown kind prints with a null payload rather than failing.
    pub fn dump(&self) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT seq, at_millis, kind, payload FROM events ORDER BY seq")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, Vec<u8>>(3)?))
        })?;
        for row in rows {
            let (seq, at_millis, kind, payload) = row?;
            let payload = match kind.as_str() {
                GamePosted::NAME => serde_json::to_value(decode::<GamePosted>(&payload)?)?,
                RankObserved::NAME => serde_json::to_value(decode::<RankObserved>(&payload)?)?,
                ClipAttempt::NAME => serde_json::to_value(decode::<ClipAttempt>(&payload)?)?,
                ClipsAttached::NAME => serde_json::to_value(decode::<ClipsAttached>(&payload)?)?,
                ClipsAbandoned::NAME => serde_json::to_value(decode::<ClipsAbandoned>(&payload)?)?,
                PicksAssigned::NAME => serde_json::to_value(decode::<PicksAssigned>(&payload)?)?,
                AccountResolved::NAME => serde_json::to_value(decode::<AccountResolved>(&payload)?)?,
                AccountRenamed::NAME => serde_json::to_value(decode::<AccountRenamed>(&payload)?)?,
                _ => serde_json::Value::Null,
            };
            println!("{}", serde_json::json!({ "seq": seq, "at_millis": at_millis, "kind": kind, "payload": payload }));
        }
        Ok(())
    }

    /// Fold every event, in append order, into the current state.
    ///
    /// Two passes: the first collects the name→PUUID pins (account_resolved
    /// events plus any game_posted that carries its puuid), the second applies
    /// events with every name-keyed row re-keyed to its permanent identity —
    /// so rows written before the puuid field, or before a rename, land under
    /// the same identity as everything after.
    pub fn fold(&self) -> Result<Projection> {
        let mut stmt = self.conn.prepare("SELECT kind, payload FROM events ORDER BY seq")?;
        let rows: Vec<(String, Vec<u8>)> = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
            .collect::<Result<_, _>>()?;

        let mut projection = Projection::default();
        for (kind, payload) in &rows {
            match kind.as_str() {
                AccountResolved::NAME => {
                    let e = decode::<AccountResolved>(payload)?;
                    projection.pin(&e.riot_id, e.puuid);
                }
                GamePosted::NAME => {
                    let e = decode::<GamePosted>(payload)?;
                    if let Some(puuid) = e.puuid {
                        projection.pin(&e.riot_id, puuid);
                    }
                }
                PicksAssigned::NAME => {
                    let e = decode::<PicksAssigned>(payload)?;
                    projection.pin(&e.riot_id, e.puuid);
                }
                _ => {}
            }
        }
        for (kind, payload) in &rows {
            let payload = payload.as_slice();
            match kind.as_str() {
                GamePosted::NAME => {
                    let e = decode::<GamePosted>(payload)?;
                    projection.apply_posted(e);
                }
                RankObserved::NAME => projection.apply_rank(decode::<RankObserved>(payload)?),
                ClipAttempt::NAME => projection.apply_attempt(&decode::<ClipAttempt>(payload)?),
                ClipsAttached::NAME => {
                    let e = decode::<ClipsAttached>(payload)?;
                    projection.apply_terminal(&e.platform, e.game_id, e.riot_id.as_deref());
                }
                ClipsAbandoned::NAME => {
                    let e = decode::<ClipsAbandoned>(payload)?;
                    projection.apply_terminal(&e.platform, e.game_id, e.riot_id.as_deref());
                }
                PicksAssigned::NAME => {
                    let e = decode::<PicksAssigned>(payload)?;
                    let ident = projection.canonical(&e.puuid);
                    projection.picks.insert((e.platform.clone(), e.game_id, ident), e);
                }
                // Consumed by the pin pass above; audit-only here.
                AccountResolved::NAME | AccountRenamed::NAME => {}
                other => tracing::debug!(kind = other, "skipping journal event from a newer writer"),
            }
        }
        Ok(projection)
    }
}

fn decode<E: Storage + DeserializeOwned>(payload: &[u8]) -> Result<E> {
    E::decode_storage(payload).map(|data| data.value).map_err(|e| anyhow!("decoding {}: {e}", E::NAME))
}

fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// A pending or finished clip job for one posted game.
#[derive(Default, Debug, Clone)]
pub struct ClipJob {
    /// The tracked player whose perspective the clips take (the last poster,
    /// matching the old `.clip-rid` semantic for shared games).
    pub riot_id: String,
    /// Attempts already made.
    pub tries: u32,
    /// Terminal (attached or abandoned) — no further attempts.
    pub done: bool,
}

/// Read-side state: a pure fold of the journal.
#[derive(Default)]
pub struct Projection {
    /// (platform, game_id, riot_id lowercase) triples already posted.
    posted: std::collections::HashSet<(String, u64, String)>,
    /// Discord message id per (game, identity) — every tracked player's post
    /// of a shared game is its own message.
    message_ids: HashMap<(String, u64, String), String>,
    /// Clip job state per (platform, game, identity) — tracked players share
    /// games, and each poster gets their own perspective recorded.
    clips: HashMap<(String, u64, String), ClipJob>,
    /// Newest posted game per (identity, queue_id) — the floor below which
    /// older window entries are history, not news.
    newest: HashMap<(String, u32), (String, u64)>,
    /// Name (lowercase) → latest PUUID pins; the identity every name-keyed
    /// query re-keys through, so a rename never orphans state.
    pins: HashMap<String, String>,
    /// PUUID → the older PUUID it succeeded. Riot encrypts PUUIDs per API
    /// key, so a key swap hands every account a fresh PUUID; when a name
    /// re-pins to a different one, the new era joins the old identity here
    /// and every identity-keyed lookup resolves through the chain.
    canon: HashMap<String, String>,
    /// Last observed ladder value per (puuid, queue_id).
    ranks: HashMap<(String, u32), i32>,
    /// Post-time LP line per (game, identity).
    rank_lines: HashMap<(String, u64, String), crate::stats::RankInfo>,
    /// Clip picks per (game, identity) — the analysis output the post's
    /// captions and the recorder's windows read. Last write wins.
    picks: HashMap<(String, u64, String), PicksAssigned>,
}

impl Projection {
    /// Record a name → PUUID pin, unioning PUUID eras: when the name already
    /// pointed at a different identity, the new PUUID's root joins the old
    /// one, so a key swap (which re-encrypts every PUUID) never forks a
    /// player's history. Each root gains at most one outgoing edge and only
    /// toward a distinct current root, so the chains stay acyclic.
    fn pin(&mut self, riot_id: &str, puuid: String) {
        let new_root = self.canonical(&puuid);
        if let Some(prev) = self.pins.get(&riot_id.to_lowercase()) {
            let prev_root = self.canonical(prev);
            if prev_root != new_root {
                self.canon.insert(new_root, prev_root);
            }
        }
        self.pins.insert(riot_id.to_lowercase(), puuid);
    }

    /// The oldest PUUID in this PUUID's era chain — the one identity every
    /// projection map keys by. Budgeted walk; the chain is one hop per key
    /// swap in practice.
    fn canonical(&self, puuid: &str) -> String {
        let mut current = puuid;
        for _ in 0..64 {
            match self.canon.get(current) {
                Some(older) => current = older,
                None => break,
            }
        }
        current.to_string()
    }

    /// The permanent identity a display name resolves to: its PUUID pin's
    /// era root, or the lowercased name itself when nothing has pinned it.
    fn ident(&self, riot_id: &str) -> String {
        let lower = riot_id.to_lowercase();
        self.pins.get(&lower).map(|p| self.canonical(p)).unwrap_or(lower)
    }

    fn apply_posted(&mut self, event: GamePosted) {
        let ident = event.puuid.as_deref().map(|p| self.canonical(p)).unwrap_or_else(|| self.ident(&event.riot_id));
        let key = (event.platform.clone(), event.game_id);
        let floor = self.newest.entry((ident.clone(), event.queue_id)).or_insert_with(|| (event.platform.clone(), 0));
        if event.game_id > floor.1 {
            *floor = (event.platform.clone(), event.game_id);
        }
        self.posted.insert((event.platform, key.1, ident.clone()));
        let per_player = (key.0.clone(), key.1, ident.clone());
        if !event.message_id.is_empty() {
            self.message_ids.insert(per_player.clone(), event.message_id);
        }
        if let Some(rank) = event.rank {
            self.rank_lines.insert(per_player, rank);
        }
        let job = self.clips.entry((key.0, key.1, ident)).or_default();
        // Refresh the display name; a re-post never resurrects a finished job.
        if !job.done {
            job.riot_id = event.riot_id;
        }
    }

    fn apply_rank(&mut self, event: RankObserved) {
        self.ranks.insert((self.canonical(&event.puuid), event.queue_id), event.ladder_value);
    }

    fn apply_attempt(&mut self, event: &ClipAttempt) {
        match &event.riot_id {
            Some(riot_id) => {
                let ident = self.ident(riot_id);
                let job = self.clips.entry((event.platform.clone(), event.game_id, ident)).or_default();
                job.tries = job.tries.max(event.try_number);
            }
            // Legacy / whole-game: every perspective of the game.
            None => {
                for ((platform, game_id, _), job) in &mut self.clips {
                    if *platform == event.platform && *game_id == event.game_id {
                        job.tries = job.tries.max(event.try_number);
                    }
                }
            }
        }
    }

    fn apply_terminal(&mut self, platform: &str, game_id: u64, riot_id: Option<&str>) {
        match riot_id {
            Some(riot_id) => {
                let ident = self.ident(riot_id);
                self.clips.entry((platform.to_string(), game_id, ident)).or_default().done = true;
            }
            None => {
                for ((p, g, _), job) in &mut self.clips {
                    if *p == platform && *g == game_id {
                        job.done = true;
                    }
                }
            }
        }
    }

    /// Was this game already posted for this player? The name re-keys through
    /// its PUUID pin, so the answer survives renames.
    pub fn is_posted(&self, platform: &str, game_id: u64, riot_id: &str) -> bool {
        self.posted.contains(&(platform.to_string(), game_id, self.ident(riot_id)))
    }

    /// The Discord message id recorded for this player's post of a game.
    pub fn message_id(&self, platform: &str, game_id: u64, riot_id: &str) -> Option<&str> {
        self.message_ids.get(&(platform.to_string(), game_id, self.ident(riot_id))).map(String::as_str)
    }

    /// Unfinished clip jobs, newest game first (the old `sort -r` order: the
    /// latest post gets its clips promptly and a stuck replay can't starve
    /// newer games behind it).
    pub fn pending_clips(&self) -> Vec<(String, u64, ClipJob)> {
        let mut jobs: Vec<_> = self
            .clips
            .iter()
            .filter(|(_, job)| !job.done && !job.riot_id.is_empty())
            .map(|((platform, game_id, _), job)| (platform.clone(), *game_id, job.clone()))
            .collect();
        jobs.sort_by(|a, b| {
            std::cmp::Reverse((a.1, &a.2.riot_id)).cmp(&std::cmp::Reverse((b.1, &b.2.riot_id))).reverse()
        });
        jobs.sort_by_key(|(_, game_id, _)| std::cmp::Reverse(*game_id));
        jobs
    }

    /// The newest game id already posted for this player — in one queue, or
    /// across all queues when `queue_id` is `None` (an `all` watch line).
    pub fn newest_posted(&self, riot_id: &str, queue_id: Option<u32>) -> Option<u64> {
        let ident = self.ident(riot_id);
        self.newest
            .iter()
            .filter(|((r, q), _)| *r == ident && queue_id.is_none_or(|want| *q == want))
            .map(|(_, (_, id))| *id)
            .max()
    }

    /// The pinned PUUID for a display name, if any pass recorded one.
    pub fn puuid_of(&self, riot_id: &str) -> Option<&str> {
        self.pins.get(&riot_id.to_lowercase()).map(String::as_str)
    }

    /// Every clip job of one posted game (one per tracked perspective).
    pub fn clip_jobs(&self, platform: &str, game_id: u64) -> Vec<&ClipJob> {
        self.clips.iter().filter(|((p, g, _), _)| p == platform && *g == game_id).map(|(_, job)| job).collect()
    }

    /// The LP line rendered when this player's post of the game went out.
    pub fn rank_line(&self, platform: &str, game_id: u64, riot_id: &str) -> Option<&crate::stats::RankInfo> {
        self.rank_lines.get(&(platform.to_string(), game_id, self.ident(riot_id)))
    }

    /// The journaled clip picks for this player's perspective of a game.
    pub fn picks(&self, platform: &str, game_id: u64, riot_id: &str) -> Option<&PicksAssigned> {
        self.picks.get(&(platform.to_string(), game_id, self.ident(riot_id)))
    }

    /// The previous ladder value for (puuid, queue), the LP-delta baseline.
    /// Resolves through the era chain so a key swap doesn't null the delta.
    pub fn previous_ladder(&self, puuid: &str, queue_id: u32) -> Option<i32> {
        self.ranks.get(&(self.canonical(puuid), queue_id)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted(platform: &str, game_id: u64, riot_id: &str, message_id: &str) -> GamePosted {
        GamePosted {
            platform: platform.to_string(),
            game_id,
            riot_id: riot_id.to_string(),
            queue_id: 420,
            message_id: message_id.to_string(),
            rank: None,
            puuid: None,
        }
    }

    #[test]
    fn journal_roundtrips_and_folds() {
        let dir = std::env::temp_dir().join(format!("scry-journal-test-{}", std::process::id()));
        let path = dir.join("scry.sqlite");
        let journal = Journal::open(&path).unwrap();

        journal.append(&posted("NA1", 100, "Moon#132", "m-100")).unwrap();
        journal.append(&ClipAttempt { platform: "NA1".into(), game_id: 100, try_number: 1, riot_id: None }).unwrap();
        journal.append(&posted("NA1", 101, "Himles#9267", "m-101")).unwrap();
        journal.append(&ClipsAttached { platform: "NA1".into(), game_id: 100, riot_id: None }).unwrap();
        journal
            .append(&RankObserved {
                puuid: "p-1".into(),
                riot_id: "Moon#132".into(),
                queue_id: 420,
                ladder_value: 1355,
                label: "Gold II".into(),
                lp: 55,
            })
            .unwrap();

        // Reopen from disk: the fold sees everything the first handle wrote.
        drop(journal);
        let journal2 = Journal::open(&path).unwrap();
        let projection = journal2.fold().unwrap();

        assert!(projection.is_posted("NA1", 100, "moon#132"), "case-insensitive dedup");
        assert!(!projection.is_posted("NA1", 102, "Moon#132"));
        assert_eq!(projection.message_id("NA1", 101, "Himles#9267"), Some("m-101"));
        assert_eq!(projection.message_id("NA1", 101, "Moon#132"), None, "another player's post is not mine");
        let pending = projection.pending_clips();
        assert_eq!(pending.len(), 1, "game 100 finished; only 101 pending");
        assert_eq!((pending[0].0.as_str(), pending[0].1), ("NA1", 101));
        assert_eq!(projection.previous_ladder("p-1", 420), Some(1355));
        // The floor: newest posted per player/queue, any-queue when None.
        assert_eq!(projection.newest_posted("MOON#132", Some(420)), Some(100));
        assert_eq!(projection.newest_posted("moon#132", Some(440)), None);
        assert_eq!(projection.newest_posted("himles#9267", None), Some(101));

        // Picks: journaled per (game, identity), read back for captions and
        // clip windows; last write wins.
        let pick = ClipPick { t_millis: 125_000, seek_secs: 118, duration_secs: 20, summary: "the outplay".into() };
        journal2
            .append(&PicksAssigned {
                platform: "NA1".into(),
                game_id: 101,
                riot_id: "Himles#9267".into(),
                puuid: "p-him".into(),
                highlight: Some(pick.clone()),
                lowlight: None,
            })
            .unwrap();
        let with_picks = journal2.fold().unwrap();
        let picks = with_picks.picks("NA1", 101, "himles#9267").expect("picks re-key through the puuid pin");
        assert_eq!(picks.highlight, Some(pick.clone()));
        assert_eq!(picks.lowlight, None);
        assert_eq!(pick.caption(), "**2:05** — the outplay");
        assert!(with_picks.picks("NA1", 100, "Moon#132").is_none());

        // Identity: a pin re-keys name-era rows to the PUUID, so after a
        // rename the NEW name still sees the old posts and floors.
        journal2.append(&AccountResolved { riot_id: "Moon#132".into(), puuid: "p-moon".into() }).unwrap();
        journal2.append(&AccountResolved { riot_id: "Moonlight#NEW".into(), puuid: "p-moon".into() }).unwrap();
        let renamed = journal2.fold().unwrap();
        assert!(renamed.is_posted("NA1", 100, "Moonlight#NEW"), "rename keeps dedup");
        assert_eq!(renamed.newest_posted("Moonlight#NEW", Some(420)), Some(100), "rename keeps the floor");

        // Key swap: Riot encrypts PUUIDs per API key, so a new key hands the
        // same name a different PUUID. The re-pin unions the eras — dedup,
        // floors, picks, and the LP baseline all survive under either PUUID.
        journal2.append(&AccountResolved { riot_id: "Himles#9267".into(), puuid: "p-him-rekeyed".into() }).unwrap();
        journal2.append(&AccountResolved { riot_id: "Moonlight#NEW".into(), puuid: "p-moon-rekeyed".into() }).unwrap();
        let swapped = journal2.fold().unwrap();
        assert!(swapped.is_posted("NA1", 100, "Moonlight#NEW"), "key swap keeps dedup");
        assert_eq!(swapped.newest_posted("Moonlight#NEW", Some(420)), Some(100), "key swap keeps the floor");
        assert!(swapped.picks("NA1", 101, "Himles#9267").is_some(), "key swap keeps picks");
        assert_eq!(swapped.previous_ladder("p-1", 420), Some(1355), "old-era ladder key still resolves");

        // A rank observed under the NEW puuid lands on the same identity: the
        // old-era baseline is what the next delta reads.
        journal2.append(&AccountResolved { riot_id: "OldMoon#132".into(), puuid: "p-1".into() }).unwrap();
        journal2.append(&AccountResolved { riot_id: "OldMoon#132".into(), puuid: "p-1-rekeyed".into() }).unwrap();
        let bridged = journal2.fold().unwrap();
        assert_eq!(bridged.previous_ladder("p-1-rekeyed", 420), Some(1355), "LP delta bridges the key swap");

        std::fs::remove_dir_all(&dir).ok();
    }
}
