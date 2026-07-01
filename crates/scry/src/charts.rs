//! Render a single dark-themed "dashboard" PNG (gold lead + damage + lobby
//! ranking) from the archived match data, styled for a Discord embed.
//!
//! Everything is drawn at `SS`× resolution and downscaled with a Lanczos
//! filter, which anti-aliases plotters' otherwise-aliased lines and edges.

use std::error::Error;
use std::path::Path;
use std::sync::Once;

use anyhow::{Result, anyhow};
use plotters::coord::{CoordTranslate, Shift};
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
use plotters::style::{FontStyle, register_font};
use riven::consts::Team;
use riven::models::match_v5::{Match, Participant};
use serde_json::Value;

/// Logical (post-downscale) canvas.
const WIDTH: u32 = 1000;
const HEIGHT: u32 = 940;
const TOP_H: u32 = 420;
/// Supersampling factor — render this many times larger, then downscale.
const SS: u32 = 2;

// Discord-ish palette.
const BG: RGBColor = RGBColor(43, 45, 49); // #2B2D31
const FG: RGBColor = RGBColor(219, 222, 225); // near-white text
const GRID: RGBColor = RGBColor(70, 74, 81); // subtle axis/lines
const GREEN: RGBColor = RGBColor(87, 242, 135); // #57F287 (tracked player)
const ALLY: RGBColor = RGBColor(59, 130, 246); // #3B82F6 blue (your team)
const ENEMY: RGBColor = RGBColor(237, 66, 69); // #ED4245 red (enemy team)

type Area<'a> = DrawingArea<BitMapBackend<'a>, Shift>;
type Drawn = std::result::Result<(), Box<dyn Error>>;

// Share Tech Mono (SIL OFL 1.1) — embedded so rendering is host-independent.
// License at crates/scry/assets/fonts/OFL.txt.
const FONT: &str = "sans-serif";
const FONT_MAIN: &[u8] = include_bytes!("../assets/fonts/ShareTechMono-Regular.ttf");
static FONTS: Once = Once::new();

/// Register the embedded font (as both Normal and Bold) with plotters.
fn init_fonts() {
    FONTS.call_once(|| {
        let n = register_font(FONT, FontStyle::Normal, FONT_MAIN).is_ok();
        let b = register_font(FONT, FontStyle::Bold, FONT_MAIN).is_ok();
        if !(n && b) {
            eprintln!("scry: embedded font failed to register");
        }
    });
}

/// A pixel measurement scaled up for supersampling.
fn px(n: u32) -> u32 {
    n * SS
}

/// Insert spaces at camelCase boundaries: "LeeSin" -> "Lee Sin".
fn spaced_name(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if prev_lower && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
        prev_lower = c.is_lowercase();
    }
    out
}

/// A text style at the given (logical) point size, scaled for supersampling.
fn caption(size: u32) -> TextStyle<'static> {
    ("sans-serif", (size * SS) as i32).into_font().color(&FG)
}

/// Top strip (logical px) reserved in each panel for a manually-drawn title.
const TITLE_STRIP: u32 = 44;

/// Draw a panel title horizontally centered at `center_x` (a local pixel x).
fn draw_title(area: &Area, title: &str, size: u32, center_x: i32) -> Drawn {
    let style = (FONT, (size * SS) as i32)
        .into_font()
        .color(&FG)
        .pos(Pos::new(HPos::Center, VPos::Top));
    let y = px(10) as i32;
    // Faux-bold: Share Tech Mono ships one weight, so overdraw a 1px box smear.
    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        area.draw(&Text::new(
            title.to_string(),
            (center_x + dx, y + dy),
            style.clone(),
        ))?;
    }
    Ok(())
}

/// Horizontal center of the chart's plot region, in the panel's local pixels.
fn plot_center_x<DB: DrawingBackend, CT: CoordTranslate>(
    area: &DrawingArea<DB, Shift>,
    chart: &ChartContext<DB, CT>,
) -> i32 {
    let (area_x, _) = area.get_pixel_range();
    let (plot_x, _) = chart.plotting_area().get_pixel_range();
    (plot_x.start + plot_x.end) / 2 - area_x.start
}

