// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Dialogs, menus and the generated help screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use rtop_app::keymap::{BINDINGS, describe};
use rtop_app::reducer::filtered_signals;
use rtop_app::state::Modal;
use rtop_app::{AppState, Hit};

use crate::hits::HitMap;
use crate::theme;
use crate::widgets::{framed, panel};

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn popup(f: &mut Frame, r: Rect, title: Line<'static>, hits: &mut HitMap) -> Rect {
    f.render_widget(Clear, r);
    hits.modal = Some(r);
    framed(f, r, panel(title, true))
}

fn items(
    f: &mut Frame,
    inner: Rect,
    lines: Vec<(Line<'static>, bool)>,
    hits: &mut HitMap,
    first_index: usize,
) {
    for (i, (l, sel)) in lines.into_iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let r = Rect {
            y,
            height: 1,
            ..inner
        };
        let st = if sel {
            Style::default().bg(theme::SELECT_BG)
        } else {
            Style::default()
        };
        f.render_widget(Paragraph::new(l).style(st), r);
        hits.push(r, Hit::MenuItem(first_index + i));
    }
}

pub fn render(f: &mut Frame, area: Rect, s: &AppState, hits: &mut HitMap) {
    let Some(m) = &s.modal else { return };
    match m {
        Modal::Help { scroll } => help(f, area, *scroll, hits),
        Modal::SortMenu { cursor } => {
            let keys = s.location.table().panel.sort_keys();
            let r = centered(area, 36, keys.len() as u16 + 2);
            let inner = popup(
                f,
                r,
                Line::from(Span::styled(" Sort by ", theme::title())),
                hits,
            );
            let cur = s.location.table().sort;
            let lines = keys
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    (
                        Line::from(vec![Span::raw(format!(
                            " {} {}",
                            if *k == cur { "●" } else { " " },
                            k.label()
                        ))]),
                        i == *cursor,
                    )
                })
                .collect();
            items(f, inner, lines, hits, 0);
        }
        Modal::Confirm {
            targets,
            signal_name,
            ..
        } => {
            let verb = match *signal_name {
                "SIGTERM" => "Terminate",
                "SIGKILL" => "Force kill",
                "SIGSTOP" => "Suspend",
                "SIGCONT" => "Resume",
                other => other,
            };
            let h = (targets.len().min(8) as u16) + 6;
            let r = centered(area, 60, h);
            let inner = popup(
                f,
                r,
                Line::from(Span::styled(
                    format!(
                        " {verb} {}? ",
                        if targets.len() == 1 {
                            "process"
                        } else {
                            "processes"
                        }
                    ),
                    Style::default()
                        .fg(theme::WARN)
                        .add_modifier(Modifier::BOLD),
                )),
                hits,
            );
            let mut lines: Vec<Line<'static>> = targets
                .iter()
                .take(8)
                .map(|t| {
                    Line::from(vec![
                        Span::styled(format!("  {:>7}  ", t.identity.pid), theme::dim()),
                        Span::raw(t.name.clone()),
                    ])
                })
                .collect();
            if targets.len() > 8 {
                lines.push(Line::from(Span::styled(
                    format!("  … and {} more", targets.len() - 8),
                    theme::dim(),
                )));
            }
            lines.push(Line::from(Span::styled(format!("  {signal_name} is sent through a pidfd only if each process is still the one listed."), theme::faint())));
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                Rect {
                    height: inner.height.saturating_sub(1),
                    ..inner
                },
            );
            let by = inner.y + inner.height.saturating_sub(1);
            let yes = Rect {
                x: inner.x + 2,
                y: by,
                width: 20,
                height: 1,
            };
            let no = Rect {
                x: inner.x + 24,
                y: by,
                width: 14,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" y ", theme::key()),
                    Span::styled(
                        format!(" {verb} "),
                        Style::default().fg(theme::BAD).add_modifier(Modifier::BOLD),
                    ),
                ])),
                yes,
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" n ", theme::key()),
                    Span::raw(" Cancel "),
                ])),
                no,
            );
            hits.push(yes, Hit::MenuItem(0));
            hits.push(no, Hit::MenuItem(1));
        }
        Modal::Signals {
            targets,
            query,
            cursor,
        } => {
            let list = filtered_signals(query);
            let h = (list.len() as u16 + 4)
                .min(area.height.saturating_sub(2))
                .max(6);
            let r = centered(area, 58, h);
            let who = if targets.len() == 1 {
                format!("{} ({})", targets[0].name, targets[0].identity.pid)
            } else {
                format!("{} processes", targets.len())
            };
            let inner = popup(
                f,
                r,
                Line::from(Span::styled(
                    format!(" Send signal to {who} "),
                    theme::title(),
                )),
                hits,
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" search: ", theme::dim()),
                    Span::raw(query.clone()),
                    Span::styled("▏", theme::accent()),
                ])),
                Rect { height: 1, ..inner },
            );
            let body = Rect {
                y: inner.y + 1,
                height: inner.height.saturating_sub(1),
                ..inner
            };
            let skip = cursor.saturating_sub(body.height as usize - 1);
            let lines = list
                .iter()
                .enumerate()
                .skip(skip)
                .map(|(i, sig)| {
                    (
                        Line::from(vec![
                            Span::styled(format!(" {:<12}", sig.name), theme::title()),
                            Span::styled(format!("{:>3}  ", sig.number), theme::dim()),
                            Span::raw(sig.description),
                        ]),
                        i == *cursor,
                    )
                })
                .collect();
            items(f, body, lines, hits, skip);
        }
        Modal::Context {
            title,
            items: menu,
            cursor,
            x,
            y,
            ..
        } => {
            let w = menu
                .iter()
                .map(|i| i.label.len() + i.hint.len() + 6)
                .max()
                .unwrap_or(20)
                .max(title.len() + 4) as u16;
            let h = menu.len() as u16 + 2;
            let x = (*x).min(area.x + area.width.saturating_sub(w));
            let y = (*y).min(area.y + area.height.saturating_sub(h));
            let r = Rect {
                x,
                y,
                width: w.min(area.width),
                height: h.min(area.height),
            };
            let inner = popup(
                f,
                r,
                Line::from(Span::styled(format!(" {title} "), theme::title())),
                hits,
            );
            let lines = menu
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let danger = it.label.contains("kill") || it.label.contains("Terminate");
                    let gap =
                        (inner.width as usize).saturating_sub(it.label.len() + it.hint.len() + 2);
                    (
                        Line::from(vec![
                            Span::styled(
                                format!(" {}", it.label),
                                if danger {
                                    Style::default().fg(theme::BAD)
                                } else {
                                    Style::default()
                                },
                            ),
                            Span::raw(" ".repeat(gap)),
                            Span::styled(format!("{} ", it.hint), theme::dim()),
                        ]),
                        i == *cursor,
                    )
                })
                .collect();
            items(f, inner, lines, hits, 0);
        }
    }
}

