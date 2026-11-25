// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Headless rendering of every screen at many terminal sizes: no panics,
//! expected content present, clickable regions consistent with what is
//! drawn, and hostile process names never reaching the terminal raw.

use proptest::prelude::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rtop_app::input::{Key, KeyCode};
use rtop_app::{AppState, Event, Hit, reduce};
use rtop_collectors::demo::DemoMachine;
use rtop_collectors::{Collector, SampleContext, Subscriptions};
use rtop_core::{
    CollectorId, EntityId, EntityKind, GraphPatch, NodeUpdate, SystemGraph, Timestamp,
};
use rtop_tui::HitMap;

struct World {
    g: SystemGraph,
    s: AppState,
    t: u64,
}

fn world() -> World {
    let m = DemoMachine::new();
    let mut cs: Vec<Box<dyn Collector>> = m.collectors(&m.env(Subscriptions::all()));
    let mut w = World {
        g: SystemGraph::default(),
        s: AppState::new(true),
        t: 0,
    };
    for i in 1..=3 {
        for c in &mut cs {
            if let Ok(x) = c.sample(&SampleContext {
                now: Timestamp::from_millis(i * 1000),
                sequence: i,
            }) {
                w.g.apply(x.patch);
            }
        }
        m.advance(1.0);
        w.ev(Event::GraphChanged);
    }
    w
}

impl World {
    fn ev(&mut self, e: Event) {
        self.t += 1;
        reduce(
            &mut self.s,
            &self.g,
            e,
            Timestamp::from_millis(10_000 + self.t),
            &mut Vec::new(),
        );
    }

    fn keys(&mut self, k: &str) {
        for c in k.chars() {
            self.ev(Event::Key(Key::ch(c)));
        }
    }

