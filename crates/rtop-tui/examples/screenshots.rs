// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Renders every screen of rtop, at several terminal sizes, from the
//! simulated demo machine into standalone HTML files. `scripts/shots.mjs`
//! turns them into PNGs. No property of the machine running this appears
//! in the output: all data comes from the invented demo system.
//!
//! cargo run -p rtop-tui --example screenshots -- <out-dir> [filter]

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rtop_app::input::{Key, KeyCode};
use rtop_app::{AppState, Event, Hit, MouseAction, reduce};
use rtop_collectors::demo::DemoMachine;
use rtop_collectors::{SampleContext, Subscriptions};
use rtop_core::{SystemGraph, Timestamp};
use rtop_tui::HitMap;

struct Session {
    collectors: Vec<Box<dyn rtop_collectors::Collector>>,
    m: DemoMachine,
    g: SystemGraph,
    s: AppState,
    pass: u64,
}

impl Session {
    fn new(w: u16, h: u16) -> Self {
        Self::with_machine(DemoMachine::new(), w, h)
    }

    fn with_machine(m: DemoMachine, w: u16, h: u16) -> Self {
        let collectors = m.collectors(&m.env(Subscriptions::all()));
        let mut s = Session {
            collectors,
            m,
            g: SystemGraph::default(),
            s: AppState::new(true),
            pass: 0,
        };
        s.ev(Event::Resize {
            width: w,
            height: h,
        });
        // Enough samples for rates and graph history.
        for _ in 0..60 {
            s.sample();
        }
        s
    }

    /// Same machine and graph, new application state with the same history.
    fn fork(&self) -> Session {
        let mut s = Session {
            collectors: Vec::new(),
            m: self.m.clone(),
            g: SystemGraph::default(),
            s: AppState::new(true),
            pass: self.pass,
        };
        // Replay history cheaply: re-sample twice so rates exist, then copy
        // the accumulated history series.
        s.g = clone_graph(&self.m, self.pass);
        s.s.history = self.s.history.clone();
        s.ev(Event::GraphChanged);
        s
    }

    fn sample(&mut self) {
        self.pass += 1;
        sample_into(&mut self.collectors, &mut self.g, self.pass);
        self.m.advance(1.0);
        self.ev(Event::GraphChanged);
    }

    fn ev(&mut self, e: Event) {
        let mut fx = Vec::new();
        reduce(
            &mut self.s,
            &self.g,
            e,
            Timestamp::from_millis(self.pass * 1000),
            &mut fx,
        );
    }

    fn keys(&mut self, k: &str) {
        for c in k.chars() {
            self.ev(Event::Key(Key::ch(c)));
        }
    }

    fn key(&mut self, k: KeyCode) {
        self.ev(Event::Key(Key::plain(k)));
    }

    fn render(&self, w: u16, h: u16) -> (String, HitMap) {
        let mut t = Terminal::new(TestBackend::new(w, h)).expect("test backend");
        let mut hits = HitMap::default();
        t.draw(|f| rtop_tui::render(f, &self.s, &self.g, &mut hits))
            .expect("draw");
        (
            rtop_tui::html::buffer_to_html(t.backend().buffer(), "rtop"),
            hits,
        )
    }
}

/// Samples every collector once and records its health the way the
/// runtime would.
fn sample_into(cs: &mut [Box<dyn rtop_collectors::Collector>], g: &mut SystemGraph, seq: u64) {
    for c in cs.iter_mut() {
        let d = c.descriptor();
        let started = std::time::Instant::now();
        let r = c.sample(&SampleContext {
            now: Timestamp::from_millis(seq * 1000),
            sequence: seq,
        });
        let mut st = rtop_core::CollectorStatus::pending(d.interval);
        st.last_duration = Some(started.elapsed());
        st.samples = seq;
        match r {
            Ok(x) => {
                st.state = if x.degraded.is_some() {
                    rtop_core::CollectorState::Degraded
                } else {
                    rtop_core::CollectorState::Available
                };
                st.detail = x.degraded;
                g.apply(x.patch);
            }
            Err(e) => {
                st.state = rtop_core::CollectorState::Unavailable;
                st.reason = Some(e.reason);
                st.detail = Some(e.detail);
            }
        }
        g.set_collector_status(d.id, st);
    }
}

