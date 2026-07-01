//! LP tracking. Match data has no rank info, so an LP *difference* is computed
//! by snapshotting the player's current standing (league-v4) and diffing it
//! against the previous snapshot. Deltas are attributed per game as long as we
//! catalog games in chronological order (one ranked game per poll interval).

use anyhow::{Context, Result};
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

/// A persisted snapshot — the ladder value (for diffing) plus readable context.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    value: i32,
    label: String,
    lp: i32,
}

/// The previous snapshot's ladder value, if one exists.
pub fn read_previous(path: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Snapshot>(&raw).ok().map(|s| s.value)
}

/// Persist the current standing as the new snapshot for next time.
pub fn write_snapshot(path: &Path, rank: &Rank) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let snap = Snapshot {
        value: rank.ladder_value(),
        label: rank.label(),
        lp: rank.lp,
    };
    std::fs::write(path, serde_json::to_string(&snap)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
