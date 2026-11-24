// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The per-CPU grid: every logical CPU visible at once.
//!
//! A saturated core that is scrolled out of view looks like a healthy
//! machine, so the grid packs CPUs into as many columns as the area allows,
//! in three tiers: full cells (name, core type, bar, percent, frequency),
//! compact cells (index, bar, percent), and a heat strip of one two-column
//! cell per CPU whose block height and colour show its load. Only when even
//! the heat strip cannot fit (thousands of CPUs in a small terminal) does it
//! scroll, keeping the selected CPU visible and showing its position.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use rtop_app::Hit;

use super::Ctx;
use crate::hits::HitMap;
use crate::theme;
use crate::widgets::{bar, framed, panel};

/// Minimum width of a full cell (`cpu127 P ▕bar▏ 100% 3.73G`).
pub const FULL_MIN: u16 = 24;
/// Minimum width of a compact cell (`127 ▕bar▏100`).
pub const COMPACT_MIN: u16 = 11;
const GAP: u16 = 1;

/// Width of a heat-strip cell (block + gap).
pub const MICRO_W: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Compact,
    /// Heat strip, row-major, prefixed by the index of each row's first CPU.
    Micro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridLayout {
    pub columns: u16,
    pub cell_width: u16,
    /// Rows needed to show every CPU.
    pub rows: u16,
    pub mode: Mode,
    /// Width of the row label (heat strip only).
    pub label_width: u16,
}

impl GridLayout {
    pub fn fits(&self, height: u16) -> bool {
        self.rows <= height
    }
}

/// Chooses the fewest columns that show all `n` CPUs within `width` x
/// `height` cells, preferring full cells; falls back to the densest compact
/// grid (which then scrolls).
pub fn layout(n: usize, width: u16, height: u16) -> GridLayout {
    let n = n.max(1) as u32;
    let height = height.max(1);
    for (mode, min) in [(Mode::Full, FULL_MIN), (Mode::Compact, COMPACT_MIN)] {
        let max_cols = ((width + GAP) / (min + GAP)).max(1);
        let need = n.div_ceil(u32::from(height)).max(1) as u16;
        if need <= max_cols {
            let columns = need;
            let cell_width = (width + GAP) / columns - GAP;
            return GridLayout {
                columns,
                cell_width,
                rows: n.div_ceil(u32::from(columns)) as u16,
                mode,
                label_width: 0,
            };
        }
    }
    let label_width = (n - 1).to_string().len() as u16 + 1;
    let columns = (width.saturating_sub(label_width) / MICRO_W).max(1);
    GridLayout {
        columns,
        cell_width: MICRO_W,
        rows: n.div_ceil(u32::from(columns)) as u16,
        mode: Mode::Micro,
        label_width,
    }
}

/// Rows the grid wants for `n` CPUs in `width` columns, given at most
/// `max_rows` rows of space.
pub fn wanted_rows(n: usize, width: u16, max_rows: u16) -> u16 {
    layout(n, width, max_rows).rows.min(max_rows.max(1))
}

/// Whether the grid shows full cells in this area.
pub fn full_cells(n: usize, width: u16, max_rows: u16) -> bool {
    let g = layout(n, width, max_rows);
    g.mode == Mode::Full && g.fits(max_rows)
}

