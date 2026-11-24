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
        ("1\r", "Relationships"),
        ("\x1b1x", "Terminate"),
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
            w.ev(Event::Key(Key::plain(KeyCode::Enter)));
        }
        w.ev(Event::Resize { width: cols, height: rows });
        let _ = w.draw(cols, rows);
    }
}
