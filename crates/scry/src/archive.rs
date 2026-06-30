use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Dump one match's raw data under `<dir>/<matchId>/` for offline AI analysis:
///   - `match.json`            — full match-v5 detail, pretty-printed
///   - `timeline-events.jsonl` — every timeline event, one per line (grep-friendly)
///   - `timeline-frames.jsonl` — one per-minute frame per line, slimmed to the
///     per-player economy/position (the heavy champion/damage stat blocks dropped)
///
/// Returns the directory written.
pub fn write(dir: &Path, match_id: &str, match_json: &Value, timeline: &Value) -> Result<PathBuf> {
    let out = dir.join(match_id);
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    fs::write(out.join("match.json"), serde_json::to_vec_pretty(match_json)?)
        .with_context(|| format!("writing {}", out.join("match.json").display()))?;

    let frames = timeline
        .pointer("/info/frames")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let mut events = String::new();
    for frame in frames {
        if let Some(evs) = frame.get("events").and_then(Value::as_array) {
            for e in evs {
                events.push_str(&serde_json::to_string(e)?);
                events.push('\n');
            }
        }
    }
    fs::write(out.join("timeline-events.jsonl"), events)
        .with_context(|| format!("writing {}", out.join("timeline-events.jsonl").display()))?;

    let mut slim = String::new();
    for frame in frames {
        let mut players: Vec<Value> = frame
            .get("participantFrames")
            .and_then(Value::as_object)
            .map(|pf| pf.values().map(slim_frame).collect())
            .unwrap_or_default();
        players.sort_by_key(|p| p.get("participantId").and_then(Value::as_i64).unwrap_or(0));

        let line = json!({ "timestamp": frame.get("timestamp"), "participants": players });
        slim.push_str(&serde_json::to_string(&line)?);
        slim.push('\n');
    }
    fs::write(out.join("timeline-frames.jsonl"), slim)
        .with_context(|| format!("writing {}", out.join("timeline-frames.jsonl").display()))?;

    Ok(out)
}

/// Keep only the economy/position signal from a participant frame.
fn slim_frame(pf: &Value) -> Value {
    json!({
        "participantId": pf.get("participantId"),
        "totalGold": pf.get("totalGold"),
        "xp": pf.get("xp"),
        "level": pf.get("level"),
        "minionsKilled": pf.get("minionsKilled"),
        "jungleMinionsKilled": pf.get("jungleMinionsKilled"),
        "position": pf.get("position"),
    })
}
