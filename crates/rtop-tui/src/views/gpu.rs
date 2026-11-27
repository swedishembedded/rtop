// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for making accelerator
// bottlenecks visible at a glance for its clients. If your team needs
// expertise in GPU observability tooling then you can procure our services
// by sending an email to info@swedishembedded.com.

//! GPU view: the GPU table, the full detail of the selected GPU (it follows
//! the cursor and mouse clicks), and the processes using GPUs. The detail
//! panel is also the deep section of a GPU's inspection page.
//!
//! The detail is a set of titled sections of aligned `label value` rows,
//! flowed into as many columns as fit; a final "Other" section lists every
//! attribute and metric no section shows, so nothing the collector reports
//! is hidden.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use rtop_app::SplitId;
use rtop_app::view::RowId;
use rtop_core::units::format_bytes;
use rtop_core::{Attr, CollectorState, EntityKind, Node, SystemGraph};

use super::inspect::{attr_text, fmt_for};
use super::{Ctx, panel_table, resizable};
use crate::fmt::{self, Fmt};
use crate::hits::{Axis, HitMap};
use crate::theme;
use crate::widgets::{self, Fill, framed};

/// Width of the gauge bars.
const BAR_W: usize = 10;
/// Space between section columns.
const GAP: u16 = 3;
/// Narrowest graph column worth drawing.
const MIN_GRAPH_W: u16 = 24;

pub fn view(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let gpus = c.s.panel_rows(0).len() as u16;
    let selected = selected_gpu(c);
    // The table takes the rows it needs, the detail what it needs at this
    // width, and the processes the rest; on short terminals all shrink.
    let table_h = (gpus + 3).clamp(4, (area.height / 3).max(4));
    let detail_h = selected
        .map_or(4, |n| panel_height(c, n, area.width))
        .min(area.height.saturating_sub(table_h + 5).max(3));
    let r = resizable(
        Axis::Vertical,
        area,
        [
            Constraint::Length(table_h),
            Constraint::Length(detail_h),
            Constraint::Min(4),
        ],
        &[SplitId("gpu.table"), SplitId("gpu.detail")],
        c.s,
        hits,
    );
    panel_table(f, r[0], c, 0, hits);
    match selected {
        Some(n) => panel(f, r[1], c, n, hits),
        None => {
            let inner = framed(
                f,
                r[1],
                widgets::panel(
                    Line::from(Span::styled(" GPU detail ", theme::title())),
                    false,
                ),
            );
            f.render_widget(
                Paragraph::new(status_lines(c, true)).wrap(Wrap { trim: false }),
                inner,
            );
        }
    }
    panel_table(f, r[2], c, 1, hits);
}

/// The GPU under the table cursor, else the first one listed.
fn selected_gpu<'g>(c: &Ctx<'g>) -> Option<&'g Node> {
    let pick = |r: &RowId| {
        r.entity()
            .filter(|id| id.kind() == Some(EntityKind::Gpu))
            .and_then(|id| c.g.node(id))
    };
    c.s.location
        .tables
        .first()
        .and_then(|t| t.selected.as_ref())
        .and_then(pick)
        .or_else(|| c.s.panel_rows(0).ids.iter().find_map(pick))
}