pub fn render(f: &mut Frame, area: Rect, c: &Ctx<'_>, idx: usize, hits: &mut HitMap) {
    let Some(t) = c.s.location.tables.get(idx) else {
        return;
    };
    let rows = c.s.panel_rows(idx);
    let focused = c.s.location.focus == idx;
    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", t.panel.title()),
            if focused {
                theme::accent()
            } else {
                theme::title()
            },
        ),
        Span::styled(format!("{} ", rows.len()), theme::dim()),
    ]);
    let inner = framed(f, area, panel(title, focused));
    hits.panel(area, t.panel);
    if inner.width == 0 || inner.height == 0 || rows.is_empty() {
        return;
    }
    let g = layout(rows.len(), inner.width, inner.height);
    // usize::MAX: nothing selected; nothing highlighted, view not moved.
    let cursor = t
        .selected
        .as_ref()
        .and_then(|r| rows.position(r))
        .unwrap_or(usize::MAX);
    if g.mode == Mode::Micro {
        return micro(f, area, inner, c, idx, g, cursor, focused, hits);
    }
    // Column-major order keeps neighbouring CPU numbers in one column.
    let per_col = g.rows as usize;
    let visible_rows = inner.height as usize;
    let mut offset = t.offset.get();
    let cursor_row = if cursor == usize::MAX {
        offset
    } else {
        cursor % per_col
    };
    if cursor_row < offset {
        offset = cursor_row;
    } else if cursor_row >= offset + visible_rows {
        offset = cursor_row + 1 - visible_rows;
    }
    offset = offset.min(per_col.saturating_sub(visible_rows));
    t.offset.set(offset);
    t.viewport.set(visible_rows.min(per_col));
    let max_label = rows.len().saturating_sub(1).to_string().len();
    for (i, row) in rows.ids.iter().enumerate() {
        let (col, r) = (i / per_col, i % per_col);
        if r < offset || r >= offset + visible_rows {
            continue;
        }
        let x = inner.x + col as u16 * (g.cell_width + GAP);
        let y = inner.y + (r - offset) as u16;
        let cell = Rect {
            x,
            y,
            width: g.cell_width.min(inner.x + inner.width - x),
            height: 1,
        };
        let node = row.entity().and_then(|id| c.g.node(id));
        let idx_label = node
            .and_then(|n| n.attr("index"))
            .and_then(|a| a.as_u64())
            .map_or_else(|| "?".into(), |v| v.to_string());
        let pct = node.and_then(|n| n.f64("cpu_usage_percent"));
        let color = pct.map_or(theme::FAINT, theme::level);
        let selected = i == cursor && focused;
        let bg = if selected {
            theme::SELECT_BG
        } else {
            theme::PANEL
        };
        let pct_text = match pct {
            Some(p) if g.mode == Mode::Compact => format!("{:>3.0}", p.min(999.0)),
            Some(p) => format!("{:>5.1}%", p),
            None => {
                if g.mode == Mode::Compact {
                    "  …".into()
                } else {
                    "     …".into()
                }
            }
        };
        let mut spans = Vec::new();
        let used;
        if g.mode == Mode::Compact {
            let label = format!("{idx_label:>w$} ", w = max_label);
            used = label.chars().count() + pct_text.chars().count() + 1;
            spans.push(Span::styled(label, theme::dim()));
        } else {
            let label = format!("cpu{idx_label:<w$} ", w = max_label);
            let kind = match node.and_then(|n| n.text("core_type")) {
                Some("performance") => "P ",
                Some("efficiency") => "E ",
                _ => "  ",
            };
            let freq = if g.cell_width >= FULL_MIN + 7 {
                node.and_then(|n| n.f64("frequency_mhz"))
                    .map_or(String::new(), |m| format!(" {:.2}G", m / 1000.0))
            } else {
                String::new()
            };
            used = label.chars().count() + 2 + pct_text.chars().count() + 1 + freq.chars().count();
            spans.push(Span::styled(label, theme::dim()));
            spans.push(Span::styled(kind, Style::default().fg(theme::MAGENTA)));
            let w = (g.cell_width as usize).saturating_sub(used).max(1);
            spans.extend(bar(pct, w, color));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                pct_text,
                Style::default()
                    .fg(color)
                    .add_modifier(if pct.unwrap_or(0.0) >= 99.0 {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ));
            spans.push(Span::styled(freq, theme::dim()));
            f.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
                cell,
            );
            hits.push(
                cell,
                Hit::Row {
                    panel: t.panel,
                    row: row.clone(),
                },
            );
            continue;
        }
        let w = (g.cell_width as usize).saturating_sub(used).max(1);
        spans.extend(bar(pct, w, color));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pct_text, Style::default().fg(color)));
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
            cell,
        );
        hits.push(
            cell,
            Hit::Row {
                panel: t.panel,
                row: row.clone(),
            },
        );
    }
    if per_col > visible_rows {
        let txt = format!(
            " rows {}-{}/{} ",
            offset + 1,
            (offset + visible_rows).min(per_col),
            per_col
        );
        let w = txt.len() as u16;
        let r = Rect {
            x: area.x + area.width.saturating_sub(w + 2),
            y: area.y + area.height - 1,
            width: w,
            height: 1,
        };
        f.render_widget(Paragraph::new(Span::styled(txt, theme::dim())), r);
    }
}

