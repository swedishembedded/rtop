// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Column definitions of every table panel.

use ratatui::style::{Modifier, Style};
use rtop_app::project::label_of;
use rtop_app::view::{Panel, RowId, SortKey};
use rtop_core::{CollectorState, EdgeKind, Node, Severity, SystemGraph};

use crate::fmt::{self, Fmt};
use crate::theme;
use crate::widgets::Column;

fn node<'g>(g: &'g SystemGraph, r: &RowId) -> Option<&'g Node> {
    r.entity().and_then(|id| g.node(id))
}

fn text<'g>(
    g: &'g SystemGraph,
    key: &'static str,
) -> impl Fn(&RowId, usize, usize) -> (String, Style) + 'g {
    move |r, _, _| {
        (
            node(g, r)
                .and_then(|n| n.text(key))
                .unwrap_or("")
                .to_owned(),
            Style::default(),
        )
    }
}

fn m<'g>(
    g: &'g SystemGraph,
    key: &'static str,
    f: Fmt,
) -> impl Fn(&RowId, usize, usize) -> (String, Style) + 'g {
    move |r, _, _| {
        node(g, r).map_or((String::new(), Style::default()), |n| {
            fmt::metric(g, n, key, f)
        })
    }
}

fn colored_pct<'g>(
    g: &'g SystemGraph,
    key: &'static str,
    scale: f64,
) -> impl Fn(&RowId, usize, usize) -> (String, Style) + 'g {
    move |r, _, _| match node(g, r) {
        Some(n) => {
            let (t, st) = fmt::metric(g, n, key, Fmt::Percent);
            match n.f64(key) {
                Some(v) if st == Style::default() => {
                    (t, Style::default().fg(theme::level(v / scale)))
                }
                _ => (t, st),
            }
        }
        None => (String::new(), Style::default()),
    }
}

fn pair<'g>(
    g: &'g SystemGraph,
    a: &'static str,
    b: &'static str,
    f: Fmt,
) -> impl Fn(&RowId, usize, usize) -> (String, Style) + 'g {
    move |r, _, _| {
        node(g, r).map_or((String::new(), Style::default()), |n| {
            fmt::pair(g, n, a, b, f)
        })
    }
}

pub fn processes<'g>(g: &'g SystemGraph, ncpu: f64) -> Vec<Column<'g>> {
    vec![
        Column::new("PID", 7, move |r, _, _| {
            (
                node(g, r)
                    .and_then(|n| n.attr("pid"))
                    .and_then(|a| a.as_u64())
                    .map_or(String::new(), |p| p.to_string()),
                theme::dim(),
            )
        })
        .right()
        .prio(9)
        .sort(SortKey::Pid),
        Column::new("USER", 9, text(g, "user")).prio(4),
        Column::new("S", 1, move |r, _, _| {
            let code = node(g, r)
                .and_then(|n| n.text("state_code"))
                .unwrap_or("?")
                .to_owned();
            let st = match code.as_str() {
                "R" => Style::default().fg(theme::GOOD),
                "D" => Style::default().fg(theme::BAD).add_modifier(Modifier::BOLD),
                "Z" | "T" => Style::default().fg(theme::WARN),
                _ => theme::dim(),
            };
            (code, st)
        })
        .prio(3),
        Column::new(
            "CPU%",
            6,
            colored_pct(g, "cpu_usage_percent", ncpu.max(1.0)),
        )
        .right()
        .prio(9)
        .sort(SortKey::Cpu),
        Column::new("MEM", 9, m(g, "rss_bytes", Fmt::Bytes))
            .right()
            .prio(8)
            .sort(SortKey::Mem),
        Column::new("VIRT", 9, m(g, "virtual_bytes", Fmt::Bytes))
            .right()
            .prio(1)
            .sort(SortKey::Virt),
        Column::new("THR", 4, m(g, "threads", Fmt::Count))
            .right()
            .prio(2)
            .sort(SortKey::Threads),
        Column::new(
            "DISK I/O",
            10,
            pair(
                g,
                "io_read_bytes_per_second",
                "io_write_bytes_per_second",
                Fmt::Rate,
            ),
        )
        .right()
        .prio(5)
        .sort(SortKey::Io),
        Column::new(
            "NET",
            10,
            pair(
                g,
                "tcp_rx_bytes_per_second",
                "tcp_tx_bytes_per_second",
                Fmt::Rate,
            ),
        )
        .right()
        .prio(3)
        .sort(SortKey::Net),
        Column::new("AGE", 7, m(g, "age_seconds", Fmt::Duration))
            .right()
            .prio(2)
            .sort(SortKey::Age),
        Column::new("COMMAND", 18, move |r, depth, _| {
            let Some(n) = node(g, r) else {
                return (String::new(), Style::default());
            };
            let indent = if depth > 0 {
                format!("{}└ ", "  ".repeat(depth - 1))
            } else {
                String::new()
            };
            let kernel = n.attr("kernel_thread").is_some();
            let st = if kernel {
                theme::dim()
            } else if n.attr("is_self").is_some() {
                Style::default().fg(theme::CYAN)
            } else {
                Style::default()
            };
            // The command line when there is one (it names the program);
            // kernel threads and processes without one show [name].
            let text = match n.text("cmdline").filter(|c| !c.is_empty()) {
                Some(c) => c.to_owned(),
                None => format!("[{}]", n.label()),
            };
            (format!("{indent}{text}"), st)
        })
        .flex()
        .sort(SortKey::Name),
    ]
}