    fn draw(&self, w: u16, h: u16) -> (String, HitMap) {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::default();
        t.draw(|f| rtop_tui::render(f, &self.s, &self.g, &mut hits))
            .unwrap();
        let buf = t.backend().buffer();
        let mut text = String::new();
        for y in 0..h {
            for x in 0..w {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        (text, hits)
    }
}

const SIZES: [(u16, u16); 8] = [
    (1, 1),
    (20, 8),
    (40, 15),
    (80, 24),
    (120, 40),
    (160, 50),
    (240, 70),
    (300, 100),
];

#[test]
fn every_view_renders_at_every_size_with_consistent_hits() {
    let mut w = world();
    let screens: [(&str, &str); 12] = [
        ("0", "Processes"),
        ("1", "COMMAND"),
        ("2", "Logical CPUs"),
        ("3", "Memory"),
        ("4", "Block devices"),
        ("5", "Communication"),
        ("6", "postgres-db"),
        ("7", "Demo Radeon"),
        ("8", "Telemetry sources"),
        ("9", "Package id 0"),
        ("1j\r", "Relationships"),
        ("\x1b1jx", "Terminate"),
    ];
    for (keys, expect) in screens {
        for c in keys.chars() {
            let k = match c {
                '\r' => Key::plain(KeyCode::Enter),
                '\x1b' => Key::plain(KeyCode::Esc),
                c => Key::ch(c),
            };
            w.ev(Event::Key(k));
        }
        for (cols, rows) in SIZES {
            let (text, hits) = w.draw(cols, rows);
            if cols >= 120 {
                assert!(
                    text.contains(expect),
                    "{keys:?} at {cols}x{rows} lacks {expect:?}:\n{text}"
                );
            }
            // Every clickable row is a row the app knows about.
            for (_, h) in hits.iter() {
                if let Hit::Row { panel, row } = h {
                    let idx =
                        w.s.location
                            .tables
                            .iter()
                            .position(|t| t.panel == *panel)
                            .expect("panel of current location");
                    assert!(w.s.panel_rows(idx).position(row).is_some());
                }
            }
        }
        w.ev(Event::Key(Key::plain(KeyCode::Esc)));
    }
}

#[test]
fn hostile_names_never_reach_the_terminal_raw() {
    let mut w = world();
    let mut p = GraphPatch::new(CollectorId("test"), 1, Timestamp(1));
    p.node(
        NodeUpdate::new(
            EntityKind::Process,
            EntityId::new(EntityKind::Process, "666:1"),
        )
        .attr("pid", 666u64)
        .text("name", "evil\x1b]0;pwned\x07")
        .text("cmdline", "\x1b[2J\x1b[31mrm -rf"),
    );
    w.g.apply(p);
    w.ev(Event::GraphChanged);
    w.keys("1");
    let (text, _) = w.draw(200, 60);
    assert!(!text.contains('\x1b') && !text.contains('\x07'));
    assert!(
        text.contains("\\x1b[2J"),
        "escapes are shown, not interpreted"
    );
}

#[test]
fn every_modal_and_help_render() {
    let mut w = world();
    for k in ["1jgs", "\x1b", "1jm", "\x1b", "s?", "\x1b"] {
        for c in k.chars() {
            w.ev(Event::Key(if c == '\x1b' {
                Key::plain(KeyCode::Esc)
            } else {
                Key::ch(c)
            }));
        }
        let (_, hits) = w.draw(120, 40);
        if w.s.modal.is_some() {
            assert!(hits.modal.is_some(), "open modal registers its area");
        }
    }
    w.ev(Event::Key(Key::plain(KeyCode::F(1))));
    let (text, _) = w.draw(120, 40);
    assert!(text.contains("rtop help") && text.contains("System overview"));
    // The rest of the generated reference is reachable by scrolling.
    for _ in 0..40 {
        w.keys("j");
    }
    let (text, _) = w.draw(120, 40);
    assert!(text.contains("Force kill"), "{text}");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]
    #[test]
    fn arbitrary_sizes_never_panic(view in 0u32..10, cols in 1u16..320, rows in 1u16..120, inspect in any::<bool>()) {
        let mut w = world();
        w.keys(&view.to_string());
        if inspect {
            w.keys("j");
            w.ev(Event::Key(Key::plain(KeyCode::Enter)));
        }
        w.ev(Event::Resize { width: cols, height: rows });
        let _ = w.draw(cols, rows);
    }
}

#[test]
fn every_cpu_is_visible_without_scrolling_at_common_sizes() {
    for (cpus, sizes) in [
        (16u32, &[(50u16, 16u16), (80, 24), (120, 36)][..]),
        (22, &[(80, 24), (160, 48)]),
        (64, &[(50, 16), (80, 24), (120, 36), (160, 48)]),
        (256, &[(80, 24), (120, 36), (160, 48), (300, 100)]),
    ] {
        let m = DemoMachine::with_cpus(cpus);
        let mut cs: Vec<Box<dyn Collector>> = m.collectors(&m.env(Subscriptions::all()));
        let mut w = World {
            g: SystemGraph::default(),
            s: AppState::new(true),
            t: 0,
        };
        for i in 1..=2 {
            for c in &mut cs {
                if let Ok(x) = c.sample(&SampleContext {
                    now: Timestamp::from_millis(i * 1000),
                    sequence: i,
                }) {
                    w.g.apply(x.patch);
                }
            }
            m.advance(1.0);
            w.ev(Event::GraphChanged);
        }
        w.keys("2");
        for &(cols, rows) in sizes {
            w.ev(Event::Resize {
                width: cols,
                height: rows,
            });
            let (_, hits) = w.draw(cols, rows);
            let shown = hits
                .iter()
                .filter(|(_, h)| {
                    matches!(
                        h,
                        Hit::Row {
                            panel: rtop_app::Panel::Cpus,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(
                shown, cpus as usize,
                "{cpus} CPUs at {cols}x{rows}: {shown} visible"
            );
        }
    }
}

// ----- draggable pane borders ----------------------------------------------

use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use rtop_app::{MouseAction, Panel, View};
use rtop_tui::terminal::MouseTracker;

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

impl World {
    /// Feeds raw mouse events through the tracker and the reducer, redrawing
    /// after each one as the event loop does.
    fn mouse(&mut self, size: (u16, u16), events: &[MouseEvent]) -> HitMap {
        let mut tracker = MouseTracker::default();
        let (_, mut hits) = self.draw(size.0, size.1);
        let mut now = std::time::Instant::now();
        for e in events {
            // Far enough apart that two presses are not a double click.
            now += std::time::Duration::from_secs(1);
            if let Some(a) = tracker.translate(*e, &hits, now) {
                self.ev(Event::Mouse(a));
            }
            hits = self.draw(size.0, size.1).1;
        }
        hits
    }

    fn drag(&mut self, size: (u16, u16), from: (u16, u16), to: (u16, u16)) -> HitMap {
        self.mouse(
            size,
            &[
                mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1),
                mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1),
                mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1),
            ],
        )
    }
}

fn rect(hits: &HitMap, p: Panel) -> Rect {
    hits.panel_rect(p)
        .unwrap_or_else(|| panic!("{p:?} not drawn"))
}

/// The border between two stacked panes follows the mouse, keeps its
/// proportion when the terminal is resized, never squeezes a pane below a
/// usable size, and a double click restores the default layout.
#[test]
fn dragging_a_border_resizes_the_adjacent_panes() {
    let mut w = world();
    w.keys("4");
    let size = (120, 40);
    let (_, hits) = w.draw(size.0, size.1);
    let (top, bottom) = (rect(&hits, Panel::BlockDevices), rect(&hits, Panel::Mounts));
    assert_eq!(top.y + top.height, bottom.y, "adjacent panes");
    let default_top = top;

    // A plain click on the border focuses the pane below, as before, and
    // moves nothing.
    let hits = w.mouse(
        size,
        &[
            mouse(MouseEventKind::Down(MouseButton::Left), 30, bottom.y),
            mouse(MouseEventKind::Up(MouseButton::Left), 30, bottom.y),
        ],
    );
    assert_eq!(rect(&hits, Panel::BlockDevices), default_top);
    assert_eq!(w.s.location.focus, 1);

    // Dragging the border (either of its two lines) moves it to the mouse.
    let hits = w.drag(size, (30, bottom.y), (30, bottom.y - 4));
    let (top, bottom) = (rect(&hits, Panel::BlockDevices), rect(&hits, Panel::Mounts));
    assert_eq!(top.height, default_top.height - 4);
    assert_eq!(top.y + top.height, bottom.y);
    assert_eq!(
        bottom.y + bottom.height,
        39,
        "the pair still fills the view"
    );
    // Whichever of the two border lines is grabbed stays under the pointer:
    // here the upper pane's bottom edge.
    let hits = w.drag(size, (30, bottom.y - 1), (30, bottom.y + 6));
    let top = rect(&hits, Panel::BlockDevices);
    assert_eq!(top.y + top.height - 1, bottom.y + 6);

    // The boundary is stored relative to the view, so it scales with it.
    let moved = rect(&hits, Panel::Mounts).y;
    let (_, big) = w.draw(120, 80);
    let scaled = rect(&big, Panel::Mounts).y;
    let expect = f64::from(moved) * 80.0 / 40.0;
    assert!(
        (f64::from(scaled) - expect).abs() <= 3.0,
        "{scaled} vs {expect}"
    );

    // Dragging past the edge leaves both panes usable.
    let b = rect(&hits, Panel::Mounts).y;
    let hits = w.drag(size, (30, b), (30, 0));
    assert!(rect(&hits, Panel::BlockDevices).height >= 3);
    let b = rect(&hits, Panel::Mounts).y;
    let hits = w.drag(size, (30, b), (30, 39));
    assert!(rect(&hits, Panel::Mounts).height >= 3);

    // Double click on the border: back to the default.
    let b = rect(&hits, Panel::Mounts).y;
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 30, b);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 30, b);
    let mut tracker = MouseTracker::default();
    let now = std::time::Instant::now();
    let mut actions = Vec::new();
    for (i, e) in [down, up, down, up].into_iter().enumerate() {
        let t = now + std::time::Duration::from_millis(50 * i as u64);
        actions.extend(tracker.translate(e, &hits, t));
    }
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, MouseAction::ResetSplit(_))),
        "{actions:?}"
    );
    for a in actions {
        w.ev(Event::Mouse(a));
    }
    let (_, hits) = w.draw(size.0, size.1);
    assert_eq!(rect(&hits, Panel::BlockDevices), default_top);
}