/// The collector's own explanation when it is degraded or found nothing,
/// e.g. why NVML could not be used for some GPUs.
fn status_lines(c: &Ctx<'_>, empty: bool) -> Vec<Line<'static>> {
    let st = c.g.collector_status(rtop_collectors::gpu::ID);
    match (st.map(|s| s.state), st.and_then(|s| s.detail.as_deref())) {
        (Some(CollectorState::Degraded | CollectorState::Unavailable), Some(d)) => {
            vec![Line::from(Span::styled(
                format!("⚠ {}", rtop_core::text::sanitize(d)),
                Style::default().fg(theme::WARN),
            ))]
        }
        _ if empty => vec![Line::from(Span::styled(
            "No supported GPU or accelerator found",
            theme::faint(),
        ))],
        _ => Vec::new(),
    }
}

/// Rows of wrapped text `lines` take at `width`.
fn wrapped_rows(lines: &[Line<'_>], width: u16) -> u16 {
    let w = usize::from(width.max(1));
    lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(w) as u16)
        .sum()
}

/// Whether GPU `n` has history worth a graph pane.
fn has_history(c: &Ctx<'_>, n: &Node) -> bool {
    graphs(c, n)
        .iter()
        .any(|(_, s, _, _)| s.iter().flatten().count() > 1)
}

/// Width left for the detail sections when the history pane takes its
/// minimum beside them.
fn detail_room(c: &Ctx<'_>, n: &Node, width: u16) -> u16 {
    if has_history(c, n) && width >= 2 * MIN_GRAPH_W {
        width - MIN_GRAPH_W.max(width / 4)
    } else {
        width
    }
}

/// Height the detail panel of `n` needs at `width`, frames included.
pub fn panel_height(c: &Ctx<'_>, n: &Node, width: u16) -> u16 {
    let inner_w = detail_room(c, n, width).saturating_sub(2);
    let (_, h, _) = flow(&sections(c.g, n), inner_w);
    wrapped_rows(&status_lines(c, false), inner_w) + h + 2
}

/// Full detail of GPU `n`: a pane with the warnings and the sections in
/// columns, and beside it, when there is room and history, a pane of
/// utilisation, memory and power graphs. The border between them drags.
pub fn panel(f: &mut Frame, area: Rect, c: &Ctx<'_>, n: &Node, hits: &mut HitMap) {
    let sections = sections(c.g, n);
    let room = detail_room(c, n, area.width);
    let (_, _, used_w) = flow(&sections, room.saturating_sub(2));
    let panes = if room < area.width {
        resizable(
            Axis::Horizontal,
            area,
            [Constraint::Length(used_w + 3), Constraint::Min(MIN_GRAPH_W)],
            &[SplitId("gpu.history")],
            c.s,
            hits,
        )
    } else {
        vec![area]
    };
    let title = format!(
        " {} · {} ",
        rtop_app::project::label_of(c.g, &n.id),
        n.id.key()
    );
    let mut inner = framed(
        f,
        panes[0],
        widgets::panel(Line::from(Span::styled(title, theme::title())), false),
    );
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let status = status_lines(c, false);
    if !status.is_empty() {
        let h = wrapped_rows(&status, inner.width).min(inner.height);
        f.render_widget(
            Paragraph::new(status).wrap(Wrap { trim: false }),
            Rect { height: h, ..inner },
        );
        inner.y += h;
        inner.height -= h;
    }
    let (columns, _, _) = flow(&sections, inner.width);
    let right = inner.x + inner.width;
    let mut x = inner.x;
    for col in columns {
        let w = col
            .iter()
            .map(|&i| sections[i].width())
            .max()
            .unwrap_or(0)
            .min(right.saturating_sub(x));
        if w == 0 {
            break;
        }
        let mut lines = Vec::new();
        for &i in &col {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(sections[i].lines());
        }
        f.render_widget(
            Paragraph::new(lines),
            Rect {
                x,
                width: w,
                ..inner
            },
        );
        x = x.saturating_add(w + GAP);
    }
    if let Some(&pane) = panes.get(1) {
        let inner = framed(
            f,
            pane,
            widgets::panel(Line::from(Span::styled(" History ", theme::title())), false),
        );
        draw_graphs(f, inner, graphs(c, n));
    }
}

/// Packs sections, in order, into columns that fit `width` using the
/// fewest rows: returns the columns (section indices), their height and
/// the width they use. Sections are separated by a blank line.
fn flow(sections: &[Section], width: u16) -> (Vec<Vec<usize>>, u16, u16) {
    let heights: Vec<u16> = sections.iter().map(Section::height).collect();
    let total: u16 = heights.iter().sum::<u16>() + heights.len().saturating_sub(1) as u16;
    let tallest = heights.iter().copied().max().unwrap_or(0);
    let pack = |limit: u16| {
        let mut columns: Vec<Vec<usize>> = Vec::new();
        let mut h = 0;
        for (i, &sh) in heights.iter().enumerate() {
            match columns.last_mut() {
                Some(col) if h + 1 + sh <= limit => {
                    col.push(i);
                    h += 1 + sh;
                }
                _ => {
                    columns.push(vec![i]);
                    h = sh;
                }
            }
        }
        let widths: Vec<u16> = columns
            .iter()
            .map(|c| c.iter().map(|&i| sections[i].width()).max().unwrap_or(0))
            .collect();
        let used = widths.iter().sum::<u16>() + GAP * widths.len().saturating_sub(1) as u16;
        let height = columns
            .iter()
            .map(|c| c.iter().map(|&i| heights[i]).sum::<u16>() + c.len().saturating_sub(1) as u16)
            .max()
            .unwrap_or(0);
        (columns, height, used)
    };
    for limit in tallest..=total.max(tallest) {
        let packed = pack(limit);
        if packed.2 <= width {
            return packed;
        }
    }
    pack(total.max(tallest))
}

/// A titled block of `label value` rows.
struct Section {
    title: &'static str,
    rows: Vec<(String, Vec<Span<'static>>)>,
}

impl Section {
    fn label_w(&self) -> usize {
        self.rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0)
    }

    fn width(&self) -> u16 {
        let lw = self.label_w();
        let row = self
            .rows
            .iter()
            .map(|(_, v)| lw + 1 + v.iter().map(Span::width).sum::<usize>())
            .max()
            .unwrap_or(0);
        row.max(self.title.chars().count()) as u16
    }

    fn height(&self) -> u16 {
        self.rows.len() as u16 + 1
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let lw = self.label_w();
        let mut out = vec![Line::from(Span::styled(
            self.title,
            theme::title().add_modifier(Modifier::UNDERLINED),
        ))];
        for (l, v) in &self.rows {
            let mut spans = vec![Span::styled(format!("{l:<lw$} "), theme::dim())];
            spans.extend(v.iter().cloned());
            out.push(Line::from(spans));
        }
        out
    }
}

/// Builds sections from a node, remembering which keys they showed.
struct Detail<'a> {
    g: &'a SystemGraph,
    n: &'a Node,
    shown: HashSet<&'static str>,
    sections: Vec<Section>,
    rows: Vec<(String, Vec<Span<'static>>)>,
}

impl<'a> Detail<'a> {
    fn section(&mut self, title: &'static str) {
        let rows = std::mem::take(&mut self.rows);
        if !rows.is_empty() {
            self.sections.push(Section { title, rows });
        }
    }

    fn row(&mut self, label: &str, value: Vec<Span<'static>>) {
        self.rows.push((label.to_owned(), value));
    }

    fn has(&self, key: &str) -> bool {
        self.n.metric(key).is_some() || self.n.attr(key).is_some()
    }

    /// A metric formatted by its unit, with availability markers.
    fn metric(&mut self, label: &str, key: &'static str) {
        if self.n.metric(key).is_none() {
            return;
        }
        self.shown.insert(key);
        let (v, st) = fmt::metric(self.g, self.n, key, fmt_for(key));
        self.row(label, vec![Span::styled(v, st)]);
    }

    /// A percentage with a short bar coloured by its level.
    fn percent(&mut self, label: &str, key: &'static str) {
        if self.n.metric(key).is_none() {
            return;
        }
        self.shown.insert(key);
        let (v, st) = fmt::metric(self.g, self.n, key, Fmt::Percent);
        let p = self.n.f64(key);
        let mut spans = vec![Span::styled(format!("{v:>6} "), st)];
        spans.extend(gauge(p));
        self.row(label, spans);
    }

    /// `cur / max unit` with a bar of their ratio when both are known.
    fn of(&mut self, label: &str, cur: &'static str, max: &'static str, bar: bool) {
        if self.n.metric(cur).is_none() {
            return;
        }
        self.shown.insert(cur);
        let f = fmt_for(cur);
        let (v, st) = fmt::metric(self.g, self.n, cur, f);
        let mut spans = vec![Span::styled(v, st)];
        if let Some(m) = self.n.f64(max) {
            self.shown.insert(max);
            spans.push(Span::styled(" / ", theme::faint()));
            spans.push(Span::raw(fmt::value(m, f)));
            if bar && let Some(c) = self.n.f64(cur) {
                spans.push(Span::raw("  "));
                spans.extend(gauge((m > 0.0).then(|| 100.0 * c / m)));
            }
        }
        self.row(label, spans);
    }

    fn text(&mut self, label: &str, key: &'static str) {
        let Some(a) = self.n.attr(key) else { return };
        self.shown.insert(key);
        let v = match a {
            Attr::Bool(b) => if *b { "on" } else { "off" }.to_owned(),
            a => attr_text(a),
        };
        self.row(label, vec![Span::raw(v)]);
    }

    /// Everything no section showed.
    fn other(&mut self) {
        const IDENTITY: [&str; 2] = ["name", "vendor"];
        let n = self.n;
        for (k, a) in n.attrs.iter() {
            if !self.shown.contains(*k) && !IDENTITY.contains(k) {
                self.rows
                    .push(((*k).to_owned(), vec![Span::raw(attr_text(&a.value))]));
            }
        }
        for (k, e) in &n.metrics {
            if !self.shown.contains(*k) {
                let (v, st) = fmt::entry(self.g, Some(e), fmt_for(k));
                self.rows.push(((*k).to_owned(), vec![Span::styled(v, st)]));
            }
        }
        self.section("Other");
    }
}

/// A `BAR_W` bar filled to `pct`, in one colour chosen by its level.
fn gauge(pct: Option<f64>) -> Vec<Span<'static>> {
    let color = pct.map_or(theme::FAINT, theme::level);
    widgets::bar(pct, BAR_W, Fill::Solid(color))
}