pub fn cpus(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("CPU", 5, move |r, _, _| {
            (
                node(g, r)
                    .and_then(|n| n.attr("index"))
                    .and_then(|a| a.as_u64())
                    .map_or(String::new(), |i| format!("cpu{i}")),
                theme::dim(),
            )
        })
        .sort(SortKey::Name),
        Column::new("TYPE", 4, move |r, _, _| {
            (
                node(g, r)
                    .and_then(|n| n.text("core_type"))
                    .map_or("", |t| if t == "performance" { "P" } else { "E" })
                    .to_owned(),
                Style::default().fg(theme::MAGENTA),
            )
        })
        .prio(2),
        Column::new("USAGE", 14, move |r, _, w| {
            let v = node(g, r).and_then(|n| n.f64("cpu_usage_percent"));
            let s: String = crate::widgets::bar(v, w, theme::level(v.unwrap_or(-1.0)))
                .iter()
                .map(|s| s.content.to_string())
                .collect();
            (s, Style::default().fg(theme::level(v.unwrap_or(-1.0))))
        })
        .flex(),
        Column::new("%", 6, colored_pct(g, "cpu_usage_percent", 1.0))
            .right()
            .sort(SortKey::Cpu),
        Column::new("FREQ", 9, m(g, "frequency_mhz", Fmt::Mhz))
            .right()
            .prio(3),
    ]
}

pub fn block_devices(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("DEVICE", 10, move |r, _, _| {
            let n = node(g, r);
            let part = n
                .and_then(|n| n.attr("partition"))
                .is_some_and(|a| *a == rtop_core::Attr::Bool(true));
            (
                format!(
                    "{}{}",
                    if part { " " } else { "" },
                    n.map_or("", |n| n.label())
                ),
                if part { theme::dim() } else { Style::default() },
            )
        })
        .sort(SortKey::Name),
        Column::new("MODEL", 12, text(g, "model")).flex().prio(1),
        Column::new("READ", 10, m(g, "read_bytes_per_second", Fmt::Rate))
            .right()
            .prio(8)
            .sort(SortKey::Io),
        Column::new("WRITE", 10, m(g, "write_bytes_per_second", Fmt::Rate))
            .right()
            .prio(8),
        Column::new("R IOPS", 7, m(g, "read_iops", Fmt::Float1))
            .right()
            .prio(3),
        Column::new("W IOPS", 7, m(g, "write_iops", Fmt::Float1))
            .right()
            .prio(3),
        Column::new("R LAT", 9, m(g, "read_latency_ms", Fmt::Ms))
            .right()
            .prio(4),
        Column::new("W LAT", 9, m(g, "write_latency_ms", Fmt::Ms))
            .right()
            .prio(4),
        Column::new("BUSY", 6, colored_pct(g, "utilization_percent", 1.0))
            .right()
            .prio(6),
        Column::new("QD", 5, m(g, "queue_depth", Fmt::Float1))
            .right()
            .prio(2),
        Column::new("SIZE", 9, m(g, "size_bytes", Fmt::Bytes))
            .right()
            .prio(2),
    ]
}

