//! Render a single dark-themed "dashboard" PNG (gold lead + damage + lobby
//! ranking) from the archived match data, styled to sit in a Discord embed.

use std::error::Error;
use std::path::Path;

use anyhow::{Result, anyhow};
use plotters::coord::Shift;
use plotters::prelude::*;
use riven::consts::Team;
use riven::models::match_v5::{Match, Participant};
use serde_json::Value;

const WIDTH: u32 = 1000;
const HEIGHT: u32 = 940;
const TOP_H: u32 = 420;

// Discord-ish palette.
const BG: RGBColor = RGBColor(43, 45, 49); // #2B2D31
const FG: RGBColor = RGBColor(219, 222, 225); // near-white text
const GRID: RGBColor = RGBColor(70, 74, 81); // subtle axis/lines
const GREEN: RGBColor = RGBColor(87, 242, 135); // #57F287
const BLURPLE: RGBColor = RGBColor(88, 101, 242); // #5865F2
const MUTED: RGBColor = RGBColor(90, 95, 104); // other players

type Area<'a> = DrawingArea<BitMapBackend<'a>, Shift>;
type Drawn = std::result::Result<(), Box<dyn Error>>;

/// Render the composite dashboard to `out` as one PNG.
pub fn dashboard(game: &Match, frames_jsonl: &str, puuid: &str, out: &Path) -> Result<()> {
    let root = BitMapBackend::new(out, (WIDTH, HEIGHT)).into_drawing_area();
    root.fill(&BG).map_err(|e| anyhow!("chart fill: {e}"))?;

    let (top, bottom) = root.split_vertically(TOP_H);
    let (bottom_left, bottom_right) = bottom.split_horizontally(WIDTH / 2);

    draw_gold(&top, game, frames_jsonl, puuid).map_err(|e| anyhow!("gold chart: {e}"))?;
    draw_damage(&bottom_left, game, puuid).map_err(|e| anyhow!("damage chart: {e}"))?;
    draw_ranking(&bottom_right, game, puuid).map_err(|e| anyhow!("ranking chart: {e}"))?;

    root.present().map_err(|e| anyhow!("chart present: {e}"))?;
    Ok(())
}

fn caption(size: i32) -> TextStyle<'static> {
    ("sans-serif", size).into_font().color(&FG)
}

/// Gold lead over time, from the tracked player's team's perspective.
fn draw_gold(area: &Area, game: &Match, frames_jsonl: &str, puuid: &str) -> Drawn {
    let player = game
        .info
        .participants
        .iter()
        .find(|p| p.puuid == puuid)
        .ok_or_else(|| "player not found".to_string())?;
    let player_team: Team = player.team_id;
    let team_of: std::collections::HashMap<i32, Team> = game
        .info
        .participants
        .iter()
        .map(|p| (p.participant_id, p.team_id))
        .collect();

    let mut pts: Vec<(f64, f64)> = Vec::new();
    for line in frames_jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line)?;
        let minute = v["timestamp"].as_f64().unwrap_or(0.0) / 60_000.0;
        let (mut mine, mut theirs) = (0i64, 0i64);
        if let Some(arr) = v["participants"].as_array() {
            for p in arr {
                let pid = p["participantId"].as_i64().unwrap_or(0) as i32;
                let gold = p["totalGold"].as_i64().unwrap_or(0);
                if team_of.get(&pid) == Some(&player_team) {
                    mine += gold;
                } else {
                    theirs += gold;
                }
            }
        }
        pts.push((minute, (mine - theirs) as f64));
    }

    let max_min = pts.last().map(|p| p.0).unwrap_or(1.0).max(1.0);
    let max_abs = pts.iter().map(|p| p.1.abs()).fold(1500.0_f64, f64::max);
    let name = player.riot_id_game_name.clone().unwrap_or_else(|| "player".into());

    let mut chart = ChartBuilder::on(area)
        .caption(format!("Gold Lead — {name}'s team"), caption(24))
        .margin(16)
        .x_label_area_size(34)
        .y_label_area_size(70)
        .build_cartesian_2d(0f64..max_min, -max_abs..max_abs)?;
    chart
        .configure_mesh()
        .label_style(caption(13))
        .axis_style(GRID)
        .bold_line_style(GRID)
        .light_line_style(BG)
        .axis_desc_style(caption(14))
        .x_desc("Minute")
        .y_desc("Gold lead")
        .y_label_formatter(&|v| format!("{:+.0}k", v / 1000.0))
        .draw()?;
    // Zero baseline.
    chart.draw_series(LineSeries::new(
        [(0.0, 0.0), (max_min, 0.0)],
        ShapeStyle::from(GRID).stroke_width(1),
    ))?;
    chart.draw_series(LineSeries::new(
        pts,
        ShapeStyle::from(GREEN).stroke_width(3),
    ))?;
    Ok(())
}

