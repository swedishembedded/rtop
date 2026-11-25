// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Small building blocks: bars, graphs, panels and the generic table.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use rtop_app::state::{Mode, TableState};
use rtop_app::view::{Panel, RowId, SortKey};
use rtop_app::{AppState, Hit};
use rtop_core::SystemGraph;
use unicode_width::UnicodeWidthStr;

use crate::fmt::truncate;
use crate::hits::HitMap;
use crate::theme;

const EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// How a bar or graph is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// One colour, for quantities that are not a load (rates, categories).
    Solid(Color),
    /// Load lanes by position along the scale ([`theme::level`]): a
    /// full bar or graph shows green, then yellow, then red.
    Lanes,
}

impl Fill {
    /// Colour of the cell covering the share `from..to` (0–1) of the scale
    /// for a value of `value` (0–1). Lanes are judged at the cell's middle,
    /// or at the value when that is lower: a cell never shows a lane above
    /// the value it draws.
    fn at(self, from: f64, to: f64, value: f64) -> Color {
        match self {
            Fill::Solid(c) => c,
            Fill::Lanes => theme::level(100.0 * (0.5 * (from + to)).min(value)),
        }
    }
}

/// Horizontal bar of `width` cells filled to `pct` (0–100).
pub fn bar(pct: Option<f64>, width: usize, fill: Fill) -> Vec<Span<'static>> {
    let Some(p) = pct.filter(|p| p.is_finite()) else {
        return vec![Span::styled("·".repeat(width), theme::faint())];
    };
    let eighths = ((p.clamp(0.0, 100.0) / 100.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let value = p.clamp(0.0, 100.0) / 100.0;
    let cell = |i: usize| {
        fill.at(
            i as f64 / width as f64,
            (i + 1) as f64 / width as f64,
            value,
        )
    };
    // One span per run of equal colour.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |ch: char, color: Color| match spans.last_mut() {
        Some(last) if last.style.fg == Some(color) => last.content.to_mut().push(ch),
        _ => spans.push(Span::styled(ch.to_string(), Style::default().fg(color))),
    };
    for i in 0..full.min(width) {
        push('█', cell(i));
    }
    if full < width && !eighths.is_multiple_of(8) {
        push(EIGHTHS[eighths % 8], cell(full));
    }
    let drawn: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let rest = width.saturating_sub(drawn);
    if rest > 0 {
        spans.push(Span::styled(
            "·".repeat(rest),
            Style::default().fg(theme::FAINT),
        ));
    }
    spans
}

/// The characters of [`bar`], for table cells drawn in one style.
pub fn bar_text(pct: Option<f64>, width: usize) -> String {
    bar(pct, width, Fill::Lanes)
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// One-row sparkline of the last `width` samples; gaps stay blank.
pub fn sparkline(samples: &[Option<f64>], width: usize, max: f64, color: Color) -> Span<'static> {
    let start = samples.len().saturating_sub(width);
    let mut s: String =
        std::iter::repeat_n(' ', width.saturating_sub(samples.len() - start)).collect();
    for v in &samples[start..] {
        s.push(match v {
            Some(v) if max > 0.0 => LEVELS[((v / max).clamp(0.0, 1.0) * 8.0).round() as usize],
            Some(_) => LEVELS[0],
            None => ' ',
        });
    }
    Span::styled(s, Style::default().fg(color))
}

/// Multi-row area graph filling `area`, newest sample at the right.
pub fn graph(buf: &mut Buffer, area: Rect, samples: &[Option<f64>], max: f64, fill: Fill) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let w = area.width as usize;
    let start = samples.len().saturating_sub(w);
    let pad = w - (samples.len() - start);
    let h8 = f64::from(area.height) * 8.0;
    for (i, v) in samples[start..].iter().enumerate() {
        let x = area.x + (pad + i) as u16;
        let Some(v) = v else { continue };
        let filled = if max > 0.0 {
            ((v / max).clamp(0.0, 1.0) * h8).round() as u16
        } else {
            0
        };
        for row in 0..area.height {
            let level = filled.saturating_sub(row * 8).min(8);
            if level == 0 {
                break;
            }
            let y = area.y + area.height - 1 - row;
            let h = f64::from(area.height);
            let color = fill.at(
                f64::from(row) / h,
                f64::from(row + 1) / h,
                f64::from(filled) / h8,
            );
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(LEVELS[level as usize]).set_fg(color);
            }
        }
    }
}

