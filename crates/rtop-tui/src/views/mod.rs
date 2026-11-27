// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Layouts of the ten views and the inspection page.

pub mod columns;
pub mod cpugrid;
pub mod gpu;
pub mod inspect;
pub mod modals;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use rtop_app::insight::insights;
use rtop_app::view::View;
use rtop_app::{AppState, Hit, SplitId};
use rtop_core::units::{format_bytes, format_rate};
use rtop_core::{EntityId, EntityKind, Node, Severity, SystemGraph};

use crate::fmt::{self, Fmt};
use crate::hits::{Axis, HitMap, Splitter, boundary_at};
use crate::theme;
use crate::widgets::{self, Fill, framed, kv_line, panel, table};

#[allow(missing_debug_implementations)]
pub struct Ctx<'a> {
    pub s: &'a AppState,
    pub g: &'a SystemGraph,
    pub host: Option<&'a Node>,
    pub ncpu: f64,
}

impl<'a> Ctx<'a> {
    pub fn new(s: &'a AppState, g: &'a SystemGraph) -> Self {
        let host = g.node(&EntityId::host());
        Ctx {
            s,
            g,
            host,
            ncpu: host.and_then(|h| h.f64("logical_cpus")).unwrap_or(1.0),
        }
    }

    pub fn h(&self, key: &str) -> Option<f64> {
        self.host.and_then(|h| h.f64(key))
    }

    pub fn hs(&self, key: &str, f: Fmt) -> Span<'static> {
        match self.host {
            Some(h) => fmt::span(self.g, h, key, f),
            None => Span::styled("N/A", theme::faint()),
        }
    }

    pub fn series(&self, id: &EntityId, key: &'static str) -> Vec<Option<f64>> {
        self.s
            .history
            .get(&(id.clone(), key))
            .map(|s| s.live().collect())
            .unwrap_or_default()
    }

    pub fn host_series(&self, key: &'static str) -> Vec<Option<f64>> {
        self.series(&EntityId::host(), key)
    }
}

/// Draws table panel `idx` with its standard columns.
pub fn panel_table(f: &mut Frame, area: Rect, c: &Ctx<'_>, idx: usize, hits: &mut HitMap) {
    let Some(t) = c.s.location.tables.get(idx) else {
        return;
    };
    let cols = columns::for_panel(c.g, t.panel, c.ncpu, c.s.location.inspect.as_ref());
    table(
        f,
        area,
        c.s,
        c.g,
        idx,
        &cols,
        hits,
        columns::empty_text(t.panel),
    );
}

fn split(dir: Direction, area: Rect, c: impl IntoIterator<Item = Constraint>) -> Vec<Rect> {
    Layout::default()
        .direction(dir)
        .constraints(c)
        .split(area)
        .to_vec()
}

pub fn rows(area: Rect, c: impl IntoIterator<Item = Constraint>) -> Vec<Rect> {
    split(Direction::Vertical, area, c)
}

pub fn cols(area: Rect, c: impl IntoIterator<Item = Constraint>) -> Vec<Rect> {
    split(Direction::Horizontal, area, c)
}

/// Smallest pane a dragged border may leave: its frame and one line.
const MIN_PANE: u16 = 3;

/// Splits `area` like a ratatui `Layout`, then moves every boundary the user dragged
/// (`ids[i]` names the boundary between pane `i` and `i + 1`) and registers
/// each boundary between two non-empty panes as a draggable border.
///
/// Without a dragged boundary the result is exactly the layout's own. A
/// dragged boundary never shrinks a pane below `MIN_PANE` (or below its
/// default, when that is smaller), and empty panes stay empty.
pub fn resizable(
    axis: Axis,
    area: Rect,
    constraints: impl IntoIterator<Item = Constraint>,
    ids: &[SplitId],
    s: &AppState,
    hits: &mut HitMap,
) -> Vec<Rect> {
    let dir = match axis {
        Axis::Vertical => Direction::Vertical,
        Axis::Horizontal => Direction::Horizontal,
    };
    let rects = split(dir, area, constraints);
    debug_assert_eq!(ids.len() + 1, rects.len(), "one id per boundary");
    let (start, len) = match axis {
        Axis::Vertical => (area.y, area.height),
        Axis::Horizontal => (area.x, area.width),
    };
    let pos = |r: Rect| match axis {
        Axis::Vertical => r.y,
        Axis::Horizontal => r.x,
    };
    let size = |r: Rect| match axis {
        Axis::Vertical => r.height,
        Axis::Horizontal => r.width,
    };
    let end = start + len;
    let n = rects.len();
    let min: Vec<u16> = rects.iter().map(|r| size(*r).min(MIN_PANE)).collect();
    let draggable = |i: usize| size(rects[i]) > 0 && size(rects[i + 1]) > 0;
    // b[i]: where pane i + 1 starts.
    let mut b: Vec<u16> = rects[1..].iter().map(|r| pos(*r)).collect();
    for (i, id) in ids.iter().enumerate().take(n - 1) {
        if let Some(p) = s.splits.get(id).filter(|_| draggable(i)) {
            b[i] = boundary_at(start, len, *p);
        }
    }
    // Keep every pane at its minimum: forward from the start, then back
    // from the end (the defaults satisfy both, so this always succeeds).
    for i in 0..b.len() {
        let lo = if i == 0 { start } else { b[i - 1] } + min[i];
        b[i] = b[i].max(lo);
    }
    for i in (0..b.len()).rev() {
        let hi = if i + 1 == b.len() { end } else { b[i + 1] }.saturating_sub(min[i + 1]);
        b[i] = b[i].min(hi);
    }
    let out: Vec<Rect> = (0..n)
        .map(|i| {
            let a = if i == 0 { start } else { b[i - 1] };
            let z = if i + 1 == n { end } else { b[i] };
            match axis {
                Axis::Vertical => Rect::new(area.x, a, area.width, z - a),
                Axis::Horizontal => Rect::new(a, area.y, z - a, area.height),
            }
        })
        .collect();
    for (i, id) in ids.iter().enumerate().take(n - 1) {
        if !draggable(i) {
            continue;
        }
        let line = match axis {
            Axis::Vertical => Rect::new(area.x, b[i] - 1, area.width, 2),
            Axis::Horizontal => Rect::new(b[i] - 1, area.y, 2, area.height),
        };
        hits.splitter(Splitter {
            id: *id,
            axis,
            line,
            boundary: b[i],
            start,
            len,
        });
    }
    out
}