pub fn mounts(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("MOUNT", 14, text(g, "mount_point"))
            .flex()
            .sort(SortKey::Name),
        Column::new("FS", 6, text(g, "fs_type")).prio(3),
        Column::new("USED", 12, move |r, _, w| {
            let v = node(g, r).and_then(|n| n.f64("used_percent"));
            (
                crate::widgets::bar(v, w, theme::level(v.unwrap_or(-1.0)))
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect(),
                Style::default().fg(theme::level(v.unwrap_or(-1.0))),
            )
        })
        .prio(2),
        Column::new("%", 6, colored_pct(g, "used_percent", 1.0))
            .right()
            .sort(SortKey::Mem),
        Column::new("USED", 9, m(g, "used_bytes", Fmt::Bytes))
            .right()
            .prio(5),
        Column::new("FREE", 9, m(g, "available_bytes", Fmt::Bytes))
            .right()
            .prio(6),
        Column::new("SIZE", 9, m(g, "capacity_bytes", Fmt::Bytes))
            .right()
            .prio(4),
        Column::new("SOURCE", 14, text(g, "source")).prio(1),
    ]
}

pub fn interfaces(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("INTERFACE", 10, move |r, _, _| {
            (
                node(g, r).map_or("", |n| n.label()).to_owned(),
                Style::default(),
            )
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("STATE", 6, move |r, _, _| {
            let s = node(g, r)
                .and_then(|n| n.text("operstate"))
                .unwrap_or("?")
                .to_owned();
            let st = if s == "up" {
                Style::default().fg(theme::GOOD)
            } else {
                theme::dim()
            };
            (s, st)
        }),
        Column::new("RX", 10, m(g, "rx_bytes_per_second", Fmt::Rate))
            .right()
            .sort(SortKey::Net),
        Column::new("TX", 10, m(g, "tx_bytes_per_second", Fmt::Rate)).right(),
        Column::new("RX PKT", 8, m(g, "rx_packets_per_second", Fmt::PerSecond))
            .right()
            .prio(2),
        Column::new("TX PKT", 8, m(g, "tx_packets_per_second", Fmt::PerSecond))
            .right()
            .prio(2),
        Column::new(
            "DROPS",
            7,
            pair(
                g,
                "rx_dropped_per_second",
                "tx_dropped_per_second",
                Fmt::PerSecond,
            ),
        )
        .right()
        .prio(3),
        Column::new(
            "ERRS",
            7,
            pair(
                g,
                "rx_errors_per_second",
                "tx_errors_per_second",
                Fmt::PerSecond,
            ),
        )
        .right()
        .prio(3),
        Column::new("LINK", 9, move |r, _, _| {
            node(g, r).map_or((String::new(), Style::default()), |n| {
                match n.f64("link_speed_mbps") {
                    Some(v) => (format!("{v:.0} Mb/s"), theme::dim()),
                    None => ("—".into(), theme::faint()),
                }
            })
        })
        .right()
        .prio(1),
    ]
}

fn owner_of(g: &SystemGraph, r: &RowId) -> String {
    let Some(id) = r.entity() else {
        return String::new();
    };
    let owners: Vec<String> = g
        .incoming(id)
        .filter(|e| e.key.kind == EdgeKind::OwnsSocket)
        .map(|e| label_of(g, &e.key.source))
        .collect();
    match owners.len() {
        0 => "—".into(),
        1 => owners[0].clone(),
        n => format!("{} +{}", owners[0], n - 1),
    }
}

pub fn sockets(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("PROTO", 6, text(g, "protocol")),
        Column::new("LOCAL", 21, move |r, _, _| {
            (
                node(g, r)
                    .and_then(|n| {
                        n.text("local_address")
                            .or_else(|| n.text("path").filter(|p| !p.is_empty()))
                    })
                    .unwrap_or("(unnamed)")
                    .to_owned(),
                Style::default(),
            )
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("REMOTE", 21, move |r, _, _| {
            let n = node(g, r);
            // Unix peers: show who is on the other end, not a socket number.
            let peer = r
                .entity()
                .and_then(|id| g.outgoing(id).find(|e| e.key.kind == EdgeKind::ConnectedTo))
                .map(|e| {
                    let owner = owner_of(g, &RowId::Node(e.key.target.clone()));
                    if owner == "—" {
                        label_of(g, &e.key.target)
                    } else {
                        format!("peer {owner}")
                    }
                });
            (
                n.and_then(|n| n.text("remote_address"))
                    .map(str::to_owned)
                    .or(peer)
                    .unwrap_or_default(),
                Style::default(),
            )
        })
        .prio(6),
        Column::new("STATE", 11, move |r, _, _| {
            let s = node(g, r)
                .and_then(|n| n.text("state"))
                .unwrap_or("")
                .to_owned();
            let st = match s.as_str() {
                "established" | "connected" => Style::default().fg(theme::GOOD),
                "listen" => Style::default().fg(theme::CYAN),
                _ => theme::dim(),
            };
            (s, st)
        })
        .prio(5),
        Column::new("RX", 10, m(g, "rx_bytes_per_second", Fmt::Rate))
            .right()
            .prio(4)
            .sort(SortKey::Net),
        Column::new("TX", 10, m(g, "tx_bytes_per_second", Fmt::Rate))
            .right()
            .prio(4),
        Column::new("OWNER", 16, move |r, _, _| (owner_of(g, r), theme::dim())).prio(7),
    ]
}

fn edge_endpoints(g: &SystemGraph, r: &RowId) -> (String, String) {
    match r {
        RowId::Edge(k) => (label_of(g, &k.source), label_of(g, &k.target)),
        _ => (String::new(), String::new()),
    }
}

fn edge_metric<'g>(
    g: &'g SystemGraph,
    key: &'static str,
    f: Fmt,
) -> impl Fn(&RowId, usize, usize) -> (String, Style) + 'g {
    move |r, _, _| match r {
        RowId::Edge(k) => fmt::entry(g, g.edge(k).and_then(|e| e.metrics.get(key)), f),
        _ => (String::new(), Style::default()),
    }
}