pub fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused {
            theme::BORDER_FOCUS
        } else {
            theme::BORDER
        }))
        .title(title.into())
        .style(Style::default().bg(theme::PANEL).fg(theme::TEXT))
}

/// Draws `block` and returns its inner area.
pub fn framed(f: &mut Frame, area: Rect, block: Block<'static>) -> Rect {
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// A table column.
#[allow(missing_debug_implementations)]
pub struct Column<'a> {
    pub title: &'static str,
    pub width: u16,
    /// Takes remaining width.
    pub flex: bool,
    pub right: bool,
    /// Lower is dropped first when space is short (0 = never dropped).
    pub priority: u8,
    pub sort: Option<SortKey>,
    #[allow(clippy::type_complexity)]
    pub cell: Box<dyn Fn(&RowId, usize, usize) -> (String, Style) + 'a>,
}

impl<'a> Column<'a> {
    pub fn new(
        title: &'static str,
        width: u16,
        cell: impl Fn(&RowId, usize, usize) -> (String, Style) + 'a,
    ) -> Self {
        Column {
            title,
            width,
            flex: false,
            right: false,
            priority: 5,
            sort: None,
            cell: Box::new(cell),
        }
    }
    pub fn flex(mut self) -> Self {
        self.flex = true;
        self.priority = 0;
        self
    }
    pub fn right(mut self) -> Self {
        self.right = true;
        self
    }
    pub fn prio(mut self, p: u8) -> Self {
        self.priority = p;
        self
    }
    pub fn sort(mut self, k: SortKey) -> Self {
        self.sort = Some(k);
        self
    }
}

