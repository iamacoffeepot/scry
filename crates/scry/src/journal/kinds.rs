//! The `scry.journal.*` event vocabulary — ADR-0059 storage-shaped kinds
//! (content-hashed field tags, unknown-field bucket), so a reader built before
//! a field was added still decodes the record and a future field lands as a
//! trailing optional.

use serde::{Deserialize, Serialize};

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

/// One clip-recording attempt started for a posted game (the retry budget).
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.clip_attempt")]
pub struct ClipAttempt {
    pub platform: String,
    pub game_id: u64,
    /// 1-based attempt number; the projection keeps the max seen.
    pub try_number: u32,
}

/// Highlight/lowlight clips were recorded and attached to the post — the
/// successful terminal state of a clip job.
#[derive(aether_data::Storage, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "scry.journal.clips_attached")]
pub struct ClipsAttached {
    pub platform: String,
    pub game_id: u64,
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
}
