use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::stats::MatchSummary;

/// A PNG (or other) file attached to a webhook message. Reference it from a
/// media component with `attachment://<filename>`.
pub struct Attachment {
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// One clip to embed: the attached video's filename plus the caption rendered
/// beside it (the pick's `**m:ss** — summary` line, when the journal has one).
pub struct Clip {
    pub filename: String,
    pub caption: Option<String>,
}

/// A Discord incoming webhook the message is POSTed to.
pub struct Webhook {
    url: String,
    http: reqwest::Client,
}

impl Webhook {
    pub fn new(url: String) -> Self {
        Self { url, http: reqwest::Client::new() }
    }

    /// Post a prebuilt message payload (and any attachments) as one webhook
    /// message, returning the created message's id (needed to edit it later).
    /// With attachments the request is multipart (`payload_json` + `files[i]`).
    pub async fn post(&self, payload: &Value, attachments: &[Attachment]) -> Result<Option<String>> {
        // Incoming webhooks only process Components V2 with this flag; `wait`
        // makes Discord return the created message (so we can read its id).
        let sep = if self.url.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!("{}{sep}with_components=true&wait=true", self.url);
        let builder = self.attach(self.http.post(&url), payload, attachments)?;
        let resp = self.send_checked(builder, "posting to Discord webhook").await?;
        let body = resp.text().await.unwrap_or_default();
        Ok(serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string)))
    }

    /// Edit a previously-posted webhook message in place (used to attach the
    /// clips once recorded). Attachments sent here fully replace the message's
    /// files, so the caller must include every attachment it wants to keep.
    pub async fn edit(&self, message_id: &str, payload: &Value, attachments: &[Attachment]) -> Result<()> {
        let base = self.url.split('?').next().unwrap_or(&self.url);
        let url = format!("{base}/messages/{message_id}?with_components=true");
        // On edit, Discord APPENDS uploaded files unless the payload's
        // `attachments` array declares the full set. Reference each new file by
        // its `files[i]` index so this edit replaces the message's attachments
        // (otherwise repeated edits pile up to the 10-attachment cap).
        let mut payload = payload.clone();
        if let Some(obj) = payload.as_object_mut() {
            let atts: Vec<Value> =
                attachments.iter().enumerate().map(|(i, a)| json!({ "id": i, "filename": a.filename })).collect();
            obj.insert("attachments".to_string(), Value::Array(atts));
        }
        let builder = self.attach(self.http.patch(&url), &payload, attachments)?;
        self.send_checked(builder, "editing Discord webhook message").await?;
        Ok(())
    }

    /// Attach the payload to a request as JSON, or multipart when there are files.
    fn attach(
        &self,
        builder: reqwest::RequestBuilder,
        payload: &Value,
        attachments: &[Attachment],
    ) -> Result<reqwest::RequestBuilder> {
        if attachments.is_empty() {
            return Ok(builder.json(payload));
        }
        let mut form = reqwest::multipart::Form::new()
            .text("payload_json", serde_json::to_string(payload).context("serializing webhook payload")?);
        for (i, a) in attachments.iter().enumerate() {
            let mime = match a.filename.rsplit('.').next() {
                Some("mp4") => "video/mp4",
                Some("webm") => "video/webm",
                _ => "image/png",
            };
            let part = reqwest::multipart::Part::bytes(a.bytes.clone())
                .file_name(a.filename.clone())
                .mime_str(mime)
                .context("building attachment part")?;
            form = form.part(format!("files[{i}]"), part);
        }
        Ok(builder.multipart(form))
    }

    async fn send_checked(&self, builder: reqwest::RequestBuilder, what: &'static str) -> Result<reqwest::Response> {
        let resp = builder.send().await.context(what)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Discord webhook returned {status}: {body}");
        }
        Ok(resp)
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
    if win {
        0x2ecc71
    } else {
        0xe74c3c
    }
}

