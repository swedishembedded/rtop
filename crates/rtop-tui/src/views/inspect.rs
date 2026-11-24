// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Entity inspection (progressive reveal levels 2 and 3): every attribute,
//! every metric with its availability, every relationship (navigable), and
//! for processes the on-demand deep detail.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use rtop_app::state::Detail;
use rtop_collectors::metrics::{Unit, describe, unit_of};
use rtop_core::units::format_bytes;
use rtop_core::{Attr, EntityKind};

use super::{Ctx, cols, panel_table, rows};
use crate::fmt::{self, Fmt};
use crate::hits::HitMap;
use crate::theme;
use crate::widgets::{framed, panel};

fn fmt_for(key: &str) -> Fmt {
    match unit_of(key) {
        Unit::Percent => Fmt::Percent,
        Unit::Bytes => Fmt::Bytes,
        Unit::BytesPerSecond => Fmt::Rate,
        Unit::PerSecond => Fmt::PerSecond,
        Unit::Seconds => Fmt::Duration,
        Unit::Milliseconds => Fmt::Ms,
        Unit::Celsius => Fmt::Celsius,
        Unit::Watts => Fmt::Watts,
        Unit::Mhz => Fmt::Mhz,
        Unit::Count => Fmt::Count,
        _ => Fmt::Float2,
    }
}

fn attr_text(a: &Attr) -> String {
    match a {
        Attr::Text(s) => s.clone(),
        Attr::Int(v) => v.to_string(),
        Attr::UInt(v) => v.to_string(),
        Attr::Bool(v) => v.to_string(),
    }
}

pub fn render(f: &mut Frame, area: Rect, c: &Ctx<'_>, hits: &mut HitMap) {
    let Some(id) = c.s.location.inspect.clone() else {
        return;
    };
    let node = c.g.node(&id);
    let kind = id.kind().map_or("entity", |k| k.as_str());
    let title = node.map_or_else(
        || id.key().to_owned(),
        |n| rtop_app::project::label_of(c.g, &n.id),
    );
    // Header: identity and descriptive attributes.
    let mut head = vec![Line::from(vec![
        Span::styled(format!("{kind} "), theme::dim()),
        Span::styled(title, theme::title().fg(theme::ACCENT)),
        Span::styled(format!("  {id}"), theme::faint()),
    ])];
    match node {
        None => head.push(Line::from(Span::styled(
            "No longer observed: the entity exited or was removed. Actions on it are refused.",
            Style::default().fg(theme::WARN),
        ))),
        Some(n) => {
            let mut spans = Vec::new();
            for (k, a) in n
                .attrs
                .iter()
                .filter(|(k, _)| !matches!(**k, "name" | "cmdline"))
            {
                spans.push(Span::styled(format!("{k} "), theme::dim()));
                spans.push(Span::raw(format!(
                    "{}  ",
                    fmt::truncate(&attr_text(&a.value), 48)
                )));
            }
            head.push(Line::from(spans));
            if let Some(cmd) = n.text("cmdline").filter(|c| !c.is_empty()) {
                head.push(Line::from(vec![
                    Span::styled("cmd ", theme::dim()),
                    Span::raw(cmd.to_owned()),
                ]));
            }
        }
    }
    let head_h = head.len() as u16 + 2;
    let r = rows(
        area,
        [
            Constraint::Length(head_h.min(area.height / 3)),
            Constraint::Min(4),
        ],
    );
    let inner = framed(
        f,
        r[0],
        panel(Line::from(Span::styled(" Inspect ", theme::title())), false),
    );
    f.render_widget(
        Paragraph::new(head).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );

    let is_process = id.kind() == Some(EntityKind::Process);
    let wide = area.width >= 110;
    let body = if is_process && r[1].height >= 16 {
        rows(r[1], [Constraint::Percentage(55), Constraint::Min(6)])
    } else {
        vec![r[1]]
    };
    let top = if wide {
        cols(
            body[0],
            [Constraint::Percentage(42), Constraint::Percentage(58)],
        )
    } else {
        rows(
            body[0],
            [Constraint::Percentage(50), Constraint::Percentage(50)],
        )
    };

    // Metrics with value, availability and meaning.
    let mut lines = Vec::new();
    if let Some(n) = node {
        for (k, e) in &n.metrics {
            let (v, st) = fmt::entry(c.g, Some(e), fmt_for(k));
            let desc = describe(k).map_or("", |d| d.description);
            let series: Vec<Option<f64>> = c.series(&n.id, k);
            let spark = if series.iter().flatten().count() > 1 {
                crate::widgets::sparkline(
                    &series,
                    12,
                    series.iter().flatten().fold(0.0f64, |a, b| a.max(*b)),
                    theme::ACCENT,
                )
            } else {
                Span::raw(" ".repeat(12))
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<28}", fmt::truncate(k, 28)), theme::dim()),
                Span::styled(format!("{v:>12} "), st),
                spark,
                Span::styled(format!(" {desc}"), theme::faint()),
            ]));
        }
    }
    let minner = framed(
        f,
        top[0],
        panel(Line::from(Span::styled(" Metrics ", theme::title())), false),
    );
    f.render_widget(Paragraph::new(lines), minner);
    panel_table(f, top[1], c, 0, hits);

    if body.len() > 1 {
        detail(f, body[1], c);
    }
}

