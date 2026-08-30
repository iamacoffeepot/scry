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
use serde_json::{Value, json};

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

    let tl = parse_timeline(events_jsonl, &team_of);

    let mut moments = fight_conversions(&tl.kills, &tl.objectives);
    if let (Some(pid), Some(pteam)) = (player_id, player_team) {
        moments.extend(death_moments(&tl.kills, pid, pteam, game));
        moments.extend(nemesis_moment(&tl.kills, pid, game));
        moments.extend(objective_absence_moment(&tl.objectives, pid));
    }
    moments.extend(dragon_monopoly_moment(&tl.objectives));

    moments.sort_by_key(|m| m.t_ms);
    moments
}

/// The timeline reduced to the event kinds the analysis joins over, sorted by time.
struct Timeline {
    kills: Vec<KillEv>,
    specials: Vec<Special>,
    objectives: Vec<Objective>,
    /// The game's first blood `(timestamp, killer)`, when the event appeared.
    first_blood: Option<(i64, i32)>,
}

/// Parse the raw event stream into typed kills, special-kills (multikills), and
/// objectives — the shared front-end for both moment analysis and clip picking.
fn parse_timeline(events_jsonl: &str, team_of: &[(i32, i32)]) -> Timeline {
    let mut kills: Vec<KillEv> = Vec::new();
    let mut specials: Vec<Special> = Vec::new();
    let mut objectives: Vec<Objective> = Vec::new();
    let mut first_blood: Option<(i64, i32)> = None;
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
                let assists = ev
                    .get("assistingParticipantIds")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_i64).map(|n| n as i32).collect())
                    .unwrap_or_default();
                kills.push(KillEv {
                    t: ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                    killer: ev.get("killerId").and_then(Value::as_i64).unwrap_or(0) as i32,
                    victim: ev.get("victimId").and_then(Value::as_i64).unwrap_or(0) as i32,
                    killer_team: team_of_participant(team_of, ev.get("killerId").and_then(Value::as_i64).unwrap_or(0)),
                    assists,
                    bounty: ev.get("bounty").and_then(Value::as_i64).unwrap_or(0),
                    shutdown: ev.get("shutdownBounty").and_then(Value::as_i64).unwrap_or(0),
                    x: ev.pointer("/position/x").and_then(Value::as_f64).unwrap_or(0.0),
                    y: ev.pointer("/position/y").and_then(Value::as_f64).unwrap_or(0.0),
                });
            }
            // KILL_MULTI carries a multikill length; KILL_FIRST_BLOOD marks
            // the game's first kill (a small scoring bonus).
            Some("CHAMPION_SPECIAL_KILL") => match ev.get("killType").and_then(Value::as_str) {
                Some("KILL_MULTI") => specials.push(Special {
                    t: ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                    killer: ev.get("killerId").and_then(Value::as_i64).unwrap_or(0) as i32,
                    len: ev.get("multiKillLength").and_then(Value::as_i64).unwrap_or(0),
                }),
                Some("KILL_FIRST_BLOOD") => {
                    first_blood = Some((
                        ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                        ev.get("killerId").and_then(Value::as_i64).unwrap_or(0) as i32,
                    ));
                }
                _ => {}
            },
            Some("ELITE_MONSTER_KILL") => {
                let team = ev.get("killerTeamId").and_then(Value::as_i64).unwrap_or(0) as i32;
                // 0/300 are neutral-despawn sentinels — nobody secured it.
                if team == 100 || team == 200 {
                    objectives.push(Objective {
                        t: ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                        team,
                        monster: monster_kind(&ev),
                        killer: ev.get("killerId").and_then(Value::as_i64).unwrap_or(0) as i32,
                        elder: ev.pointer("/monsterSubType").and_then(Value::as_str) == Some("ELDER_DRAGON"),
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
                        killer: 0,
                        elder: false,
                        label: building_label(&ev),
                        participants: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    kills.sort_by_key(|k| k.t);
    specials.sort_by_key(|s| s.t);
    objectives.sort_by_key(|o| o.t);
    Timeline { kills, specials, objectives, first_blood }
}

/// Render the moments as an authoritative grounded-facts brief for the OVERVIEW
/// prompt: a chronological list plus the aggregate player signals.
pub fn render_moments_md(
    moments: &[Moment],
    highlights: &[Candidate],
    lowlights: &[Candidate],
    player: &str,
) -> String {
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

    let deaths = moments.iter().filter(|m| matches!(m.kind, MomentKind::Death { .. })).count();
    let free = moments.iter().filter(|m| matches!(m.kind, MomentKind::Death { free: true })).count();
    out.push_str("\n## Player signals\n");
    if deaths > 0 {
        out.push_str(&format!("- Deaths: {deaths}, of which {free} were free (died with no trade)\n"));
    }
    for m in moments {
        match m.kind {
            MomentKind::Nemesis | MomentKind::ObjectiveAbsence => {
                out.push_str(&format!("- {}\n", m.summary));
            }
            _ => {}
        }
    }

    // Clip anchors: the OVERVIEW prompt picks one of each and echoes its m:ss.
    out.push_str("\n## Highlight candidates (ranked; the pick takes the best-scored)\n");
    render_candidates(&mut out, highlights);
    out.push_str("\n## Lowlight candidates (ranked; the pick takes the best-scored)\n");
    render_candidates(&mut out, lowlights);

    out
}

/// Render a candidate list as `- m:ss — summary`, or a `(none)` marker so the
/// prompt knows to write `none` for that clip.
fn render_candidates(out: &mut String, candidates: &[Candidate]) {
    if candidates.is_empty() {
        out.push_str("- (none — no clip-worthy moment)\n");
        return;
    }
    for c in candidates {
        out.push_str(&format!("- {} — {}\n", mmss(c.t_ms), c.summary));
    }
}

// --- fight -> objective conversion ------------------------------------------

fn fight_conversions(kills: &[KillEv], objectives: &[Objective]) -> Vec<Moment> {
    let team_kills: Vec<Kill> = kills
        .iter()
        .filter(|k| k.killer_team == 100 || k.killer_team == 200)
        .map(|k| Kill { t: k.t, team: k.killer_team })
        .collect();

    let mut won: Vec<WonFight> = cluster_fights(&team_kills).into_iter().filter_map(WonFight::from_fight).collect();

    // Attribute each objective to a SINGLE fight — the nearest won fight (by the
    // same team) it could have come from — so a shared tower isn't credited to
    // several fights at once.
    for o in objectives {
        let claimant = won
            .iter_mut()
            .filter(|f| {
                f.team == o.team && f.converting.is_none() && o.t >= f.start && o.t <= f.end + CONVERSION_WINDOW_MS
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
        summary: format!("{champ} was your nemesis — {best_count} of your {} deaths", deaths.len()),
        evidence: vec![format!("{best_count}/{} deaths to one enemy", deaths.len())],
    })
}

// --- objective absence ------------------------------------------------------

fn objective_absence_moment(objectives: &[Objective], pid: i32) -> Option<Moment> {
    let majors: Vec<&Objective> = objectives.iter().filter(|o| o.monster.is_major()).collect();
    if majors.len() < 3 {
        return None;
    }
    let present = majors.iter().filter(|o| o.participants.contains(&pid)).count();
    if present > 0 {
        return None;
    }
    let last = majors.iter().map(|o| o.t).max().unwrap_or(0);
    Some(Moment {
        t_ms: last,
        kind: MomentKind::ObjectiveAbsence,
        summary: format!("You took part in none of the {} major objectives (dragons/Baron/Herald)", majors.len()),
        evidence: vec![format!("0/{} major objectives", majors.len())],
    })
}

// --- dragon monopoly --------------------------------------------------------

fn dragon_monopoly_moment(objectives: &[Objective]) -> Option<Moment> {
    let dragons: Vec<&Objective> = objectives.iter().filter(|o| o.monster == Monster::Dragon).collect();
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
        summary: format!("{} monopolized the dragons — all {} of them", side_name(team), dragons.len()),
        evidence: vec![format!("{}-0 on dragons", dragons.len())],
    })
}

// --- clip candidates --------------------------------------------------------

/// How many candidates of each kind to surface to the OVERVIEW prompt.
const MAX_CANDIDATES: usize = 3;
/// A death still counts as "caught before an objective" if the enemy takes one
/// within this long after it.
const PUNISH_WINDOW_MS: i64 = 30_000;
/// After a fight ends, the player must survive this long for it to read as a
/// clean "walked away" highlight.
const SURVIVE_AFTER_MS: i64 = 8_000;
/// A fight or death this close to the game's end decided it.
const CLOSING_WINDOW_MS: i64 = 90_000;
/// A death this soon after the previous one reads as compounding the throw.
const REPEAT_DEATH_MS: i64 = 60_000;

/// Compute the ranked Highlight and Lowlight clip candidates for the centered
/// player, each an exact-timestamp anchor the OVERVIEW prompt picks from.
pub fn clip_candidates(game: &Match, events_jsonl: &str, puuid: &str) -> (Vec<Candidate>, Vec<Candidate>) {
    let team_of = team_by_participant(game);
    let tl = parse_timeline(events_jsonl, &team_of);
    let Some(player) = game.info.participants.iter().find(|p| p.puuid == puuid) else {
        return (Vec::new(), Vec::new());
    };
    let pid = player.participant_id;
    let pteam = team_i32(player.team_id);
    let ctx = ScoreContext {
        // CC that actually held enemies (seconds) — the timeline has no CC
        // events, so this match-detail aggregate is the playmaker signal.
        cc_seconds: i64::from(player.time_ccing_others),
        end_ms: game.info.game_duration * 1000,
    };
    let mut highlights = highlight_candidates(&tl, pid, pteam, &ctx);
    highlights.extend(objective_candidates(&tl, pid, pteam, &ctx));
    (
        top_candidates(highlights.into_iter().map(|c| (c.score, c)).collect()),
        lowlight_candidates(&tl, pid, pteam, game, &ctx),
    )
}

/// Per-player scoring context beyond the timeline itself.
struct ScoreContext {
    /// Match-detail `timeCCingOthers`: seconds the player's CC held enemies.
    cc_seconds: i64,
    /// Game end in timeline milliseconds (the closing-fight bonus window).
    end_ms: i64,
}

/// The player's best plays: fights they were central to, scored on kills/assists,
/// kill gold, multikills, survival, and whether an objective followed.
fn highlight_candidates(tl: &Timeline, pid: i32, pteam: i32, ctx: &ScoreContext) -> Vec<Candidate> {
    let mut scored: Vec<(i64, Candidate)> = Vec::new();
    for range in cluster_kill_indices(&tl.kills) {
        let fight = &tl.kills[range];
        let (start, end) = (fight[0].t, fight[fight.len() - 1].t);

        let pkills: Vec<&KillEv> = fight.iter().filter(|k| k.killer == pid).collect();
        let passists = fight.iter().filter(|k| k.assists.contains(&pid)).count();
        let pdeaths = fight.iter().filter(|k| k.victim == pid).count();
        if pkills.is_empty() && passists == 0 {
            continue; // nothing of the player's to show
        }

        let gold: i64 = pkills.iter().map(|k| k.bounty + k.shutdown).sum();
        let multi = tl
            .specials
            .iter()
            .filter(|s| s.killer == pid && s.t >= start - 1_000 && s.t <= end + 1_000)
            .map(|s| s.len)
            .max()
            .unwrap_or(0);
        // Survived if not a victim during the fight, nor shortly after it.
        let survived = !fight.iter().any(|k| k.victim == pid)
            && !tl.kills.iter().any(|k| k.victim == pid && k.t > end && k.t <= end + SURVIVE_AFTER_MS);
        let objective = tl.objectives.iter().find(|o| {
            o.team == pteam && o.participants.contains(&pid) && o.t >= start && o.t <= end + CONVERSION_WINDOW_MS
        });

        let mut score = pkills.len() as i64 * 3 + passists as i64 + gold / 100;
        if multi >= 2 {
            score += multi * 4;
        }
        if survived {
            score += 2;
        }
        score -= pdeaths as i64 * 2;
        if objective.is_some() {
            score += 3;
        }
        // Solo kills read better on video than shared cleanup.
        let solo_kills = pkills.iter().filter(|k| k.assists.is_empty()).count() as i64;
        score += solo_kills * 3;
        // Outnumbered fights are the outplays worth watching; headcount from
        // the fight's own kill events is the grounded proxy for the odds.
        let (allies, enemies) = fight_headcount(fight, pteam);
        if enemies > allies {
            score += ((enemies - allies) * 4).min(8);
        }
        // A fight at 30 minutes decides more than the same fight at 8.
        score += start / 600_000;
        // Playmaker weighting: assists count double when the player ran the
        // fight — most of the team's kills bear their mark — or when their
        // CC held enemies long enough to read as the engage/peel archetype
        // (the timeline has no CC events; the aggregate is the signal).
        let team_kills = fight.iter().filter(|k| k.killer_team == pteam).count();
        let ran_the_fight = passists >= 2 && passists * 2 >= team_kills;
        if ran_the_fight || ctx.cc_seconds >= 30 {
            score += passists as i64;
        }
        // First blood in this fight, by the player.
        let first_blood =
            tl.first_blood.is_some_and(|(t, killer)| killer == pid && t >= start - 1_000 && t <= end + 1_000);
        if first_blood {
            score += 3;
        }
        // The closing fight decided the game; the clip should tell that story.
        let closing = end >= ctx.end_ms - CLOSING_WINDOW_MS;
        if closing {
            score += 6;
        }
        if score <= 0 {
            continue;
        }

        // The clip spans the player's own action: from their first involved kill
        // to their last, plus lead-in/tail — so a whole multikill sequence fits.
        // When the span outgrows the cap, anchor the window to the END: the
        // payoff (the multikill's last kill) must survive the cut, not the
        // opening trade.
        let involved: Vec<i64> =
            fight.iter().filter(|k| k.killer == pid || k.assists.contains(&pid)).map(|k| k.t).collect();
        let anchor = *involved.first().unwrap_or(&start);
        let last_involved = *involved.last().unwrap_or(&anchor);
        let span_s = (last_involved - anchor) / 1000;
        let dur_s = (span_s + CLIP_LEAD_S + CLIP_TAIL_S).min(CLIP_MAX_S);
        let seek_s = if span_s + CLIP_LEAD_S + CLIP_TAIL_S > CLIP_MAX_S {
            (last_involved / 1000 + CLIP_TAIL_S - CLIP_MAX_S).max(0)
        } else {
            (anchor / 1000 - CLIP_LEAD_S).max(0)
        };

        let multi_s = if multi >= 2 {
            format!(", a {}", multi_name(multi))
        } else {
            String::new()
        };
        let surv_s = if survived {
            ", walked away"
        } else if pdeaths > 0 {
            ", but died in it"
        } else {
            ""
        };
        let fb_s = if first_blood {
            ", first blood"
        } else {
            ""
        };
        let closing_s = if closing {
            "; the closing fight"
        } else {
            ""
        };
        let obj_s = objective.map(|o| format!("; {} followed", o.label)).unwrap_or_default();
        let solo_s = match solo_kills {
            0 => String::new(),
            1 => ", incl. a solo kill".to_string(),
            n => format!(", incl. {n} solo kills"),
        };
        let odds_s = if enemies > allies {
            format!(", outnumbered {allies}v{enemies}")
        } else {
            String::new()
        };
        let summary = format!(
            "you went {}/{}/{} in a fight{multi_s}{solo_s}{odds_s}{fb_s}{surv_s}{obj_s}{closing_s} (+{gold}g)",
            pkills.len(),
            pdeaths,
            passists,
        );
        scored.push((score, Candidate { t_ms: anchor, seek_s, dur_s, summary, score }));
    }
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Objective secures by the player's own killing blow (the smite): Baron,
/// Elder, drakes, Herald — the plays a kill-cluster scorer can never see.
/// Few allied assists on a major monster reads as a steal-or-solo-secure.
fn objective_candidates(tl: &Timeline, pid: i32, pteam: i32, ctx: &ScoreContext) -> Vec<Candidate> {
    let mut out = Vec::new();
    for objective in &tl.objectives {
        if objective.killer != pid || !objective.monster.is_major() {
            continue;
        }
        let mut score = match objective.monster {
            Monster::Baron => 8,
            Monster::Dragon if objective.elder => 9,
            Monster::Dragon => 4,
            Monster::Herald => 4,
            _ => continue,
        };
        let allied_assists = objective.participants.iter().filter(|p| **p != pid && objective.team == pteam).count();
        let contested = allied_assists <= 1;
        if contested {
            score += 6;
        }
        score += objective.t / 600_000;
        let closing = objective.t >= ctx.end_ms - CLOSING_WINDOW_MS;
        if closing {
            score += 6;
        }

        let how = if contested {
            "your smite, barely contested — a steal or solo secure"
        } else {
            "your killing blow"
        };
        let closing_s = if closing {
            "; the closing take"
        } else {
            ""
        };
        let summary = format!("you secured {} ({how}){closing_s}", objective.label);
        let seek_s = (objective.t / 1000 - CLIP_LEAD_S).max(0);
        out.push(Candidate { t_ms: objective.t, seek_s, dur_s: CLIP_LEAD_S + 13, summary, score });
    }
    out
}

/// The player's worst moments: their deaths, scored on being a free death, the
/// shutdown gold surrendered, and whether the enemy took an objective off it.
fn lowlight_candidates(tl: &Timeline, pid: i32, pteam: i32, game: &Match, ctx: &ScoreContext) -> Vec<Candidate> {
    let enemy = other_team(pteam as i64);
    let mut scored: Vec<(i64, Candidate)> = Vec::new();
    for death in tl.kills.iter().filter(|k| k.victim == pid) {
        let traded = tl.kills.iter().any(|a| {
            a.killer_team == pteam
                && a.t >= death.t - TRADE_BEFORE_MS
                && a.t <= death.t + TRADE_AFTER_MS
                && dist(a.x, a.y, death.x, death.y) <= TRADE_RADIUS
        });
        let punished =
            tl.objectives.iter().find(|o| o.team == enemy && o.t >= death.t && o.t <= death.t + PUNISH_WINDOW_MS);

        let mut score = 2 + death.shutdown / 100 + death.bounty / 200;
        if !traded {
            score += 2;
        }
        if punished.is_some() {
            score += 4;
        }
        // An execute (no killer credit) is the free-est death there is.
        let executed = death.killer <= 0;
        if executed {
            score += 3;
        }
        // Dying again within a minute of the last death compounds the throw.
        let repeated = tl.kills.iter().any(|k| k.victim == pid && k.t < death.t && death.t - k.t <= REPEAT_DEATH_MS);
        if repeated {
            score += 2;
        }
        // A late throw costs more than an early one; a death in the closing
        // window may have been the game.
        score += death.t / 600_000;
        if death.t >= ctx.end_ms - CLOSING_WINDOW_MS {
            score += 4;
        }

        let opener = if executed {
            "executed".to_string()
        } else {
            format!("caught by {}", champ_of(game, death.killer))
        };
        let free_s = if traded {
            ""
        } else {
            " (free death)"
        };
        let rep_s = if repeated {
            ", back-to-back"
        } else {
            ""
        };
        let gold_s = if death.shutdown > 0 {
            format!(", gave up a {}g shutdown", death.shutdown)
        } else {
            String::new()
        };
        let obj_s = punished.map(|o| format!("; {} followed", o.label)).unwrap_or_default();
        let summary = format!("{opener}{free_s}{rep_s}{gold_s}{obj_s}");
        // A death is a single instant: lead-in, the death, a short tail.
        let seek_s = (death.t / 1000 - CLIP_LEAD_S).max(0);
        let dur_s = CLIP_LEAD_S + 9;
        scored.push((score, Candidate { t_ms: death.t, seek_s, dur_s, summary, score }));
    }
    top_candidates(scored)
}

/// Render one player's assigned picks: each as a `## <section>` holding
/// `**m:ss** — summary` — the timestamp `highlight.sh` reads and the caption
/// the post renders. A side with no pick gets no section (skipped clip).
pub fn render_picks_md(highlight: Option<&Candidate>, lowlight: Option<&Candidate>) -> String {
    let mut out = String::new();
    for (heading, pick) in [("Highlight", highlight), ("Lowlight", lowlight)] {
        if let Some(best) = pick {
            out.push_str(&format!("## {heading}\n**{}** — {}\n\n", mmss(best.t_ms), best.summary));
        }
    }
    out
}

/// Jointly assign every tracked player in the game their Highlight and
/// Lowlight picks, spreading picks across distinct moments: a shared
/// teamfight is often several players' best-scored candidate, and N
/// near-identical clips of it teach the channel nothing. Pure in its inputs
/// (players keyed and ordered by PUUID), so whichever player's analyze runs
/// first computes the same assignment.
pub fn assign_picks(
    game: &Match,
    events_jsonl: &str,
    tracked_puuids: &[String],
) -> Vec<(String, Option<Candidate>, Option<Candidate>)> {
    let mut puuids: Vec<&String> = tracked_puuids.iter().collect();
    puuids.sort();
    puuids.dedup();
    let per_player: Vec<(String, Vec<Candidate>, Vec<Candidate>)> = puuids
        .into_iter()
        .map(|puuid| {
            let (highlights, lowlights) = clip_candidates(game, events_jsonl, puuid);
            (puuid.clone(), highlights, lowlights)
        })
        .collect();

    let mut highlight_picks =
        assign_side(&per_player.iter().map(|(p, h, _)| (p.as_str(), h.as_slice())).collect::<Vec<_>>());
    let mut lowlight_picks =
        assign_side(&per_player.iter().map(|(p, _, l)| (p.as_str(), l.as_slice())).collect::<Vec<_>>());
    per_player
        .iter()
        .map(|(puuid, _, _)| {
            (puuid.clone(), highlight_picks.remove(puuid.as_str()), lowlight_picks.remove(puuid.as_str()))
        })
        .collect()
}

/// Greedy joint assignment for one side: the strongest unassigned claim wins
/// each round, and a player whose best candidate overlaps a claimed window
/// diverts to their best free alternative when it's worth at least half their
/// best — otherwise the shared moment really is their story too.
fn assign_side(players: &[(&str, &[Candidate])]) -> std::collections::HashMap<String, Candidate> {
    let mut claimed: Vec<(i64, i64)> = Vec::new();
    let mut assigned = std::collections::HashMap::new();
    let mut remaining: Vec<&(&str, &[Candidate])> = players.iter().collect();

    while !remaining.is_empty() {
        let mut round_best: Option<(usize, Candidate)> = None;
        for (index, (_, candidates)) in remaining.iter().enumerate() {
            let best_any = candidates.iter().max_by_key(|c| (c.score, std::cmp::Reverse(c.t_ms)));
            let Some(best_any) = best_any else {
                continue;
            };
            let best_free = candidates
                .iter()
                .filter(|c| !claimed.iter().any(|w| overlaps(c, *w)))
                .max_by_key(|c| (c.score, std::cmp::Reverse(c.t_ms)));
            let choice = match best_free {
                Some(free) if free.score * 2 >= best_any.score => free,
                _ => best_any,
            };
            if round_best.as_ref().is_none_or(|(_, current)| choice.score > current.score) {
                round_best = Some((index, choice.clone()));
            }
        }
        let Some((index, choice)) = round_best else {
            break;
        };
        let (puuid, _) = remaining.remove(index);
        claimed.push((choice.seek_s, choice.seek_s + choice.dur_s));
        assigned.insert((*puuid).to_string(), choice);
    }
    assigned
}

/// Two clip windows showing substantially the same footage: the overlap
/// covers more than half of the shorter window.
fn overlaps(candidate: &Candidate, (start, end): (i64, i64)) -> bool {
    let (c_start, c_end) = (candidate.seek_s, candidate.seek_s + candidate.dur_s);
    let overlap = c_end.min(end) - c_start.max(start);
    overlap * 2 > candidate.dur_s.min(end - start)
}

/// Render the clip windows as JSON keyed by `m:ss`, so `highlight.sh` can look
/// up the exact seek + duration for the timestamp it was handed:
/// `{"22:06": {"seek": 1319, "dur": 32}, ...}`.
pub fn render_clips_json(highlights: &[Candidate], lowlights: &[Candidate]) -> String {
    let mut map = serde_json::Map::new();
    for c in highlights.iter().chain(lowlights) {
        map.insert(mmss(c.t_ms), json!({ "seek": c.seek_s, "dur": c.dur_s }));
    }
    serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
}

/// Sort `(score, candidate)` pairs descending and keep the top few, in
/// chronological order so the list reads naturally.
fn top_candidates(mut scored: Vec<(i64, Candidate)>) -> Vec<Candidate> {
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.truncate(MAX_CANDIDATES);
    scored.sort_by_key(|(_, c)| c.t_ms);
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Distinct participants of each side appearing in a fight's kill events
/// (killer, victim, or assist) — the grounded proxy for the fight's odds; the
/// timeline carries no positions for champions who dealt no kill-relevant
/// action, so uninvolved bystanders don't count.
fn fight_headcount(fight: &[KillEv], pteam: i32) -> (i64, i64) {
    let mut allies = std::collections::HashSet::new();
    let mut enemies = std::collections::HashSet::new();
    for k in fight {
        let (killer_side, victim_side) = if k.killer_team == pteam {
            (&mut allies, &mut enemies)
        } else {
            (&mut enemies, &mut allies)
        };
        if k.killer > 0 {
            killer_side.insert(k.killer);
        }
        killer_side.extend(k.assists.iter().copied());
        if k.victim > 0 {
            victim_side.insert(k.victim);
        }
    }
    (allies.len() as i64, enemies.len() as i64)
}

/// Group the player's kills the same way fights cluster (by the shared time gap),
/// returning index ranges into the sorted kill list.
fn cluster_kill_indices(kills: &[KillEv]) -> Vec<std::ops::Range<usize>> {
    let mut clusters = Vec::new();
    if kills.is_empty() {
        return clusters;
    }
    let mut start = 0;
    for i in 1..kills.len() {
        if kills[i].t - kills[i - 1].t > FIGHT_GAP_MS {
            clusters.push(start..i);
            start = i;
        }
    }
    clusters.push(start..kills.len());
    clusters
}

fn multi_name(len: i64) -> &'static str {
    match len {
        2 => "double kill",
        3 => "triple kill",
        4 => "quadra kill",
        n if n >= 5 => "pentakill",
        _ => "multikill",
    }
}

// --- types ------------------------------------------------------------------

struct KillEv {
    t: i64,
    killer: i32,
    victim: i32,
    killer_team: i32,
    /// Participant ids credited with an assist on the kill.
    assists: Vec<i32>,
    /// Base gold bounty for the kill.
    bounty: i64,
    /// Extra shutdown gold (the victim was on a streak/bounty).
    shutdown: i64,
    x: f64,
    y: f64,
}

/// A `CHAMPION_SPECIAL_KILL` of type `KILL_MULTI` — a multikill worth clipping.
struct Special {
    t: i64,
    killer: i32,
    len: i64,
}

/// A candidate clip moment: an exact game-time anchor plus a grounded
/// one-liner. The deterministic pick ([`render_overview_md`]) takes the
/// best-scored candidate of each list; the ranked lists land in `moments.md`
/// as a record. `seek_s`/`dur_s` describe the video window to record so the
/// whole play fits.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub t_ms: i64,
    /// Game-second the clip should start at (lead-in already included).
    pub seek_s: i64,
    /// Clip length in seconds — sized to the fight span, capped.
    pub dur_s: i64,
    pub summary: String,
    /// The grounded score the pick maximizes.
    pub score: i64,
}

/// Seconds of lead-in before the play and tail after it, and the hard cap on a
/// clip's length (keeps the transcode under Discord's 10MB attachment limit).
const CLIP_LEAD_S: i64 = 7;
const CLIP_TAIL_S: i64 = 6;
const CLIP_MAX_S: i64 = 32;

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
    /// The participant credited with the killing blow (the smite, for
    /// monsters) — the anchor for objective-secure highlight candidates.
    killer: i32,
    /// Elder dragon (a `DRAGON` with the elder subtype) — worth Baron-tier
    /// score, unlike an elemental drake.
    elder: bool,
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
        let (won, lost) = if team == 100 {
            (blue, red)
        } else {
            (red, blue)
        };
        Some(WonFight { team, start: f.start, end: f.end, won, lost, converting: None })
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
                vec![format!("fight {}-{} ({}–{})", self.won, self.lost, mmss(self.start), mmss(self.end))],
            ),
        };
        Moment {
            t_ms: self.end,
            kind: MomentKind::FightConversion { team: self.team, converted: self.converting.is_some() },
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
            _ => fights.push(Fight { start: k.t, end: k.t, kills: vec![k.team] }),
        }
    }
    fights
}

