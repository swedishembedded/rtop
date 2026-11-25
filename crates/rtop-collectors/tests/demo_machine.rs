// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Every collector against the simulated demo machine: the graph it builds
//! must contain the expected entities, relationships and correct rates.

use rtop_collectors::demo::DemoMachine;
use rtop_collectors::{SampleContext, Subscriptions, metrics};
use rtop_core::{
    EdgeKind, EntityId, EntityKind, Evidence, Metric, SystemGraph, Timestamp, Unavailable,
};

fn run(m: &DemoMachine, passes: u32) -> SystemGraph {
    let env = m.env(Subscriptions::all());
    let mut cs = m.collectors(&env);
    let mut g = SystemGraph::default();
    for pass in 1..=passes {
        let ctx = SampleContext {
            now: Timestamp::from_millis(u64::from(pass) * 1000),
            sequence: u64::from(pass),
        };
        for c in &mut cs {
            if let Ok(s) = c.sample(&ctx) {
                g.apply(s.patch);
            }
        }
        m.advance(1.0);
    }
    g
}

#[test]
fn first_pass_rates_are_pending_not_zero() {
    let g = run(&DemoMachine::new(), 1);
    let host = g.node(&EntityId::host()).unwrap();
    assert_eq!(
        host.metric("cpu_usage_percent"),
        Some(&Metric::Unavailable(Unavailable::Pending))
    );
    assert!(host.f64("memory_total_bytes").unwrap() > 0.0);
}

#[test]
fn demo_graph_has_connected_workloads() {
    let m = DemoMachine::new();
    let g = run(&m, 3);
    let host = g.node(&EntityId::host()).unwrap();
    let cpu = host.f64("cpu_usage_percent").unwrap();
    assert!((1.0..100.0).contains(&cpu), "cpu {cpu}");
    assert_eq!(host.f64("logical_cpus"), Some(16.0));
    assert_eq!(host.text("hostname"), Some("demo-host"));

    // The compiler runs at ~360% in per-CPU convention.
    let rustc = g.node(&m.identity(2311).unwrap().entity_id()).unwrap();
    let rc = rustc.f64("cpu_usage_percent").unwrap();
    assert!((200.0..500.0).contains(&rc), "rustc {rc}");
    assert_eq!(rustc.text("user"), Some("demo"));

    // Containers from cgroups, named by the Docker API, with volumes.
    let containers: Vec<_> = g.nodes_of(EntityKind::Container).collect();
    assert_eq!(containers.len(), 3);
    assert!(
        containers
            .iter()
            .any(|c| c.text("name") == Some("postgres-db"))
    );
    assert!(g.edges_of(EdgeKind::Mounts).count() >= 3);
    assert!(g.edges_of(EdgeKind::AccountedTo).count() >= 3);

    // Kernel-confirmed Unix peers and endpoint-matched TCP pairs become
    // process-to-process communication.
    let comm: Vec<_> = g.edges_of(EdgeKind::CommunicatesWith).collect();
    let api = m.identity(1420).unwrap().entity_id();
    let pg = m.identity(1263).unwrap().entity_id();
    let e = comm
        .iter()
        .find(|e| e.key.source == api && e.key.target == pg)
        .expect("api -> postgres");
    assert_eq!(e.evidence, Evidence::EndpointMatched);
    assert!(
        e.metric("tx_bytes_per_second")
            .and_then(Metric::as_f64)
            .unwrap()
            > 1000.0
    );
    assert!(
        comm.iter()
            .any(|e| e.evidence == Evidence::SocketPeerConfirmed)
    );

    // A listening socket inherited from the master by two workers has three owners.
    let listener = g
        .nodes_of(EntityKind::Socket)
        .find(|s| s.attr("inode").and_then(|a| a.as_u64()) == Some(9002))
        .unwrap();
    assert_eq!(
        g.incoming(&listener.id)
            .filter(|e| e.key.kind == EdgeKind::OwnsSocket)
            .count(),
        3
    );

    // GPUs, sensors, battery, security events.
    assert_eq!(g.nodes_of(EntityKind::Gpu).count(), 2);
    assert!(g.edges_of(EdgeKind::UsesGpu).count() >= 1);
    assert!(g.nodes_of(EntityKind::Sensor).count() >= 5);
    let bat = g.nodes_of(EntityKind::Battery).next().unwrap();
    assert!(bat.f64("time_remaining_seconds").unwrap() > 0.0);
    assert!(g.events().any(|e| e.message.contains("AppArmor")));
    // Disk health: the NVMe drive is healthy, the SATA disk has
    // reallocated sectors.
    let nvme = g
        .node(&EntityId::new(EntityKind::BlockDevice, "nvme0n1"))
        .unwrap();
    assert_eq!(nvme.text("health"), Some("ok"));
    assert_eq!(nvme.f64("wear_percent"), Some(3.0));
    let sda = g
        .node(&EntityId::new(EntityKind::BlockDevice, "sda"))
        .unwrap();
    assert_eq!(sda.text("health"), Some("warning"));
    assert_eq!(sda.f64("reallocated_sectors_total"), Some(8.0));
    // Firewall: counted rules have rates, the uninstrumented one is N/A.
    let rules: Vec<_> = g.nodes_of(EntityKind::FirewallRule).collect();
    assert_eq!(rules.len(), 6);
    let deny = rules
        .iter()
        .find(|r| r.text("name") == Some("default deny"))
        .unwrap();
    assert!(deny.f64("packets_per_second").unwrap() > 0.0);
    let output = rules
        .iter()
        .find(|r| r.text("chain") == Some("output"))
        .unwrap();
    assert_eq!(
        output.metric("packets_per_second"),
        Some(&Metric::Unavailable(Unavailable::NotMeasurable))
    );
    assert!(
        g.node(&EntityId::host())
            .unwrap()
            .f64("firewall_drop_packets_per_second")
            .unwrap()
            > 0.0
    );
    // Remote NFS mount that fails is unavailable, not zero.
    let nfs = g
        .node(&EntityId::new(EntityKind::Mount, "/srv/backup"))
        .unwrap();
    assert!(matches!(
        nfs.metric("capacity_bytes"),
        Some(Metric::Unavailable(_))
    ));
}