/// Damage to champions across all 10 players; tracked player highlighted.
fn draw_damage(area: &Area, game: &Match, puuid: &str) -> Drawn {
    let mut rows: Vec<(String, i64, bool)> = game
        .info
        .participants
        .iter()
        .map(|p| {
            (
                p.champion_name.clone(),
                p.total_damage_dealt_to_champions as i64,
                p.puuid == puuid,
            )
        })
        .collect();
    rows.sort_by_key(|r| r.1); // ascending -> biggest bar on top
    let n = rows.len() as i32;
    let max_dmg = rows.iter().map(|r| r.1).max().unwrap_or(1).max(1) as f64;

    let mut chart = ChartBuilder::on(area)
        .caption("Damage to Champions", caption(22))
        .margin(14)
        .x_label_area_size(34)
        .y_label_area_size(96)
        .build_cartesian_2d(0f64..(max_dmg * 1.14), (0..n).into_segmented())?;
    chart
        .configure_mesh()
        .disable_y_mesh()
        .label_style(caption(13))
        .axis_style(GRID)
        .bold_line_style(BG)
        .light_line_style(BG)
        .axis_desc_style(caption(13))
        .x_desc("Damage")
        .x_label_formatter(&|v| format!("{:.0}k", v / 1000.0))
        .y_label_formatter(&|y| match y {
            SegmentValue::CenterOf(v) => rows.get(*v as usize).map(|r| r.0.clone()).unwrap_or_default(),
            _ => String::new(),
        })
        .draw()?;
    for (i, (_, dmg, target)) in rows.iter().enumerate() {
        let color = if *target { GREEN } else { MUTED };
        chart.draw_series(std::iter::once(Rectangle::new(
            [
                (0f64, SegmentValue::Exact(i as i32)),
                (*dmg as f64, SegmentValue::Exact(i as i32 + 1)),
            ],
            color.filled(),
        )))?;
    }
    Ok(())
}

/// Where the tracked player ranks in the lobby across four metrics.
fn draw_ranking(area: &Area, game: &Match, puuid: &str) -> Drawn {
    let parts = &game.info.participants;
    let player = parts
        .iter()
        .find(|p| p.puuid == puuid)
        .ok_or_else(|| "player not found".to_string())?;

    let dmg = |p: &Participant| p.total_damage_dealt_to_champions as f64;
    let gold = |p: &Participant| p.gold_earned as f64;
    let kda = |p: &Participant| (p.kills + p.assists) as f64 / p.deaths.max(1) as f64;
    let vis = |p: &Participant| p.vision_score as f64;
    let rank = |val: f64, f: &dyn Fn(&Participant) -> f64| -> i32 {
        1 + parts.iter().filter(|p| f(p) > val).count() as i32
    };

    let metrics = [
        ("Damage", rank(dmg(player), &dmg)),
        ("Gold", rank(gold(player), &gold)),
        ("KDA", rank(kda(player), &kda)),
        ("Vision", rank(vis(player), &vis)),
    ];
    let total = parts.len() as i32;
    let n = metrics.len() as i32;

    let mut chart = ChartBuilder::on(area)
        .caption(format!("Lobby Ranking (of {total})"), caption(22))
        .margin(14)
        .x_label_area_size(34)
        .y_label_area_size(52)
        .build_cartesian_2d((0..n).into_segmented(), 0f64..(total as f64 - 0.5))?;
    chart
        .configure_mesh()
        .disable_x_mesh()
        .label_style(caption(13))
        .axis_style(GRID)
        .bold_line_style(GRID)
        .light_line_style(BG)
        .axis_desc_style(caption(13))
        .y_desc("Players outperformed")
        .x_label_formatter(&|x| match x {
            SegmentValue::CenterOf(v) => metrics.get(*v as usize).map(|m| m.0.to_string()).unwrap_or_default(),
            _ => String::new(),
        })
        .draw()?;
    for (i, (_, r)) in metrics.iter().enumerate() {
        let beaten = (total - r) as f64;
        chart.draw_series(std::iter::once(Rectangle::new(
            [
                (SegmentValue::Exact(i as i32), 0f64),
                (SegmentValue::Exact(i as i32 + 1), beaten),
            ],
            BLURPLE.filled(),
        )))?;
        // Background-colored outline separates touching bars.
        chart.draw_series(std::iter::once(Rectangle::new(
            [
                (SegmentValue::Exact(i as i32), 0f64),
                (SegmentValue::Exact(i as i32 + 1), beaten),
            ],
            ShapeStyle::from(BG).stroke_width(3),
        )))?;
        chart.draw_series(std::iter::once(Text::new(
            format!("#{r}"),
            (SegmentValue::CenterOf(i as i32), beaten + 0.15),
            caption(17),
        )))?;
    }
    Ok(())
}