fn help(f: &mut Frame, area: Rect, scroll: usize, hits: &mut HitMap) {
    let r = centered(
        area,
        area.width.saturating_sub(4).min(110),
        area.height.saturating_sub(2),
    );
    let inner = popup(
        f,
        r,
        Line::from(vec![
            Span::styled(" rtop help ", theme::title()),
            Span::styled(" j/k scroll · Esc close ", theme::faint()),
        ]),
        hits,
    );
    let mut lines = vec![
        Line::from(Span::styled(
            "Every command is also reachable with the mouse: click tabs, rows and headers; double-click to inspect; right-click for actions; drag a border between panes to resize them.",
            theme::dim(),
        )),
        Line::from(Span::styled(
            "Filters: chrome · user:root · cpu>10 · rss>1GiB · state:D · age>1h · name:x AND rss>200MiB · a OR b · !kworker",
            theme::dim(),
        )),
        Line::from(""),
    ];
    let mut group = "";
    let half = inner.width as usize / 2;
    let mut pending: Option<String> = None;
    for b in BINDINGS {
        if b.group != group {
            if let Some(p) = pending.take() {
                lines.push(Line::from(p));
            }
            group = b.group;
            lines.push(Line::from(Span::styled(group.to_owned(), theme::accent())));
        }
        let entry = format!("  {:<10} {}", describe(b.keys), b.help);
        match pending.take() {
            None if inner.width >= 80 => {
                pending = Some(format!("{:<half$}", crate::fmt::truncate(&entry, half)))
            }
            None => lines.push(Line::from(entry)),
            Some(p) => lines.push(Line::from(format!("{p}{entry}"))),
        }
    }
    if let Some(p) = pending {
        lines.push(Line::from(p));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Commands: :q  :help  :sort <key> [asc|desc]  :filter <expr>  :nofilter  :signal <name>  :view <name>  :tree  :emergency  :pause  :interval <factor>", theme::dim())));
    lines.push(Line::from(Span::styled("Values: 0 measured zero · N/A (reason) unavailable · … pending first rate · ~ estimate · * stale (source not answering)", theme::dim())));
    let max_scroll = lines.len().saturating_sub(inner.height as usize);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll.min(max_scroll) as u16, 0)),
        inner,
    );
}