// ----- overview cards -----------------------------------------------------

/// Borders between rows of overview cards (at most four rows).
const CARD_ROW_IDS: [SplitId; 3] = [
    SplitId("overview.cards.row1"),
    SplitId("overview.cards.row2"),
    SplitId("overview.cards.row3"),
];
/// Borders between card columns, four or two cards to a row.
const CARD_COL_IDS_4: [SplitId; 3] = [
    SplitId("overview.cards4.col1"),
    SplitId("overview.cards4.col2"),
    SplitId("overview.cards4.col3"),
];
const CARD_COL_IDS_2: [SplitId; 1] = [SplitId("overview.cards2.col1")];

struct Card {
    view: View,
    title: &'static str,
    value: String,
    value_color: Color,
    /// The graph is a load on a 0–100 scale, drawn in load lanes.
    lanes: bool,
    series: Vec<Option<f64>>,
    series_max: f64,
    lines: Vec<Line<'static>>,
}

fn card(f: &mut Frame, area: Rect, c: Card, hits: &mut HitMap) {
    let title = Line::from(vec![
        Span::styled(format!(" {} ", c.view.index()), theme::key()),
        Span::styled(format!(" {} ", c.title), theme::title()),
    ]);
    let inner = framed(f, area, panel(title, false));
    hits.push(area, Hit::Tab(c.view));
    if inner.height == 0 {
        return;
    }
    let mut y = inner.y;
    let head = Span::styled(
        c.value.clone(),
        Style::default()
            .fg(c.value_color)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(
        Paragraph::new(Line::from(head)),
        Rect {
            y,
            height: 1,
            ..inner
        },
    );
    y += 1;
    let lines_needed = c.lines.len() as u16;
    let graph_h = inner.height.saturating_sub(1 + lines_needed);
    if graph_h > 0 && !c.series.is_empty() {
        let max = if c.series_max > 0.0 {
            c.series_max
        } else {
            c.series.iter().flatten().fold(1.0f64, |a, b| a.max(*b))
        };
        widgets::graph(
            f.buffer_mut(),
            Rect {
                y,
                height: graph_h,
                ..inner
            },
            &c.series,
            max,
            if c.lanes {
                Fill::Lanes
            } else {
                Fill::Solid(c.value_color)
            },
        );
        y += graph_h;
    }
    for l in c.lines {
        if y >= inner.y + inner.height {
            break;
        }
        f.render_widget(
            Paragraph::new(l),
            Rect {
                y,
                height: 1,
                ..inner
            },
        );
        y += 1;
    }
}

fn pct_color(v: Option<f64>) -> Color {
    v.map_or(theme::FAINT, theme::level)
}

fn na_or(v: Option<f64>, f: Fmt) -> String {
    v.map_or("N/A".into(), |v| fmt::value(v, f))
}

fn cards(c: &Ctx<'_>) -> Vec<Card> {
    let g = c.g;
    let cpu = c.h("cpu_usage_percent");
    let mem = c.h("memory_used_percent");
    let swap_pct = c
        .h("swap_used_bytes")
        .zip(c.h("swap_total_bytes"))
        .filter(|(_, t)| *t > 0.0)
        .map(|(u, t)| 100.0 * u / t);
    let busiest_disk = g
        .nodes_of(EntityKind::BlockDevice)
        .filter(|d| d.attr("partition") != Some(&rtop_core::Attr::Bool(true)))
        .max_by(|a, b| {
            a.f64("utilization_percent")
                .unwrap_or(-1.0)
                .total_cmp(&b.f64("utilization_percent").unwrap_or(-1.0))
        });
    let root = g.node(&EntityId::new(EntityKind::Mount, "/"));
    let gpu = g.nodes_of(EntityKind::Gpu).max_by(|a, b| {
        a.f64("gpu_usage_percent")
            .unwrap_or(-1.0)
            .total_cmp(&b.f64("gpu_usage_percent").unwrap_or(-1.0))
    });
    let containers: Vec<&Node> = g.nodes_of(EntityKind::Container).collect();
    let top_container = containers.iter().max_by(|a, b| {
        a.f64("cpu_usage_percent")
            .unwrap_or(-1.0)
            .total_cmp(&b.f64("cpu_usage_percent").unwrap_or(-1.0))
    });
    let battery = g.nodes_of(EntityKind::Battery).next();
    let psi_cpu = c.h("psi_cpu_some_avg10");
    let disk_series: Vec<Option<f64>> = c
        .host_series("disk_read_bytes_per_second")
        .iter()
        .zip(c.host_series("disk_write_bytes_per_second"))
        .map(|(a, b)| a.zip(b).map(|(a, b)| a + b))
        .collect();
    let line = |pairs: Vec<(&str, Span<'static>)>| kv_line(pairs);
    let dim = |s: String| Span::styled(s, theme::dim());
    vec![
        Card {
            view: View::Cpu,
            title: "CPU",
            value: na_or(cpu, Fmt::Percent),
            value_color: pct_color(cpu),
            lanes: true,
            series: c.host_series("cpu_usage_percent"),
            series_max: 100.0,
            lines: vec![line(vec![
                ("load", c.hs("load_1", Fmt::Float2)),
                ("cpus", c.hs("logical_cpus", Fmt::Count)),
                (
                    "wait",
                    Span::styled(
                        na_or(psi_cpu, Fmt::Percent),
                        Style::default().fg(pct_color(psi_cpu.map(|p| p * 3.0))),
                    ),
                ),
            ])],
        },
        Card {
            view: View::Memory,
            title: "Memory",
            value: na_or(mem, Fmt::Percent),
            value_color: pct_color(mem),
            lanes: true,
            series: c.host_series("memory_used_percent"),
            series_max: 100.0,
            lines: vec![
                line(vec![
                    ("avail", c.hs("memory_available_bytes", Fmt::Bytes)),
                    ("of", c.hs("memory_total_bytes", Fmt::Bytes)),
                ]),
                line(vec![
                    (
                        "swap",
                        Span::styled(
                            na_or(swap_pct, Fmt::Percent),
                            Style::default().fg(pct_color(swap_pct)),
                        ),
                    ),
                    ("in", c.hs("swap_in_pages_per_second", Fmt::PerSecond)),
                    ("psi", c.hs("psi_memory_some_avg10", Fmt::Percent)),
                ]),
            ],
        },
        Card {
            view: View::Storage,
            title: "Storage",
            value: format!(
                "{} r · {} w",
                c.h("disk_read_bytes_per_second")
                    .map_or("N/A".into(), format_rate),
                c.h("disk_write_bytes_per_second")
                    .map_or("N/A".into(), format_rate)
            ),
            value_color: theme::CYAN,
            lanes: false,
            series: disk_series,
            series_max: 0.0,
            lines: vec![line(vec![
                (
                    "busiest",
                    dim(busiest_disk.map_or("—".into(), |d| {
                        format!(
                            "{} {}",
                            d.label(),
                            na_or(d.f64("utilization_percent"), Fmt::Percent)
                        )
                    })),
                ),
                (
                    "/",
                    Span::styled(
                        root.and_then(|r| r.f64("used_percent"))
                            .map_or("N/A".into(), |u| format!("{u:.0}% full")),
                        Style::default().fg(pct_color(root.and_then(|r| r.f64("used_percent")))),
                    ),
                ),
            ])],
        },
        Card {
            view: View::Network,
            title: "Network",
            value: format!(
                "↓ {}  ↑ {}",
                c.h("network_rx_bytes_per_second")
                    .map_or("N/A".into(), format_rate),
                c.h("network_tx_bytes_per_second")
                    .map_or("N/A".into(), format_rate)
            ),
            value_color: theme::GOOD,
            lanes: false,
            series: c.host_series("network_rx_bytes_per_second"),
            series_max: 0.0,
            lines: vec![line(vec![
                ("tcp", c.hs("tcp_established", Fmt::Count)),
                ("listen", c.hs("sockets_tcp_listening", Fmt::Count)),
                (
                    "retrans",
                    c.hs("tcp_retransmits_per_second", Fmt::PerSecond),
                ),
            ])],
        },
        Card {
            view: View::Gpu,
            title: "GPU",
            value: gpu.map_or("N/A".into(), |g| {
                na_or(g.f64("gpu_usage_percent"), Fmt::Percent)
            }),
            value_color: theme::MAGENTA,
            lanes: true,
            series: gpu
                .map(|g| c.series(&g.id, "gpu_usage_percent"))
                .unwrap_or_default(),
            series_max: 100.0,
            lines: vec![match gpu {
                Some(gp) => line(vec![
                    ("", dim(fmt::truncate(gp.label(), 18))),
                    (
                        "vram",
                        Span::raw(gp.f64("vram_used_bytes").map_or("N/A".into(), format_bytes)),
                    ),
                ]),
                None => Line::from(Span::styled("no supported device", theme::faint())),
            }],
        },
        Card {
            view: View::Containers,
            title: "Containers",
            value: if containers.is_empty() {
                "none".into()
            } else {
                format!(
                    "{} running",
                    containers
                        .iter()
                        .filter(|c| c.text("state").is_none_or(|s| s == "running"))
                        .count()
                )
            },
            value_color: theme::ACCENT,
            lanes: false,
            series: Vec::new(),
            series_max: 0.0,
            lines: top_container
                .map(|t| {
                    vec![line(vec![
                        ("top", Span::raw(fmt::truncate(t.label(), 14))),
                        (
                            "cpu",
                            Span::raw(na_or(t.f64("cpu_usage_percent"), Fmt::Percent)),
                        ),
                        (
                            "mem",
                            Span::raw(
                                t.f64("memory_current_bytes")
                                    .map_or("N/A".into(), format_bytes),
                            ),
                        ),
                    ])]
                })
                .unwrap_or_else(|| {
                    vec![Line::from(Span::styled(
                        "no container cgroups found",
                        theme::faint(),
                    ))]
                }),
        },
        Card {
            view: View::Sensors,
            title: "Sensors & power",
            value: c
                .h("temperature_max_celsius")
                .map_or("N/A".into(), |t| format!("{t:.0}°C")),
            value_color: c
                .h("temperature_max_celsius")
                .map_or(theme::FAINT, theme::level),
            lanes: false,
            series: c.host_series("temperature_max_celsius"),
            series_max: 110.0,
            lines: vec![line(vec![
                ("cpu", c.hs("cpu_package_power_w", Fmt::Watts)),
                (
                    "battery",
                    Span::raw(battery.map_or("—".into(), |b| {
                        format!(
                            "{} {}",
                            na_or(b.f64("capacity_percent"), Fmt::Count) + "%",
                            b.text("status").unwrap_or("")
                        )
                    })),
                ),
            ])],
        },
        Card {
            view: View::Security,
            title: "Security",
            value: c.h("security_events_per_minute").map_or_else(
                || {
                    c.host.map_or("N/A".into(), |h| {
                        fmt::metric(c.g, h, "security_events_per_minute", Fmt::Count).0
                    })
                },
                |e| format!("{e:.0} events/min"),
            ),
            value_color: if c.h("security_events_per_minute").unwrap_or(0.0) > 0.0 {
                theme::WARN
            } else {
                theme::GOOD
            },
            lanes: false,
            series: Vec::new(),
            series_max: 0.0,
            lines: vec![line(vec![
                ("conntrack", c.hs("conntrack_entries", Fmt::Count)),
                ("drops", c.hs("conntrack_drops_per_second", Fmt::PerSecond)),
            ])],
        },
    ]
}

fn insight_lines(c: &Ctx<'_>, max: usize) -> Vec<Line<'static>> {
    insights(c.g)
        .into_iter()
        .take(max)
        .map(|i| {
            let (mark, color) = match i.severity {
                Severity::Critical => ("▲", theme::BAD),
                Severity::Warning => ("●", theme::WARN),
                Severity::Info => ("ℹ", theme::ACCENT),
            };
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::raw(i.text),
            ])
        })
        .collect()
}

pub fn overview(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    if c.s.emergency {
        return emergency(f, area, c, hits);
    }
    let all = cards(c);
    let (w, h) = (area.width, area.height);
    if w < 60 || h < 18 {
        // Compact: one summary line per resource, then the table.
        let lines: Vec<Line<'static>> = all
            .iter()
            .take(4)
            .map(|k| {
                Line::from(vec![
                    Span::styled(format!("{:<8}", k.title), theme::dim()),
                    Span::styled(k.value.clone(), Style::default().fg(k.value_color)),
                ])
            })
            .collect();
        let n = lines.len() as u16;
        let r = rows(area, [Constraint::Length(n), Constraint::Min(3)]);
        f.render_widget(Paragraph::new(lines), r[0]);
        return panel_table(f, r[1], c, 0, hits);
    }
    let per_row: usize = if w >= 100 { 4 } else { 2 };
    let card_h: u16 = if h >= 40 {
        8
    } else if h >= 30 {
        7
    } else {
        5
    };
    let max_rows = ((h.saturating_sub(8)) / card_h).max(1) as usize;
    let card_rows = (all.len().div_ceil(per_row))
        .min(max_rows)
        .min(CARD_ROW_IDS.len() + 1);
    let shown = card_rows * per_row;
    let mut constraints: Vec<Constraint> =
        (0..card_rows).map(|_| Constraint::Length(card_h)).collect();
    // Boundaries: between card rows, below the cards, above the table.
    let mut ids: Vec<SplitId> = CARD_ROW_IDS[..card_rows - 1].to_vec();
    ids.push(SplitId("overview.cards"));
    let insight = insight_lines(c, if w >= 140 { 8 } else { 2 });
    let side = w >= 140 && !insight.is_empty();
    let inline_insights = if side { 0 } else { insight.len().min(2) as u16 };
    if inline_insights > 0 {
        constraints.push(Constraint::Length(inline_insights));
        ids.push(SplitId("overview.insights.inline"));
    }
    constraints.push(Constraint::Min(4));
    let r = resizable(Axis::Vertical, area, constraints, &ids, c.s, hits);
    // Column borders are shared by every row of cards.
    let col_ids: &[SplitId] = if per_row == 4 {
        &CARD_COL_IDS_4
    } else {
        &CARD_COL_IDS_2
    };
    for (row, chunk) in all
        .into_iter()
        .take(shown)
        .collect::<Vec<_>>()
        .chunks_mut(per_row)
        .enumerate()
    {
        let cells = resizable(
            Axis::Horizontal,
            r[row],
            (0..per_row).map(|_| Constraint::Ratio(1, per_row as u32)),
            col_ids,
            c.s,
            hits,
        );
        for (i, k) in chunk.iter_mut().enumerate() {
            let k = std::mem::replace(
                k,
                Card {
                    view: View::Overview,
                    title: "",
                    value: String::new(),
                    value_color: theme::TEXT,
                    lanes: false,
                    series: Vec::new(),
                    series_max: 0.0,
                    lines: Vec::new(),
                },
            );
            card(f, cells[i], k, hits);
        }
    }
    let mut next = card_rows;
    if inline_insights > 0 {
        f.render_widget(Paragraph::new(insight.clone()), r[next]);
        next += 1;
    }
    let bottom = r[next];
    if side {
        let parts = resizable(
            Axis::Horizontal,
            bottom,
            [Constraint::Percentage(64), Constraint::Percentage(36)],
            &[SplitId("overview.insights")],
            c.s,
            hits,
        );
        panel_table(f, parts[0], c, 0, hits);
        let inner = framed(
            f,
            parts[1],
            panel(
                Line::from(Span::styled(" Insights ", theme::title())),
                false,
            ),
        );
        f.render_widget(Paragraph::new(insight).wrap(Wrap { trim: true }), inner);
    } else {
        panel_table(f, bottom, c, 0, hits);
    }
}

/// Emergency layout: a minimal summary and the process table, nothing that
/// depends on non-essential collectors.
fn emergency(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let summary = vec![
        Line::from(Span::styled(
            "EMERGENCY MODE — minimal collection; press ! to restore",
            Style::default().fg(theme::BAD).add_modifier(Modifier::BOLD),
        )),
        kv_line(vec![
            ("cpu", c.hs("cpu_usage_percent", Fmt::Percent)),
            ("load", c.hs("load_1", Fmt::Float2)),
            ("mem", c.hs("memory_used_percent", Fmt::Percent)),
            ("avail", c.hs("memory_available_bytes", Fmt::Bytes)),
            (
                "psi cpu/mem/io",
                Span::raw(format!(
                    "{} / {} / {}",
                    na_or(c.h("psi_cpu_some_avg10"), Fmt::Percent),
                    na_or(c.h("psi_memory_some_avg10"), Fmt::Percent),
                    na_or(c.h("psi_io_some_avg10"), Fmt::Percent)
                )),
            ),
            ("blocked", c.hs("processes_blocked", Fmt::Count)),
        ]),
    ];
    let r = rows(area, [Constraint::Length(2), Constraint::Min(3)]);
    f.render_widget(Paragraph::new(summary), r[0]);
    panel_table(f, r[1], c, 0, hits);
}

pub(super) fn summary_block(
    f: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    graphs: Vec<(String, Vec<Option<f64>>, f64, Fill)>,
) {
    let inner = framed(
        f,
        area,
        panel(
            Line::from(Span::styled(format!(" {title} "), theme::title())),
            false,
        ),
    );
    if inner.height == 0 {
        return;
    }
    let text_w = if graphs.is_empty() || inner.width < 70 {
        inner.width
    } else {
        inner.width / 2
    };
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }),
        Rect {
            width: text_w,
            ..inner
        },
    );
    if text_w < inner.width && !graphs.is_empty() {
        let gx = inner.x + text_w + 1;
        let gw = inner.width - text_w - 1;
        let each = (inner.height / graphs.len() as u16).max(1);
        for (i, (label, series, max, color)) in graphs.into_iter().enumerate() {
            let y = inner.y + i as u16 * each;
            if y >= inner.y + inner.height {
                break;
            }
            f.render_widget(
                Paragraph::new(Span::styled(label, theme::dim())),
                Rect {
                    x: gx,
                    y,
                    width: gw,
                    height: 1,
                },
            );
            let max = if max > 0.0 {
                max
            } else {
                series.iter().flatten().fold(1.0f64, |a, b| a.max(*b))
            };
            widgets::graph(
                f.buffer_mut(),
                Rect {
                    x: gx,
                    y: y + 1,
                    width: gw,
                    height: each.saturating_sub(1).max(1),
                },
                &series,
                max,
                color,
            );
        }
    }
}

