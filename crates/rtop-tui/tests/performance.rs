// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Performance and scale budgets from the product requirements, measured on
//! synthetic systems far larger than a typical machine.
//!
//! Run with `make test/performance` (release build). In debug builds these
//! tests are ignored: unoptimised timings say nothing about the product.
//! Budgets are the specification's targets; each measurement takes the
//! median of several runs to reduce noise on shared machines.

use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rtop_app::input::Key;
use rtop_app::{AppState, Event, reduce};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, GraphPatch, Metric,
    NodeUpdate, ProcessIdentity, Scope, SystemGraph, Timestamp,
};
use rtop_tui::HitMap;

fn median(mut f: impl FnMut() -> Duration, runs: usize) -> Duration {
    let mut v: Vec<Duration> = (0..runs).map(|_| f()).collect();
    v.sort();
    v[runs / 2]
}

fn processes_patch(n: u32, seq: u64, sockets_per_process: u32) -> GraphPatch {
    let mut p = GraphPatch::new(CollectorId("processes"), seq, Timestamp(seq))
        .complete(Scope::Nodes(EntityKind::Process))
        .complete(Scope::Edges(EdgeKind::ParentOf))
        .complete(Scope::Nodes(EntityKind::Socket))
        .complete(Scope::Edges(EdgeKind::OwnsSocket));
    for pid in 1..=n {
        let id = ProcessIdentity {
            pid,
            start_time_ticks: u64::from(pid) * 3,
        }
        .entity_id();
        p.node(
            NodeUpdate::new(EntityKind::Process, id.clone())
                .attr("pid", pid)
                .attr("name", format!("proc-{}", pid % 97))
                .attr(
                    "cmdline",
                    format!("/usr/bin/proc-{} --worker {pid}", pid % 97),
                )
                .attr("user", if pid % 3 == 0 { "root" } else { "app" })
                .attr("state_code", "S")
                .metric(
                    "cpu_usage_percent",
                    Metric::f64(f64::from((pid * 7919 + seq as u32) % 1000) / 10.0),
                )
                .metric("rss_bytes", Metric::u64(u64::from(pid % 4096) << 20))
                .metric("threads", Metric::u64(u64::from(pid % 64 + 1))),
        );
        if pid > 1 {
            let parent = ProcessIdentity {
                pid: pid / 2,
                start_time_ticks: u64::from(pid / 2) * 3,
            }
            .entity_id();
            p.edge(EdgeUpdate::new(
                EdgeKind::ParentOf,
                parent,
                id.clone(),
                Evidence::KernelConfirmed,
            ));
        }
        for s in 0..sockets_per_process {
            let sid = EntityId::new(EntityKind::Socket, format_args!("1:{}", pid * 16 + s));
            p.node(NodeUpdate::new(EntityKind::Socket, sid.clone()).attr("protocol", "tcp"));
            p.edge(EdgeUpdate::new(
                EdgeKind::OwnsSocket,
                id.clone(),
                sid,
                Evidence::KernelConfirmed,
            ));
        }
    }
    p
}

fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        * 1024
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "performance budgets apply to release builds"
)]
fn graph_of_100k_processes_with_200k_sockets_applies_and_reconciles_within_budget() {
    let before = rss_bytes();
    let mut g = SystemGraph::default();
    // Patches are built before timing: only the graph is measured.
    let (p1, p2, p3) = (
        processes_patch(100_000, 1, 2),
        processes_patch(100_000, 2, 2),
        processes_patch(50_000, 3, 2),
    );
    let t = Instant::now();
    g.apply(p1);
    let first = t.elapsed();
    // A full re-observation (all metrics change) and a churn pass where
    // half the processes exited.
    let t = Instant::now();
    g.apply(p2);
    let again = t.elapsed();
    let t = Instant::now();
    g.apply(p3);
    let churn = t.elapsed();
    let grown = rss_bytes().saturating_sub(before);
    eprintln!(
        "100k processes: insert {first:?}, update {again:?}, remove half {churn:?}, rss +{} MiB",
        grown >> 20
    );
    assert_eq!(g.nodes_of(EntityKind::Process).count(), 50_000);
    assert_eq!(g.edges_of(EdgeKind::OwnsSocket).count(), 100_000);
    for d in [first, again, churn] {
        assert!(d < Duration::from_secs(3), "{d:?}");
    }
    assert!(grown < 2 << 30, "graph memory {} MiB", grown >> 20);
}