pub fn communication(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("FROM", 18, move |r, _, _| {
            (edge_endpoints(g, r).0, Style::default())
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("", 1, |_, _, _| ("→".into(), theme::dim())),
        Column::new("TO", 18, move |r, _, _| {
            (edge_endpoints(g, r).1, Style::default())
        })
        .flex(),
        Column::new("PROTO", 5, move |r, _, _| match r {
            RowId::Edge(k) => (
                g.edge(k)
                    .and_then(|e| e.attr("protocol"))
                    .and_then(|a| a.as_text())
                    .unwrap_or("")
                    .to_uppercase(),
                Style::default().fg(theme::CYAN),
            ),
            _ => (String::new(), Style::default()),
        }),
        Column::new(
            "TX RATE",
            10,
            edge_metric(g, "tx_bytes_per_second", Fmt::Rate),
        )
        .right()
        .sort(SortKey::Net),
        Column::new("SOCK", 4, edge_metric(g, "sockets", Fmt::Count))
            .right()
            .prio(2),
        Column::new("EVIDENCE", 13, move |r, _, _| match r {
            RowId::Edge(k) => (
                g.edge(k)
                    .map_or("", |e| e.evidence.as_str())
                    .replace('_', " "),
                theme::dim(),
            ),
            _ => (String::new(), Style::default()),
        })
        .prio(3),
    ]
}

pub fn workloads(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("NAME", 16, move |r, _, _| {
            (
                node(g, r).map_or("", |n| n.label()).to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            )
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("STATE", 9, move |r, _, _| {
            let n = node(g, r);
            let s = n
                .and_then(|n| n.text("health").or_else(|| n.text("state")))
                .unwrap_or("—")
                .to_owned();
            let st = match s.as_str() {
                "running" | "healthy" => Style::default().fg(theme::GOOD),
                "unhealthy" | "exited" | "dead" => Style::default().fg(theme::BAD),
                _ => theme::dim(),
            };
            (s, st)
        })
        .prio(6),
        Column::new("CPU%", 6, colored_pct(g, "cpu_usage_percent", 1.0))
            .right()
            .sort(SortKey::Cpu),
        Column::new("MEMORY", 9, m(g, "memory_current_bytes", Fmt::Bytes))
            .right()
            .sort(SortKey::Mem),
        Column::new("WORKSET", 9, m(g, "memory_working_set_bytes", Fmt::Bytes))
            .right()
            .prio(3),
        Column::new("LIMIT", 9, m(g, "memory_limit_bytes", Fmt::Bytes))
            .right()
            .prio(2),
        Column::new("THROTTLE", 8, m(g, "cpu_throttled_percent", Fmt::Percent))
            .right()
            .prio(4),
        Column::new(
            "NET",
            10,
            pair(
                g,
                "network_rx_bytes_per_second",
                "network_tx_bytes_per_second",
                Fmt::Rate,
            ),
        )
        .right()
        .prio(5)
        .sort(SortKey::Net),
        Column::new(
            "DISK I/O",
            10,
            pair(
                g,
                "io_read_bytes_per_second",
                "io_write_bytes_per_second",
                Fmt::Rate,
            ),
        )
        .right()
        .prio(5)
        .sort(SortKey::Io),
        Column::new("PIDS", 5, m(g, "pids_current", Fmt::Count))
            .right()
            .prio(1),
        Column::new("IMAGE / UNIT", 14, move |r, _, _| {
            (
                node(g, r)
                    .and_then(|n| n.text("image").or_else(|| n.text("unit")))
                    .unwrap_or("")
                    .to_owned(),
                theme::dim(),
            )
        })
        .prio(1),
    ]
}

pub fn gpus(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("GPU", 18, move |r, _, _| {
            (
                node(g, r).map_or("", |n| n.label()).to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            )
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("DRIVER", 7, text(g, "driver")).prio(2),
        Column::new("UTIL", 10, move |r, _, w| {
            let v = node(g, r).and_then(|n| n.f64("gpu_usage_percent"));
            (
                crate::widgets::bar(v, w, theme::ACCENT)
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect(),
                Style::default().fg(theme::ACCENT),
            )
        })
        .prio(3),
        Column::new("%", 6, colored_pct(g, "gpu_usage_percent", 1.0))
            .right()
            .sort(SortKey::Gpu),
        Column::new("VRAM", 9, m(g, "vram_used_bytes", Fmt::Bytes))
            .right()
            .sort(SortKey::Mem),
        Column::new("OF", 9, m(g, "vram_total_bytes", Fmt::Bytes))
            .right()
            .prio(2),
        Column::new("TEMP", 6, m(g, "temperature_celsius", Fmt::Celsius))
            .right()
            .prio(4),
        Column::new("POWER", 8, m(g, "power_w", Fmt::Watts))
            .right()
            .prio(4),
        Column::new("CLOCK", 9, m(g, "frequency_mhz", Fmt::Mhz))
            .right()
            .prio(1),
    ]
}

pub fn gpu_clients(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("PROCESS", 18, move |r, _, _| {
            (edge_endpoints(g, r).0, Style::default())
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("GPU", 18, move |r, _, _| {
            (edge_endpoints(g, r).1, theme::dim())
        })
        .prio(3),
        Column::new(
            "ENGINE%",
            8,
            edge_metric(g, "gpu_usage_percent", Fmt::Percent),
        )
        .right()
        .sort(SortKey::Gpu),
        Column::new("MEMORY", 9, edge_metric(g, "gpu_memory_bytes", Fmt::Bytes))
            .right()
            .sort(SortKey::Mem),
    ]
}

pub fn sensors(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("SENSOR", 20, move |r, _, _| {
            (
                node(g, r).map_or("", |n| n.label()).to_owned(),
                Style::default(),
            )
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("KIND", 11, text(g, "sensor_kind")).prio(2),
        Column::new("VALUE", 10, move |r, _, _| {
            let Some(n) = node(g, r) else {
                return (String::new(), Style::default());
            };
            let f = match n.text("unit") {
                Some("celsius") => Fmt::Celsius,
                Some("watts") => Fmt::Watts,
                Some("rpm") => {
                    return (
                        n.f64("value")
                            .map_or("N/A".into(), |v| format!("{v:.0} RPM")),
                        Style::default(),
                    );
                }
                Some("volts") => {
                    return (
                        n.f64("value").map_or("N/A".into(), |v| format!("{v:.3} V")),
                        Style::default(),
                    );
                }
                _ => Fmt::Float2,
            };
            let (t, st) = fmt::metric(g, n, "value", f);
            let st = match (n.f64("value"), n.f64("critical").or_else(|| n.f64("max"))) {
                (Some(v), Some(c)) if f == Fmt::Celsius && c > 0.0 => {
                    Style::default().fg(theme::level(100.0 * v / c))
                }
                _ => st,
            };
            (t, st)
        })
        .right()
        .sort(SortKey::Value),
        Column::new("HIGH", 7, m(g, "max", Fmt::Float1))
            .right()
            .prio(3),
        Column::new("CRIT", 7, m(g, "critical", Fmt::Float1))
            .right()
            .prio(3),
    ]
}

pub fn power(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("SUPPLY", 8, move |r, _, _| {
            (
                node(g, r).map_or("", |n| n.label()).to_owned(),
                Style::default(),
            )
        })
        .sort(SortKey::Name),
        Column::new("STATUS", 12, move |r, _, _| {
            let n = node(g, r);
            (
                n.and_then(|n| {
                    n.text("status").map(str::to_owned).or_else(|| {
                        n.attr("online").map(|o| {
                            if *o == rtop_core::Attr::Bool(true) {
                                "online".into()
                            } else {
                                "offline".into()
                            }
                        })
                    })
                })
                .unwrap_or_default(),
                Style::default(),
            )
        }),
        Column::new("CHARGE", 12, move |r, _, w| {
            let v = node(g, r).and_then(|n| n.f64("capacity_percent"));
            (
                crate::widgets::bar(v, w, theme::level(100.0 - v.unwrap_or(101.0)))
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect(),
                Style::default().fg(if v.is_some_and(|v| v < 20.0) {
                    theme::BAD
                } else {
                    theme::GOOD
                }),
            )
        })
        .flex(),
        Column::new("%", 5, m(g, "capacity_percent", Fmt::Count)).right(),
        Column::new("POWER", 8, m(g, "power_w", Fmt::Watts))
            .right()
            .prio(4),
        Column::new("LEFT", 7, m(g, "time_remaining_seconds", Fmt::Duration))
            .right()
            .prio(5),
        Column::new("HEALTH", 7, m(g, "health_percent", Fmt::Percent))
            .right()
            .prio(3),
        Column::new("CYCLES", 6, m(g, "cycle_count", Fmt::Count))
            .right()
            .prio(2),
    ]
}

pub fn firewall(g: &SystemGraph) -> Vec<Column<'_>> {
    vec![
        Column::new("RULE", 20, move |r, _, _| {
            (
                node(g, r).map_or("", |n| n.label()).to_owned(),
                Style::default(),
            )
        })
        .flex()
        .sort(SortKey::Name),
        Column::new("CHAIN", 14, move |r, _, _| {
            let n = node(g, r);
            (
                n.map_or(String::new(), |n| {
                    format!(
                        "{} {}",
                        n.text("family").unwrap_or(""),
                        n.text("chain").unwrap_or("")
                    )
                }),
                theme::dim(),
            )
        })
        .prio(4),
        Column::new("VERDICT", 8, move |r, _, _| {
            let v = node(g, r)
                .and_then(|n| n.text("verdict"))
                .unwrap_or("")
                .to_owned();
            let st = match v.as_str() {
                "drop" | "reject" => Style::default().fg(theme::BAD),
                "accept" => Style::default().fg(theme::GOOD),
                _ => theme::dim(),
            };
            (v, st)
        }),
        Column::new("PKT/S", 9, m(g, "packets_per_second", Fmt::PerSecond))
            .right()
            .sort(SortKey::Net),
        Column::new("RATE", 10, m(g, "bytes_per_second", Fmt::Rate))
            .right()
            .prio(5),
        Column::new("PACKETS", 10, m(g, "packets_total", Fmt::Count))
            .right()
            .prio(3),
        Column::new("BYTES", 9, m(g, "bytes_total", Fmt::Bytes))
            .right()
            .prio(2),
    ]
}

pub fn events(g: &SystemGraph) -> Vec<Column<'_>> {
    let ev = move |r: &RowId| match r {
        RowId::Seq(seq) => seq
            .checked_sub(g.events_dropped())
            .and_then(|i| g.events().nth(i as usize)),
        _ => None,
    };
    vec![
        Column::new("TIME", 8, move |r, _, _| {
            (
                ev(r).and_then(|e| e.wall_time).map_or("—".into(), clock),
                theme::dim(),
            )
        }),
        Column::new("SEV", 4, move |r, _, _| match ev(r).map(|e| e.severity) {
            Some(Severity::Critical) => (
                "CRIT".into(),
                Style::default().fg(theme::BAD).add_modifier(Modifier::BOLD),
            ),
            Some(Severity::Warning) => ("WARN".into(), Style::default().fg(theme::WARN)),
            Some(Severity::Info) => ("INFO".into(), theme::dim()),
            None => (String::new(), Style::default()),
        }),
        Column::new("TYPE", 8, move |r, _, _| {
            (
                ev(r).map_or("", |e| e.category.as_str()).to_owned(),
                Style::default().fg(theme::CYAN),
            )
        })
        .prio(3),
        Column::new("EVENT", 30, move |r, _, _| {
            (
                ev(r).map_or(String::new(), |e| e.message.clone()),
                Style::default(),
            )
        })
        .flex(),
    ]
}

/// `HH:MM:SS` UTC of a Unix timestamp (no local timezone database needed).
fn clock(t: f64) -> String {
    let s = t as u64 % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn collectors(g: &SystemGraph) -> Vec<Column<'_>> {
    let st = move |r: &RowId| match r {
        RowId::Collector(c) => g
            .collectors()
            .find(|(id, _)| id.as_str() == *c)
            .map(|(_, s)| s),
        _ => None,
    };
    vec![
        Column::new("SOURCE", 11, |r, _, _| match r {
            RowId::Collector(c) => ((*c).to_owned(), Style::default()),
            _ => (String::new(), Style::default()),
        }),
        Column::new("STATUS", 11, move |r, _, _| match st(r) {
            Some(s) => (
                s.state.as_str().to_owned(),
                Style::default().fg(match s.state {
                    CollectorState::Available => theme::GOOD,
                    CollectorState::Degraded
                    | CollectorState::Pending
                    | CollectorState::Disabled => theme::WARN,
                    _ => theme::BAD,
                }),
            ),
            None => (String::new(), Style::default()),
        }),
        Column::new("COST", 8, move |r, _, _| {
            (
                st(r).and_then(|s| s.last_duration).map_or("—".into(), |d| {
                    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
                }),
                theme::dim(),
            )
        })
        .right()
        .prio(4),
        Column::new("EVERY", 6, move |r, _, _| {
            (
                st(r).map_or(String::new(), |s| {
                    format!("{:.1}s", s.interval.as_secs_f64())
                }),
                theme::dim(),
            )
        })
        .right()
        .prio(3),
        Column::new("MISSED", 6, move |r, _, _| {
            (
                st(r).map_or(String::new(), |s| s.missed_deadlines.to_string()),
                theme::dim(),
            )
        })
        .right()
        .prio(2),
        Column::new("DETAIL", 20, move |r, _, _| {
            (
                st(r)
                    .and_then(|s| {
                        s.detail
                            .clone()
                            .or_else(|| s.reason.map(|r| r.as_str().to_owned()))
                    })
                    .unwrap_or_default(),
                theme::dim(),
            )
        })
        .flex(),
    ]
}

/// Relationships of the inspected entity.
pub fn relations<'g>(
    g: &'g SystemGraph,
    inspected: Option<&'g rtop_core::EntityId>,
) -> Vec<Column<'g>> {
    vec![
        Column::new("RELATION", 13, move |r, _, _| match r {
            RowId::Edge(k) => {
                let outgoing = inspected == Some(&k.source);
                let label = match (k.kind, outgoing) {
                    (EdgeKind::ParentOf, true) => "child",
                    (EdgeKind::ParentOf, false) => "parent",
                    (EdgeKind::OwnsSocket, true) => "socket",
                    (EdgeKind::OwnsSocket, false) => "owned by",
                    (EdgeKind::MemberOf, true) => "cgroup",
                    (EdgeKind::MemberOf, false) => "member",
                    (EdgeKind::Contains, true) => "contains",
                    (EdgeKind::Contains, false) => "inside",
                    (EdgeKind::AccountedTo, true) => "accounted to",
                    (EdgeKind::AccountedTo, false) => "workload",
                    (EdgeKind::CommunicatesWith, true) => "sends to",
                    (EdgeKind::CommunicatesWith, false) => "receives from",
                    (EdgeKind::ConnectedTo, _) => "peer",
                    (EdgeKind::UsesGpu, true) => "uses GPU",
                    (EdgeKind::UsesGpu, false) => "GPU client",
                    (EdgeKind::Mounts, true) => "mounts",
                    (EdgeKind::Mounts, false) => "mounted by",
                    (EdgeKind::BackedBy, true) => "backed by",
                    (EdgeKind::BackedBy, false) => "backs",
                    (k, _) => k.as_str(),
                }
                .to_owned();
                (label, Style::default().fg(theme::CYAN))
            }
            _ => (String::new(), Style::default()),
        }),
        Column::new("ENTITY", 24, move |r, _, _| match r {
            RowId::Edge(k) => {
                let other = if inspected == Some(&k.source) {
                    &k.target
                } else {
                    &k.source
                };
                let kind = other.kind().map_or("", |k| k.as_str());
                let present = g.node(other).is_some();
                (
                    format!("{}  {kind}", label_of(g, other)),
                    if present {
                        Style::default()
                    } else {
                        theme::faint()
                    },
                )
            }
            _ => (String::new(), Style::default()),
        })
        .flex(),
        Column::new(
            "TRAFFIC",
            10,
            edge_metric(g, "tx_bytes_per_second", Fmt::Rate),
        )
        .right()
        .prio(2),
        Column::new("EVIDENCE", 13, move |r, _, _| match r {
            RowId::Edge(k) => (
                g.edge(k)
                    .map_or("", |e| e.evidence.as_str())
                    .replace('_', " "),
                theme::dim(),
            ),
            _ => (String::new(), Style::default()),
        })
        .prio(1),
    ]
}