/// Every section GPU `n` has data for, then everything else.
fn sections(g: &SystemGraph, n: &Node) -> Vec<Section> {
    let mut d = Detail {
        g,
        n,
        shown: HashSet::new(),
        sections: Vec::new(),
        rows: Vec::new(),
    };
    d.percent("GPU", "gpu_usage_percent");
    d.percent("memory bus", "memory_busy_percent");
    d.percent("encoder", "encoder_usage_percent");
    d.percent("decoder", "decoder_usage_percent");
    d.text("P-state", "performance_state");
    if let Some(r) = n.text("clock_event_reasons") {
        d.shown.insert("clock_event_reasons");
        let r = r.replace(',', ", ").replace('_', " ");
        let st = if r == "none" || r == "gpu idle" {
            theme::dim()
        } else {
            Style::default().fg(theme::WARN)
        };
        d.row("limited by", vec![Span::styled(r, st)]);
    }
    d.section("Load");

    d.of("used", "vram_used_bytes", "vram_total_bytes", true);
    if !d.has("vram_used_bytes") {
        d.metric("total", "vram_total_bytes");
    }
    d.metric("reserved", "vram_reserved_bytes");
    if let (Some(t), Some(u)) = (n.f64("vram_total_bytes"), n.f64("vram_used_bytes")) {
        let free = t - u - n.f64("vram_reserved_bytes").unwrap_or(0.0);
        d.row("free", vec![Span::raw(format_bytes(free.max(0.0)))]);
    }
    d.of("GTT used", "gtt_used_bytes", "gtt_total_bytes", true);
    if let Some(b) = n.f64("memory_bus_width_bits") {
        d.shown.insert("memory_bus_width_bits");
        d.row("bus width", vec![Span::raw(format!("{b:.0}-bit"))]);
    }
    d.of("clock", "memory_clock_mhz", "memory_clock_max_mhz", false);
    d.of(
        "temperature",
        "memory_temperature_celsius",
        "memory_temperature_max_celsius",
        true,
    );
    d.section("Memory");

    d.of("draw", "power_w", "power_limit_w", true);
    d.metric("default limit", "power_limit_default_w");
    if let (Some(lo), Some(hi)) = (n.f64("power_limit_min_w"), n.f64("power_limit_max_w")) {
        d.shown.extend(["power_limit_min_w", "power_limit_max_w"]);
        d.row(
            "limit range",
            vec![Span::raw(format!("{lo:.0} – {hi:.0} W"))],
        );
    }
    if let Some(wh) = n.f64("energy_consumed_wh") {
        d.shown.insert("energy_consumed_wh");
        let e = if wh >= 1000.0 {
            format!("{:.2} kWh", wh / 1000.0)
        } else {
            format!("{wh:.1} Wh")
        };
        d.row(
            "energy",
            vec![
                Span::raw(e),
                Span::styled(" since driver load", theme::faint()),
            ],
        );
    }
    d.section("Power");

    let limit = if d.has("temperature_slowdown_celsius") {
        "temperature_slowdown_celsius"
    } else {
        "temperature_max_operating_celsius"
    };
    d.of("GPU", "temperature_celsius", limit, true);
    d.metric("max operating", "temperature_max_operating_celsius");
    d.metric("slowdown", "temperature_slowdown_celsius");
    d.metric("shutdown", "temperature_shutdown_celsius");
    d.percent("fan", "fan_speed_percent");
    d.section("Thermal");

    d.of("graphics", "frequency_mhz", "frequency_max_mhz", false);
    d.metric("boost max", "frequency_boost_mhz");
    d.of("SM", "sm_clock_mhz", "sm_clock_max_mhz", false);
    d.of("video", "video_clock_mhz", "video_clock_max_mhz", false);
    d.section("Clocks");

    d.of(
        "PCIe gen",
        "pcie_link_generation",
        "pcie_link_generation_max",
        false,
    );
    d.of(
        "PCIe width",
        "pcie_link_width",
        "pcie_link_width_max",
        false,
    );
    d.metric("PCIe replays", "pcie_replays_total");
    d.of("NVLinks up", "nvlink_links_active", "nvlink_links", false);
    d.metric("NVLink tx", "nvlink_tx_bytes_per_second");
    d.metric("NVLink rx", "nvlink_rx_bytes_per_second");
    d.metric("C2C links", "c2c_links");
    d.metric("C2C per link", "c2c_link_bandwidth_bytes_per_second");
    d.text("C2C active", "c2c_active");
    d.section("Links");

    d.text("ECC", "ecc_mode");
    d.metric("corrected", "ecc_corrected_volatile_total");
    d.metric("uncorrected", "ecc_uncorrected_volatile_total");
    d.metric("lifetime corr", "ecc_corrected_aggregate_total");
    d.metric("lifetime unc", "ecc_uncorrected_aggregate_total");
    d.metric("rows remap c", "remapped_rows_corrected_total");
    d.metric("rows remap u", "remapped_rows_uncorrected_total");
    d.text("remap pending", "remapping_pending");
    d.text("remap failed", "remapping_failed");
    d.section("Errors");

    d.text("persistence", "persistence_mode");
    d.text("compute mode", "compute_mode");
    d.text("MIG", "mig_mode");
    d.section("Configuration");

    d.text("node", "node");
    d.text("index", "index");
    d.text("brand", "brand");
    d.text("architecture", "architecture");
    d.text("compute cap", "compute_capability");
    d.metric("CUDA cores", "cuda_cores");
    d.text("PCI address", "pci_address");
    d.text("PCI device", "pci_device_id");
    d.text("PCI subsys", "pci_subsystem_id");
    d.text("NUMA node", "numa_node");
    d.text("card", "card");
    d.text("board id", "board_id");
    d.text("part", "board_part_number");
    d.text("serial", "serial");
    d.text("UUID", "uuid");
    d.section("Identity");

    d.text("driver", "driver");
    d.text("version", "driver_version");
    d.text("CUDA", "cuda_driver_version");
    d.text("NVML", "nvml_version");
    d.text("GSP firmware", "gsp_firmware_version");
    d.text("VBIOS", "vbios_version");
    d.text("InfoROM", "inforom_version");
    d.text("source", "provider");
    d.section("Driver and firmware");

    d.other();
    d.sections
}

