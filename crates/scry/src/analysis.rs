//! Causal analysis of a single match: turn the raw timeline into classified
//! **moments** — event-joins that explain *what happened and why* — rather than
//! standalone stats. See `docs/game-analysis.md` for the design and the
//! governing rule (a fact earns its place only if it relates two events in time
//! and space).
//!
//! First join implemented: **fight → objective conversion**, the spine of "what
//! decided the game". A team that wins a skirmish but takes no objective from it
//! *squandered* it; the team that converts its fights into dragons/towers wins.

use riven::consts::Team;
use riven::models::match_v5::Match;
use serde_json::Value;

/// A classified, already-grounded moment ready to hand to the OVERVIEW prompt.
#[derive(Debug, Clone)]
pub struct Moment {
    /// Game time of the moment, ms from start.
    pub t_ms: i64,
    pub kind: MomentKind,
    /// One-line, League-literate description.
    pub summary: String,
    /// Supporting event citations (timestamps + what happened).
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MomentKind {
    /// A won skirmish and whether it was cashed into an objective.
    FightConversion { team: i32, converted: bool },
}

/// A won fight must have at least this kill lead and the loser at most one kill.
const WON_FIGHT_MIN_KILLS: usize = 2;
const WON_FIGHT_MAX_LOSER_KILLS: usize = 1;
/// A new kill more than this long after the previous one starts a new fight.
const FIGHT_GAP_MS: i64 = 20_000;
/// An objective within this long after a fight's last kill counts as converted.
const CONVERSION_WINDOW_MS: i64 = 90_000;

/// Analyze `events_jsonl` (one timeline event per line) against the typed match,
/// returning the moments in chronological order.
pub fn analyze(game: &Match, events_jsonl: &str) -> Vec<Moment> {
    let team_of = team_by_participant(game);

    let mut kills: Vec<Kill> = Vec::new();
    let mut objectives: Vec<Objective> = Vec::new();
    for line in events_jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("CHAMPION_KILL") => {
                let t = ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                let killer = ev.get("killerId").and_then(Value::as_i64).unwrap_or(0);
                let team = team_of_participant(&team_of, killer);
                if team == 100 || team == 200 {
                    kills.push(Kill { t, team });
                }
            }
            Some("ELITE_MONSTER_KILL") => {
                let t = ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                let team = ev.get("killerTeamId").and_then(Value::as_i64).unwrap_or(0) as i32;
                // 0/300 are neutral-despawn sentinels — nobody secured it.
                if team == 100 || team == 200 {
                    objectives.push(Objective {
                        t,
                        team,
                        label: monster_label(&ev),
                    });
                }
            }
            Some("BUILDING_KILL") => {
                let t = ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                // `teamId` is the building's OWNER; the destroyer is the enemy.
                let owner = ev.get("teamId").and_then(Value::as_i64).unwrap_or(0);
                let team = other_team(owner);
                if team == 100 || team == 200 {
                    objectives.push(Objective {
                        t,
                        team,
                        label: building_label(&ev),
                    });
                }
            }
            _ => {}
        }
    }

    kills.sort_by_key(|k| k.t);
    objectives.sort_by_key(|o| o.t);

    let mut won: Vec<WonFight> = cluster_fights(&kills)
        .into_iter()
        .filter_map(WonFight::from_fight)
        .collect();

    // Attribute each objective to a SINGLE fight — the nearest won fight (by the
    // same team) it could have come from — so a shared tower isn't credited to
    // several fights at once.
    for o in &objectives {
        let claimant = won
            .iter_mut()
            .filter(|f| {
                f.team == o.team
                    && f.converting.is_none()
                    && o.t >= f.start
                    && o.t <= f.end + CONVERSION_WINDOW_MS
            })
            .max_by_key(|f| f.end);
        if let Some(f) = claimant {
            f.converting = Some((o.label.clone(), o.t));
        }
    }

    let mut moments: Vec<Moment> = won.into_iter().map(WonFight::into_moment).collect();
    moments.sort_by_key(|m| m.t_ms);
    moments
}

struct Kill {
    t: i64,
    team: i32,
}

struct Objective {
    t: i64,
    team: i32,
    label: String,
}

/// A time-clustered skirmish.
struct Fight {
    start: i64,
    end: i64,
    kills: Vec<i32>,
}

impl Fight {
    fn kills_for(&self, team: i32) -> usize {
        self.kills.iter().filter(|&&t| t == team).count()
    }

    /// The team that won the fight, if it meets the lead threshold.
    fn winner(&self) -> Option<i32> {
        let (blue, red) = (self.kills_for(100), self.kills_for(200));
        if blue >= WON_FIGHT_MIN_KILLS && red <= WON_FIGHT_MAX_LOSER_KILLS {
            Some(100)
        } else if red >= WON_FIGHT_MIN_KILLS && blue <= WON_FIGHT_MAX_LOSER_KILLS {
            Some(200)
        } else {
            None
        }
    }
}

/// A won fight, with the objective (if any) attributed to it.
struct WonFight {
    team: i32,
    start: i64,
    end: i64,
    won: usize,
    lost: usize,
    converting: Option<(String, i64)>,
}

impl WonFight {
    fn from_fight(f: Fight) -> Option<WonFight> {
        let team = f.winner()?;
        let (blue, red) = (f.kills_for(100), f.kills_for(200));
        let (won, lost) = if team == 100 { (blue, red) } else { (red, blue) };
        Some(WonFight {
            team,
            start: f.start,
            end: f.end,
            won,
            lost,
            converting: None,
        })
    }