/// Render the composite dashboard to `out` as one anti-aliased PNG.
pub fn dashboard(game: &Match, frames_jsonl: &str, puuid: &str, out: &Path) -> Result<()> {
    init_fonts();
    let (w, h) = (WIDTH * SS, HEIGHT * SS);
    let mut buf = vec![0u8; (w * h * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buf, (w, h)).into_drawing_area();
        root.fill(&BG).map_err(|e| anyhow!("chart fill: {e}"))?;
        let canvas = root.margin(px(20), 0, 0, 0); // extra breathing room up top

        let (top, bottom) = canvas.split_vertically(TOP_H * SS);
        // Bottom row with equal padding: [pad][damage][pad][ranking][pad].
        let pad = w / 24;
        let chart_w = (w - 3 * pad) / 2;
        let (_, rest) = bottom.split_horizontally(pad);
        let (bottom_left, rest) = rest.split_horizontally(chart_w);
        let (_, rest) = rest.split_horizontally(pad);
        let (bottom_right, _) = rest.split_horizontally(chart_w);

        draw_gold(&top, game, frames_jsonl, puuid).map_err(|e| anyhow!("gold chart: {e}"))?;
        draw_damage(&bottom_left, game, puuid).map_err(|e| anyhow!("damage chart: {e}"))?;
        draw_matchup(&bottom_right, game, puuid).map_err(|e| anyhow!("matchup chart: {e}"))?;

        root.present().map_err(|e| anyhow!("chart present: {e}"))?;
    }

    let img = image::RgbImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("bad chart buffer"))?;
    let scaled = image::imageops::resize(&img, WIDTH, HEIGHT, image::imageops::FilterType::Lanczos3);
    scaled.save(out).map_err(|e| anyhow!("saving {}: {e}", out.display()))?;
    Ok(())
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

    let mut chart = ChartBuilder::on(area)
        .margin(px(22))
        .margin_top(px(TITLE_STRIP))
        .margin_right(px(80))
        .x_label_area_size(px(50))
        .y_label_area_size(px(58))
        .build_cartesian_2d(0f64..max_min, -max_abs..max_abs)?;
    let title_x = plot_center_x(area, &chart);
    chart
        .configure_mesh()
        .label_style(caption(13))
        .axis_style(GRID)
        .bold_line_style(GRID)
        .light_line_style(BG)
        .x_label_formatter(&|v| format!("{v:.0}"))
        .y_label_formatter(&|v| format!("{:+.0}k", v / 1000.0))
        .draw()?;
    chart.draw_series(LineSeries::new(
        [(0.0, 0.0), (max_min, 0.0)],
        ShapeStyle::from(GRID).stroke_width(SS),
    ))?;
    chart.draw_series(LineSeries::new(
        pts,
        ShapeStyle::from(GREEN).stroke_width(px(3)),
    ))?;
    draw_title(area, "Gold Lead", 24, title_x)?;
    Ok(())
}

/// Damage to champions across all 10 players. Bars are colored by team (blue =
/// your team, red = enemy); the tracked player's bar gets a green outline.
fn draw_damage(area: &Area, game: &Match, puuid: &str) -> Drawn {
    let player_team = game
        .info
        .participants
        .iter()
        .find(|p| p.puuid == puuid)
        .map(|p| p.team_id)
        .ok_or_else(|| "player not found".to_string())?;
    // (champion, damage, is_tracked_player, is_ally)
    let mut rows: Vec<(String, i64, bool, bool)> = game
        .info
        .participants
        .iter()
        .map(|p| {
            (
                spaced_name(&p.champion_name),
                p.total_damage_dealt_to_champions as i64,
                p.puuid == puuid,
                p.team_id == player_team,
            )
        })
        .collect();
    rows.sort_by_key(|r| r.1); // ascending -> biggest bar on top
    let n = rows.len() as i32;
    let max_dmg = rows.iter().map(|r| r.1).max().unwrap_or(1).max(1) as f64;

    let mut chart = ChartBuilder::on(area)
        .margin(px(20))
        .margin_top(px(TITLE_STRIP))
        .margin_left(px(0))
        .margin_right(px(0))
        .x_label_area_size(px(50))
        .y_label_area_size(px(116))
        .build_cartesian_2d(0f64..(max_dmg * 1.14), (0..n).into_segmented())?;
    let title_x = plot_center_x(area, &chart);
    chart
        .configure_mesh()
        .disable_y_mesh()
        .label_style(caption(13))
        .axis_style(GRID)
        .bold_line_style(BG)
        .light_line_style(BG)
        .x_label_formatter(&|v| format!("{:.0}k", v / 1000.0))
        .y_label_formatter(&|y| match y {
            SegmentValue::CenterOf(v) => rows.get(*v as usize).map(|r| r.0.clone()).unwrap_or_default(),
            _ => String::new(),
        })
        .draw()?;
    for (i, (_, dmg, target, ally)) in rows.iter().enumerate() {
        let color = if *target {
            GREEN
        } else if *ally {
            ALLY
        } else {
            ENEMY
        };
        chart.draw_series(std::iter::once(Rectangle::new(
            [
                (0f64, SegmentValue::Exact(i as i32)),
                (*dmg as f64, SegmentValue::Exact(i as i32 + 1)),
            ],
            color.filled(),
        )))?;
        // Thick background outline gives the bars visible spacing.
        chart.draw_series(std::iter::once(Rectangle::new(
            [
                (0f64, SegmentValue::Exact(i as i32)),
                (*dmg as f64, SegmentValue::Exact(i as i32 + 1)),
            ],
            ShapeStyle::from(BG).stroke_width(px(3)),
        )))?;
    }
    draw_title(area, "Damage to Champions", 22, title_x)?;
    Ok(())
}