/// Every border of every view can be dragged to both extremes without a pane
/// leaving the screen, overlapping another, or becoming too small to show
/// its frame and one line.
#[test]
fn every_border_of_every_view_stays_usable_at_its_extremes() {
    for size in [(80, 24), (200, 60)] {
        for (digit, view) in (b'0'..).zip(View::ALL) {
            let mut w = world();
            w.ev(Event::Key(Key::ch(char::from(digit))));
            let (_, hits) = w.draw(size.0, size.1);
            let splitters: Vec<_> = hits.splitters().copied().collect();
            for sp in splitters {
                for target in [0, u16::MAX] {
                    let (from, to) = match sp.axis {
                        rtop_tui::hits::Axis::Vertical => (
                            (sp.line.x + 1, sp.line.y),
                            (sp.line.x + 1, target.min(size.1 - 1)),
                        ),
                        rtop_tui::hits::Axis::Horizontal => (
                            (sp.line.x, sp.line.y + 1),
                            (target.min(size.0 - 1), sp.line.y + 1),
                        ),
                    };
                    let hits = w.drag(size, from, to);
                    let panes: Vec<Rect> = hits.panel_rects().collect();
                    for (i, a) in panes.iter().enumerate() {
                        assert!(
                            a.width >= 3 && a.height >= 3,
                            "{view:?} {size:?}: pane {a:?} too small"
                        );
                        assert!(a.x + a.width <= size.0 && a.y + a.height <= size.1);
                        for b in &panes[i + 1..] {
                            assert!(!a.intersects(*b), "{view:?} {size:?}: {a:?} overlaps {b:?}");
                        }
                    }
                }
            }
        }
    }
}

