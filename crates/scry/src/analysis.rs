//! Causal analysis of a single match: turn the raw timeline into classified
//! **moments** — event-joins that explain *what happened and why* — rather than
//! standalone stats. See `docs/game-analysis.md` for the design and the
//! governing rule (a fact earns its place only if it relates two events in time
//! and space).
//!
//! Joins implemented (all Tier-1 Solid, events-only):
//! - **fight → objective conversion** — the spine of "what decided the game".
//! - **death quality** — each of the centered player's deaths: free or traded.
//! - **nemesis** — one enemy accounting for the majority of the player's deaths.
//! - **objective absence** — the player present for none of the major objectives.
//! - **dragon monopoly** — one team taking every dragon.

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
    /// One of the centered player's deaths.
    Death { free: bool },
    /// One enemy killed the player a majority of the time.
    Nemesis,
    /// The player took part in none of the game's major objectives.
    ObjectiveAbsence,
    /// One team took every dragon.
    DragonMonopoly { team: i32 },
}

/// A won fight must have at least this kill lead and the loser at most one kill.
const WON_FIGHT_MIN_KILLS: usize = 2;
const WON_FIGHT_MAX_LOSER_KILLS: usize = 1;
/// A new kill more than this long after the previous one starts a new fight.
const FIGHT_GAP_MS: i64 = 20_000;
/// An objective within this long after a fight's last kill counts as converted.
const CONVERSION_WINDOW_MS: i64 = 90_000;
/// A death is "traded" if an allied kill lands from this long before it to
/// `TRADE_AFTER_MS` after it, within `TRADE_RADIUS` units of the death.
const TRADE_BEFORE_MS: i64 = 3_000;
const TRADE_AFTER_MS: i64 = 10_000;
const TRADE_RADIUS: f64 = 2_000.0;

