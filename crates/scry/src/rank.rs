//! LP tracking. Match data has no rank info, so an LP *difference* is computed
//! by snapshotting the player's current standing (league-v4) and diffing it
//! against the previous snapshot. Deltas are attributed per game as long as we
//! catalog games in chronological order (one ranked game per poll interval).

use riven::consts::{Division, Queue, QueueType, Tier};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Current ranked standing in one queue.
#[derive(Debug, Clone)]
pub struct Rank {
    pub tier: Tier,
    pub division: Division,
    pub lp: i32,
}

/// Master and above — a single continuous LP pool with no divisions.
const APEX: i32 = 7;

impl Rank {
    /// A monotonic ladder position so a delta survives promotions/demotions
    /// (e.g. Gold II 98 -> Gold I 20 reads as +22, not -78).
    pub fn ladder_value(&self) -> i32 {
        let idx = tier_index(self.tier);
        if idx >= APEX {
            APEX * 400 + self.lp
        } else {
            let div = match self.division {
                Division::IV => 0,
                Division::III => 1,
                Division::II => 2,
                _ => 3, // I (or legacy V) — top of the tier.
            };
            idx * 400 + div * 100 + self.lp
        }
    }

    /// Display like "Gold II", or just "Master" for apex tiers.
    pub fn label(&self) -> String {
        let tier = titlecase(self.tier.as_ref());
        if tier_index(self.tier) >= APEX {
            tier
        } else {
            format!("{tier} {}", self.division)
        }
    }
}

fn tier_index(t: Tier) -> i32 {
    match t {
        Tier::IRON => 0,
        Tier::BRONZE => 1,
        Tier::SILVER => 2,
        Tier::GOLD => 3,
        Tier::PLATINUM => 4,
        Tier::EMERALD => 5,
        Tier::DIAMOND => 6,
        Tier::MASTER | Tier::GRANDMASTER | Tier::CHALLENGER => APEX,
        _ => 0,
    }
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// The ranked queue type for a match queue, if it is ranked.
pub fn queue_type(queue: Queue) -> Option<QueueType> {
    match queue.0 {
        420 => Some(QueueType::RANKED_SOLO_5x5),
        440 => Some(QueueType::RANKED_FLEX_SR),
        _ => None,
    }
}

/// A legacy `state/lp` snapshot — the ladder value (for diffing) plus readable
/// context. Superseded by the journal's `rank_observed` events; read only by
/// `--journal-import`'s one-shot backfill.
#[derive(Serialize, Deserialize)]
pub struct Snapshot {
    pub value: i32,
    pub label: String,
    pub lp: i32,
}

/// A legacy snapshot's contents, if the file parses.
pub fn read_snapshot(path: &Path) -> Option<Snapshot> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}