pub fn for_panel<'g>(
    g: &'g SystemGraph,
    p: Panel,
    ncpu: f64,
    inspected: Option<&'g rtop_core::EntityId>,
) -> Vec<Column<'g>> {
    match p {
        Panel::Processes => processes(g, ncpu),
        Panel::Cpus => cpus(g),
        Panel::BlockDevices => block_devices(g),
        Panel::Mounts => mounts(g),
        Panel::Interfaces => interfaces(g),
        Panel::Sockets => sockets(g),
        Panel::Communication => communication(g),
        Panel::Containers | Panel::Services => workloads(g),
        Panel::Gpus => gpus(g),
        Panel::GpuClients => gpu_clients(g),
        Panel::Sensors => sensors(g),
        Panel::Batteries => power(g),
        Panel::Events => events(g),
        Panel::Firewall => firewall(g),
        Panel::Collectors => collectors(g),
        Panel::Relations => relations(g, inspected),
    }
}

pub fn empty_text(p: Panel) -> &'static str {
    match p {
        Panel::Containers => "No containers: no Docker/Podman/containerd cgroups found",
        Panel::Gpus => "No supported GPU or accelerator found",
        Panel::GpuClients => "No process is using a GPU (or per-process accounting is unavailable)",
        Panel::Communication => "No process-to-process connections observed",
        Panel::Batteries => "No battery or power supply",
        Panel::Events => "No security-related events observed since start",
        Panel::Firewall => {
            "No nftables rules (or reading them needs CAP_NET_ADMIN; see Telemetry sources)"
        }
        Panel::Relations => "No relationships observed",
        _ => "Nothing to show",
    }
}
