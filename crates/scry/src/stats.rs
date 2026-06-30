use riven::models::match_v5::Match;

/// A flattened, post-ready summary of one player's performance in one match.
pub struct MatchSummary {
    pub champion: String,
    pub win: bool,
    pub duration_secs: i64,
    pub kills: i32,
    pub deaths: i32,
    pub assists: i32,
    pub cs: i32,
    pub cs_per_min: f64,
    pub damage_to_champions: i32,
    pub gold: i32,
    pub vision: VisionStats,
}

/// Warding / vision block — the headline "advanced" stat for v1.
pub struct VisionStats {
    pub vision_score: i32,
    pub vision_per_min: f64,
    pub wards_placed: i32,
    pub wards_killed: i32,
    pub control_wards_bought: i32,
}

impl MatchSummary {
    /// (kills + assists) / deaths, treating a deathless game as a perfect ratio.
    pub fn kda(&self) -> f64 {
        if self.deaths == 0 {
            (self.kills + self.assists) as f64
        } else {
            (self.kills + self.assists) as f64 / self.deaths as f64
        }
    }
}

/// Build a summary for the participant matching `puuid`, or `None` if absent.
pub fn summarize(game: &Match, puuid: &str) -> Option<MatchSummary> {
    let info = &game.info;
    let p = info.participants.iter().find(|p| p.puuid == puuid)?;

    let duration_secs = info.game_duration;
    // Guard against div-by-zero on remakes/zero-length games.
    let minutes = (duration_secs as f64 / 60.0).max(1.0 / 60.0);
    let cs = p.total_minions_killed + p.neutral_minions_killed;

    Some(MatchSummary {
        champion: p.champion_name.clone(),
        win: p.win,
        duration_secs,
        kills: p.kills,
        deaths: p.deaths,
        assists: p.assists,
        cs,
        cs_per_min: cs as f64 / minutes,
        damage_to_champions: p.total_damage_dealt_to_champions,
        gold: p.gold_earned,
        vision: VisionStats {
            vision_score: p.vision_score,
            vision_per_min: p.vision_score as f64 / minutes,
            wards_placed: p.wards_placed,
            wards_killed: p.wards_killed,
            control_wards_bought: p.vision_wards_bought_in_game,
        },
    })
}
