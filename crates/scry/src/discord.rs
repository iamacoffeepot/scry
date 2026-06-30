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
            // Champion icon rides as the small author avatar; a top-right
            // thumbnail would squeeze the field grid down to two columns.
            "author": { "name": s.player, "url": s.profile_url, "icon_url": s.icon_url },
            "title": format!("{} — {result}", s.champion),
            "url": s.match_url,
            "color": color,
            // Six inline fields = two clean rows of three, then a full-width
            // Wards line.
            "fields": [
                { "name": "KDA", "value": format!("{}/{}/{} ({:.2})", s.kills, s.deaths, s.assists, s.kda()), "inline": true },
                { "name": "CS", "value": format!("{} ({:.1}/min)", s.cs, s.cs_per_min), "inline": true },
                { "name": "Damage", "value": thousands(s.damage_to_champions), "inline": true },
                { "name": "Gold", "value": thousands(s.gold), "inline": true },
                { "name": "Duration", "value": format!("{mins}:{secs:02}"), "inline": true },
                { "name": "Vision", "value": format!("{} ({:.2}/min)", s.vision.vision_score, s.vision.vision_per_min), "inline": true },
                { "name": "Wards", "value": format!(
                    "{} placed · {} killed · {} control",
                    s.vision.wards_placed, s.vision.wards_killed, s.vision.control_wards_bought),
                    "inline": false },
            ],
            "footer": { "text": "scry" }
        }]
    })
}

/// Group digits with commas: `18420` -> `"18,420"`.
fn thousands(n: i32) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}