/// Analyze `events_jsonl` (one timeline event per line) against the typed match,
/// centered on the player with `puuid`, returning moments in chronological order.
pub fn analyze(game: &Match, events_jsonl: &str, puuid: &str) -> Vec<Moment> {
    let team_of = team_by_participant(game);
    let player = game.info.participants.iter().find(|p| p.puuid == puuid);
    let player_id = player.map(|p| p.participant_id);
    let player_team = player.map(|p| team_i32(p.team_id));

    let mut kills: Vec<KillEv> = Vec::new();
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
                kills.push(KillEv {
                    t: ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                    killer: ev.get("killerId").and_then(Value::as_i64).unwrap_or(0) as i32,
                    victim: ev.get("victimId").and_then(Value::as_i64).unwrap_or(0) as i32,
                    killer_team: team_of_participant(
                        &team_of,
                        ev.get("killerId").and_then(Value::as_i64).unwrap_or(0),
                    ),
                    x: ev.pointer("/position/x").and_then(Value::as_f64).unwrap_or(0.0),
                    y: ev.pointer("/position/y").and_then(Value::as_f64).unwrap_or(0.0),
                });
            }
            Some("ELITE_MONSTER_KILL") => {
                let team = ev.get("killerTeamId").and_then(Value::as_i64).unwrap_or(0) as i32;
                // 0/300 are neutral-despawn sentinels — nobody secured it.
                if team == 100 || team == 200 {
                    objectives.push(Objective {
                        t: ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                        team,
                        monster: monster_kind(&ev),
                        label: monster_label(&ev),
                        participants: monster_participants(&ev),
                    });
                }
            }
            Some("BUILDING_KILL") => {
                // `teamId` is the building's OWNER; the destroyer is the enemy.
                let owner = ev.get("teamId").and_then(Value::as_i64).unwrap_or(0);
                let team = other_team(owner);
                if team == 100 || team == 200 {
                    objectives.push(Objective {
                        t: ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                        team,
                        monster: Monster::Building,
                        label: building_label(&ev),
                        participants: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    kills.sort_by_key(|k| k.t);
    objectives.sort_by_key(|o| o.t);

    let mut moments = fight_conversions(&kills, &objectives);
    if let (Some(pid), Some(pteam)) = (player_id, player_team) {
        moments.extend(death_moments(&kills, pid, pteam, game));
        moments.extend(nemesis_moment(&kills, pid, game));
        moments.extend(objective_absence_moment(&objectives, pid));
    }
    moments.extend(dragon_monopoly_moment(&objectives));

    moments.sort_by_key(|m| m.t_ms);
    moments
}

/// Render the moments as an authoritative grounded-facts brief for the OVERVIEW
/// prompt: a chronological list plus the aggregate player signals.
pub fn render_moments_md(moments: &[Moment], player: &str) -> String {
    let mut out = String::new();
    out.push_str("# Grounded analysis (precomputed — authoritative)\n\n");
    out.push_str(&format!(
        "These causal facts were computed directly from the match timeline, centered on \
         **{player}**. Treat them as ground truth: build the overview from them, and do not \
         recompute, contradict, or invent beyond them. Use the raw files only for color \
         (champion matchups, item/level context), never to override a fact below.\n\n",
    ));

    out.push_str("## Decisive moments (chronological)\n");
    for m in moments {
        out.push_str(&format!("- {} — {}\n", mmss(m.t_ms), m.summary));
    }

    let deaths = moments
        .iter()
        .filter(|m| matches!(m.kind, MomentKind::Death { .. }))
        .count();
    let free = moments
        .iter()
        .filter(|m| matches!(m.kind, MomentKind::Death { free: true }))
        .count();
    out.push_str("\n## Player signals\n");
    if deaths > 0 {
        out.push_str(&format!(
            "- Deaths: {deaths}, of which {free} were free (died with no trade)\n"
        ));
    }
    for m in moments {
        match m.kind {
            MomentKind::Nemesis | MomentKind::ObjectiveAbsence => {
                out.push_str(&format!("- {}\n", m.summary));
            }
            _ => {}
        }
    }
    out
}

// --- fight -> objective conversion ------------------------------------------

fn fight_conversions(kills: &[KillEv], objectives: &[Objective]) -> Vec<Moment> {
    let team_kills: Vec<Kill> = kills
        .iter()
        .filter(|k| k.killer_team == 100 || k.killer_team == 200)
        .map(|k| Kill {
            t: k.t,
            team: k.killer_team,
        })
        .collect();

    let mut won: Vec<WonFight> = cluster_fights(&team_kills)
        .into_iter()
        .filter_map(WonFight::from_fight)
        .collect();

    // Attribute each objective to a SINGLE fight — the nearest won fight (by the
    // same team) it could have come from — so a shared tower isn't credited to
    // several fights at once.
    for o in objectives {
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

    won.into_iter().map(WonFight::into_moment).collect()
}

// --- death quality ----------------------------------------------------------

fn death_moments(kills: &[KillEv], pid: i32, pteam: i32, game: &Match) -> Vec<Moment> {
    kills
        .iter()
        .filter(|k| k.victim == pid)
        .map(|death| {
            // Traded if an allied kill lands close in time and space.
            let traded = kills.iter().any(|a| {
                a.killer_team == pteam
                    && a.t >= death.t - TRADE_BEFORE_MS
                    && a.t <= death.t + TRADE_AFTER_MS
                    && dist(a.x, a.y, death.x, death.y) <= TRADE_RADIUS
            });
            let killer = champ_of(game, death.killer);
            let summary = if traded {
                format!("You died to {killer} at {} — a trade", mmss(death.t))
            } else {
                format!("You died to {killer} at {} with no trade (a free death)", mmss(death.t))
            };
            Moment {
                t_ms: death.t,
                kind: MomentKind::Death { free: !traded },
                summary,
                evidence: vec![format!("death at ({:.0},{:.0})", death.x, death.y)],
            }
        })
        .collect()
}

// --- nemesis ----------------------------------------------------------------

fn nemesis_moment(kills: &[KillEv], pid: i32, game: &Match) -> Option<Moment> {
    let deaths: Vec<&KillEv> = kills.iter().filter(|k| k.victim == pid).collect();
    if deaths.len() < 3 {
        return None;
    }
    // Most frequent killer.
    let mut best_killer = 0;
    let mut best_count = 0;
    for &k in &deaths {
        let count = deaths.iter().filter(|d| d.killer == k.killer).count();
        if count > best_count {
            best_count = count;
            best_killer = k.killer;
        }
    }
    if best_count * 2 < deaths.len() {
        return None;
    }
    let champ = champ_of(game, best_killer);
    let last = deaths.iter().map(|d| d.t).max().unwrap_or(0);
    Some(Moment {
        t_ms: last,
        kind: MomentKind::Nemesis,
        summary: format!(
            "{champ} was your nemesis — {best_count} of your {} deaths",
            deaths.len()
        ),
        evidence: vec![format!("{best_count}/{} deaths to one enemy", deaths.len())],
    })
}

// --- objective absence ------------------------------------------------------

fn objective_absence_moment(objectives: &[Objective], pid: i32) -> Option<Moment> {
    let majors: Vec<&Objective> = objectives.iter().filter(|o| o.monster.is_major()).collect();
    if majors.len() < 3 {
        return None;
    }
    let present = majors
        .iter()
        .filter(|o| o.participants.contains(&pid))
        .count();
    if present > 0 {
        return None;
    }
    let last = majors.iter().map(|o| o.t).max().unwrap_or(0);
    Some(Moment {
        t_ms: last,
        kind: MomentKind::ObjectiveAbsence,
        summary: format!(
            "You took part in none of the {} major objectives (dragons/Baron/Herald)",
            majors.len()
        ),
        evidence: vec![format!("0/{} major objectives", majors.len())],
    })
}

// --- dragon monopoly --------------------------------------------------------

fn dragon_monopoly_moment(objectives: &[Objective]) -> Option<Moment> {
    let dragons: Vec<&Objective> = objectives
        .iter()
        .filter(|o| o.monster == Monster::Dragon)
        .collect();
    if dragons.len() < 3 {
        return None;
    }
    let team = dragons[0].team;
    if !dragons.iter().all(|d| d.team == team) {
        return None;
    }
    let last = dragons.iter().map(|d| d.t).max().unwrap_or(0);
    Some(Moment {
        t_ms: last,
        kind: MomentKind::DragonMonopoly { team },
        summary: format!(
            "{} monopolized the dragons — all {} of them",
            side_name(team),
            dragons.len()
        ),
        evidence: vec![format!("{}-0 on dragons", dragons.len())],
    })
}

// --- types ------------------------------------------------------------------

struct KillEv {
    t: i64,
    killer: i32,
    victim: i32,
    killer_team: i32,
    x: f64,
    y: f64,
}

struct Kill {
    t: i64,
    team: i32,
}

#[derive(PartialEq, Eq)]
enum Monster {
    Dragon,
    Baron,
    Herald,
    Grub,
    Building,
}

impl Monster {
    /// Major neutral objectives worth participating in.
    fn is_major(&self) -> bool {
        matches!(self, Monster::Dragon | Monster::Baron | Monster::Herald)
    }
}

struct Objective {
    t: i64,
    team: i32,
    monster: Monster,
    label: String,
    /// killer + assists (participant ids) for the take.
    participants: Vec<i32>,
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

// --- helpers ----------------------------------------------------------------

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
        .find(|(id, _)| i64::from(*id) == participant_id)
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

fn champ_of(game: &Match, participant_id: i32) -> String {
    game.info
        .participants
        .iter()
        .find(|p| p.participant_id == participant_id)
        .map(|p| p.champion_name.clone())
        .unwrap_or_else(|| "an enemy".to_string())
}

fn monster_kind(ev: &Value) -> Monster {
    match ev.get("monsterType").and_then(Value::as_str) {
        Some("BARON_NASHOR") => Monster::Baron,
        Some("RIFTHERALD") => Monster::Herald,
        Some("HORDE") => Monster::Grub,
        _ => Monster::Dragon,
    }
}

fn monster_participants(ev: &Value) -> Vec<i32> {
    let mut parts = Vec::new();
    if let Some(k) = ev.get("killerId").and_then(Value::as_i64) {
        parts.push(k as i32);
    }
    if let Some(a) = ev.get("assistingParticipantIds").and_then(Value::as_array) {
        parts.extend(a.iter().filter_map(Value::as_i64).map(|n| n as i32));
    }
    parts
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

fn dist(x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt()
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

    const MOON_PUUID: &str =
        "Mbv5r1kdyiQ3LgUGDz_gGPw73yE1GUvkPpAnI3-1Yg3wWivrzaI1E-TvYS7bNvSw3RZzg4yRhw35AA";

    fn fixture() -> Option<(Match, String)> {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../archive/NA1/5592214271");
        let match_json = std::fs::read_to_string(dir.join("match.json")).ok()?;
        let events = std::fs::read_to_string(dir.join("timeline-events.jsonl")).ok()?;
        let game: Match = serde_json::from_str(&match_json).ok()?;
        Some((game, events))
    }

    /// The headline: Red side (Moon's team) won multiple early fights but
    /// squandered them, while Blue side converted its fights.
    #[test]
    fn conversion_gap_is_the_story() {
        let Some((game, events)) = fixture() else {
            eprintln!("fixture archive absent; skipping");
            return;
        };
        let moments = analyze(&game, &events, MOON_PUUID);

        let (mut red_won, mut red_conv, mut blue_won, mut blue_conv) = (0, 0, 0, 0);
        for m in &moments {
            if let MomentKind::FightConversion { team, converted } = m.kind {
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
        }
        assert!(red_won >= 3, "expected Red to win >=3 fights, got {red_won}");
        assert!(
            red_conv * 2 <= red_won,
            "expected Red to squander most fights ({red_conv}/{red_won} converted)"
        );
        assert!(
            blue_conv * 2 > blue_won,
            "expected Blue to convert most fights ({blue_conv}/{blue_won} converted)"
        );
    }

    /// Moon died mostly for nothing, to one repeat killer, and touched no epics.
    #[test]
    fn moon_death_and_objective_signals() {
        let Some((game, events)) = fixture() else {
            eprintln!("fixture archive absent; skipping");
            return;
        };
        let moments = analyze(&game, &events, MOON_PUUID);

        let free = moments
            .iter()
            .filter(|m| matches!(m.kind, MomentKind::Death { free: true }))
            .count();
        assert!(free >= 5, "expected many free deaths, got {free}");
        assert!(
            moments.iter().any(|m| m.kind == MomentKind::Nemesis),
            "expected a nemesis moment"
        );
        assert!(
            moments.iter().any(|m| m.kind == MomentKind::ObjectiveAbsence),
            "expected Moon absent from all majors"
        );
        assert!(
            moments
                .iter()
                .any(|m| matches!(m.kind, MomentKind::DragonMonopoly { team: 100 })),
            "expected Blue dragon monopoly"
        );
    }
}