    fn into_moment(self) -> Moment {
        let side = side_name(self.team);
        let (summary, evidence) = match &self.converting {
            Some((label, t)) => (
                format!(
                    "{side} won a {}-{} fight at {} and converted it into {label} at {}",
                    self.won,
                    self.lost,
                    mmss(self.end),
                    mmss(*t),
                ),
                vec![
                    format!("fight {}-{} ({}–{})", self.won, self.lost, mmss(self.start), mmss(self.end)),
                    format!("{label} at {}", mmss(*t)),
                ],
            ),
            None => (
                format!(
                    "{side} won a {}-{} fight at {} but took no objective from it",
                    self.won,
                    self.lost,
                    mmss(self.end),
                ),
                vec![format!(
                    "fight {}-{} ({}–{})",
                    self.won,
                    self.lost,
                    mmss(self.start),
                    mmss(self.end)
                )],
            ),
        };
        Moment {
            t_ms: self.end,
            kind: MomentKind::FightConversion {
                team: self.team,
                converted: self.converting.is_some(),
            },
            summary,
            evidence,
        }
    }
}

/// Group kills into fights, splitting whenever the gap exceeds `FIGHT_GAP_MS`.
fn cluster_fights(kills: &[Kill]) -> Vec<Fight> {
    let mut fights: Vec<Fight> = Vec::new();
    for k in kills {
        match fights.last_mut() {
            Some(f) if k.t - f.end <= FIGHT_GAP_MS => {
                f.end = k.t;
                f.kills.push(k.team);
            }
            _ => fights.push(Fight {
                start: k.t,
                end: k.t,
                kills: vec![k.team],
            }),
        }
    }
    fights
}

/// participantId (1..=10) -> team id (100/200), indexed by participant order.
fn team_by_participant(game: &Match) -> Vec<(i32, i32)> {
    game.info
        .participants
        .iter()
        .map(|p| (p.participant_id, team_i32(p.team_id)))
        .collect()
}

fn team_of_participant(map: &[(i32, i32)], participant_id: i64) -> i32 {
    map.iter()
        .find(|(id, _)| *id as i64 == participant_id)
        .map(|(_, team)| *team)
        .unwrap_or(0)
}

fn team_i32(team: Team) -> i32 {
    match team {
        Team::BLUE => 100,
        Team::RED => 200,
        _ => 0,
    }
}

fn other_team(team: i64) -> i32 {
    match team {
        100 => 200,
        200 => 100,
        _ => 0,
    }
}

fn side_name(team: i32) -> &'static str {
    match team {
        100 => "Blue side",
        200 => "Red side",
        _ => "A team",
    }
}

fn monster_label(ev: &Value) -> String {
    match ev.get("monsterType").and_then(Value::as_str) {
        Some("BARON_NASHOR") => "Baron".to_string(),
        Some("RIFTHERALD") => "Rift Herald".to_string(),
        Some("HORDE") => "a Void Grub".to_string(),
        Some("DRAGON") => match ev.get("monsterSubType").and_then(Value::as_str) {
            Some(sub) => format!("the {} Dragon", drake_element(sub)),
            None => "a Dragon".to_string(),
        },
        _ => "an objective".to_string(),
    }
}

fn drake_element(sub: &str) -> &str {
    match sub {
        "FIRE_DRAGON" => "Infernal",
        "WATER_DRAGON" => "Ocean",
        "EARTH_DRAGON" => "Mountain",
        "AIR_DRAGON" => "Cloud",
        "HEXTECH_DRAGON" => "Hextech",
        "CHEMTECH_DRAGON" => "Chemtech",
        "ELDER_DRAGON" => "Elder",
        _ => "elemental",
    }
}

fn building_label(ev: &Value) -> String {
    match ev.get("buildingType").and_then(Value::as_str) {
        Some("INHIBITOR_BUILDING") => "an inhibitor".to_string(),
        _ => "a tower".to_string(),
    }
}

/// Format ms-from-start as `m:ss`.
fn mmss(ms: i64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> Option<(Match, String)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../archive/NA1/5592214271");
        let match_json = std::fs::read_to_string(dir.join("match.json")).ok()?;
        let events = std::fs::read_to_string(dir.join("timeline-events.jsonl")).ok()?;
        let game: Match = serde_json::from_str(&match_json).ok()?;
        Some((game, events))
    }

    /// The headline finding: Red side (Moon's team) won multiple early fights
    /// but squandered them, while Blue side converted its fights. That
    /// conversion gap — not laning — is why Red lost.
    #[test]
    fn conversion_gap_is_the_story() {
        let Some((game, events)) = fixture() else {
            eprintln!("fixture archive absent; skipping");
            return;
        };
        let moments = analyze(&game, &events);

        let (mut red_won, mut red_conv, mut blue_won, mut blue_conv) = (0, 0, 0, 0);
        for m in &moments {
            let MomentKind::FightConversion { team, converted } = m.kind;
            match team {
                200 => {
                    red_won += 1;
                    red_conv += i32::from(converted);
                }
                100 => {
                    blue_won += 1;
                    blue_conv += i32::from(converted);
                }
                _ => {}
            }
        }

        // Red won several fights but converted the minority of them.
        assert!(red_won >= 3, "expected Red to win >=3 fights, got {red_won}");
        assert!(
            red_conv * 2 <= red_won,
            "expected Red to squander most fights ({red_conv}/{red_won} converted)"
        );
        // Blue converted the majority of its fights.
        assert!(
            blue_conv * 2 > blue_won,
            "expected Blue to convert most fights ({blue_conv}/{blue_won} converted)"
        );
    }
}