pub fn cpu(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    // The grid gets as many rows as it needs to show every CPU, up to 60%
    // of the view; the scheduler summary sits beside it when wide enough.
    const SIDE_W: u16 = 46;
    let ncpu = c.s.panel_rows(0).len();
    let max_rows = (area.height * 3 / 5).saturating_sub(2).max(1);
    // The scheduler summary yields its space whenever the grid would
    // otherwise have to fall back to smaller cells.
    let wide = area.width >= 110 && cpugrid::full_cells(ncpu, area.width - SIDE_W - 2, max_rows);
    let grid_w = if wide {
        area.width - SIDE_W
    } else {
        area.width
    }
    .saturating_sub(2);
    let grid_rows = cpugrid::wanted_rows(ncpu, grid_w, max_rows);
    let top_h = (grid_rows + 2)
        .max(if wide { 10 } else { 3 })
        .min(area.height.saturating_sub(4).max(3));
    let r = resizable(
        Axis::Vertical,
        area,
        [Constraint::Length(top_h), Constraint::Min(4)],
        &[SplitId("cpu.grid")],
        c.s,
        hits,
    );
    let top = if wide {
        resizable(
            Axis::Horizontal,
            r[0],
            [Constraint::Min(20), Constraint::Length(SIDE_W)],
            &[SplitId("cpu.scheduler")],
            c.s,
            hits,
        )
    } else {
        vec![r[0]]
    };
    cpugrid::render(f, top[0], c, 0, hits);
    if top.len() > 1 {
        let model = c
            .host
            .and_then(|h| h.text("cpu_model"))
            .unwrap_or("CPU")
            .to_owned();
        let lines = vec![
            Line::from(Span::styled(model, theme::title())),
            kv_line(vec![
                ("busy", c.hs("cpu_usage_percent", Fmt::Percent)),
                ("user", c.hs("cpu_user_percent", Fmt::Percent)),
                ("system", c.hs("cpu_system_percent", Fmt::Percent)),
            ]),
            kv_line(vec![
                ("iowait", c.hs("cpu_iowait_percent", Fmt::Percent)),
                ("steal", c.hs("cpu_steal_percent", Fmt::Percent)),
            ]),
            kv_line(vec![(
                "load",
                Span::raw(format!(
                    "{} {} {}",
                    na_or(c.h("load_1"), Fmt::Float2),
                    na_or(c.h("load_5"), Fmt::Float2),
                    na_or(c.h("load_15"), Fmt::Float2)
                )),
            )]),
            kv_line(vec![
                ("runnable", c.hs("tasks_runnable", Fmt::Count)),
                ("blocked", c.hs("procs_blocked", Fmt::Count)),
            ]),
            kv_line(vec![
                (
                    "ctx switches",
                    c.hs("context_switches_per_second", Fmt::PerSecond),
                ),
                ("irqs", c.hs("interrupts_per_second", Fmt::PerSecond)),
            ]),
            kv_line(vec![
                ("forks", c.hs("forks_per_second", Fmt::PerSecond)),
                ("package", c.hs("cpu_package_power_w", Fmt::Watts)),
            ]),
            kv_line(vec![
                ("pressure some", c.hs("psi_cpu_some_avg10", Fmt::Percent)),
                ("60s", c.hs("psi_cpu_some_avg60", Fmt::Percent)),
            ]),
        ];
        summary_block(
            f,
            top[1],
            "Scheduler",
            lines,
            vec![
                (
                    "utilisation".into(),
                    c.host_series("cpu_usage_percent"),
                    100.0,
                    Fill::Lanes,
                ),
                (
                    "pressure".into(),
                    c.host_series("psi_cpu_some_avg10"),
                    100.0,
                    Fill::Lanes,
                ),
            ],
        );
    }
    panel_table(f, r[1], c, 1, hits);
}

