// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Colours and text styles.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(16, 18, 24);
pub const PANEL: Color = Color::Rgb(22, 25, 33);
pub const BORDER: Color = Color::Rgb(52, 58, 74);
pub const BORDER_FOCUS: Color = Color::Rgb(110, 160, 255);
pub const TEXT: Color = Color::Rgb(214, 219, 230);
pub const DIM: Color = Color::Rgb(122, 130, 148);
pub const FAINT: Color = Color::Rgb(78, 85, 102);
pub const ACCENT: Color = Color::Rgb(110, 160, 255);
pub const GOOD: Color = Color::Rgb(98, 204, 140);
pub const WARN: Color = Color::Rgb(240, 190, 80);
pub const BAD: Color = Color::Rgb(245, 98, 98);
pub const CYAN: Color = Color::Rgb(90, 200, 220);
pub const MAGENTA: Color = Color::Rgb(200, 130, 240);
pub const SELECT_BG: Color = Color::Rgb(38, 56, 92);
pub const MARK_BG: Color = Color::Rgb(70, 52, 30);

pub fn base() -> Style {
    Style::default().fg(TEXT).bg(BG)
}

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn title() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn key() -> Style {
    Style::default().fg(BG).bg(DIM).add_modifier(Modifier::BOLD)
}

/// The load scale for a share of capacity (0–100): green for the lower
/// half, yellow up to 80 %, red for the top 20 %. Graphs and bars use the
/// same lanes, so a value is drawn in the colour of the lane it is in.
pub fn level(pct: f64) -> Color {
    if pct >= 80.0 {
        BAD
    } else if pct >= 50.0 {
        WARN
    } else if pct >= 0.0 {
        GOOD
    } else {
        DIM
    }
}
