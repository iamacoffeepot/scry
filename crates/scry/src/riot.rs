use anyhow::{Context, Result, anyhow, bail};
use riven::RiotApi;
use riven::consts::{PlatformRoute, RegionalRoute};
use riven::models::match_v5::Match;

/// Thin wrapper over `riven` pinned to a single regional route.
///
/// match-v5 and account-v1 both route regionally (AMERICAS / ASIA / EUROPE),
/// which we derive from the caller's platform code (na1, euw1, …).
pub struct Client {
    api: RiotApi,
    regional: RegionalRoute,
    platform: PlatformRoute,
}

impl Client {
    pub fn new(api_key: &str, region: &str) -> Result<Self> {
        let platform = parse_platform(region)?;
        Ok(Self {
            api: RiotApi::new(api_key),
            regional: platform.to_regional(),
            platform,
        })
    }

    /// Region slug used by OP.GG / League of Graphs match URLs (na, euw, kr, …).
    pub fn region_slug(&self) -> &'static str {
        match self.platform {
            PlatformRoute::NA1 => "na",
            PlatformRoute::EUW1 => "euw",
            PlatformRoute::EUN1 => "eune",
            PlatformRoute::KR => "kr",
            PlatformRoute::BR1 => "br",
            PlatformRoute::JP1 => "jp",
            PlatformRoute::LA1 => "lan",
            PlatformRoute::LA2 => "las",
            PlatformRoute::OC1 => "oce",
            PlatformRoute::TR1 => "tr",
            PlatformRoute::RU => "ru",
            _ => "na",
        }
    }

    /// Resolve a `gameName#tagLine` Riot ID to its PUUID via account-v1.
    pub async fn resolve_puuid(&self, riot_id: &str) -> Result<String> {
        let (game_name, tag_line) = riot_id
            .split_once('#')
            .ok_or_else(|| anyhow!("Riot ID must be `gameName#tagLine`, got `{riot_id}`"))?;

        let account = self
            .api
            .account_v1()
            .get_by_riot_id(self.regional, game_name, tag_line)
            .await
            .context("account-v1 request failed")?
            .ok_or_else(|| anyhow!("no account found for `{riot_id}`"))?;

        Ok(account.puuid)
    }

    /// Most-recent match IDs for a PUUID, newest first.
    pub async fn recent_match_ids(&self, puuid: &str, count: i32) -> Result<Vec<String>> {
        self.api
            .match_v5()
            .get_match_ids_by_puuid(self.regional, puuid, Some(count), None, None, None, None, None)
            .await
            .context("match-v5 match-id list request failed")
    }

    /// Full detail for one match.
    pub async fn match_detail(&self, match_id: &str) -> Result<Match> {
        self.api
            .match_v5()
            .get_match(self.regional, match_id)
            .await
            .context("match-v5 match request failed")?
            .ok_or_else(|| anyhow!("match `{match_id}` not found"))
    }
}

/// Map a platform code to a riven `PlatformRoute`. Covers the main live regions.
fn parse_platform(region: &str) -> Result<PlatformRoute> {
    Ok(match region.to_ascii_lowercase().as_str() {
        "na1" | "na" => PlatformRoute::NA1,
        "euw1" | "euw" => PlatformRoute::EUW1,
        "eun1" | "eune" => PlatformRoute::EUN1,
        "kr" => PlatformRoute::KR,
        "br1" | "br" => PlatformRoute::BR1,
        "jp1" | "jp" => PlatformRoute::JP1,
        "la1" => PlatformRoute::LA1,
        "la2" => PlatformRoute::LA2,
        "oc1" | "oce" => PlatformRoute::OC1,
        "tr1" | "tr" => PlatformRoute::TR1,
        "ru" => PlatformRoute::RU,
        other => bail!(
            "unknown region `{other}` \
             (expected one of: na1, euw1, eun1, kr, br1, jp1, la1, la2, oc1, tr1, ru)"
        ),
    })
}
