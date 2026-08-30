//! LP tracking. Match data has no rank info, so an LP *difference* is computed
//! by snapshotting the player's current standing (league-v4) and diffing it
//! against the previous snapshot. Deltas are attributed per game as long as we
//! catalog games in chronological order (one ranked game per poll interval).

use riven::consts::Queue;

/// Current ranked standing in one queue. Tier and division stay the raw
/// league-v4 strings ("GOLD", "II") so a tier this build has never heard of
/// still displays and diffs; only the ladder ordering needs the known table.
#[derive(Debug, Clone)]
pub struct Rank {
    pub tier: String,
    pub division: String,
    pub lp: i32,
}

/// Master and above — a single continuous LP pool with no divisions.
const APEX: i32 = 7;

impl Rank {
    /// A monotonic ladder position so a delta survives promotions/demotions
    /// (e.g. Gold II 98 -> Gold I 20 reads as +22, not -78).
    pub fn ladder_value(&self) -> i32 {
        let idx = tier_index(&self.tier);
        if idx >= APEX {
            APEX * 400 + self.lp
        } else {
            let div = match self.division.as_str() {
                "IV" => 0,
                "III" => 1,
                "II" => 2,
                _ => 3, // I (or legacy V) — top of the tier.
            };
            idx * 400 + div * 100 + self.lp
        }
    }

    /// Display like "Gold II", or just "Master" for apex tiers.
    pub fn label(&self) -> String {
        let tier = titlecase(&self.tier);
        if tier_index(&self.tier) >= APEX {
            tier
        } else {
            format!("{tier} {}", self.division)
        }
    }
}

/// Ladder ordering for the known tiers. An unknown (newly added) tier sorts
/// below Iron with a warn — its label and LP still render; only cross-tier
/// deltas around it are approximate until this table learns it.
fn tier_index(t: &str) -> i32 {
    match t.to_ascii_uppercase().as_str() {
        "IRON" => 0,
        "BRONZE" => 1,
        "SILVER" => 2,
        "GOLD" => 3,
        "PLATINUM" => 4,
        "EMERALD" => 5,
        "DIAMOND" => 6,
        "MASTER" | "GRANDMASTER" | "CHALLENGER" => APEX,
        other => {
            tracing::warn!(tier = other, "unknown ranked tier; ladder ordering approximate");
            -1
        }
    }
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// The league-v4 `queueType` string for a match queue, if it is ranked.
pub fn queue_type(queue: Queue) -> Option<&'static str> {
    match queue.0 {
        420 => Some("RANKED_SOLO_5x5"),
        440 => Some("RANKED_FLEX_SR"),
        // Ranked 5v5 (premade teams, queue 710) — its own league-v4 ladder.
        710 => Some("RANKED_PREMADE_5x5"),
        _ => None,
    }
}
