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

pub use kinds::{ClipAttempt, ClipsAbandoned, ClipsAttached, GamePosted, RankObserved};

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

    /// True if the journal holds any events (guards a second `--journal-import`).
    pub fn is_empty(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count == 0)
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
                _ => serde_json::Value::Null,
            };
            println!("{}", serde_json::json!({ "seq": seq, "at_millis": at_millis, "kind": kind, "payload": payload }));
        }
        Ok(())
    }

    /// Fold every event, in append order, into the current state.
    pub fn fold(&self) -> Result<Projection> {
        let mut stmt = self.conn.prepare("SELECT kind, payload FROM events ORDER BY seq")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?;

        let mut projection = Projection::default();
        for row in rows {
            let (kind, payload) = row?;
            match kind.as_str() {
                GamePosted::NAME => projection.apply_posted(decode::<GamePosted>(&payload)?),
                RankObserved::NAME => projection.apply_rank(decode::<RankObserved>(&payload)?),
                ClipAttempt::NAME => projection.apply_attempt(&decode::<ClipAttempt>(&payload)?),
                ClipsAttached::NAME => {
                    let e = decode::<ClipsAttached>(&payload)?;
                    projection.apply_terminal(&e.platform, e.game_id);
                }
                ClipsAbandoned::NAME => {
                    let e = decode::<ClipsAbandoned>(&payload)?;
                    projection.apply_terminal(&e.platform, e.game_id);
                }
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
    /// Discord message id per posted game (last post wins).
    message_ids: HashMap<(String, u64), String>,
    /// Clip job state per posted game.
    clips: HashMap<(String, u64), ClipJob>,
    /// Newest posted game id per (riot_id lowercase, queue_id) — the floor
    /// below which older window entries are history, not news.
    newest: HashMap<(String, u32), u64>,
    /// Last observed ladder value per (puuid, queue_id).
    ranks: HashMap<(String, u32), i32>,
    /// Post-time LP line per posted game (last post wins).
    rank_lines: HashMap<(String, u64), crate::stats::RankInfo>,
}

impl Projection {
    fn apply_posted(&mut self, event: GamePosted) {
        let key = (event.platform.clone(), event.game_id);
        let floor = self.newest.entry((event.riot_id.to_lowercase(), event.queue_id)).or_insert(0);
        *floor = (*floor).max(event.game_id);
        self.posted.insert((event.platform, key.1, event.riot_id.to_lowercase()));
        if !event.message_id.is_empty() {
            self.message_ids.insert(key.clone(), event.message_id);
        }
        if let Some(rank) = event.rank {
            self.rank_lines.insert(key.clone(), rank);
        }
        let job = self.clips.entry(key).or_default();
        // A re-post moves the clip perspective (last poster wins) but never
        // resurrects a finished job.
        if !job.done {
            job.riot_id = event.riot_id;
        }
    }

    fn apply_rank(&mut self, event: RankObserved) {
        self.ranks.insert((event.puuid, event.queue_id), event.ladder_value);
    }

    fn apply_attempt(&mut self, event: &ClipAttempt) {
        let job = self.clips.entry((event.platform.clone(), event.game_id)).or_default();
        job.tries = job.tries.max(event.try_number);
    }

    fn apply_terminal(&mut self, platform: &str, game_id: u64) {
        self.clips.entry((platform.to_string(), game_id)).or_default().done = true;
    }

    /// Was this game already posted for this player? (Riot IDs compare
    /// case-insensitively.)
    pub fn is_posted(&self, platform: &str, game_id: u64, riot_id: &str) -> bool {
        self.posted.contains(&(platform.to_string(), game_id, riot_id.to_lowercase()))
    }

    /// The Discord message id recorded for a posted game.
    pub fn message_id(&self, platform: &str, game_id: u64) -> Option<&str> {
        self.message_ids.get(&(platform.to_string(), game_id)).map(String::as_str)
    }

    /// Unfinished clip jobs, newest game first (the old `sort -r` order: the
    /// latest post gets its clips promptly and a stuck replay can't starve
    /// newer games behind it).
    pub fn pending_clips(&self) -> Vec<(String, u64, ClipJob)> {
        let mut jobs: Vec<_> = self
            .clips
            .iter()
            .filter(|(_, job)| !job.done && !job.riot_id.is_empty())
            .map(|((platform, game_id), job)| (platform.clone(), *game_id, job.clone()))
            .collect();
        jobs.sort_by_key(|(_, game_id, _)| std::cmp::Reverse(*game_id));
        jobs
    }

    /// The newest game id already posted for this player — in one queue, or
    /// across all queues when `queue_id` is `None` (an `all` watch line).
    pub fn newest_posted(&self, riot_id: &str, queue_id: Option<u32>) -> Option<u64> {
        let rid = riot_id.to_lowercase();
        self.newest
            .iter()
            .filter(|((r, q), _)| *r == rid && queue_id.is_none_or(|want| *q == want))
            .map(|(_, id)| *id)
            .max()
    }

    /// The clip job for one posted game, if any.
    pub fn clip_job(&self, platform: &str, game_id: u64) -> Option<&ClipJob> {
        self.clips.get(&(platform.to_string(), game_id))
    }

    /// The LP line rendered when this game posted, if any.
    pub fn rank_line(&self, platform: &str, game_id: u64) -> Option<&crate::stats::RankInfo> {
        self.rank_lines.get(&(platform.to_string(), game_id))
    }

    /// The previous ladder value for (puuid, queue), the LP-delta baseline.
    pub fn previous_ladder(&self, puuid: &str, queue_id: u32) -> Option<i32> {
        self.ranks.get(&(puuid.to_string(), queue_id)).copied()
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
        }
    }

    #[test]
    fn journal_roundtrips_and_folds() {
        let dir = std::env::temp_dir().join(format!("scry-journal-test-{}", std::process::id()));
        let path = dir.join("scry.sqlite");
        let journal = Journal::open(&path).unwrap();

        journal.append(&posted("NA1", 100, "Moon#132", "m-100")).unwrap();
        journal.append(&ClipAttempt { platform: "NA1".into(), game_id: 100, try_number: 1 }).unwrap();
        journal.append(&posted("NA1", 101, "Himles#9267", "m-101")).unwrap();
        journal.append(&ClipsAttached { platform: "NA1".into(), game_id: 100 }).unwrap();
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
        let projection = Journal::open(&path).unwrap().fold().unwrap();

        assert!(projection.is_posted("NA1", 100, "moon#132"), "case-insensitive dedup");
        assert!(!projection.is_posted("NA1", 102, "Moon#132"));
        assert_eq!(projection.message_id("NA1", 101), Some("m-101"));
        let pending = projection.pending_clips();
        assert_eq!(pending.len(), 1, "game 100 finished; only 101 pending");
        assert_eq!((pending[0].0.as_str(), pending[0].1), ("NA1", 101));
        assert_eq!(projection.previous_ladder("p-1", 420), Some(1355));
        // The floor: newest posted per player/queue, any-queue when None.
        assert_eq!(projection.newest_posted("MOON#132", Some(420)), Some(100));
        assert_eq!(projection.newest_posted("moon#132", Some(440)), None);
        assert_eq!(projection.newest_posted("himles#9267", None), Some(101));

        std::fs::remove_dir_all(&dir).ok();
    }
}
