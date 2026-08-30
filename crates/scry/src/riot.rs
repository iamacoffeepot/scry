use anyhow::{Context, Result, anyhow, bail};
use riven::RiotApi;
use riven::consts::{PlatformRoute, RegionalRoute};
use riven::models::match_v5::Match;
use serde_json::Value;

/// Thin wrapper over `riven` pinned to a single regional route.
///
/// match-v5 and account-v1 both route regionally (AMERICAS / ASIA / EUROPE),
/// which we derive from the caller's platform code (na1, euw1, …).
pub struct Client {
    api: RiotApi,
    regional: RegionalRoute,
    platform: PlatformRoute,
    // Held for the raw-JSON archive path (riven exposes only typed models).
    api_key: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(api_key: &str, region: &str) -> Result<Self> {
        let platform = parse_platform(region)?;
        Ok(Self {
            api: RiotApi::new(api_key),
            regional: platform.to_regional(),
            platform,
            api_key: api_key.to_owned(),
            http: reqwest::Client::new(),
        })
    }

    /// Raw match-v5 detail JSON, exactly as Riot returns it (captures fields
    /// riven's typed model may not cover yet).
    pub async fn raw_match_json(&self, match_id: &str) -> Result<Value> {
        self.raw_get(&format!("/lol/match/v5/matches/{match_id}")).await
    }

    /// Raw match-v5 timeline JSON.
    pub async fn raw_timeline_json(&self, match_id: &str) -> Result<Value> {
        self.raw_get(&format!("/lol/match/v5/matches/{match_id}/timeline")).await
    }

    async fn raw_get(&self, path: &str) -> Result<Value> {
        self.raw_get_host(self.regional_host(), path).await
    }

    /// league-v4 routes by platform (na1, euw1, …), not by regional cluster.
    fn platform_host(&self) -> String {
        format!("{:?}", self.platform).to_lowercase()
    }

    async fn raw_get_host(&self, host: &str, path: &str) -> Result<Value> {
        let url = format!("https://{host}.api.riotgames.com{path}");
        let resp = self
            .http
            .get(&url)
            .header("X-Riot-Token", &self.api_key)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Riot returned {status} for {url}: {body}");
        }
        resp.json::<Value>().await.context("parsing Riot JSON")
    }

    fn regional_host(&self) -> &'static str {
        match self.regional {
            RegionalRoute::AMERICAS => "americas",
            RegionalRoute::ASIA => "asia",
            RegionalRoute::EUROPE => "europe",
            RegionalRoute::SEA => "sea",
            _ => "americas",
        }
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
    /// `Ok(None)` means Riot has no account under that ID — for a previously
    /// tracked account that usually means a rename (PUUIDs are permanent;
    /// names are labels).
    pub async fn resolve_puuid(&self, riot_id: &str) -> Result<Option<String>> {
        let (game_name, tag_line) =
            riot_id.split_once('#').ok_or_else(|| anyhow!("Riot ID must be `gameName#tagLine`, got `{riot_id}`"))?;

        Ok(self
            .api
            .account_v1()
            .get_by_riot_id(self.regional, game_name, tag_line)
            .await
            .context("account-v1 request failed")?
            .map(|account| account.puuid))
    }

    /// The current `gameName#tagLine` of a PUUID — the reverse lookup that
    /// turns a stale watch-list name into the account's new one.
    pub async fn riot_id_of(&self, puuid: &str) -> Result<Option<String>> {
        let account = self
            .api
            .account_v1()
            .get_by_puuid(self.regional, puuid)
            .await
            .context("account-v1 by-puuid request failed")?;
        Ok(match (account.game_name, account.tag_line) {
            (Some(name), Some(tag)) if !name.is_empty() => Some(format!("{name}#{tag}")),
            _ => None,
        })
    }

    /// The player's current standing in `queue_type` (league-v4), or `None`
    /// if unranked there. Parsed leniently from the raw JSON rather than
    /// riven's typed model: a ranked tier Riot adds (the 2026 `SALT` tier
    /// broke the enum) must degrade the ladder ordering, never the LP line.
    pub async fn rank(&self, puuid: &str, queue_type: &str) -> Result<Option<crate::rank::Rank>> {
        let entries = self
            .raw_get_host(&self.platform_host(), &format!("/lol/league/v4/entries/by-puuid/{puuid}"))
            .await
            .context("league-v4 entries request failed")?;
        Ok(entries
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e.get("queueType").and_then(Value::as_str) == Some(queue_type))
            .map(|e| crate::rank::Rank {
                tier: e.get("tier").and_then(Value::as_str).unwrap_or("UNRANKED").to_string(),
                division: e.get("rank").and_then(Value::as_str).unwrap_or("I").to_string(),
                lp: e.get("leaguePoints").and_then(Value::as_i64).unwrap_or(0) as i32,
            }))
    }

    pub async fn recent_match_ids(&self, puuid: &str, count: i32, queue: Option<u16>) -> Result<Vec<String>> {
        self.api
            .match_v5()
            .get_match_ids_by_puuid(
                self.regional,
                puuid,
                Some(count),
                None,
                queue.map(riven::consts::Queue),
                None,
                None,
                None,
            )
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

/// OP.GG / League of Graphs region slug from a Riot `platformId` (e.g. "NA1").
/// Used by the archive-post path, which has the platform string rather than a
/// typed `PlatformRoute`.
pub fn web_region_slug(platform_id: &str) -> &'static str {
    match platform_id.to_ascii_uppercase().as_str() {
        "NA1" => "na",
        "EUW1" => "euw",
        "EUN1" => "eune",
        "KR" => "kr",
        "BR1" => "br",
        "JP1" => "jp",
        "LA1" => "lan",
        "LA2" => "las",
        "OC1" => "oce",
        "TR1" => "tr",
        "RU" => "ru",
        _ => "na",
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
