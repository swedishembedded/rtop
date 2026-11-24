// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The frame: header with view tabs, the current view, status line, modals.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use rtop_app::state::Mode;
use rtop_app::view::View;
use rtop_app::{AppState, Hit};
use rtop_core::units::{format_bytes, format_duration_secs};
use rtop_core::{EntityId, SystemGraph};
use unicode_width::UnicodeWidthStr;

use crate::hits::HitMap;
use crate::theme;
use crate::views::{self, Ctx, rows};

/// Draws one frame and records its clickable regions in `hits`.
pub fn render(f: &mut Frame, s: &AppState, g: &SystemGraph, hits: &mut HitMap) {
    hits.clear();
    let area = f.area();
    f.render_widget(Block::default().style(theme::base()), area);
    if area.width < 20 || area.height < 6 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "rtop: enlarge terminal",
                theme::dim(),
            ))),
            area,
        );
        return;
    }
    let c = Ctx::new(s, g);
    let r = rows(
        area,
        [
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ],
    );
    header(f, r[0], &c, hits);
    views::body(f, r[1], &c, hits);
    status(f, r[2], &c);
    views::modals::render(f, area, s, hits);
}

fn header(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let s = c.s;
    let mut spans = vec![
        Span::styled(
            " rtop ",
            Style::default()
                .fg(theme::BG)
                .bg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    let host = c
        .host
        .and_then(|h| h.text("hostname"))
        .unwrap_or("?")
        .to_owned();
    let mut x = area.x + 7;
    let tabs_w: usize = View::ALL.iter().map(|v| v.title().len() + 4).sum();
    let compact = (area.width as usize) < tabs_w + 30;
    if s.location.inspect.is_some() {
        let back = Span::styled(" ← back ", theme::key());
        hits.push(
            Rect {
                x,
                y: area.y,
                width: 8,
                height: 1,
            },
            Hit::Back,
        );
        x += 9;
        spans.push(back);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{} › ", s.location.view.title()),
            theme::dim(),
        ));
        let label = s
            .location
            .inspect
            .as_ref()
            .map(|id| rtop_app::project::label_of(c.g, id))
            .unwrap_or_default();
        spans.push(Span::styled(label, theme::title()));
        let _ = x;
    } else {
        for v in View::ALL {
            let active = v == s.location.view;
            let label = if compact {
                format!("{}", v.index())
            } else {
                format!("{} {}", v.index(), v.title())
            };
            let w = label.width() as u16 + 2;
            let st = if active {
                Style::default()
                    .fg(theme::BG)
                    .bg(theme::TEXT)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme::dim()
            };
            spans.push(Span::styled(format!(" {label} "), st));
            hits.push(
                Rect {
                    x,
                    y: area.y,
                    width: w,
                    height: 1,
                },
                Hit::Tab(v),
            );
            x += w;
        }
    }
    let state = if s.emergency {
        Span::styled(
            " EMERGENCY ",
            Style::default()
                .fg(theme::BG)
                .bg(theme::BAD)
                .add_modifier(Modifier::BOLD),
        )
    } else if s.paused {
        Span::styled(
            " PAUSED ",
            Style::default()
                .fg(theme::BG)
                .bg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!(" LIVE {}s ", s.interval_scale),
            Style::default().fg(theme::GOOD),
        )
    };
    let up = c
        .h("uptime_seconds")
        .map(format_duration_secs)
        .unwrap_or_default();
    let right = vec![
        Span::styled(format!("{host} "), theme::title()),
        Span::styled(format!("up {up} "), theme::dim()),
        state,
    ];
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    let right_w: usize = right.iter().map(|s| s.content.width()).sum();
    if used + right_w < area.width as usize {
        spans.push(Span::raw(" ".repeat(area.width as usize - used - right_w)));
        spans.extend(right);
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::PANEL)),
        area,
    );
}

fn status(f: &mut Frame, area: Rect, c: &Ctx<'_>) {
    let s = c.s;
    let (badge, color) = match &s.mode {
        Mode::Normal if !s.pending_keys.is_empty() => (
            format!(" {} ", rtop_app::keymap::describe(&s.pending_keys)),
            theme::WARN,
        ),
        Mode::Normal => (" NORMAL ".into(), theme::ACCENT),
        Mode::Search { .. } => (" SEARCH ".into(), theme::GOOD),
        Mode::Filter { .. } => (" FILTER ".into(), theme::WARN),
        Mode::Command { .. } => (" COMMAND ".into(), theme::MAGENTA),
        Mode::Visual { .. } => (" VISUAL ".into(), theme::WARN),
    };
    let mut spans = vec![
        Span::styled(
            badge,
            Style::default()
                .fg(theme::BG)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    let prompt = match &s.mode {
        Mode::Search { backward, input } => {
            Some(format!("{}{input}", if *backward { '?' } else { '/' }))
        }
        Mode::Filter { input, .. } => Some(format!("filter: {input}")),
        Mode::Command { input } => Some(format!(":{input}")),
        _ => None,
    };
    if let Some(p) = prompt {
        spans.push(Span::styled(p, theme::title()));
        spans.push(Span::styled("▏", theme::accent()));
        if let Some(e) = &s.prompt_error {
            spans.push(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::BAD),
            ));
        }
    } else if let Some(n) = s.current_notice() {
        spans.push(Span::styled(
            n.text.clone(),
            Style::default().fg(if n.error { theme::BAD } else { theme::TEXT }),
        ));
    } else {
        let hints: &[(&str, &str)] = if s.location.inspect.is_some() {
            &[
                ("Enter", "follow"),
                ("Esc", "back"),
                ("x", "terminate"),
                ("X", "kill"),
                ("m", "menu"),
                ("F1", "help"),
            ]
        } else {
            &[
                ("0-9", "views"),
                ("hjkl", "move"),
                ("/", "search"),
                ("F", "filter"),
                ("Enter", "inspect"),
                ("s?", "sort"),
                ("x", "term"),
                ("X", "kill"),
                ("!", "emergency"),
                ("F1", "help"),
            ]
        };
        for (k, d) in hints {
            spans.push(Span::styled(format!(" {k} "), theme::key()));
            spans.push(Span::styled(format!(" {d}  "), theme::dim()));
        }
    }
    let me =
        c.g.node(&EntityId::host())
            .and_then(|h| {
                h.f64("rtop_rss_bytes").map(|rss| {
                    format!(
                        "rtop {} · {} ",
                        h.f64("rtop_cpu_percent")
                            .map_or("…".into(), |v| format!("{v:.1}%")),
                        format_bytes(rss)
                    )
                })
            })
            .unwrap_or_default();
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if used + me.width() < area.width as usize {
        spans.push(Span::raw(
            " ".repeat(area.width as usize - used - me.width()),
        ));
        spans.push(Span::styled(me, theme::faint()));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::PANEL)),
        area,
    );
}
