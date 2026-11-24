// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Scripted and randomised event sequences through the reducer, against the
//! simulated demo machine.

use proptest::prelude::*;
use rtop_app::reducer::refresh;
use rtop_app::state::{Modal, Mode};
use rtop_app::{
    AppState, Effect, Event, Hit, Key, KeyCode, MouseAction, Panel, RowId, View, reduce,
};
use rtop_collectors::demo::DemoMachine;
use rtop_collectors::{SampleContext, Subscriptions};
use rtop_core::{EntityId, Metric, NodeUpdate, ProcessIdentity, SystemGraph, Timestamp};

struct Harness {
    collectors: Vec<Box<dyn rtop_collectors::Collector>>,
    m: DemoMachine,
    g: SystemGraph,
    s: AppState,
    fx: Vec<Effect>,
    pass: u64,
}

impl Harness {
    fn new() -> Self {
        let m = DemoMachine::new();
        let collectors = m.collectors(&m.env(Subscriptions::all()));
        let mut h = Harness {
            collectors,
            m,
            g: SystemGraph::default(),
            s: AppState::new(true),
            fx: Vec::new(),
            pass: 0,
        };
        h.sample();
        h.sample();
        h
    }

    fn sample(&mut self) {
        self.pass += 1;
        for c in &mut self.collectors {
            if let Ok(s) = c.sample(&SampleContext {
                now: Timestamp::from_millis(self.pass * 1000),
                sequence: self.pass,
            }) {
                self.g.apply(s.patch);
            }
        }
        self.m.advance(1.0);
        self.ev(Event::GraphChanged);
    }

    fn ev(&mut self, e: Event) {
        reduce(
            &mut self.s,
            &self.g,
            e,
            Timestamp::from_millis(self.pass * 1000),
            &mut self.fx,
        );
    }

    fn keys(&mut self, ks: &str) {
        for c in ks.chars() {
            self.ev(Event::Key(Key::ch(c)));
        }
    }

    fn key(&mut self, k: KeyCode) {
        self.ev(Event::Key(Key::plain(k)));
    }

    fn selected(&self) -> Option<RowId> {
        self.s.location.table().selected.clone()
    }

    fn signals(&mut self) -> Vec<(Vec<ProcessIdentity>, &'static str)> {
        self.fx
            .drain(..)
            .filter_map(|e| match e {
                Effect::Signal {
                    targets,
                    signal_name,
                    ..
                } => Some((targets.iter().map(|t| t.identity).collect(), signal_name)),
                _ => None,
            })
            .collect()
    }

    fn pid_row(&self, pid: u32) -> RowId {
        RowId::Node(self.m.identity(pid).unwrap().entity_id())
    }
}

#[test]
fn digits_switch_views_but_are_text_in_search() {
    let mut h = Harness::new();
    for (d, v) in View::ALL.iter().enumerate() {
        h.keys(&d.to_string());
        assert_eq!(h.s.location.view, *v);
    }
    h.keys("1/123");
    assert_eq!(h.s.location.view, View::Processes);
    assert_eq!(
        h.s.mode,
        Mode::Search {
            backward: false,
            input: "123".into()
        }
    );
}

#[test]
fn selection_follows_identity_when_rows_reorder() {
    let mut h = Harness::new();
    h.keys("1");
    let target = h.pid_row(2312);
    assert!(h.s.focused_rows().position(&target).is_some());
    // Select via mouse click on the row.
    h.ev(Event::Mouse(MouseAction::Click(Hit::Row {
        panel: Panel::Processes,
        row: target.clone(),
    })));
    assert_eq!(h.selected(), Some(target.clone()));
    let before = h.s.cursor();
    // Make it the top CPU consumer: the table reorders, selection stays.
    let mut p = rtop_core::GraphPatch::new(rtop_core::CollectorId("processes"), 999, Timestamp(1));
    p.node(
        NodeUpdate::new(
            rtop_core::EntityKind::Process,
            target.entity().unwrap().clone(),
        )
        .metric("cpu_usage_percent", Metric::f64(99_999.0)),
    );
    h.g.apply(p);
    h.ev(Event::GraphChanged);
    assert_eq!(h.selected(), Some(target));
    assert_ne!(h.s.cursor(), before);
    assert_eq!(h.s.cursor(), Some(0));
}