pub fn memory(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let total = c.h("memory_total_bytes").unwrap_or(0.0);
    let share = |k: &str| {
        c.h(k)
            .map(|v| if total > 0.0 { 100.0 * v / total } else { 0.0 })
    };
    let bar_w = (area.width / 3).clamp(10, 40) as usize;
    let row = |label: &str, key: &'static str, fill: Fill| {
        let mut spans = vec![Span::styled(format!("{label:<12}"), theme::dim())];
        spans.extend(widgets::bar(share(key), bar_w, fill));
        spans.push(Span::raw(format!(
            " {}",
            c.h(key).map_or("N/A".into(), format_bytes)
        )));
        Line::from(spans)
    };
    let swap_pct = c
        .h("swap_used_bytes")
        .zip(c.h("swap_total_bytes"))
        .filter(|(_, t)| *t > 0.0)
        .map(|(u, t)| 100.0 * u / t);
    let mut swap = vec![Span::styled(format!("{:<12}", "swap used"), theme::dim())];
    swap.extend(widgets::bar(swap_pct, bar_w, Fill::Lanes));
    swap.push(Span::raw(format!(
        " {}",
        c.h("swap_used_bytes").map_or("N/A".into(), format_bytes)
    )));
    let mut lines = vec![
        row("used", "memory_used_bytes", Fill::Lanes),
        row(
            "available",
            "memory_available_bytes",
            Fill::Solid(theme::GOOD),
        ),
        row(
            "page cache",
            "memory_cached_bytes",
            Fill::Solid(theme::CYAN),
        ),
        row(
            "anonymous",
            "memory_anon_bytes",
            Fill::Solid(theme::MAGENTA),
        ),
        row(
            "kernel slab",
            "memory_slab_unreclaimable_bytes",
            Fill::Solid(theme::WARN),
        ),
        Line::from(swap),
        kv_line(vec![
            ("swap in", c.hs("swap_in_pages_per_second", Fmt::PerSecond)),
            ("out", c.hs("swap_out_pages_per_second", Fmt::PerSecond)),
            (
                "major faults",
                c.hs("major_faults_per_second", Fmt::PerSecond),
            ),
        ]),
        kv_line(vec![
            (
                "direct reclaim",
                c.hs("direct_reclaim_pages_per_second", Fmt::PerSecond),
            ),
            ("PSI some", c.hs("psi_memory_some_avg10", Fmt::Percent)),
            ("full", c.hs("psi_memory_full_avg10", Fmt::Percent)),
        ]),
    ];
    lines.extend(insight_lines(c, 6).into_iter().filter(|l| {
        l.spans
            .iter()
            .any(|s| s.content.contains("wap") || s.content.contains("memory"))
    }));
    let top_h = (lines.len() as u16 + 2).min(area.height / 2).max(5);
    let r = resizable(
        Axis::Vertical,
        area,
        [Constraint::Length(top_h), Constraint::Min(4)],
        &[SplitId("memory.summary")],
        c.s,
        hits,
    );
    summary_block(
        f,
        r[0],
        "Memory",
        lines,
        vec![
            (
                "used %".into(),
                c.host_series("memory_used_percent"),
                100.0,
                Fill::Lanes,
            ),
            (
                "memory pressure".into(),
                c.host_series("psi_memory_some_avg10"),
                100.0,
                Fill::Lanes,
            ),
        ],
    );
    panel_table(f, r[1], c, 0, hits);
}

