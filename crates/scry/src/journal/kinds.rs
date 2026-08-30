//! The `scry.journal.*` event vocabulary — ADR-0059 storage-shaped kinds
//! (content-hashed field tags, unknown-field bucket), so a reader built before
//! a field was added still decodes the record and a future field lands as a
//! trailing optional.

use aether_data::storage::{RecordReader, RecordWriter, fold_path_segment};
use aether_data::{StorageError, StorageLeaves};
use serde::{Deserialize, Serialize};

use crate::stats::RankInfo;

// Nested storage containers hand-roll their leaves (the derive covers only
// the root record) — the same pattern aether-bloomery uses for its nested
// view structs. Field names feed the content hash, so renaming one is a
// schema change.
impl StorageLeaves for RankInfo {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.label.contribute(fold_path_segment(carry, b"label", depth), depth + 1, sink)?;
        self.lp.contribute(fold_path_segment(carry, b"lp", depth), depth + 1, sink)?;
        self.delta.contribute(fold_path_segment(carry, b"delta", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            label: String::assemble(fold_path_segment(carry, b"label", depth), depth + 1, source)?,
            lp: i32::assemble(fold_path_segment(carry, b"lp", depth), depth + 1, source)?,
            delta: Option::<i32>::assemble(fold_path_segment(carry, b"delta", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        String::is_absent(fold_path_segment(carry, b"label", depth), depth + 1, source)
            && i32::is_absent(fold_path_segment(carry, b"lp", depth), depth + 1, source)
            && Option::<i32>::is_absent(fold_path_segment(carry, b"delta", depth), depth + 1, source)
    }
}

/// One chosen clip: the moment's timestamp, the replay window sized to it,
/// and the grounded one-line caption the post renders beside the video.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ClipPick {
    /// The moment's in-game time, unix-style millis from game start.
    pub t_millis: i64,
    /// Game-second the recording should start at (lead-in already included).
    pub seek_secs: i64,
    /// Recording length in seconds — sized to the fight span, capped.
    pub duration_secs: i64,
    /// The grounded one-liner describing the moment.
    pub summary: String,
}

impl ClipPick {
    /// The moment's timestamp as `m:ss` (how captions and logs render it).
    pub fn timestamp(&self) -> String {
        let secs = self.t_millis / 1000;
        format!("{}:{:02}", secs / 60, secs % 60)
    }

    /// The caption the post renders beside the clip: `**m:ss** — summary`.
    pub fn caption(&self) -> String {
        format!("**{}** — {}", self.timestamp(), self.summary)
    }
}

impl StorageLeaves for ClipPick {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        self.t_millis.contribute(fold_path_segment(carry, b"t_millis", depth), depth + 1, sink)?;
        self.seek_secs.contribute(fold_path_segment(carry, b"seek_secs", depth), depth + 1, sink)?;
        self.duration_secs.contribute(fold_path_segment(carry, b"duration_secs", depth), depth + 1, sink)?;
        self.summary.contribute(fold_path_segment(carry, b"summary", depth), depth + 1, sink)
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        Ok(Self {
            t_millis: i64::assemble(fold_path_segment(carry, b"t_millis", depth), depth + 1, source)?,
            seek_secs: i64::assemble(fold_path_segment(carry, b"seek_secs", depth), depth + 1, source)?,
            duration_secs: i64::assemble(fold_path_segment(carry, b"duration_secs", depth), depth + 1, source)?,
            summary: String::assemble(fold_path_segment(carry, b"summary", depth), depth + 1, source)?,
        })
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        i64::is_absent(fold_path_segment(carry, b"t_millis", depth), depth + 1, source)
            && i64::is_absent(fold_path_segment(carry, b"seek_secs", depth), depth + 1, source)
            && i64::is_absent(fold_path_segment(carry, b"duration_secs", depth), depth + 1, source)
            && String::is_absent(fold_path_segment(carry, b"summary", depth), depth + 1, source)
    }
}

/// The deterministic clip picks for one tracked player's perspective of a
/// game — the output of the joint assignment over every tracked player in the
/// lobby. Journaled at analysis time; the post's captions and the recorder's
/// clip windows read this event, never an archive file. Last write wins, so a
/// re-analysis refreshes the picks.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.picks_assigned")]
pub struct PicksAssigned {
    /// Riot platform id, e.g. "NA1".
    pub platform: String,
    /// Numeric game id (the match id with the platform prefix stripped).
    pub game_id: u64,
    /// The tracked player this perspective centers on (`gameName#tagLine`).
    pub riot_id: String,
    /// The player's permanent PUUID (the identity the projection keys by).
    pub puuid: String,
    /// The best moment worth celebrating, if any scored.
    pub highlight: Option<ClipPick>,
    /// The best moment worth learning from, if any scored.
    pub lowlight: Option<ClipPick>,
}

/// A game's package was posted to Discord for one tracked player.
///
/// This is the dedup truth the poller consults (was this game already posted
/// for this player?) and the clip pass's job source: a posted game without a
/// terminal clip event is a pending clip job for `riot_id`'s perspective.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.game_posted")]
pub struct GamePosted {
    /// Riot platform id, e.g. "NA1".
    pub platform: String,
    /// Numeric game id (the match id with the platform prefix stripped).
    pub game_id: u64,
    /// The tracked player the post centers on (`gameName#tagLine`).
    pub riot_id: String,
    /// Match queue id (420 ranked solo, 440 flex, …).
    pub queue_id: u32,
    /// Discord message id, so the clip pass can edit the post later. Empty
    /// when the webhook returned none (clips then can't attach).
    pub message_id: String,
    /// The LP line rendered at post time, so the clip-attach edit reproduces
    /// it without a Riot call. Trailing optional (ADR-0059): rows written
    /// before this field decode with it absent.
    pub rank: Option<RankInfo>,
    /// The player's permanent PUUID — the identity dedup and floors key by
    /// (names are display labels that change). Trailing optional; rows
    /// written before this field re-key through the account_resolved pins.
    pub puuid: Option<String>,
}

/// The player's ranked standing at post time (league-v4). The LP delta on a
/// post is the ladder-value diff between the two most recent observations for
/// the same (`puuid`, `queue_id`) — this replaces the old `state/lp` snapshot
/// files.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.rank_observed")]
pub struct RankObserved {
    pub puuid: String,
    /// Display context only; the baseline is keyed by `puuid`.
    pub riot_id: String,
    pub queue_id: u32,
    /// Monotonic ladder position (`rank::Rank::ladder_value`), the diff basis.
    pub ladder_value: i32,
    /// Display label at observation time, e.g. "Gold II".
    pub label: String,
    pub lp: i32,
}

/// An account's Riot ID resolved to its permanent PUUID — the memory that
/// lets a later resolution failure be recognized as a rename and reversed
/// through account-v1 by-puuid. Appended only when the mapping changes.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.account_resolved")]
pub struct AccountResolved {
    pub riot_id: String,
    pub puuid: String,
}

/// A tracked account's Riot ID changed; the watch list was rewritten and the
/// posting floor carried to the new name. Audit record of the self-heal.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.account_renamed")]
pub struct AccountRenamed {
    pub old_riot_id: String,
    pub new_riot_id: String,
    pub puuid: String,
}

/// One clip-recording attempt started for a posted game (the retry budget).
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.clip_attempt")]
pub struct ClipAttempt {
    pub platform: String,
    pub game_id: u64,
    /// 1-based attempt number; the projection keeps the max seen.
    pub try_number: u32,
    /// The tracked player whose perspective this attempt records — clip jobs
    /// are per (game, player) since tracked players share games. Trailing
    /// optional: a legacy event (absent) applies to every job of the game.
    pub riot_id: Option<String>,
}

/// Highlight/lowlight clips were recorded and attached to the post — the
/// successful terminal state of a clip job.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.clips_attached")]
pub struct ClipsAttached {
    pub platform: String,
    pub game_id: u64,
    /// The perspective attached; a legacy event (absent) finishes every job
    /// of the game.
    pub riot_id: Option<String>,
}

/// Clip recording abandoned after exhausting the retry budget — the failed
/// terminal state of a clip job.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.clips_abandoned")]
pub struct ClipsAbandoned {
    pub platform: String,
    pub game_id: u64,
    /// Attempts made before giving up.
    pub tries: u32,
    /// The perspective abandoned; a legacy event (absent) — including the
    /// --abandon-clips maintenance flag — finishes every job of the game.
    pub riot_id: Option<String>,
}
