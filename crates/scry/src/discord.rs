use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::stats::MatchSummary;
use crate::summary::Summary;

/// A PNG (or other) file attached to a webhook message. Reference it from a
/// media component with `attachment://<filename>`.
pub struct Attachment {
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// A Discord incoming webhook the message is POSTed to.
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

    /// Post a prebuilt message payload (and any attachments) as one webhook
    /// message. With attachments the request is multipart (`payload_json` +
    /// `files[i]`).
    pub async fn post(&self, payload: &Value, attachments: &[Attachment]) -> Result<()> {
        // Incoming webhooks only process Components V2 when this is set.
        let sep = if self.url.contains('?') { '&' } else { '?' };
        let url = format!("{}{sep}with_components=true", self.url);
        let builder = if attachments.is_empty() {
            self.http.post(&url).json(payload)
        } else {
            let mut form = reqwest::multipart::Form::new().text(
                "payload_json",
                serde_json::to_string(payload).context("serializing webhook payload")?,
            );
            for (i, a) in attachments.iter().enumerate() {
                let part = reqwest::multipart::Part::bytes(a.bytes.clone())
                    .file_name(a.filename.clone())
                    .mime_str("image/png")
                    .context("building attachment part")?;
                form = form.part(format!("files[{i}]"), part);
            }
            self.http.post(&url).multipart(form)
        };

        let resp = builder.send().await.context("posting to Discord webhook")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Discord webhook returned {status}: {body}");
        }
        Ok(())
    }
}

// Components V2 — https://discord.com/developers/docs/components/reference
// The message must set the IS_COMPONENTS_V2 flag and carry no content/embeds.
const FLAG_COMPONENTS_V2: u64 = 1 << 15;
// Component type ids.
const CONTAINER: u64 = 17;
const SECTION: u64 = 9;
const TEXT_DISPLAY: u64 = 10;
const THUMBNAIL: u64 = 11;
const MEDIA_GALLERY: u64 = 12;
const SEPARATOR: u64 = 14;

const FOOTER: &str = "scry - github.com/iamacoffeepot/scry";

/// Embed accent color by result: green on win, red on loss.
fn result_color(win: bool) -> u32 {
    if win { 0x2ecc71 } else { 0xe74c3c }
}

/// The stats-only message (live posting and the no-overview archive case).
pub fn stats_message(s: &MatchSummary, chart: Option<&str>) -> Value {
    let mut body = vec![header_section(s), text(stats_text(s))];
    if let Some(name) = chart {
        body.push(media(name));
    }
    body.push(footer(None));
    container_message(s.win, body)
}

/// The full package: stats, a real Separator, the AI overview, and the chart —
/// all in one Components-V2 container (the accent bar keys off the result).
pub fn combined_message(
    s: &MatchSummary,
    summary: &Summary,
    model: &str,
    chart: Option<&str>,
) -> Value {
    let mut body = vec![
        header_section(s),
        text(stats_text(s)),
        json!({ "type": SEPARATOR, "divider": true, "spacing": 2 }),
        text(overview_text(summary)),
    ];
    if let Some(name) = chart {
        body.push(media(name));
    }
    body.push(footer(Some(model)));
    container_message(s.win, body)
}

/// Wrap child components in an accent-colored container as the whole message.
fn container_message(win: bool, components: Vec<Value>) -> Value {
    json!({
        "flags": FLAG_COMPONENTS_V2,
        "components": [{
            "type": CONTAINER,
            "accent_color": result_color(win),
            "components": components,
        }],
    })
}

/// A markdown text component.
fn text(content: impl Into<String>) -> Value {
    json!({ "type": TEXT_DISPLAY, "content": content.into() })
}

/// The chart as a single-item media gallery referencing the attachment.
fn media(filename: &str) -> Value {
    json!({
        "type": MEDIA_GALLERY,
        "items": [{ "media": { "url": format!("attachment://{filename}") } }],
    })
}

/// Title + player link, with the summoner icon as the section's thumbnail.
fn header_section(s: &MatchSummary) -> Value {
    let result = if s.win { "Victory" } else { "Defeat" };
    let mut content = format!(
        "-# <t:{}:F> · <t:{}:R>\n### {} — {result}\n[{}]({}) · {} side",
        s.started_at_secs, s.started_at_secs, s.champion, s.player, s.profile_url, s.side
    );
    if let Some(r) = &s.rank {
        let delta = match r.delta {
            Some(d) => format!(" · {d:+} LP"),
            None => String::new(),
        };
        content.push_str(&format!("\n**{}** · {} LP{delta}", r.label, r.lp));
    }
    json!({
        "type": SECTION,
        "components": [ text(content) ],
        "accessory": { "type": THUMBNAIL, "media": { "url": s.icon_url } },
    })
}

/// The stat grid rendered as text (Components V2 has no native field grid).
fn stats_text(s: &MatchSummary) -> String {
    let mins = s.duration_secs / 60;
    let secs = s.duration_secs % 60;
    format!(
        "**KDA** {}/{}/{} ({:.2})  •  **CS** {} ({:.1}/min)  •  **Damage** {}\n\
         **Gold** {}  •  **Duration** {mins}:{secs:02}  •  **Vision** {} ({:.2}/min)\n\
         **Wards** {} placed • {} killed • {} control",
        s.kills,
        s.deaths,
        s.assists,
        s.kda(),
        s.cs,
        s.cs_per_min,
        thousands(s.damage_to_champions),
        thousands(s.gold),
        s.vision.vision_score,
        s.vision.vision_per_min,
        s.vision.wards_placed,
        s.vision.wards_killed,
        s.vision.control_wards_bought,
    )
}

/// Verdict (lead paragraph) then each remaining section under a bold heading.
fn overview_text(summary: &Summary) -> String {
    let mut out = String::new();
    if let Some((_, body)) = summary
        .sections
        .iter()
        .find(|(h, _)| h.eq_ignore_ascii_case("Verdict"))
    {
        // The header already states the result; drop a leading "Victory/Defeat".
        out.push_str(&truncate(strip_result_prefix(body), 1024));
        out.push_str("\n\n");
    }
    for (heading, body) in &summary.sections {
        if !heading.eq_ignore_ascii_case("Verdict") {
            out.push_str(&format!("**{heading}**\n{}\n\n", truncate(body, 1024)));
        }
    }
    out.trim_end().to_string()
}

/// Small grey subtext footer: model attribution (if any) plus the scry brand.
fn footer(model: Option<&str>) -> Value {
    let content = match model {
        Some(m) => format!("-# Generated with {m} · {FOOTER}"),
        None => format!("-# {FOOTER}"),
    };
    text(content)
}

/// Drop a leading "Victory" / "Defeat" (and its trailing punctuation) from the
/// Verdict, since the header already states the result.
fn strip_result_prefix(s: &str) -> &str {
    let t = s.trim_start();
    for p in ["Victory", "Defeat"] {
        if t.len() >= p.len() && t[..p.len()].eq_ignore_ascii_case(p) {
            return t[p.len()..].trim_start_matches(['.', ',', ':', '—', '-', ' ']);
        }
    }
    t
}

/// Truncate to at most `max` characters, appending an ellipsis if cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Group digits with commas: `18420` -> `"18,420"`.
fn thousands(n: i32) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}