/// A summary block above two tables: title, text lines, graphs.
pub type Summary<'a> = (
    &'a str,
    Vec<Line<'static>>,
    Vec<(String, Vec<Option<f64>>, f64, Fill)>,
);

pub fn two_tables(
    f: &mut Frame,
    area: Rect,
    c: &Ctx<'_>,
    hits: &mut HitMap,
    first_pct: u16,
    summary: Option<Summary<'_>>,
    [summary_id, tables_id]: [SplitId; 2],
) {
    let mut cons = Vec::new();
    let mut ids = Vec::new();
    let sum_h = summary
        .as_ref()
        .map_or(0, |(_, l, _)| (l.len() as u16 + 2).max(5))
        .min(area.height / 3);
    if summary.is_some() && area.height >= 24 {
        cons.push(Constraint::Length(sum_h));
        ids.push(summary_id);
    }
    cons.push(Constraint::Percentage(first_pct));
    cons.push(Constraint::Min(4));
    ids.push(tables_id);
    let r = resizable(Axis::Vertical, area, cons, &ids, c.s, hits);
    let mut i = 0;
    if let Some((title, lines, graphs)) = summary
        && area.height >= 24
    {
        summary_block(f, r[0], title, lines, graphs);
        i = 1;
    }
    panel_table(f, r[i], c, 0, hits);
    panel_table(f, r[i + 1], c, 1, hits);
}