/// Fits columns into `width`: drops low-priority columns first and gives
/// the rest to flexible ones.
fn fit(cols: &[Column<'_>], width: u16) -> Vec<(usize, u16)> {
    let mut keep: Vec<usize> = (0..cols.len()).collect();
    let need = |keep: &[usize]| keep.iter().map(|&i| cols[i].width + 1).sum::<u16>();
    while need(&keep) > width {
        let Some((pos, _)) = keep
            .iter()
            .enumerate()
            .filter(|(_, i)| cols[**i].priority > 0)
            .min_by_key(|(_, i)| (cols[**i].priority, std::cmp::Reverse(**i)))
        else {
            break;
        };
        keep.remove(pos);
    }
    let spare = width.saturating_sub(need(&keep));
    let flex = keep.iter().filter(|&&i| cols[i].flex).count().max(1) as u16;
    keep.iter()
        .map(|&i| {
            (
                i,
                if cols[i].flex {
                    cols[i].width + spare / flex
                } else {
                    cols[i].width
                },
            )
        })
        .collect()
}

fn pad(s: &str, w: usize, right: bool) -> String {
    let t = truncate(s, w);
    let gap = w.saturating_sub(t.width());
    if right {
        format!("{}{t}", " ".repeat(gap))
    } else {
        format!("{t}{}", " ".repeat(gap))
    }
}

/// Draws the table of panel `idx` of the current location.
#[allow(clippy::too_many_arguments)]
pub fn table(
    f: &mut Frame,
    area: Rect,
    s: &AppState,
    _g: &SystemGraph,
    idx: usize,
    cols: &[Column<'_>],
    hits: &mut HitMap,
    empty: &str,
) {
    let Some(t) = s.location.tables.get(idx) else {
        return;
    };
    let rows = s.panel_rows(idx);
    let focused = s.location.focus == idx;
    let title = table_title(t, rows.len(), focused, s);
    let inner = framed(f, area, panel(title, focused));
    hits.panel(area, t.panel);
    hits.push(Rect { height: 1, ..area }, Hit::Panel(t.panel));
    if inner.height < 2 || inner.width < 4 {
        return;
    }
    let layout = fit(cols, inner.width);
    // Header.
    let mut x = inner.x;
    let mut header = Vec::new();
    for &(i, w) in &layout {
        let c = &cols[i];
        let active = c.sort.is_some_and(|k| k == t.sort);
        let arrow = if active {
            if t.descending { "↓" } else { "↑" }
        } else {
            ""
        };
        let label = format!("{}{arrow}", c.title);
        let style = if active {
            theme::accent()
        } else {
            theme::dim().add_modifier(Modifier::BOLD)
        };
        header.push(Span::styled(pad(&label, w as usize, c.right), style));
        header.push(Span::raw(" "));
        if let Some(k) = c.sort {
            hits.push(
                Rect {
                    x,
                    y: inner.y,
                    width: w,
                    height: 1,
                },
                Hit::Header {
                    panel: t.panel,
                    sort: k,
                },
            );
        }
        x += w + 1;
    }
    f.render_widget(
        Paragraph::new(Line::from(header)),
        Rect { height: 1, ..inner },
    );

    let body = Rect {
        y: inner.y + 1,
        height: inner.height - 1,
        ..inner
    };
    let visible = body.height as usize;
    t.viewport.set(visible);
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(empty.to_owned(), theme::faint())),
            body,
        );
        return;
    }
    // Without a selection the view stays where the user left it.
    let selected_pos = t.selected.as_ref().and_then(|r| rows.position(r));
    let cursor = selected_pos.unwrap_or(usize::MAX);
    let mut offset = t.offset.get().min(rows.len().saturating_sub(1));
    if let Some(cursor) = selected_pos {
        if cursor < offset {
            offset = cursor;
        } else if cursor >= offset + visible {
            offset = cursor + 1 - visible;
        }
    }
    offset = offset.min(rows.len().saturating_sub(visible));
    t.offset.set(offset);
    let range = match (&s.mode, focused) {
        (Mode::Visual { anchor }, true) => rows
            .position(anchor)
            .map(|a| (a.min(cursor), a.max(cursor))),
        _ => None,
    };
    for (line, i) in (offset..rows.len().min(offset + visible)).enumerate() {
        let row = &rows.ids[i];
        let y = body.y + line as u16;
        let selected = i == cursor;
        let marked = s.selection.contains(row) || range.is_some_and(|(a, b)| i >= a && i <= b);
        let bg = if selected && focused {
            theme::SELECT_BG
        } else if marked {
            theme::MARK_BG
        } else {
            theme::PANEL
        };
        let mut spans = Vec::new();
        for &(ci, w) in &layout {
            let (text, style) = (cols[ci].cell)(
                row,
                rows.depth.get(i).copied().unwrap_or(0) as usize,
                w as usize,
            );
            spans.push(Span::styled(
                pad(&text, w as usize, cols[ci].right),
                style.bg(bg),
            ));
            spans.push(Span::styled(" ", Style::default().bg(bg)));
        }
        let r = Rect {
            y,
            height: 1,
            ..body
        };
        let mut l = Line::from(spans);
        if selected && !focused {
            l = l.style(Style::default().add_modifier(Modifier::UNDERLINED));
        }
        f.render_widget(Paragraph::new(l).style(Style::default().bg(bg)), r);
        hits.push(
            r,
            Hit::Row {
                panel: t.panel,
                row: row.clone(),
            },
        );
    }
    // Scroll position.
    if rows.len() > visible && inner.width > 10 {
        let txt = match selected_pos {
            Some(c) => format!(" {}/{} ", c + 1, rows.len()),
            None => format!(
                " {}-{}/{} ",
                offset + 1,
                (offset + visible).min(rows.len()),
                rows.len()
            ),
        };
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

fn table_title(t: &TableState, n: usize, focused: bool, s: &AppState) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!(" {} ", t.panel.title()),
            if focused {
                theme::accent()
            } else {
                theme::title()
            },
        ),
        Span::styled(format!("{n} "), theme::dim()),
    ];
    if !t.filter.is_empty() {
        spans.push(Span::styled(
            format!("filter: {} ", t.filter.source()),
            Style::default().fg(theme::WARN),
        ));
    }
    if t.panel == Panel::Processes {
        spans.push(Span::styled("100% = one CPU ", theme::faint()));
    }
    if t.panel == Panel::Processes && s.tree && t.filter.is_empty() {
        spans.push(Span::styled("tree ", Style::default().fg(theme::CYAN)));
    }
    if !s.selection.is_empty() && focused {
        spans.push(Span::styled(
            format!("{} selected ", s.selection.len()),
            Style::default().fg(theme::WARN),
        ));
    }
    Line::from(spans)
}