#[test]
fn gg_g_and_quick_sort() {
    let mut h = Harness::new();
    h.keys("1G");
    assert_eq!(h.s.cursor(), Some(h.s.focused_rows().len() - 1));
    h.keys("gg");
    assert_eq!(h.s.cursor(), Some(0));
    h.keys("sm");
    assert_eq!(h.s.location.table().sort, rtop_app::SortKey::Mem);
    // Header click on the active sort reverses it, like `sr`.
    let desc = h.s.location.table().descending;
    h.ev(Event::Mouse(MouseAction::Click(Hit::Header {
        panel: Panel::Processes,
        sort: rtop_app::SortKey::Mem,
    })));
    assert_eq!(h.s.location.table().descending, !desc);
    h.keys("sr");
    assert_eq!(h.s.location.table().descending, desc);
}

#[test]
fn enter_detail_and_return_preserves_selection_and_sort() {
    let mut h = Harness::new();
    h.keys("1st");
    h.keys("jjj");
    let sel = h.selected();
    h.key(KeyCode::Enter);
    assert!(h.s.location.inspect.is_some());
    assert!(h.fx.iter().any(|e| matches!(e, Effect::Inspect(Some(_)))));
    // Follow a relationship two levels deep, then come back twice.
    h.key(KeyCode::Enter);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    assert_eq!(h.s.location.inspect, None);
    assert_eq!(h.selected(), sel);
    assert_eq!(h.s.location.table().sort, rtop_app::SortKey::Threads);
}

#[test]
fn cancelled_termination_sends_nothing_and_confirmed_sends_captured_identity() {
    let mut h = Harness::new();
    h.keys("1");
    let row = h.pid_row(1420);
    h.ev(Event::Mouse(MouseAction::Click(Hit::Row {
        panel: Panel::Processes,
        row,
    })));
    h.keys("x");
    assert!(matches!(h.s.modal, Some(Modal::Confirm { .. })));
    h.key(KeyCode::Esc);
    assert!(h.signals().is_empty());
    h.keys("x");
    // Rows reorder while the dialog is open: the target is unchanged.
    h.sample();
    h.keys("y");
    assert_eq!(
        h.signals(),
        vec![(vec![h.m.identity(1420).unwrap()], "SIGTERM")]
    );
}

#[test]
fn emergency_force_kill_is_immediate_for_one_process_confirmed_for_many() {
    let mut h = Harness::new();
    h.keys("1");
    h.keys("X");
    let s = h.signals();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].1, "SIGKILL");
    assert!(h.s.modal.is_none());
    h.keys("  "); // select two rows
    h.keys("X");
    assert!(h.signals().is_empty(), "bulk kill must be confirmed");
    assert!(matches!(&h.s.modal, Some(Modal::Confirm { targets, .. }) if targets.len() == 2));
    h.keys("y");
    assert_eq!(h.signals()[0].0.len(), 2);
}

#[test]
fn stale_identity_is_kept_after_process_exit_and_pid_reuse() {
    let mut h = Harness::new();
    h.keys("1");
    let row = h.pid_row(3300);
    h.ev(Event::Mouse(MouseAction::Click(Hit::Row {
        panel: Panel::Processes,
        row,
    })));
    h.keys("x");
    let original = h.m.identity(3300).unwrap();
    h.m.kill(3300);
    h.sample();
    h.keys("y");
    // The effect still names the original identity; the controller will
    // find it gone and refuse rather than signal a recycled PID.
    assert_eq!(h.signals(), vec![(vec![original], "SIGTERM")]);
}

#[test]
fn keyboard_and_mouse_reach_identical_states() {
    let mut a = Harness::new();
    let mut b = Harness::new();
    a.keys("5");
    b.ev(Event::Mouse(MouseAction::Click(Hit::Tab(View::Network))));
    assert_eq!(a.s.location.view, b.s.location.view);
    a.keys("j");
    let row = a.selected().unwrap();
    b.ev(Event::Mouse(MouseAction::Click(Hit::Row {
        panel: a.s.location.table().panel,
        row: row.clone(),
    })));
    assert_eq!(b.selected(), Some(row.clone()));
    a.key(KeyCode::Enter);
    b.ev(Event::Mouse(MouseAction::DoubleClick(Hit::Row {
        panel: Panel::Communication,
        row,
    })));
    assert_eq!(a.s.location.inspect, b.s.location.inspect);
}