pub fn storage(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let lines = vec![
        kv_line(vec![
            ("read", c.hs("disk_read_bytes_per_second", Fmt::Rate)),
            ("write", c.hs("disk_write_bytes_per_second", Fmt::Rate)),
        ]),
        kv_line(vec![
            ("I/O pressure", c.hs("psi_io_some_avg10", Fmt::Percent)),
            ("full", c.hs("psi_io_full_avg10", Fmt::Percent)),
            ("blocked tasks", c.hs("procs_blocked", Fmt::Count)),
        ]),
        Line::from(Span::styled(
            "Latency and busy time are block-layer accounting, not application-observed latency.",
            theme::faint(),
        )),
    ];
    two_tables(
        f,
        area,
        c,
        hits,
        50,
        Some((
            "Storage",
            lines,
            vec![
                (
                    "read".into(),
                    c.host_series("disk_read_bytes_per_second"),
                    0.0,
                    Fill::Solid(theme::CYAN),
                ),
                (
                    "write".into(),
                    c.host_series("disk_write_bytes_per_second"),
                    0.0,
                    Fill::Solid(theme::MAGENTA),
                ),
            ],
        )),
        [SplitId("storage.summary"), SplitId("storage.tables")],
    );
}

pub fn network(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let lines = vec![
        kv_line(vec![
            ("↓", c.hs("network_rx_bytes_per_second", Fmt::Rate)),
            ("↑", c.hs("network_tx_bytes_per_second", Fmt::Rate)),
        ]),
        kv_line(vec![
            ("established", c.hs("tcp_established", Fmt::Count)),
            ("listening", c.hs("sockets_tcp_listening", Fmt::Count)),
            ("time-wait", c.hs("sockets_tcp_time_wait", Fmt::Count)),
            ("udp", c.hs("sockets_udp", Fmt::Count)),
            ("unix", c.hs("sockets_unix", Fmt::Count)),
        ]),
        kv_line(vec![
            (
                "retransmits",
                c.hs("tcp_retransmits_per_second", Fmt::PerSecond),
            ),
            (
                "failed opens",
                c.hs("tcp_attempt_fails_per_second", Fmt::PerSecond),
            ),
            (
                "udp errors",
                c.hs("udp_in_errors_per_second", Fmt::PerSecond),
            ),
        ]),
    ];
    let sum_h = if area.height >= 26 { 5 } else { 0 };
    let wide = area.width >= 140;
    let r = resizable(
        Axis::Vertical,
        area,
        [
            Constraint::Length(sum_h),
            Constraint::Percentage(55),
            Constraint::Min(4),
        ],
        &[
            SplitId("network.summary"),
            SplitId(if wide {
                "network.wide.tables"
            } else {
                "network.narrow.tables"
            }),
        ],
        c.s,
        hits,
    );
    if sum_h > 0 {
        summary_block(
            f,
            r[0],
            "Network",
            lines,
            vec![
                (
                    "receive".into(),
                    c.host_series("network_rx_bytes_per_second"),
                    0.0,
                    Fill::Solid(theme::GOOD),
                ),
                (
                    "transmit".into(),
                    c.host_series("network_tx_bytes_per_second"),
                    0.0,
                    Fill::Solid(theme::ACCENT),
                ),
            ],
        );
    }
    if wide {
        let mid = resizable(
            Axis::Horizontal,
            r[1],
            [Constraint::Percentage(50), Constraint::Percentage(50)],
            &[SplitId("network.wide.columns")],
            c.s,
            hits,
        );
        panel_table(f, mid[0], c, 0, hits);
        panel_table(f, mid[1], c, 1, hits);
    } else {
        panel_table(f, r[1], c, 0, hits);
        let low = resizable(
            Axis::Vertical,
            r[2],
            [Constraint::Percentage(50), Constraint::Percentage(50)],
            &[SplitId("network.narrow.lower")],
            c.s,
            hits,
        );
        panel_table(f, low[0], c, 1, hits);
        panel_table(f, low[1], c, 2, hits);
        return;
    }
    panel_table(f, r[2], c, 2, hits);
}