// --- helpers ----------------------------------------------------------------

/// participantId (1..=10) -> team id (100/200), indexed by participant order.
fn team_by_participant(game: &Match) -> Vec<(i32, i32)> {
    game.info.participants.iter().map(|p| (p.participant_id, team_i32(p.team_id))).collect()
}

fn team_of_participant(map: &[(i32, i32)], participant_id: i64) -> i32 {
    map.iter().find(|(id, _)| i64::from(*id) == participant_id).map(|(_, team)| *team).unwrap_or(0)
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

    const MOON_PUUID: &str = "Mbv5r1kdyiQ3LgUGDz_gGPw73yE1GUvkPpAnI3-1Yg3wWivrzaI1E-TvYS7bNvSw3RZzg4yRhw35AA";

    fn fixture() -> Option<(Match, String)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../archive/NA1/5592214271");
        let match_json = std::fs::read_to_string(dir.join("match.json")).ok()?;
        let events = std::fs::read_to_string(dir.join("timeline-events.jsonl")).ok()?;
        let game: Match = serde_json::from_str(&match_json).ok()?;
        Some((game, events))
    }

    /// The headline: Red side (Moon's team) won multiple early fights but
    /// squandered them, while Blue side converted its fights.
    #[test]
    fn assignment_spreads_players_across_distinct_moments() {
        let candidate = |seek: i64, score: i64, summary: &str| Candidate {
            t_ms: seek * 1000,
            seek_s: seek,
            dur_s: 20,
            summary: summary.to_string(),
            score,
        };
        // Both players' best is the same teamfight window; B has a decent
        // alternative elsewhere, so B diverts and the channel gets two
        // different moments.
        let a_cands = vec![candidate(600, 10, "the teamfight")];
        let b_cands = vec![candidate(605, 8, "the same teamfight"), candidate(900, 6, "a solo pick")];
        let picks = assign_side(&[("a", a_cands.as_slice()), ("b", b_cands.as_slice())]);
        assert_eq!(picks["a"].summary, "the teamfight");
        assert_eq!(picks["b"].summary, "a solo pick", "diverted off the claimed window");

        // But when the alternative is far weaker, the shared moment stays.
        let c_cands = vec![candidate(605, 9, "the same teamfight again"), candidate(900, 2, "farming")];
        let picks = assign_side(&[("a", a_cands.as_slice()), ("c", c_cands.as_slice())]);
        assert_eq!(picks["c"].summary, "the same teamfight again", "no worthwhile alternative");
    }

    #[test]
    fn picks_render_by_section() {
        let best = Candidate { t_ms: 125_000, seek_s: 118, dur_s: 20, summary: "the outplay".to_string(), score: 9 };
        let md = render_picks_md(Some(&best), None);
        assert!(md.contains("**2:05** — the outplay"), "{md}");
        assert!(md.starts_with("## Highlight"));
        assert!(!md.contains("## Lowlight"), "no lowlight pick, no section");
    }

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
        assert!(red_conv * 2 <= red_won, "expected Red to squander most fights ({red_conv}/{red_won} converted)");
        assert!(blue_conv * 2 > blue_won, "expected Blue to convert most fights ({blue_conv}/{blue_won} converted)");
    }

    /// Moon died mostly for nothing, to one repeat killer, and touched no epics.
    #[test]
    fn moon_death_and_objective_signals() {
        let Some((game, events)) = fixture() else {
            eprintln!("fixture archive absent; skipping");
            return;
        };
        let moments = analyze(&game, &events, MOON_PUUID);

        let free = moments.iter().filter(|m| matches!(m.kind, MomentKind::Death { free: true })).count();
        assert!(free >= 5, "expected many free deaths, got {free}");
        assert!(moments.iter().any(|m| m.kind == MomentKind::Nemesis), "expected a nemesis moment");
        assert!(moments.iter().any(|m| m.kind == MomentKind::ObjectiveAbsence), "expected Moon absent from all majors");
        assert!(
            moments.iter().any(|m| matches!(m.kind, MomentKind::DragonMonopoly { team: 100 })),
            "expected Blue dragon monopoly"
        );
    }
}