/// The one message shape: header + stats + the captioned clips (when
/// recorded), in one Components-V2 container whose accent keys off the result.
pub fn stats_message(s: &MatchSummary, highlight: Option<&Clip>, lowlight: Option<&Clip>, note: Option<&str>) -> Value {
    let mut body = vec![header_section(s), text(stats_text(s))];
    if let Some(clip) = highlight {
        clip_block(&mut body, "Highlight", clip);
    }
    if let Some(clip) = lowlight {
        clip_block(&mut body, "Lowlight", clip);
    }
    push_note(&mut body, note);
    body.push(footer());
    container_message(s.win, body)
}

/// A small grey status line (e.g. "Replay expired — no clips for this game").
fn push_note(body: &mut Vec<Value>, note: Option<&str>) {
    if let Some(note) = note {
        body.push(text(format!("-# {note}")));
    }
}

/// Append a captioned clip: a divider, a bold header + optional caption, then
/// the video.
fn clip_block(body: &mut Vec<Value>, header: &str, clip: &Clip) {
    body.push(json!({ "type": SEPARATOR, "divider": true, "spacing": 2 }));
    let content = match clip.caption.as_deref().map(str::trim) {
        Some(c) if !c.is_empty() => format!("**{header}** {c}"),
        _ => format!("**{header}**"),
    };
    body.push(text(content));
    body.push(media(&clip.filename));
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

/// A single-item media gallery referencing an attachment by filename.
fn media(filename: &str) -> Value {
    json!({
        "type": MEDIA_GALLERY,
        "items": [{ "media": { "url": format!("attachment://{filename}") } }],
    })
}

/// Title + player link, with the summoner icon as the section's thumbnail.
fn header_section(s: &MatchSummary) -> Value {
    let result = if s.win {
        "Victory"
    } else {
        "Defeat"
    };
    // Title is the player; champion/side and date are secondary rows beneath.
    let queue = if s.queue.is_empty() {
        String::new()
    } else {
        format!("{} · ", s.queue)
    };
    // LP delta (ranked games only): "+18 LP" / "-15 LP", or "+?" if no baseline.
    // Shown after the result in the title (emphasized) and on the rank line.
    let lp_delta = s.rank.as_ref().map(|r| match r.delta {
        Some(d) => format!("{d:+} LP"),
        None => "+? LP".to_string(),
    });
    let title_lp = lp_delta.as_ref().map(|d| format!(" ({d})")).unwrap_or_default();
    // Timestamp the game's end (start + duration), so "x minutes ago" reads from
    // when it finished — which is when the post goes out. Match length rides in
    // the result line: "Victory in 25:19".
    let ended_at = s.started_at_secs + s.duration_secs;
    let dur = format!("{}m {:02}s", s.duration_secs / 60, s.duration_secs % 60);
    let mut content = format!(
        "-# <t:{ended_at}:F> · <t:{ended_at}:R>\n### [{}]({}) — {result} in {dur}{title_lp}\n{} · {queue}{} side",
        s.player, s.profile_url, s.champion, s.side
    );
    if let Some(r) = &s.rank {
        let below = lp_delta.as_deref().map(|d| format!(" · {d}")).unwrap_or_default();
        content.push_str(&format!("\n**{}** · {} LP{below}", r.label, r.lp));
    }
    json!({
        "type": SECTION,
        "components": [ text(content) ],
        "accessory": { "type": THUMBNAIL, "media": { "url": s.icon_url } },
    })
}

/// The stat grid rendered as text (Components V2 has no native field grid).
fn stats_text(s: &MatchSummary) -> String {
    format!(
        "**KDA** {}/{}/{} ({:.2})  •  **CS** {} ({:.1}/min)  •  **Damage** {}\n\
         **Gold** {}  •  **Vision** {} ({:.2}/min)\n\
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

/// Small grey subtext footer: the scry brand line.
fn footer() -> Value {
    text(format!("-# {FOOTER}"))
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
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}