pub fn security(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let lines = vec![
        kv_line(vec![
            (
                "security events/min",
                c.hs("security_events_per_minute", Fmt::Count),
            ),
            ("since start", c.hs("security_events_total", Fmt::Count)),
            ("OOM kills", c.hs("oom_kills_total", Fmt::Count)),
        ]),
        kv_line(vec![
            (
                "firewall matches",
                c.hs("firewall_packets_per_second", Fmt::PerSecond),
            ),
            (
                "drops",
                c.hs("firewall_drop_packets_per_second", Fmt::PerSecond),
            ),
            (
                "counted rules",
                Span::raw(format!(
                    "{} of {}",
                    na_or(c.h("firewall_counted_rules"), Fmt::Count),
                    na_or(c.h("firewall_rules"), Fmt::Count)
                )),
            ),
        ]),
        kv_line(vec![
            ("conntrack", c.hs("conntrack_entries", Fmt::Count)),
            ("of", c.hs("conntrack_max", Fmt::Count)),
            ("drops", c.hs("conntrack_drops_per_second", Fmt::PerSecond)),
            (
                "invalid",
                c.hs("conntrack_invalid_per_second", Fmt::PerSecond),
            ),
        ]),
        kv_line(vec![(
            "rtop itself",
            Span::raw(format!(
                "{} CPU · {} RSS · nice {}",
                na_or(c.h("rtop_cpu_percent"), Fmt::Percent),
                c.h("rtop_rss_bytes").map_or("N/A".into(), format_bytes),
                na_or(c.h("rtop_nice"), Fmt::Count)
            )),
        )]),
        Line::from(Span::styled(
            "Observation only: rtop never changes audit, logging or firewall configuration.",
            theme::faint(),
        )),
    ];
    let sum_h = if area.height >= 28 { 7 } else { 0 };
    let r = resizable(
        Axis::Vertical,
        area,
        [
            Constraint::Length(sum_h),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Min(4),
        ],
        &[
            SplitId("security.summary"),
            SplitId("security.events"),
            SplitId("security.firewall"),
        ],
        c.s,
        hits,
    );
    if sum_h > 0 {
        summary_block(f, r[0], "Security & telemetry health", lines, Vec::new());
    }
    panel_table(f, r[1], c, 0, hits);
    panel_table(f, r[2], c, 1, hits);
    panel_table(f, r[3], c, 2, hits);
}

pub fn generic(
    f: &mut Frame,
    area: Rect,
    c: &Ctx<'_>,
    hits: &mut HitMap,
    first_pct: u16,
    tables: SplitId,
) {
    // No summary, so its boundary id is never registered.
    two_tables(f, area, c, hits, first_pct, None, [tables, tables]);
}

pub fn body(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    if c.s.location.inspect.is_some() {
        return inspect::render(f, area, c, hits);
    }
    match c.s.location.view {
        View::Overview => overview(f, area, c, hits),
        View::Processes => panel_table(f, area, c, 0, hits),
        View::Cpu => cpu(f, area, c, hits),
        View::Memory => memory(f, area, c, hits),
        View::Storage => storage(f, area, c, hits),
        View::Network => network(f, area, c, hits),
        View::Containers => generic(f, area, c, hits, 60, SplitId("containers.tables")),
        View::Gpu => gpu::view(f, area, c, hits),
        View::Security => security(f, area, c, hits),
        View::Sensors => generic(f, area, c, hits, 60, SplitId("sensors.tables")),
        View::Nodes => generic(f, area, c, hits, 40, SplitId("nodes.tables")),
    }
}