/// Rebuilds the graph by sampling the (shared) demo machine twice.
fn clone_graph(m: &DemoMachine, pass: u64) -> SystemGraph {
    let mut g = SystemGraph::default();
    let env = m.env(Subscriptions::all());
    let mut cs = m.collectors(&env);
    for i in 0..2 {
        sample_into(&mut cs, &mut g, pass * 10 + i);
        m.advance(1.0);
    }
    g
}

type Script = fn(&mut Session);

fn scripts() -> Vec<(&'static str, Script)> {
    vec![
        ("overview", |_| {}),
        ("processes", |s| s.keys("1")),
        ("processes-tree", |s| s.keys("1t")),
        ("processes-filter", |s| {
            s.keys("1Fuser:demo cpu>1");
            s.key(KeyCode::Enter);
        }),
        ("cpu", |s| s.keys("2")),
        ("cpu-64", |s| s.keys("2")),
        ("cpu-256", |s| s.keys("2")),
        ("memory", |s| s.keys("3")),
        ("storage", |s| s.keys("4")),
        ("network", |s| s.keys("5")),
        ("containers", |s| s.keys("6")),
        ("gpu", |s| s.keys("7")),
        ("security", |s| s.keys("8")),
        ("sensors", |s| s.keys("9")),
        ("inspect-process", |s| {
            s.keys("1");
            let row = rtop_app::RowId::Node(s.m.identity(1420).expect("api pid").entity_id());
            s.ev(Event::Mouse(MouseAction::DoubleClick(Hit::Row {
                panel: rtop_app::Panel::Processes,
                row,
            })));
            let id = s.m.identity(1420).expect("api pid");
            let detail = rtop_collectors::inspect::inspect(s.m.fs().as_ref(), id, 100);
            s.ev(Event::Detail(
                detail.map(Box::new).map_err(|e| (id, e.to_string())),
            ));
        }),
        ("inspect-container", |s| {
            s.keys("6");
            s.key(KeyCode::Enter);
        }),
        ("context-menu", |s| {
            s.keys("1jj");
            let row = s.s.location.table().selected.clone().expect("row");
            s.ev(Event::Mouse(MouseAction::RightClick {
                hit: Hit::Row {
                    panel: rtop_app::Panel::Processes,
                    row,
                },
                x: 60,
                y: 14,
            }));
        }),
        ("confirm-terminate", |s| s.keys("1jx")),
        ("signal-menu", |s| s.keys("1jgs")),
        ("help", |s| s.key(KeyCode::F(1))),
        ("emergency", |s| s.keys("!")),
    ]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().unwrap_or_else(|| "target/shots".into()));
    let only = args.next();
    std::fs::create_dir_all(&out).expect("create output directory");
    let sizes: [(u16, u16); 4] = [(160, 48), (120, 36), (80, 24), (50, 16)];
    let mut index = Vec::new();
    // One simulated history shared by all screens; each script runs on a
    // fresh copy of the application state.
    let base = Session::new(sizes[0].0, sizes[0].1);
    // Larger machines for the CPU grid; built only when requested.
    let mut big: std::collections::HashMap<u32, Session> = std::collections::HashMap::new();
    for (name, script) in scripts() {
        if only.as_deref().is_some_and(|o| !name.contains(o)) {
            continue;
        }
        for (w, h) in sizes {
            let cpus: Option<u32> = name.strip_prefix("cpu-").and_then(|n| n.parse().ok());
            let mut s = match cpus {
                Some(n) => big
                    .entry(n)
                    .or_insert_with(|| Session::with_machine(DemoMachine::with_cpus(n), w, h))
                    .fork(),
                None => base.fork(),
            };
            s.ev(Event::Resize {
                width: w,
                height: h,
            });
            script(&mut s);
            let (html, _) = s.render(w, h);
            let file = format!("{name}-{w}x{h}.html");
            std::fs::write(out.join(&file), html).expect("write screenshot");
            index.push(file);
        }
    }
    std::fs::write(out.join("index.txt"), index.join("\n")).expect("write index");
    println!("{} screens rendered into {}", index.len(), out.display());
}