/// `label value` pairs on one line, separated by dim dots.
pub fn kv_line(pairs: Vec<(&str, Span<'static>)>) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::faint()));
        }
        spans.push(Span::styled(format!("{k} "), theme::dim()));
        spans.push(v);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load scale: the lower half green, the next 30 % yellow, the top
    /// 20 % red.
    #[test]
    fn load_lanes_are_half_green_then_yellow_then_red() {
        assert_eq!(theme::level(0.0), theme::GOOD);
        assert_eq!(theme::level(49.9), theme::GOOD);
        assert_eq!(theme::level(50.0), theme::WARN);
        assert_eq!(theme::level(79.9), theme::WARN);
        assert_eq!(theme::level(80.0), theme::BAD);
        assert_eq!(theme::level(100.0), theme::BAD);
    }

    #[test]
    fn a_full_graph_shows_the_three_lanes_by_height() {
        let area = Rect::new(0, 0, 1, 10);
        let mut buf = Buffer::empty(area);
        graph(&mut buf, area, &[Some(100.0)], 100.0, Fill::Lanes);
        let colors: Vec<Color> = (0..10).rev().map(|y| buf[(0, y)].fg).collect();
        let lanes = [
            [theme::GOOD; 5].as_slice(),
            &[theme::WARN; 3],
            &[theme::BAD; 2],
        ]
        .concat();
        assert_eq!(colors, lanes, "bottom to top");

        // A value never shows a lane above its own: 47 % tops out in a cell
        // whose middle is in the yellow lane, but stays green.
        let short = Rect::new(0, 0, 1, 5);
        let mut buf = Buffer::empty(short);
        graph(&mut buf, short, &[Some(47.0)], 100.0, Fill::Lanes);
        assert_eq!(buf[(0, 2)].fg, theme::GOOD, "{:?}", buf[(0, 2)]);
        let b = bar(Some(47.0), 5, Fill::Lanes);
        assert!(
            b.iter()
                .filter(|s| !s.content.contains('·'))
                .all(|s| s.style.fg == Some(theme::GOOD))
        );

        // A low sample never reaches the upper lanes; one colour stays one.
        let mut buf = Buffer::empty(area);
        graph(&mut buf, area, &[Some(30.0)], 100.0, Fill::Lanes);
        assert_eq!(buf[(0, 9)].fg, theme::GOOD);
        assert_eq!(buf[(0, 6)].symbol(), " ");
        let mut buf = Buffer::empty(area);
        graph(
            &mut buf,
            area,
            &[Some(100.0)],
            100.0,
            Fill::Solid(theme::CYAN),
        );
        assert!((0..10).all(|y| buf[(0, y)].fg == theme::CYAN));
    }

    #[test]
    fn a_bar_is_coloured_by_position_along_it() {
        let colors = |pct| -> Vec<(char, Color)> {
            bar(Some(pct), 10, Fill::Lanes)
                .iter()
                .flat_map(|s| s.content.chars().map(move |c| (c, s.style.fg.unwrap())))
                .collect()
        };
        let full = colors(100.0);
        let lanes = [
            [theme::GOOD; 5].as_slice(),
            &[theme::WARN; 3],
            &[theme::BAD; 2],
        ]
        .concat();
        assert_eq!(full.iter().map(|(_, c)| *c).collect::<Vec<_>>(), lanes);
        // 30 %: three green cells, the rest is the faint track.
        let low = colors(30.0);
        assert!(
            low[..3]
                .iter()
                .all(|(ch, c)| *ch == '█' && *c == theme::GOOD)
        );
        assert!(low[3..].iter().all(|(ch, _)| *ch == '·'));
        assert_eq!(low.len(), 10);
    }
}