fn app_with(n: u32) -> (AppState, SystemGraph) {
    let mut g = SystemGraph::default();
    g.apply(processes_patch(n, 1, 0));
    let mut s = AppState::new(true);
    let mut fx = Vec::new();
    reduce(&mut s, &g, Event::Key(Key::ch('1')), Timestamp(1), &mut fx);
    reduce(
        &mut s,
        &g,
        Event::Resize {
            width: 160,
            height: 50,
        },
        Timestamp(2),
        &mut fx,
    );
    (s, g)
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "performance budgets apply to release builds"
)]
fn sorting_and_input_meet_their_budgets_at_1k_and_10k_processes() {
    for (n, budget) in [
        (1_000, Duration::from_millis(5)),
        (10_000, Duration::from_millis(20)),
    ] {
        let (mut s, mut g) = app_with(n);
        let mut seq = 2;
        let refresh = median(
            || {
                // New sample: every CPU value changes, so the table re-sorts.
                seq += 1;
                g.apply(processes_patch(n, seq, 0));
                let t = Instant::now();
                reduce(
                    &mut s,
                    &g,
                    Event::GraphChanged,
                    Timestamp(seq),
                    &mut Vec::new(),
                );
                t.elapsed()
            },
            7,
        );
        let key = median(
            || {
                let t = Instant::now();
                reduce(
                    &mut s,
                    &g,
                    Event::Key(Key::ch('j')),
                    Timestamp(seq),
                    &mut Vec::new(),
                );
                t.elapsed()
            },
            31,
        );
        eprintln!("{n} processes: re-sort {refresh:?}, key {key:?}");
        assert!(
            refresh < budget,
            "{n} processes re-sorted in {refresh:?} (budget {budget:?})"
        );
        assert!(key < Duration::from_millis(5), "input-to-state {key:?}");
    }
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "performance budgets apply to release builds"
)]
fn rendering_formats_only_visible_rows() {
    let (s, g) = app_with(10_000);
    let mut term = Terminal::new(TestBackend::new(160, 50)).unwrap();
    let mut hits = HitMap::default();
    let frame = median(
        || {
            let t = Instant::now();
            term.draw(|f| rtop_tui::render(f, &s, &g, &mut hits))
                .unwrap();
            t.elapsed()
        },
        9,
    );
    eprintln!("render 160x50 with 10k processes: {frame:?}");
    assert!(frame < Duration::from_millis(30), "{frame:?}");
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "performance budgets apply to release builds"
)]
fn lightweight_scan_of_1000_processes_is_fast() {
    use rtop_collectors::processes::ProcessCollector;
    use rtop_collectors::{Collector, Env, SampleContext, Subscriptions};
    let fs = std::sync::Arc::new(rtop_linux::MemoryKernelFs::new());
    fs.file("/proc/uptime", "1000.0 1.0\n");
    for pid in 1..=1000u32 {
        fs.file(&format!("/proc/{pid}/stat"), format!("{pid} (w{pid}) S 1 {pid} {pid} 0 -1 0 0 0 0 0 {pid} 0 0 0 20 0 1 0 {pid} 1000 10 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n"));
        fs.file(
            &format!("/proc/{pid}/status"),
            "Uid:\t1000\t1000\t1000\t1000\nVmRSS:\t100 kB\nThreads:\t1\n",
        );
        fs.file(&format!("/proc/{pid}/cmdline"), format!("/bin/w\0{pid}\0"));
    }
    let mut c = ProcessCollector::new(Env::simulated(
        fs,
        std::sync::Arc::new(Subscriptions::default()),
    ));
    let mut seq = 0;
    let scan = median(
        || {
            seq += 1;
            let t = Instant::now();
            c.sample(&SampleContext {
                now: Timestamp(seq * 1_000_000_000),
                sequence: seq,
            })
            .unwrap();
            t.elapsed()
        },
        9,
    );
    eprintln!("1000-process scan: {scan:?}");
    assert!(scan < Duration::from_millis(50), "{scan:?}");
}