#[test]
fn right_click_opens_menu_for_displayed_row_and_actions_disabled_mode_refuses() {
    let mut h = Harness::new();
    h.s.actions_enabled = false;
    h.keys("1");
    let row = h.pid_row(806);
    h.ev(Event::Mouse(MouseAction::RightClick {
        hit: Hit::Row {
            panel: Panel::Processes,
            row,
        },
        x: 10,
        y: 10,
    }));
    match &h.s.modal {
        Some(Modal::Context { title, targets, .. }) => {
            assert!(title.contains("806"));
            assert_eq!(targets[0].identity, h.m.identity(806).unwrap());
        }
        other => panic!("{other:?}"),
    }
    // Last item: force kill. Actions disabled: nothing is sent.
    h.ev(Event::Mouse(MouseAction::Click(Hit::MenuItem(7))));
    assert!(h.signals().is_empty());
    assert!(h.s.current_notice().unwrap().error);
}

#[test]
fn filter_is_incremental_and_restored_on_escape() {
    let mut h = Harness::new();
    h.keys("1F");
    h.keys("user:dbuser");
    assert!(h.s.focused_rows().len() >= 5 && h.s.focused_rows().len() < 10);
    h.key(KeyCode::Esc);
    assert!(h.s.focused_rows().len() > 30);
    h.keys("Fcpu>");
    assert!(
        h.s.prompt_error.is_some(),
        "incomplete expression reports an error"
    );
    h.key(KeyCode::Enter);
    h.keys(":filter rss>1GiB");
    h.key(KeyCode::Enter);
    assert!(h.s.focused_rows().len() <= 4);
}

#[test]
fn command_mode_quit_and_unknown_command() {
    let mut h = Harness::new();
    h.keys(":nonsense");
    h.key(KeyCode::Enter);
    assert!(h.s.current_notice().unwrap().error);
    h.keys(":q");
    h.key(KeyCode::Enter);
    assert!(h.s.quit);
}

#[test]
fn emergency_mode_and_subscriptions() {
    let mut h = Harness::new();
    h.fx.clear();
    h.keys("!");
    assert!(h.fx.contains(&Effect::Emergency(true)));
    h.keys("5");
    assert!(h.fx.iter().any(|e| matches!(
        e,
        Effect::Subscribe {
            socket_owners: true,
            ..
        }
    )));
}

fn key_strategy() -> impl Strategy<Value = Event> {
    let chars: Vec<char> = "0123456789jkhlgGwbHMLnNvyqxXsmcpidtar/?:FzoRM! +-"
        .chars()
        .collect();
    prop_oneof![
        proptest::sample::select(chars).prop_map(|c| Event::Key(Key::ch(c))),
        proptest::sample::select(vec![
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::PageDown
        ])
        .prop_map(|k| Event::Key(Key::plain(k))),
        (1u16..300, 1u16..120).prop_map(|(w, hgt)| Event::Resize {
            width: w,
            height: hgt
        }),
        Just(Event::GraphChanged),
        (-20i32..20).prop_map(|lines| Event::Mouse(MouseAction::Scroll { panel: None, lines })),
        Just(Event::Mouse(MouseAction::Dismiss)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]
    #[test]
    fn random_sessions_keep_invariants(events in proptest::collection::vec(key_strategy(), 1..120)) {
        let mut h = Harness::new();
        for e in events {
            h.ev(e);
            if h.s.quit { break; }
            let loc = &h.s.location;
            prop_assert!(loc.focus < loc.tables.len());
            prop_assert_eq!(h.s.rows.len(), loc.tables.len());
            for (i, t) in loc.tables.iter().enumerate() {
                if let Some(sel) = &t.selected {
                    prop_assert!(h.s.rows[i].position(sel).is_some(), "selection must be a visible row");
                }
            }
            prop_assert!(h.s.back.len() <= 100);
            // Any signal effect carries full identities.
            for e in &h.fx {
                if let Effect::Signal { targets, .. } = e {
                    prop_assert!(targets.iter().all(|t| t.identity.start_time_ticks > 0));
                }
            }
        }
        let _ = refresh;
        let _ = EntityId::host();
    }
}