/// Load cards show their percentage above a rolling graph, without a
/// horizontal bar between the two.
#[test]
fn load_cards_have_no_bar_above_their_graph() {
    let w = world();
    let (text, hits) = w.draw(160, 48);
    let lines: Vec<&str> = text.lines().collect();
    for view in [View::Cpu, View::Memory, View::Gpu] {
        let card = hits
            .iter()
            .find(|(r, h)| *h == Hit::Tab(view) && r.height > 3)
            .map(|(r, _)| *r)
            .unwrap_or_else(|| panic!("{view:?} card"));
        let head: String = lines[usize::from(card.y) + 1]
            .chars()
            .skip(usize::from(card.x) + 1)
            .take(usize::from(card.width) - 2)
            .collect();
        assert!(head.contains('%'), "{view:?}: percentage kept: {head:?}");
        assert!(!head.contains(['█', '·']), "{view:?}: no bar: {head:?}");
    }
}

fn card(hits: &HitMap, view: View) -> Rect {
    hits.iter()
        .find(|(r, h)| *h == Hit::Tab(view) && r.height > 3)
        .map(|(r, _)| *r)
        .unwrap_or_else(|| panic!("{view:?} card"))
}

/// The overview's card grid, the cards above the process table and the
/// insights beside it are all resizable, and pressing a card's border drags
/// it instead of opening that card's view.
#[test]
fn overview_card_and_table_borders_drag_without_switching_views() {
    let mut w = world();
    let size = (200, 50);
    let (_, hits) = w.draw(size.0, size.1);
    let (mem, cont) = (card(&hits, View::Memory), card(&hits, View::Containers));
    assert_eq!(mem.x, cont.x, "Containers sits below Memory");

    // Row border: Memory's bottom edge / Containers' top edge.
    let row_border = cont.y;
    let hits = w.drag(size, (mem.x + 10, row_border), (mem.x + 10, row_border + 3));
    assert_eq!(w.s.location.view, View::Overview, "no view switch");
    assert_eq!(card(&hits, View::Containers).y, row_border + 3);
    assert_eq!(card(&hits, View::Memory).height, mem.height + 3);

    // Column border: Memory's right edge, shared by every card row.
    let col = card(&hits, View::Memory).x + card(&hits, View::Memory).width - 1;
    let hits = w.drag(size, (col, mem.y + 2), (col + 6, mem.y + 2));
    assert_eq!(w.s.location.view, View::Overview);
    let (m, c) = (card(&hits, View::Memory), card(&hits, View::Containers));
    assert_eq!(m.x + m.width - 1, col + 6);
    assert_eq!(c.x + c.width, m.x + m.width, "both rows follow");

    // Cards / process table border.
    let table = rect(&hits, Panel::Processes);
    let hits = w.drag(size, (20, table.y), (20, table.y + 4));
    assert_eq!(w.s.location.view, View::Overview);
    assert_eq!(rect(&hits, Panel::Processes).y, table.y + 4);

    // Process table / insights border.
    let table = rect(&hits, Panel::Processes);
    let edge = table.x + table.width - 1;
    let hits = w.drag(size, (edge, table.y + 5), (edge - 20, table.y + 5));
    assert_eq!(rect(&hits, Panel::Processes).width, table.width - 20);
}