#[test]
fn exited_process_and_its_relationships_disappear() {
    let m = DemoMachine::new();
    let env = m.env(Subscriptions::all());
    let mut cs = m.collectors(&env);
    let mut g = SystemGraph::default();
    let mut pass = |g: &mut SystemGraph, n: u64| {
        for c in &mut cs {
            if let Ok(s) = c.sample(&SampleContext {
                now: Timestamp::from_millis(n * 1000),
                sequence: n,
            }) {
                g.apply(s.patch);
            }
        }
    };
    pass(&mut g, 1);
    let api = m.identity(1420).unwrap().entity_id();
    assert!(g.outgoing(&api).count() > 0);
    m.kill(1420);
    m.advance(1.0);
    pass(&mut g, 2);
    assert!(g.node(&api).is_none());
    assert_eq!(g.outgoing(&api).count() + g.incoming(&api).count(), 0);
}

#[test]
fn every_emitted_metric_is_registered() {
    let g = run(&DemoMachine::new(), 2);
    let mut missing = std::collections::BTreeSet::new();
    for n in g.nodes() {
        for k in n.metrics.keys() {
            if metrics::describe(k).is_none() {
                missing.insert(*k);
            }
        }
    }
    for e in g.edges() {
        for k in e.metrics.keys() {
            if metrics::describe(k).is_none() {
                missing.insert(*k);
            }
        }
    }
    assert!(missing.is_empty(), "unregistered metrics: {missing:?}");
}

#[test]
fn live_system_collectors_do_not_fail_hard() {
    let env = rtop_collectors::Env::live(Subscriptions::all());
    let mut g = SystemGraph::default();
    for c in &mut rtop_collectors::all(&env) {
        let id = c.descriptor().id;
        match c.sample(&SampleContext {
            now: Timestamp::from_millis(1),
            sequence: 1,
        }) {
            Ok(s) => {
                g.apply(s.patch);
            }
            Err(e) => eprintln!("{id}: {:?} {}", e.reason, e.detail),
        }
    }
    assert!(g.node(&EntityId::host()).is_some());
    assert!(g.nodes_of(EntityKind::Process).count() > 1);
}
