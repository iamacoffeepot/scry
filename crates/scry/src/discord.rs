use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::stats::MatchSummary;

/// A Discord incoming webhook the summaries are POSTed to.
pub struct Webhook {
    url: String,
    http: reqwest::Client,
}

impl Webhook {
    pub fn new(url: String) -> Self {
        Self {
            url,
            http: reqwest::Client::new(),
        }
    }

    pub async fn post(&self, summary: &MatchSummary) -> Result<()> {
        let resp = self
            .http
            .post(&self.url)
            .json(&embed(summary))
            .send()
            .await
            .context("posting to Discord webhook")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Discord webhook returned {status}: {body}");
        }
        Ok(())
    }
}

/// Render a summary as a Discord embed payload.
fn embed(s: &MatchSummary) -> Value {
    let (result, color) = if s.win {
        ("Victory", 0x2ecc71)
    } else {
        ("Defeat", 0xe74c3c)
    };
    let mins = s.duration_secs / 60;
    let secs = s.duration_secs % 60;

    json!({
        "embeds": [{
            "author": { "name": s.player, "url": s.profile_url },
            "title": format!("{} — {result}", s.champion),
            "url": s.match_url,
            "color": color,
            "thumbnail": { "url": s.icon_url },
            "fields": [
                { "name": "KDA", "value": format!("{}/{}/{} ({:.2})", s.kills, s.deaths, s.assists, s.kda()), "inline": true },
                { "name": "CS", "value": format!("{} ({:.1}/min)", s.cs, s.cs_per_min), "inline": true },
                { "name": "Damage", "value": s.damage_to_champions.to_string(), "inline": true },
                { "name": "Gold", "value": s.gold.to_string(), "inline": true },
                { "name": "Duration", "value": format!("{mins}:{secs:02}"), "inline": true },
                { "name": "Vision", "value": format!(
                    "Score {} ({:.2}/min)\n{} placed / {} killed\n{} control wards",
                    s.vision.vision_score, s.vision.vision_per_min,
                    s.vision.wards_placed, s.vision.wards_killed, s.vision.control_wards_bought),
                    "inline": false },
            ],
            "footer": { "text": "scry" }
        }]
    })
}