fn detail(f: &mut Frame, area: Rect, c: &Ctx<'_>) {
    let fold = |s: &str| c.s.folded.contains(s);
    let mut left = Vec::new();
    let mut right = Vec::new();
    match &c.s.detail {
        None => left.push(Line::from(Span::styled(
            "No detail requested",
            theme::faint(),
        ))),
        Some(Detail::Loading(_)) => left.push(Line::from(Span::styled(
            "Reading threads, descriptors and memory maps…",
            theme::dim(),
        ))),
        Some(Detail::Failed(_, e)) => left.push(Line::from(Span::styled(
            format!("Detail unavailable: {e}"),
            Style::default().fg(theme::WARN),
        ))),
        Some(Detail::Ready(d)) => {
            let section = |out: &mut Vec<Line<'static>>, name: &'static str, title: String| {
                let marker = if fold(name) { "▸" } else { "▾" };
                out.push(Line::from(Span::styled(
                    format!("{marker} {title}"),
                    theme::title().add_modifier(Modifier::UNDERLINED),
                )));
                !fold(name)
            };
            match &d.memory {
                Ok(m) => {
                    left.push(Line::from(Span::styled(
                        "▾ Memory (smaps_rollup)",
                        theme::title().add_modifier(Modifier::UNDERLINED),
                    )));
                    let b = |v: Option<u64>| v.map_or("N/A".into(), |v| format_bytes(v as f64));
                    left.push(Line::from(format!(
                        "  RSS {}   PSS {}   private {}   shared {}   swap {}",
                        b(m.rss),
                        b(m.pss),
                        b(m.private),
                        b(m.shared),
                        b(m.swap)
                    )));
                }
                Err(e) => left.push(Line::from(Span::styled(
                    format!("▾ Memory: N/A ({e})"),
                    theme::faint(),
                ))),
            }
            left.push(Line::from(vec![
                Span::styled("  exe ", theme::dim()),
                Span::raw(d.exe.clone().unwrap_or_else(|e| format!("N/A ({e})"))),
                Span::styled("  cwd ", theme::dim()),
                Span::raw(d.cwd.clone().unwrap_or_else(|e| format!("N/A ({e})"))),
            ]));
            left.push(Line::from(vec![
                Span::styled("  oom score ", theme::dim()),
                Span::raw(d.oom_score.map_or("N/A".into(), |v| v.to_string())),
                Span::styled("  waiting in ", theme::dim()),
                Span::raw(d.wchan.clone().unwrap_or_else(|| "—".into())),
            ]));
            if section(
                &mut left,
                "threads",
                format!(
                    "Threads ({}{})",
                    d.threads.len(),
                    if d.threads_truncated { "+" } else { "" }
                ),
            ) {
                for t in d.threads.iter().take(area.height as usize) {
                    left.push(Line::from(vec![
                        Span::styled(format!("  {:>7} ", t.tid), theme::dim()),
                        Span::raw(format!("{:<16} ", fmt::truncate(&t.name, 16))),
                        Span::styled(format!("{:<11}", t.state), theme::dim()),
                        Span::raw(format!("{:>9.1}s", t.cpu_time_seconds)),
                        Span::styled(
                            t.last_cpu.map_or(String::new(), |c| format!("  cpu{c}")),
                            theme::faint(),
                        ),
                    ]));
                }
            }
            match &d.fds {
                Ok(fds) => {
                    if section(
                        &mut right,
                        "descriptors",
                        format!(
                            "Open descriptors ({}{})",
                            fds.len(),
                            if d.fds_truncated { "+" } else { "" }
                        ),
                    ) {
                        for fd in fds.iter().take(area.height as usize) {
                            right.push(Line::from(vec![
                                Span::styled(format!("  {:>4} ", fd.fd), theme::dim()),
                                Span::styled(
                                    format!("{:<10} ", fd.kind),
                                    Style::default().fg(theme::CYAN),
                                ),
                                Span::raw(fd.target.clone()),
                            ]));
                        }
                    }
                }
                Err(e) => right.push(Line::from(Span::styled(
                    format!("▾ Descriptors: N/A ({e})"),
                    theme::faint(),
                ))),
            }
            if section(&mut right, "namespaces", "Namespaces".into()) {
                right.push(Line::from(Span::raw(format!(
                    "  {}",
                    d.namespaces
                        .iter()
                        .map(|(k, v)| format!("{k}={}", v.trim_start_matches(&format!("{k}:"))))
                        .collect::<Vec<_>>()
                        .join("  ")
                ))));
            }
            if let Ok(limits) = &d.limits
                && section(&mut right, "limits", "Resource limits (soft / hard)".into())
            {
                for (name, soft, hard) in limits.iter().filter(|(n, _, _)| {
                    n.contains("open files")
                        || n.contains("processes")
                        || n.contains("address")
                        || n.contains("locked")
                        || n.contains("stack")
                }) {
                    right.push(Line::from(vec![
                        Span::styled(format!("  {name:<22}"), theme::dim()),
                        Span::raw(format!("{soft} / {hard}")),
                    ]));
                }
            }
        }
    }
    let inner = framed(
        f,
        area,
        panel(
            Line::from(vec![
                Span::styled(" Deep inspection ", theme::title()),
                Span::styled("zc/zo fold · zM/zR all ", theme::faint()),
            ]),
            false,
        ),
    );
    let parts = if inner.width >= 100 {
        cols(
            inner,
            [Constraint::Percentage(50), Constraint::Percentage(50)],
        )
    } else {
        vec![inner]
    };
    if parts.len() == 2 {
        f.render_widget(Paragraph::new(left), parts[0]);
        f.render_widget(Paragraph::new(right), parts[1]);
    } else {
        left.extend(right);
        f.render_widget(Paragraph::new(left), parts[0]);
    }
}