/// Heat strip: one cell per CPU, row-major, rows labelled by first index.
#[allow(clippy::too_many_arguments)]
fn micro(
    f: &mut Frame,
    area: Rect,
    inner: Rect,
    c: &Ctx<'_>,
    idx: usize,
    g: GridLayout,
    cursor: usize,
    focused: bool,
    hits: &mut HitMap,
) {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let t = &c.s.location.tables[idx];
    let rows = c.s.panel_rows(idx);
    let per_row = g.columns as usize;
    let visible = inner.height as usize;
    let mut offset = t.offset.get();
    let cursor_row = if cursor == usize::MAX {
        offset
    } else {
        cursor / per_row
    };
    if cursor_row < offset {
        offset = cursor_row;
    } else if cursor_row >= offset + visible {
        offset = cursor_row + 1 - visible;
    }
    offset = offset.min((g.rows as usize).saturating_sub(visible));
    t.offset.set(offset);
    t.viewport.set(visible.min(g.rows as usize) * per_row);
    for line in 0..visible.min(g.rows as usize - offset) {
        let row = offset + line;
        let y = inner.y + line as u16;
        let first = row * per_row;
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{:>w$}", first, w = g.label_width as usize - 1),
                theme::dim(),
            )),
            Rect {
                x: inner.x,
                y,
                width: g.label_width,
                height: 1,
            },
        );
        for (k, rid) in rows.ids.iter().enumerate().skip(first).take(per_row) {
            let x = inner.x + g.label_width + ((k - first) as u16) * MICRO_W;
            let pct = rid
                .entity()
                .and_then(|id| c.g.node(id))
                .and_then(|n| n.f64("cpu_usage_percent"));
            let ch = match pct {
                Some(p) if p < 2.0 => '·',
                Some(p) => LEVELS[((p.clamp(0.0, 100.0) / 100.0) * 7.0).round() as usize],
                None => ' ',
            };
            let bg = if k == cursor && focused {
                theme::SELECT_BG
            } else {
                theme::PANEL
            };
            let cell = Rect {
                x,
                y,
                width: MICRO_W.min(inner.x + inner.width - x),
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    ch.to_string(),
                    Style::default().fg(pct.map_or(theme::FAINT, theme::level)),
                ))
                .style(Style::default().bg(bg)),
                cell,
            );
            hits.push(
                cell,
                Hit::Row {
                    panel: t.panel,
                    row: rid.clone(),
                },
            );
        }
    }
    // The selected CPU's exact value, and the hottest one, in the border.
    let label = |i: usize| {
        rows.ids
            .get(i)
            .and_then(|r| r.entity())
            .and_then(|id| c.g.node(id))
    };
    let mut info = String::new();
    if let Some(n) = label(cursor) {
        info.push_str(&format!(
            " cpu{} {} ",
            n.attr("index").and_then(|a| a.as_u64()).unwrap_or(0),
            n.f64("cpu_usage_percent")
                .map_or("…".into(), |v| format!("{v:.1}%"))
        ));
    }
    let hottest = rows
        .ids
        .iter()
        .filter_map(|r| r.entity().and_then(|id| c.g.node(id)))
        .max_by(|a, b| {
            a.f64("cpu_usage_percent")
                .unwrap_or(-1.0)
                .total_cmp(&b.f64("cpu_usage_percent").unwrap_or(-1.0))
        });
    if let Some(h) = hottest {
        info.push_str(&format!(
            "· max cpu{} {} ",
            h.attr("index").and_then(|a| a.as_u64()).unwrap_or(0),
            h.f64("cpu_usage_percent")
                .map_or("…".into(), |v| format!("{v:.0}%"))
        ));
    }
    if g.rows as usize > visible {
        info.push_str(&format!(
            "· rows {}-{}/{} ",
            offset + 1,
            (offset + visible).min(g.rows as usize),
            g.rows
        ));
    }
    let w = (info.chars().count() as u16).min(area.width.saturating_sub(4));
    f.render_widget(
        Paragraph::new(Span::styled(info, theme::dim())),
        Rect {
            x: area.x + area.width.saturating_sub(w + 2),
            y: area.y + area.height - 1,
            width: w,
            height: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn typical_machines_show_every_cpu_with_full_cells() {
        // 22 CPUs at 80x24 with ~10 rows: three full columns.
        let g = layout(22, 78, 10);
        assert!(
            g.fits(10) && g.mode == Mode::Full && g.columns == 3,
            "{g:?}"
        );
        // 16 CPUs fit in one column when there is height.
        assert_eq!(layout(16, 60, 20).columns, 1);
    }

    #[test]
    fn many_cpus_use_denser_tiers_and_scroll_only_at_absurd_counts() {
        // 64 CPUs in 120 columns x 12 rows: compact cells.
        assert_eq!(layout(64, 118, 12).mode, Mode::Compact);
        // 256 CPUs at 120 columns: a heat strip of a few rows, no scrolling.
        let g = layout(256, 118, 18);
        assert_eq!(g.mode, Mode::Micro);
        assert!(g.fits(18) && g.rows <= 5, "{g:?}");
        // 64 CPUs in a 50x16 terminal: still all visible.
        assert!(layout(64, 48, 6).fits(6));
        // Only absurd counts in tiny areas scroll.
        assert!(!layout(4096, 40, 4).fits(4));
    }

    proptest! {
        #[test]
        fn layout_never_overflows_width(n in 1usize..1024, w in 1u16..400, h in 1u16..120) {
            let g = layout(n, w, h);
            prop_assert!(g.columns >= 1);
            prop_assert!(u32::from(g.columns) * u32::from(g.rows) >= n as u32);
            let used = match g.mode {
                Mode::Micro => g.label_width + g.columns * g.cell_width,
                _ => g.columns * g.cell_width + (g.columns - 1) * GAP,
            };
            prop_assert!(used <= w.max(g.label_width + g.cell_width).max(g.cell_width));
            // When it fits at all, the fewest columns are used.
            if g.fits(h) && g.columns > 1 && g.mode != Mode::Micro {
                prop_assert!((n as u32).div_ceil(u32::from(g.columns - 1)) > u32::from(h));
            }
        }
    }
}
