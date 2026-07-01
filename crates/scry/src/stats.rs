use riven::models::match_v5::Match;

/// Per-render context: the things the embed needs that aren't in the match data
/// (which web region to link, and a display-name fallback if Riot omits it).
pub struct RenderContext<'a> {
    pub region_slug: &'a str,
    pub fallback_name: &'a str,
    pub fallback_tag: &'a str,
}

/// A flattened, post-ready summary of one player's performance in one match.
pub struct MatchSummary {
    pub player: String,
    pub champion: String,
    pub icon_url: String,
    pub profile_url: String,
    pub win: bool,
    /// "Blue" or "Red" — the side the player was on.
    pub side: &'static str,
    pub duration_secs: i64,
    /// Match start, unix seconds (for Discord's dynamic `<t:…>` timestamp).
    pub started_at_secs: i64,
    pub kills: i32,
    pub deaths: i32,
    pub assists: i32,
    pub cs: i32,
    pub cs_per_min: f64,
    pub damage_to_champions: i32,
    pub gold: i32,
    pub vision: VisionStats,
    /// Ranked standing + LP delta, when LP tracking is on (ranked games only).
    pub rank: Option<RankInfo>,
}

/// Ranked standing to show in the header, with the LP change since last game.
pub struct RankInfo {
    /// e.g. "Gold II" or "Master".
    pub label: String,
    pub lp: i32,
    /// LP gained (+) or lost (-) since the previous snapshot; `None` if this is
    /// the first game we've catalogued for the account (no baseline yet).
    pub delta: Option<i32>,
}

/// Warding / vision block for the stats embed.
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
pub fn summarize(game: &Match, puuid: &str, ctx: &RenderContext) -> Option<MatchSummary> {
    let info = &game.info;
    let p = info.participants.iter().find(|p| p.puuid == puuid)?;

    let duration_secs = info.game_duration;
    let started_at_secs = info.game_start_timestamp / 1000;
    let side = match p.team_id {
        riven::consts::Team::BLUE => "Blue",
        riven::consts::Team::RED => "Red",
        _ => "?",
    };
    // Guard against div-by-zero on remakes / zero-length games.
    let minutes = (duration_secs as f64 / 60.0).max(1.0 / 60.0);
    let cs = p.total_minions_killed + p.neutral_minions_killed;

    // `.champion()` resolves the numeric id (falling back to champion_name if
    // Riot returned a corrupted id). The numeric id keys a Community Dragon
    // icon directly, which avoids DDragon's name-key quirks (Wukong/Fiddlesticks).
    let champion = p.champion().ok();
    let champion_id: i16 = champion.map(i16::from).unwrap_or(-1);
    let champion_name = champion
        .and_then(|c| c.name())
        .map(str::to_owned)
        .unwrap_or_else(|| p.champion_name.clone());

    let game_name =
        non_empty(p.riot_id_game_name.clone()).unwrap_or_else(|| ctx.fallback_name.to_owned());
    let tag_line =
        non_empty(p.riot_id_tagline.clone()).unwrap_or_else(|| ctx.fallback_tag.to_owned());

    Some(MatchSummary {
        player: format!("{game_name} #{tag_line}"),
        champion: champion_name,
        icon_url: format!(
            "https://raw.communitydragon.org/latest/plugins/rcp-be-lol-game-data/global/default/v1/champion-icons/{champion_id}.png"
        ),
        profile_url: format!(
            "https://www.op.gg/summoners/{}/{}-{}",
            ctx.region_slug,
            urlencoding::encode(&game_name),
            urlencoding::encode(&tag_line),
        ),
        win: p.win,
        side,
        duration_secs,
        started_at_secs,
        kills: p.kills,
        deaths: p.deaths,
        assists: p.assists,
        cs,
        cs_per_min: cs as f64 / minutes,
        damage_to_champions: p.total_damage_dealt_to_champions,
        gold: p.gold_earned,
        rank: None,
        vision: VisionStats {
            vision_score: p.vision_score,
            vision_per_min: p.vision_score as f64 / minutes,
            wards_placed: p.wards_placed,
            wards_killed: p.wards_killed,
            control_wards_bought: p.vision_wards_bought_in_game,
        },
    })
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}