/// Side-by-side comparison of the tracked player (green) vs their lane
/// opponent (red) across four metrics, each normalized to the pair's max so
/// the mixed units are comparable; real values are annotated on the bars.
fn draw_matchup(area: &Area, game: &Match, puuid: &str) -> Drawn {
    let parts = &game.info.participants;
    let player = parts
        .iter()
        .find(|p| p.puuid == puuid)
        .ok_or_else(|| "player not found".to_string())?;
    let opponent = parts
        .iter()
        .find(|p| p.team_id != player.team_id && p.team_position == player.team_position)
        .or_else(|| parts.iter().find(|p| p.team_id != player.team_id))
        .ok_or_else(|| "opponent not found".to_string())?;

    let kda = |p: &Participant| (p.kills + p.assists) as f64 / p.deaths.max(1) as f64;
    let fmt_k = |v: f64| {
        if v >= 1000.0 {
            format!("{:.1}k", v / 1000.0)
        } else {
            format!("{v:.0}")
        }
    };

    // (name, player value, opponent value, player label, opponent label)
    let metrics: [(&str, f64, f64, String, String); 4] = [
        (
            "Damage",
            player.total_damage_dealt_to_champions as f64,
            opponent.total_damage_dealt_to_champions as f64,
            fmt_k(player.total_damage_dealt_to_champions as f64),
            fmt_k(opponent.total_damage_dealt_to_champions as f64),
        ),
        (
            "Gold",
            player.gold_earned as f64,
            opponent.gold_earned as f64,
            fmt_k(player.gold_earned as f64),
            fmt_k(opponent.gold_earned as f64),
        ),
        (
            "KDA",
            kda(player),
            kda(opponent),
            format!("{:.1}", kda(player)),
            format!("{:.1}", kda(opponent)),
        ),
        (
            "Vision",
            player.vision_score as f64,
            opponent.vision_score as f64,
            format!("{}", player.vision_score),
            format!("{}", opponent.vision_score),
        ),
    ];
    let n = metrics.len() as i32;

    let mut chart = ChartBuilder::on(area)
        .margin(px(20))
        .margin_top(px(TITLE_STRIP))
        .margin_left(px(0))
        .margin_right(px(0))
        .x_label_area_size(px(50))
        .y_label_area_size(px(10))
        .build_cartesian_2d(-0.5f64..(n as f64 - 0.5), 0f64..1.28)?;
    let title_x = plot_center_x(area, &chart);
    chart
        .configure_mesh()
        .disable_x_mesh()
        .disable_y_mesh()
        .label_style(caption(13))
        .axis_style(GRID)
        .x_labels(metrics.len())
        .y_labels(0)
        .x_label_formatter(&|x| {
            let idx = x.round();
            if (idx - x).abs() < 1e-6 && idx >= 0.0 {
                metrics.get(idx as usize).map(|m| m.0.to_string()).unwrap_or_default()
            } else {
                String::new()
            }
        })
        .draw()?;

    let value_style = caption(12).pos(Pos::new(HPos::Center, VPos::Bottom));
    for (i, (_, pv, ov, plabel, olabel)) in metrics.iter().enumerate() {
        let max = pv.max(*ov).max(1.0);
        let (ph, oh) = (pv / max, ov / max);
        let x = i as f64;
        // player (green) on the left, opponent (red) on the right
        chart.draw_series(std::iter::once(Rectangle::new(
            [(x - 0.36, 0.0), (x - 0.02, ph)],
            GREEN.filled(),
        )))?;
        chart.draw_series(std::iter::once(Rectangle::new(
            [(x + 0.02, 0.0), (x + 0.36, oh)],
            ENEMY.filled(),
        )))?;
        chart.draw_series(std::iter::once(Text::new(
            plabel.clone(),
            (x - 0.19, ph + 0.03),
            value_style.clone(),
        )))?;
        chart.draw_series(std::iter::once(Text::new(
            olabel.clone(),
            (x + 0.19, oh + 0.03),
            value_style.clone(),
        )))?;
    }

    let opp = spaced_name(&opponent.champion_name);
    draw_title(area, &format!("You vs {opp}"), 20, title_x)?;
    Ok(())
}
