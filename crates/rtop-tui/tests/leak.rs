// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Heap leak check of the whole pipeline (collectors, graph, reducer,
//! renderer) with a counting allocator: after warm-up, thousands more
//! identical interaction cycles must not increase live heap bytes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rtop_app::input::{Key, KeyCode};
use rtop_app::{AppState, Event, reduce};
use rtop_collectors::demo::DemoMachine;
use rtop_collectors::{Collector, SampleContext, Subscriptions};
use rtop_core::{SystemGraph, Timestamp};
use rtop_tui::HitMap;

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: delegates to the system allocator and only counts sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        LIVE.fetch_add(new as isize - l.size() as isize, Ordering::Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn live_heap_is_flat_over_thousands_of_interaction_cycles() {
    let m = DemoMachine::new();
    let mut cs: Vec<Box<dyn Collector>> = m.collectors(&m.env(Subscriptions::all()));
    let mut g = SystemGraph::default();
    let mut s = AppState::new(true);
    let mut term = Terminal::new(TestBackend::new(160, 48)).unwrap();
    let mut hits = HitMap::default();
    let mut seq = 0u64;
    let mut cycle = |g: &mut SystemGraph, s: &mut AppState| {
        seq += 1;
        for c in &mut cs {
            if let Ok(x) = c.sample(&SampleContext {
                now: Timestamp::from_millis(seq * 1000),
                sequence: seq,
            }) {
                g.apply(x.patch);
            }
        }
        m.advance(1.0);
        let mut fx = Vec::new();
        let now = Timestamp::from_millis(seq * 1000);
        reduce(s, g, Event::GraphChanged, now, &mut fx);
        // Every view, an inspection, a relationship hop and back.
        let keys = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "1", "j"];
        for k in keys {
            reduce(
                s,
                g,
                Event::Key(Key::ch(k.chars().next().unwrap())),
                now,
                &mut fx,
            );
            term.draw(|f| rtop_tui::render(f, s, g, &mut hits)).unwrap();
        }
        for k in [
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Esc,
            KeyCode::Esc,
        ] {
            reduce(s, g, Event::Key(Key::plain(k)), now, &mut fx);
            term.draw(|f| rtop_tui::render(f, s, g, &mut hits)).unwrap();
        }
    };
    // Warm-up fills every bounded structure (history rings, navigation
    // history, event ring, caches).
    for _ in 0..400 {
        cycle(&mut g, &mut s);
    }
    let warm = LIVE.load(Ordering::Relaxed);
    for _ in 0..1500 {
        cycle(&mut g, &mut s);
    }
    let after = LIVE.load(Ordering::Relaxed);
    eprintln!("live heap after warm-up {warm} B, after 1500 more cycles {after} B");
    assert!(
        after - warm < 16 * 1024,
        "live heap grew by {} bytes",
        after - warm
    );
}
