// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Projections: the rows each table shows, derived from the graph.
//!
//! Sorting always places rows whose sort value is unavailable after every
//! row with a value, in either direction, and breaks ties by stable row id
//! so equal rows never shuffle between frames.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use rtop_core::{EdgeKind, EntityId, EntityKind, Node, SystemGraph};

use crate::filter::Filter;
use crate::state::TableState;
use crate::view::{Panel, RowId, SortKey};

#[derive(Debug, Clone, Default)]
pub struct Rows {
    pub ids: Vec<RowId>,
    /// Tree depth per row (0 when not a tree).
    pub depth: Vec<u16>,
    /// Whether the row has children (tree mode), for expand markers.
    pub has_children: Vec<bool>,
    index: HashMap<RowId, usize>,
}

impl Rows {
    fn from(ids: Vec<RowId>, depth: Vec<u16>, has_children: Vec<bool>) -> Self {
        let index = ids
            .iter()
            .enumerate()
            .map(|(i, r)| (r.clone(), i))
            .collect();
        Rows {
            ids,
            depth,
            has_children,
            index,
        }
    }

    fn flat(ids: Vec<RowId>) -> Self {
        let n = ids.len();
        Rows::from(ids, vec![0; n], vec![false; n])
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn position(&self, r: &RowId) -> Option<usize> {
        self.index.get(r).copied()
    }
}

#[derive(Debug, Clone, PartialEq, PartialOrd)]
enum SortVal {
    Num(f64),
    Text(String),
    Missing,
}

fn sum(n: &Node, keys: &[&str]) -> Option<f64> {
    let vals: Vec<f64> = keys.iter().filter_map(|k| n.f64(k)).collect();
    (!vals.is_empty()).then(|| vals.iter().sum())
}

fn gpu_of(g: &SystemGraph, n: &Node) -> Option<f64> {
    let v: Vec<f64> = g
        .outgoing(&n.id)
        .filter(|e| e.key.kind == EdgeKind::UsesGpu)
        .filter_map(|e| e.metric("gpu_usage_percent").and_then(|m| m.as_f64()))
        .collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

fn node_value(g: &SystemGraph, n: &Node, key: SortKey) -> SortVal {
    let num = |v: Option<f64>| v.map_or(SortVal::Missing, SortVal::Num);
    match key {
        SortKey::Name => SortVal::Text(n.label().to_ascii_lowercase()),
        SortKey::Pid => num(n
            .attr("pid")
            .and_then(|p| p.as_u64())
            .map(|p| p as f64)
            .or_else(|| n.attr("index").and_then(|p| p.as_u64()).map(|p| p as f64))),
        SortKey::Cpu => num(n.f64("cpu_usage_percent")),
        SortKey::Mem => num(n
            .f64("rss_bytes")
            .or_else(|| n.f64("memory_current_bytes"))
            .or_else(|| n.f64("vram_used_bytes"))
            .or_else(|| n.f64("used_percent"))),
        SortKey::Pss => num(n.f64("pss_bytes")),
        SortKey::Virt => num(n.f64("virtual_bytes")),
        SortKey::Io => num(sum(
            n,
            &[
                "io_read_bytes_per_second",
                "io_write_bytes_per_second",
                "read_bytes_per_second",
                "write_bytes_per_second",
            ],
        )),
        SortKey::Net => num(sum(
            n,
            &[
                "tcp_rx_bytes_per_second",
                "tcp_tx_bytes_per_second",
                "network_rx_bytes_per_second",
                "network_tx_bytes_per_second",
                "packets_per_second",
                "rx_bytes_per_second",
                "tx_bytes_per_second",
            ],
        )),
        SortKey::Gpu => num(n.f64("gpu_usage_percent").or_else(|| gpu_of(g, n))),
        SortKey::Threads => num(n.f64("threads")),
        SortKey::Age => num(n.f64("age_seconds")),
        SortKey::Value => num(n.f64("value")),
    }
}

fn row_value(g: &SystemGraph, r: &RowId, key: SortKey) -> SortVal {
    match r {
        RowId::Node(id) => g
            .node(id)
            .map_or(SortVal::Missing, |n| node_value(g, n, key)),
        RowId::Edge(k) => {
            let Some(e) = g.edge(k) else {
                return SortVal::Missing;
            };
            match key {
                SortKey::Name => SortVal::Text(format!("{} {}", e.key.source, e.key.target)),
                _ => {
                    let v: Vec<f64> = [
                        "tx_bytes_per_second",
                        "gpu_usage_percent",
                        "gpu_memory_bytes",
                    ]
                    .iter()
                    .filter_map(|m| e.metric(m).and_then(|m| m.as_f64()))
                    .collect();
                    v.first().map_or(SortVal::Missing, |v| SortVal::Num(*v))
                }
            }
        }
        RowId::Seq(s) => SortVal::Num(*s as f64),
        RowId::Collector(c) => SortVal::Text((*c).to_owned()),
    }
}

fn compare(a: &(SortVal, RowId), b: &(SortVal, RowId), descending: bool) -> Ordering {
    let ord = match (&a.0, &b.0) {
        (SortVal::Missing, SortVal::Missing) => Ordering::Equal,
        (SortVal::Missing, _) => return Ordering::Greater,
        (_, SortVal::Missing) => return Ordering::Less,
        (SortVal::Num(x), SortVal::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (x, y) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
    };
    let ord = if descending { ord.reverse() } else { ord };
    ord.then_with(|| a.1.cmp(&b.1))
}

fn sorted(g: &SystemGraph, rows: Vec<RowId>, t: &TableState) -> Vec<RowId> {
    let mut keyed: Vec<(SortVal, RowId)> = rows
        .into_iter()
        .map(|r| (row_value(g, &r, t.sort), r))
        .collect();
    keyed.sort_unstable_by(|a, b| compare(a, b, t.descending));
    keyed.into_iter().map(|(_, r)| r).collect()
}

/// Filters and sorts nodes of `kind`, computing each sort key while the
/// node is at hand (no second lookup by id). Ties break on the stable id,
/// so the unstable sort is deterministic.
fn sorted_nodes(g: &SystemGraph, kind: EntityKind, t: &TableState) -> Vec<RowId> {
    let mut keyed: Vec<(SortVal, RowId)> = g
        .nodes_of(kind)
        .filter(|n| t.filter.matches(n))
        .map(|n| (node_value(g, n, t.sort), RowId::Node(n.id.clone())))
        .collect();
    keyed.sort_unstable_by(|a, b| compare(a, b, t.descending));
    keyed.into_iter().map(|(_, r)| r).collect()
}

fn nodes(g: &SystemGraph, kind: EntityKind, filter: &Filter) -> Vec<RowId> {
    g.nodes_of(kind)
        .filter(|n| filter.matches(n))
        .map(|n| RowId::Node(n.id.clone()))
        .collect()
}

fn edges(g: &SystemGraph, kind: EdgeKind, filter: &Filter) -> Vec<RowId> {
    g.edges_of(kind)
        .filter(|e| {
            filter.is_empty()
                || [&e.key.source, &e.key.target]
                    .iter()
                    .any(|id| g.node(id).is_some_and(|n| filter.matches(n)))
        })
        .map(|e| RowId::Edge(e.key.clone()))
        .collect()
}

/// Inputs beyond the table state that shape a projection.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionContext<'a> {
    pub tree: bool,
    pub collapsed: &'a HashSet<EntityId>,
    pub inspect: Option<&'a EntityId>,
}

pub fn project(g: &SystemGraph, t: &TableState, ctx: ProjectionContext<'_>) -> Rows {
    let f = &t.filter;
    match t.panel {
        Panel::Processes if ctx.tree && f.is_empty() => process_tree(g, t, ctx.collapsed),
        Panel::Processes => Rows::flat(sorted_nodes(g, EntityKind::Process, t)),
        Panel::Cpus => {
            let mut v: Vec<RowId> = nodes(g, EntityKind::LogicalCpu, f);
            if t.sort == SortKey::Name {
                v.sort_by_key(|r| {
                    r.entity()
                        .and_then(|id| g.node(id))
                        .and_then(|n| n.attr("index").and_then(|a| a.as_u64()))
                        .unwrap_or(u64::MAX)
                });
                if t.descending {
                    v.reverse();
                }
                Rows::flat(v)
            } else {
                Rows::flat(sorted(g, v, t))
            }
        }
        Panel::BlockDevices => Rows::flat(sorted_nodes(g, EntityKind::BlockDevice, t)),
        Panel::Mounts => Rows::flat(sorted_nodes(g, EntityKind::Mount, t)),
        Panel::Interfaces => Rows::flat(sorted_nodes(g, EntityKind::Interface, t)),
        Panel::Sockets => Rows::flat(sorted_nodes(g, EntityKind::Socket, t)),
        Panel::Communication => Rows::flat(sorted(g, edges(g, EdgeKind::CommunicatesWith, f), t)),
        Panel::Containers => Rows::flat(sorted_nodes(g, EntityKind::Container, t)),
        Panel::Services => Rows::flat(sorted_nodes(g, EntityKind::Service, t)),
        Panel::Gpus => Rows::flat(sorted_nodes(g, EntityKind::Gpu, t)),
        Panel::GpuClients => Rows::flat(sorted(g, edges(g, EdgeKind::UsesGpu, f), t)),
        Panel::Sensors => Rows::flat(sorted_nodes(g, EntityKind::Sensor, t)),
        Panel::Firewall => Rows::flat(sorted_nodes(g, EntityKind::FirewallRule, t)),
        Panel::Batteries => {
            let mut v = nodes(g, EntityKind::Battery, f);
            v.extend(nodes(g, EntityKind::PowerSupply, f));
            Rows::flat(v)
        }
        Panel::Events => {
            let base = g.events_dropped();
            let mut v: Vec<RowId> = g
                .events()
                .enumerate()
                .filter(|(_, e)| {
                    f.is_empty()
                        || e.message
                            .to_ascii_lowercase()
                            .contains(&f.source().to_ascii_lowercase())
                })
                .map(|(i, _)| RowId::Seq(base + i as u64))
                .collect();
            v.reverse(); // newest first
            Rows::flat(v)
        }
        Panel::Collectors => Rows::flat(
            g.collectors()
                .map(|(id, _)| RowId::Collector(id.as_str()))
                .collect(),
        ),
        Panel::Relations => match ctx.inspect {
            Some(id) => {
                let mut v: Vec<(EdgeKind, bool, String, RowId)> = g
                    .outgoing(id)
                    .map(|e| {
                        (
                            e.key.kind,
                            false,
                            label_of(g, &e.key.target),
                            RowId::Edge(e.key.clone()),
                        )
                    })
                    .chain(g.incoming(id).map(|e| {
                        (
                            e.key.kind,
                            true,
                            label_of(g, &e.key.source),
                            RowId::Edge(e.key.clone()),
                        )
                    }))
                    .filter(|(_, _, label, _)| {
                        f.is_empty()
                            || label
                                .to_ascii_lowercase()
                                .contains(&f.source().to_ascii_lowercase())
                    })
                    .collect();
                v.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
                Rows::flat(v.into_iter().map(|x| x.3).collect())
            }
            None => Rows::default(),
        },
    }
}

pub fn label_of(g: &SystemGraph, id: &EntityId) -> String {
    match g.node(id) {
        Some(n) => match n.attr("pid").and_then(|p| p.as_u64()) {
            Some(pid) => format!("{} ({pid})", n.label()),
            // Identical GPUs are told apart by their driver index.
            None => match n.attr("index").and_then(|i| i.as_u64()) {
                Some(i) if id.kind() == Some(EntityKind::Gpu) => format!("#{i} {}", n.label()),
                _ => n.label().to_owned(),
            },
        },
        None => id.to_string(),
    }
}

fn process_tree(g: &SystemGraph, t: &TableState, collapsed: &HashSet<EntityId>) -> Rows {
    let procs: Vec<RowId> = g
        .nodes_of(EntityKind::Process)
        .map(|n| RowId::Node(n.id.clone()))
        .collect();
    let present: HashSet<&EntityId> = procs.iter().filter_map(RowId::entity).collect();
    let mut children: HashMap<EntityId, Vec<RowId>> = HashMap::new();
    let mut has_parent = HashSet::new();
    for e in g.edges_of(EdgeKind::ParentOf) {
        if present.contains(&e.key.source)
            && present.contains(&e.key.target)
            && e.key.source != e.key.target
        {
            children
                .entry(e.key.source.clone())
                .or_default()
                .push(RowId::Node(e.key.target.clone()));
            has_parent.insert(e.key.target.clone());
        }
    }
    let roots: Vec<RowId> = procs
        .iter()
        .filter(|r| r.entity().is_some_and(|id| !has_parent.contains(id)))
        .cloned()
        .collect();
    let (mut ids, mut depth, mut kids) = (Vec::new(), Vec::new(), Vec::new());
    // Iterative DFS; `seen` guards against cycles in inconsistent snapshots.
    let mut seen = HashSet::new();
    let mut stack: Vec<(RowId, u16)> = sorted(g, roots, t)
        .into_iter()
        .rev()
        .map(|r| (r, 0))
        .collect();
    while let Some((r, d)) = stack.pop() {
        let Some(id) = r.entity().cloned() else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let ch = children.get(&id).cloned().unwrap_or_default();
        ids.push(r);
        depth.push(d);
        kids.push(!ch.is_empty());
        if !collapsed.contains(&id) {
            for c in sorted(g, ch, t).into_iter().rev() {
                stack.push((c, d.saturating_add(1)));
            }
        }
    }
    Rows::from(ids, depth, kids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rtop_core::{
        CollectorId, EdgeUpdate, Evidence, GraphPatch, Metric, NodeUpdate, Timestamp, Unavailable,
    };

    fn graph(cpus: &[Option<f64>]) -> SystemGraph {
        let mut g = SystemGraph::default();
        let mut p = GraphPatch::new(CollectorId("t"), 1, Timestamp(1));
        for (i, c) in cpus.iter().enumerate() {
            let m = c.map_or(Metric::Unavailable(Unavailable::Pending), Metric::f64);
            p.node(
                NodeUpdate::new(
                    EntityKind::Process,
                    EntityId::new(EntityKind::Process, format_args!("{}:1", i + 1)),
                )
                .attr("pid", (i + 1) as u64)
                .text("name", &format!("p{i}"))
                .metric("cpu_usage_percent", m),
            );
        }
        g.apply(p);
        g
    }

    fn ctx(c: &HashSet<EntityId>) -> ProjectionContext<'_> {
        ProjectionContext {
            tree: false,
            collapsed: c,
            inspect: None,
        }
    }

    proptest! {
        #[test]
        fn unavailable_sorts_last_in_both_directions_and_order_is_stable(cpus in proptest::collection::vec(proptest::option::of(0.0f64..400.0), 0..40), desc in any::<bool>()) {
            let g = graph(&cpus);
            let mut t = TableState::new(Panel::Processes, SortKey::Cpu);
            t.descending = desc;
            let c = HashSet::new();
            let rows = project(&g, &t, ctx(&c));
            prop_assert_eq!(rows.len(), cpus.len());
            let vals: Vec<Option<f64>> = rows.ids.iter().map(|r| g.node(r.entity().unwrap()).unwrap().f64("cpu_usage_percent")).collect();
            let first_missing = vals.iter().position(Option::is_none).unwrap_or(vals.len());
            prop_assert!(vals[first_missing..].iter().all(Option::is_none));
            let present: Vec<f64> = vals[..first_missing].iter().map(|v| v.unwrap()).collect();
            let ordered = present.windows(2).all(|w| if desc { w[0] >= w[1] } else { w[0] <= w[1] });
            prop_assert!(ordered);
            prop_assert_eq!(project(&g, &t, ctx(&c)).ids, rows.ids);
        }

        #[test]
        fn every_filtered_row_matches(cpus in proptest::collection::vec(proptest::option::of(0.0f64..100.0), 0..30), threshold in 0.0f64..100.0) {
            let g = graph(&cpus);
            let mut t = TableState::new(Panel::Processes, SortKey::Cpu);
            t.filter = Filter::parse(&format!("cpu>{threshold}")).unwrap();
            let c = HashSet::new();
            for r in project(&g, &t, ctx(&c)).ids {
                prop_assert!(g.node(r.entity().unwrap()).unwrap().f64("cpu_usage_percent").unwrap() > threshold);
            }
        }
    }

    #[test]
    fn tree_orders_children_under_parents_and_survives_cycles() {
        let mut g = graph(&[Some(1.0), Some(5.0), Some(3.0)]);
        let id = |n: u32| EntityId::new(EntityKind::Process, format_args!("{n}:1"));
        let mut p = GraphPatch::new(CollectorId("t"), 2, Timestamp(2));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            id(1),
            id(2),
            Evidence::KernelConfirmed,
        ));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            id(2),
            id(3),
            Evidence::KernelConfirmed,
        ));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            id(3),
            id(1),
            Evidence::KernelConfirmed,
        ));
        g.apply(p);
        let t = TableState::new(Panel::Processes, SortKey::Cpu);
        let c = HashSet::new();
        // Pure cycle: no roots, nothing shown, and no hang.
        assert!(
            project(
                &g,
                &t,
                ProjectionContext {
                    tree: true,
                    collapsed: &c,
                    inspect: None
                }
            )
            .is_empty()
        );
        let mut g2 = graph(&[Some(1.0), Some(5.0), Some(3.0)]);
        let mut p = GraphPatch::new(CollectorId("t"), 2, Timestamp(2));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            id(1),
            id(2),
            Evidence::KernelConfirmed,
        ));
        p.edge(EdgeUpdate::new(
            EdgeKind::ParentOf,
            id(1),
            id(3),
            Evidence::KernelConfirmed,
        ));
        g2.apply(p);
        let rows = project(
            &g2,
            &t,
            ProjectionContext {
                tree: true,
                collapsed: &c,
                inspect: None,
            },
        );
        assert_eq!(rows.depth, vec![0, 1, 1]);
        assert_eq!(rows.ids[1], RowId::Node(id(2)), "children sorted by CPU");
        let collapsed: HashSet<EntityId> = [id(1)].into();
        assert_eq!(
            project(
                &g2,
                &t,
                ProjectionContext {
                    tree: true,
                    collapsed: &collapsed,
                    inspect: None
                }
            )
            .len(),
            1
        );
    }
}