/// Labelled history graphs stacked in `area`.
fn draw_graphs(f: &mut Frame, area: Rect, graphs: Vec<(String, Vec<Option<f64>>, f64, Fill)>) {
    let each = (area.height / graphs.len().max(1) as u16).max(2);
    for (i, (label, series, max, fill)) in graphs.into_iter().enumerate() {
        let y = area.y + i as u16 * each;
        if y + 1 >= area.y + area.height {
            break;
        }
        f.render_widget(
            Paragraph::new(Span::styled(label, theme::dim())),
            Rect {
                y,
                height: 1,
                ..area
            },
        );
        let max = if max > 0.0 {
            max
        } else {
            series.iter().flatten().fold(1.0f64, |a, b| a.max(*b))
        };
        let h = each.saturating_sub(1).min(area.y + area.height - y - 1);
        widgets::graph(
            f.buffer_mut(),
            Rect {
                y: y + 1,
                height: h,
                ..area
            },
            &series,
            max,
            fill,
        );
    }
}

fn graphs(c: &Ctx<'_>, n: &Node) -> Vec<(String, Vec<Option<f64>>, f64, Fill)> {
    let now = |k: &str, f: Fmt| n.f64(k).map_or_else(String::new, |v| fmt::value(v, f));
    let mut g = vec![(
        format!("utilisation {}", now("gpu_usage_percent", Fmt::Percent)),
        c.series(&n.id, "gpu_usage_percent"),
        100.0,
        Fill::Lanes,
    )];
    if let Some(total) = n.f64("vram_total_bytes").filter(|t| *t > 0.0) {
        let used = c
            .series(&n.id, "vram_used_bytes")
            .into_iter()
            .map(|v| v.map(|v| 100.0 * v / total))
            .collect();
        g.push((
            format!("memory used {}", now("vram_used_bytes", Fmt::Bytes)),
            used,
            100.0,
            Fill::Lanes,
        ));
    }
    if n.metric("power_w").and_then(|m| m.as_f64()).is_some() {
        let max = n
            .f64("power_limit_w")
            .or_else(|| n.f64("power_limit_max_w"))
            .unwrap_or(0.0);
        g.push((
            format!("power {}", now("power_w", Fmt::Watts)),
            c.series(&n.id, "power_w"),
            max,
            Fill::Solid(theme::MAGENTA),
        ));
    }
    g
}
