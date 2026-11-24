// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The unified system telemetry graph and its patch reconciliation rules.
//!
//! # Ownership and reconciliation
//!
//! Only the graph owner calls [`SystemGraph::apply`]. Collectors describe
//! what they observed in a [`GraphPatch`]:
//!
//! * Every node, edge, attribute and metric remembers which collector
//!   asserted it (its *contributors*). A collector can only ever retract its
//!   own contributions; it cannot delete another collector's observations.
//! * A patch lists the [`Scope`]s it observed *exhaustively*. For those
//!   scopes, anything the collector asserted earlier but not in this patch is
//!   retracted. A partial scan simply omits the scope and deletes nothing.
//! * An entity disappears when its last contributor retracts it; its
//!   incident edges go with it.
//! * Patches carry a per-collector sequence number; an older or duplicate
//!   patch is rejected, so a slow worker can never overwrite newer data.
//! * Edges may reference entities that are not (yet) in the graph — a
//!   socket can be seen before its owning process. They are kept and
//!   reported as unresolved until the endpoint appears.
//!
//! Because collectors publish full observations of their scopes, a newer
//! patch supersedes an older unconsumed one completely. The runtime relies on
//! this to keep only the latest patch per collector under backpressure.
//!
//! Storage uses ordered maps keyed by stable ids rather than a slot arena:
//! ids embed collision-free kernel identities (PID + start time, ...), so
//! there are no recycled slots to guard against, iteration order is
//! deterministic for exports, and all nodes of one kind form a contiguous
//! key range.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ops::Bound;
use std::time::Duration;

use crate::identity::{CollectorId, EdgeKey, EdgeKind, EntityId, EntityKind, Evidence};
use crate::metric::{Attr, Metric, Unavailable};
use crate::text::sanitize;
use crate::time::Timestamp;

#[derive(Debug, Clone, PartialEq)]
pub struct MetricEntry {
    pub metric: Metric,
    pub source: CollectorId,
    pub observed_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttrEntry {
    pub value: Attr,
    pub source: CollectorId,
}

type Attrs = BTreeMap<&'static str, AttrEntry>;
type Metrics = BTreeMap<&'static str, MetricEntry>;

fn attr_of<'a>(attrs: &'a Attrs, key: &str) -> Option<&'a Attr> {
    attrs.get(key).map(|e| &e.value)
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: EntityId,
    pub kind: EntityKind,
    pub attrs: Attrs,
    pub metrics: Metrics,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    contributors: BTreeMap<CollectorId, u64>,
}

impl Node {
    pub fn attr(&self, key: &str) -> Option<&Attr> {
        attr_of(&self.attrs, key)
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.attr(key).and_then(Attr::as_text)
    }

    /// `None` means no collector reported the metric at all, which is itself
    /// a form of unavailability (see [`Node::metric_or_missing`]).
    pub fn metric(&self, key: &str) -> Option<&Metric> {
        self.metrics.get(key).map(|e| &e.metric)
    }

    pub fn metric_or_missing(&self, key: &str) -> Metric {
        self.metric(key)
            .copied()
            .unwrap_or(Metric::Unavailable(Unavailable::NotCollected))
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.metric(key).and_then(Metric::as_f64)
    }

    pub fn contributors(&self) -> impl Iterator<Item = CollectorId> + '_ {
        self.contributors.keys().copied()
    }

    /// Display name: `name` attribute, falling back to the id key.
    pub fn label(&self) -> &str {
        self.text("name").unwrap_or_else(|| self.id.key())
    }
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub key: EdgeKey,
    pub evidence: Evidence,
    pub attrs: Attrs,
    pub metrics: Metrics,
    pub first_seen: Timestamp,
    pub last_seen: Timestamp,
    contributors: BTreeMap<CollectorId, u64>,
}

impl Edge {
    pub fn attr(&self, key: &str) -> Option<&Attr> {
        attr_of(&self.attrs, key)
    }

    pub fn metric(&self, key: &str) -> Option<&Metric> {
        self.metrics.get(key).map(|e| &e.metric)
    }
}

/// A node observation.
#[derive(Debug, Clone)]
pub struct NodeUpdate {
    pub id: EntityId,
    pub kind: EntityKind,
    pub attrs: Vec<(&'static str, Attr)>,
    pub metrics: Vec<(&'static str, Metric)>,
    /// When false the update only augments an entity another collector
    /// owns; it is dropped if the entity does not exist (yet).
    pub create: bool,
}

impl NodeUpdate {
    pub fn new(kind: EntityKind, id: EntityId) -> Self {
        NodeUpdate {
            id,
            kind,
            attrs: Vec::new(),
            metrics: Vec::new(),
            create: true,
        }
    }

    /// An update that adds metrics/attributes to an existing entity without
    /// ever creating it (e.g. network rates for a process).
    pub fn augment(kind: EntityKind, id: EntityId) -> Self {
        NodeUpdate {
            create: false,
            ..NodeUpdate::new(kind, id)
        }
    }

    pub fn attr(mut self, key: &'static str, value: impl Into<Attr>) -> Self {
        self.attrs.push((key, value.into()));
        self
    }

    /// Sets a text attribute after neutralising control characters.
    pub fn text(self, key: &'static str, value: &str) -> Self {
        let clean = sanitize(value).into_owned();
        self.attr(key, clean)
    }

    pub fn metric(mut self, key: &'static str, value: Metric) -> Self {
        self.metrics.push((key, value));
        self
    }

    pub fn push_metric(&mut self, key: &'static str, value: Metric) {
        self.metrics.push((key, value));
    }

    pub fn push_attr(&mut self, key: &'static str, value: impl Into<Attr>) {
        self.attrs.push((key, value.into()));
    }
}

/// An edge observation.
#[derive(Debug, Clone)]
pub struct EdgeUpdate {
    pub key: EdgeKey,
    pub evidence: Evidence,
    pub attrs: Vec<(&'static str, Attr)>,
    pub metrics: Vec<(&'static str, Metric)>,
}

impl EdgeUpdate {
    pub fn new(kind: EdgeKind, source: EntityId, target: EntityId, evidence: Evidence) -> Self {
        EdgeUpdate {
            key: EdgeKey {
                kind,
                source,
                target,
            },
            evidence,
            attrs: Vec::new(),
            metrics: Vec::new(),
        }
    }

    pub fn attr(mut self, key: &'static str, value: impl Into<Attr>) -> Self {
        self.attrs.push((key, value.into()));
        self
    }

    pub fn metric(mut self, key: &'static str, value: Metric) -> Self {
        self.metrics.push((key, value));
        self
    }
}

/// What a patch observed exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Nodes(EntityKind),
    Edges(EdgeKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventCategory {
    Security,
    Oom,
    Process,
    Device,
    Collector,
    Kernel,
}

impl EventCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            EventCategory::Security => "security",
            EventCategory::Oom => "oom",
            EventCategory::Process => "process",
            EventCategory::Device => "device",
            EventCategory::Collector => "collector",
            EventCategory::Kernel => "kernel",
        }
    }
}

/// A timestamped occurrence (denial, OOM kill, collector failure, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryEvent {
    pub at: Timestamp,
    /// Seconds since the Unix epoch, when the source provides wall time.
    pub wall_time: Option<f64>,
    pub severity: Severity,
    pub category: EventCategory,
    pub source: CollectorId,
    pub message: String,
    pub subjects: Vec<EntityId>,
}

impl TelemetryEvent {
    pub fn new(
        at: Timestamp,
        severity: Severity,
        category: EventCategory,
        source: CollectorId,
        message: &str,
    ) -> Self {
        TelemetryEvent {
            at,
            wall_time: None,
            severity,
            category,
            source,
            message: sanitize(message).into_owned(),
            subjects: Vec::new(),
        }
    }
}

/// A collector's observations at one point in time.
#[derive(Debug, Clone)]
pub struct GraphPatch {
    pub source: CollectorId,
    pub sequence: u64,
    pub observed_at: Timestamp,
    pub complete: Vec<Scope>,
    pub nodes: Vec<NodeUpdate>,
    pub edges: Vec<EdgeUpdate>,
    pub events: Vec<TelemetryEvent>,
}

impl GraphPatch {
    pub fn new(source: CollectorId, sequence: u64, observed_at: Timestamp) -> Self {
        GraphPatch {
            source,
            sequence,
            observed_at,
            complete: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn complete(mut self, scope: Scope) -> Self {
        self.complete.push(scope);
        self
    }

    pub fn node(&mut self, n: NodeUpdate) {
        self.nodes.push(n);
    }

    pub fn edge(&mut self, e: EdgeUpdate) {
        self.edges.push(e);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied {
        added: usize,
        removed: usize,
        rejected: usize,
    },
    /// Sequence not newer than the last applied patch from this collector.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CollectorState {
    /// Discovered but no sample completed yet.
    Pending,
    Available,
    /// Some sources failed; partial data.
    Degraded,
    /// Sample overdue; last data kept and marked stale.
    Stale,
    /// Worker has not returned for longer than its timeout.
    Blocked,
    /// Not usable on this system (unsupported, permission denied).
    Unavailable,
    /// Switched off (e.g. emergency mode).
    Disabled,
}

impl CollectorState {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectorState::Pending => "pending",
            CollectorState::Available => "available",
            CollectorState::Degraded => "degraded",
            CollectorState::Stale => "stale",
            CollectorState::Blocked => "blocked",
            CollectorState::Unavailable => "unavailable",
            CollectorState::Disabled => "disabled",
        }
    }

    /// Whether data from a collector in this state should be shown as stale.
    pub fn is_stale(self) -> bool {
        matches!(
            self,
            CollectorState::Stale | CollectorState::Blocked | CollectorState::Disabled
        )
    }
}

/// Health of one collector, maintained by the runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectorStatus {
    pub state: CollectorState,
    pub reason: Option<Unavailable>,
    pub detail: Option<String>,
    pub last_duration: Option<Duration>,
    pub last_success: Option<Timestamp>,
    pub interval: Duration,
    pub samples: u64,
    pub failures: u64,
    pub missed_deadlines: u64,
}

impl CollectorStatus {
    pub fn pending(interval: Duration) -> Self {
        CollectorStatus {
            state: CollectorState::Pending,
            reason: None,
            detail: None,
            last_duration: None,
            last_success: None,
            interval,
            samples: 0,
            failures: 0,
            missed_deadlines: 0,
        }
    }
}

/// Limits that keep the graph bounded however large the system is.
#[derive(Debug, Clone, Copy)]
pub struct GraphLimits {
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_events: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        GraphLimits {
            max_nodes: 500_000,
            max_edges: 2_000_000,
            max_events: 1024,
        }
    }
}

type AdjKey = (EntityId, EdgeKind, EntityId);

/// The live model of the observable system.
#[derive(Debug)]
pub struct SystemGraph {
    generation: u64,
    observed_at: Timestamp,
    nodes: BTreeMap<EntityId, Node>,
    edges: BTreeMap<EdgeKey, Edge>,
    outgoing: BTreeSet<AdjKey>,
    incoming: BTreeSet<AdjKey>,
    events: VecDeque<TelemetryEvent>,
    events_dropped: u64,
    collectors: BTreeMap<CollectorId, CollectorStatus>,
    last_sequence: HashMap<CollectorId, u64>,
    limits: GraphLimits,
    rejected_total: u64,
}

impl Default for SystemGraph {
    fn default() -> Self {
        Self::new(GraphLimits::default())
    }
}

impl SystemGraph {
    pub fn new(limits: GraphLimits) -> Self {
        SystemGraph {
            generation: 0,
            observed_at: Timestamp::ZERO,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            outgoing: BTreeSet::new(),
            incoming: BTreeSet::new(),
            events: VecDeque::new(),
            events_dropped: 0,
            collectors: BTreeMap::new(),
            last_sequence: HashMap::new(),
            limits,
            rejected_total: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Updates that were refused (kind mismatch or size limits).
    pub fn rejected_total(&self) -> u64 {
        self.rejected_total
    }

    pub fn events_dropped(&self) -> u64 {
        self.events_dropped
    }

    pub fn apply(&mut self, patch: GraphPatch) -> ApplyOutcome {
        let src = patch.source;
        let seq = patch.sequence;
        if self
            .last_sequence
            .get(&src)
            .is_some_and(|last| seq <= *last)
        {
            return ApplyOutcome::Stale;
        }
        self.last_sequence.insert(src, seq);
        let at = patch.observed_at;
        let (mut added, mut removed, mut rejected) = (0, 0, 0);

        for upd in patch.nodes {
            if upd.id.kind() != Some(upd.kind) {
                rejected += 1;
                continue;
            }
            let is_new = !self.nodes.contains_key(&upd.id);
            if is_new && !upd.create {
                continue;
            }
            if is_new && self.nodes.len() >= self.limits.max_nodes {
                rejected += 1;
                continue;
            }
            let node = self.nodes.entry(upd.id.clone()).or_insert_with(|| {
                added += 1;
                Node {
                    id: upd.id.clone(),
                    kind: upd.kind,
                    attrs: BTreeMap::new(),
                    metrics: BTreeMap::new(),
                    first_seen: at,
                    last_seen: at,
                    contributors: BTreeMap::new(),
                }
            });
            if node.kind != upd.kind {
                rejected += 1;
                continue;
            }
            node.contributors.insert(src, seq);
            node.last_seen = node.last_seen.max(at);
            for (k, v) in upd.attrs {
                node.attrs.insert(
                    k,
                    AttrEntry {
                        value: v,
                        source: src,
                    },
                );
            }
            for (k, m) in upd.metrics {
                node.metrics.insert(
                    k,
                    MetricEntry {
                        metric: m,
                        source: src,
                        observed_at: at,
                    },
                );
            }
        }

        for upd in patch.edges {
            let is_new = !self.edges.contains_key(&upd.key);
            if is_new && self.edges.len() >= self.limits.max_edges {
                rejected += 1;
                continue;
            }
            if is_new {
                self.outgoing.insert((
                    upd.key.source.clone(),
                    upd.key.kind,
                    upd.key.target.clone(),
                ));
                self.incoming.insert((
                    upd.key.target.clone(),
                    upd.key.kind,
                    upd.key.source.clone(),
                ));
            }
            let edge = self.edges.entry(upd.key.clone()).or_insert_with(|| Edge {
                key: upd.key.clone(),
                evidence: upd.evidence,
                attrs: BTreeMap::new(),
                metrics: BTreeMap::new(),
                first_seen: at,
                last_seen: at,
                contributors: BTreeMap::new(),
            });
            edge.contributors.insert(src, seq);
            edge.last_seen = edge.last_seen.max(at);
            // Strongest evidence wins; enum order lists strongest first.
            edge.evidence = edge.evidence.min(upd.evidence);
            for (k, v) in upd.attrs {
                edge.attrs.insert(
                    k,
                    AttrEntry {
                        value: v,
                        source: src,
                    },
                );
            }
            for (k, m) in upd.metrics {
                edge.metrics.insert(
                    k,
                    MetricEntry {
                        metric: m,
                        source: src,
                        observed_at: at,
                    },
                );
            }
        }

        for scope in &patch.complete {
            removed += match *scope {
                Scope::Nodes(kind) => self.retract_nodes(src, Some(kind), Some(seq)),
                Scope::Edges(kind) => self.retract_edges(src, Some(kind), Some(seq)),
            };
        }

        for ev in patch.events {
            self.push_event(ev);
        }

        self.rejected_total += rejected as u64;
        self.generation += 1;
        self.observed_at = self.observed_at.max(at);
        ApplyOutcome::Applied {
            added,
            removed,
            rejected,
        }
    }

    /// Removes every contribution of `src` (e.g. a permanently failed
    /// provider). Returns how many entities and edges disappeared.
    pub fn retire_collector(&mut self, src: CollectorId) -> usize {
        let n = self.retract_nodes(src, None, None) + self.retract_edges(src, None, None);
        self.generation += 1;
        n
    }

    /// Retracts `src`'s contributions to nodes (of `kind`, if given) that were
    /// not asserted by patch `keep_seq`.
    fn retract_nodes(
        &mut self,
        src: CollectorId,
        kind: Option<EntityKind>,
        keep_seq: Option<u64>,
    ) -> usize {
        let candidates: Vec<EntityId> = match kind {
            Some(k) => {
                let prefix = EntityId::kind_prefix(k);
                self.nodes
                    .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
                    .take_while(|(id, _)| id.as_str().starts_with(&prefix))
                    .filter(|(_, n)| {
                        n.contributors
                            .get(&src)
                            .is_some_and(|s| Some(*s) != keep_seq)
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            }
            None => self
                .nodes
                .iter()
                .filter(|(_, n)| n.contributors.contains_key(&src))
                .map(|(id, _)| id.clone())
                .collect(),
        };
        let mut removed = 0;
        for id in candidates {
            let gone = {
                let n = self.nodes.get_mut(&id).expect("candidate exists");
                n.contributors.remove(&src);
                n.metrics.retain(|_, m| m.source != src);
                n.attrs.retain(|_, a| a.source != src);
                n.contributors.is_empty()
            };
            if gone {
                self.remove_node(&id);
                removed += 1;
            }
        }
        removed
    }

    fn retract_edges(
        &mut self,
        src: CollectorId,
        kind: Option<EdgeKind>,
        keep_seq: Option<u64>,
    ) -> usize {
        let stale = |e: &Edge| {
            e.contributors
                .get(&src)
                .is_some_and(|s| Some(*s) != keep_seq)
        };
        let candidates: Vec<EdgeKey> = match kind {
            Some(k) => {
                let lo = EdgeKey {
                    kind: k,
                    source: EntityId::min(),
                    target: EntityId::min(),
                };
                self.edges
                    .range(lo..)
                    .take_while(|(key, _)| key.kind == k)
                    .filter(|(_, e)| stale(e))
                    .map(|(k, _)| k.clone())
                    .collect()
            }
            None => self
                .edges
                .iter()
                .filter(|(_, e)| e.contributors.contains_key(&src))
                .map(|(k, _)| k.clone())
                .collect(),
        };
        let mut removed = 0;
        for key in candidates {
            let gone = {
                let e = self.edges.get_mut(&key).expect("candidate exists");
                e.contributors.remove(&src);
                e.metrics.retain(|_, m| m.source != src);
                e.attrs.retain(|_, a| a.source != src);
                e.contributors.is_empty()
            };
            if gone {
                self.remove_edge(&key);
                removed += 1;
            }
        }
        removed
    }

    fn remove_node(&mut self, id: &EntityId) {
        self.nodes.remove(id);
        let incident: Vec<EdgeKey> = self
            .outgoing_keys(id)
            .chain(self.incoming_keys(id))
            .collect();
        for key in incident {
            self.remove_edge(&key);
        }
    }

    fn remove_edge(&mut self, key: &EdgeKey) {
        if self.edges.remove(key).is_some() {
            self.outgoing
                .remove(&(key.source.clone(), key.kind, key.target.clone()));
            self.incoming
                .remove(&(key.target.clone(), key.kind, key.source.clone()));
        }
    }

    fn adj_range<'a>(
        set: &'a BTreeSet<AdjKey>,
        id: &'a EntityId,
    ) -> impl Iterator<Item = &'a AdjKey> + 'a {
        let lo = (id.clone(), EdgeKind::ALL[0], EntityId::min());
        set.range(lo..).take_while(move |(a, _, _)| a == id)
    }

    fn outgoing_keys<'a>(&'a self, id: &'a EntityId) -> impl Iterator<Item = EdgeKey> + 'a {
        Self::adj_range(&self.outgoing, id).map(|(s, k, t)| EdgeKey {
            kind: *k,
            source: s.clone(),
            target: t.clone(),
        })
    }

    fn incoming_keys<'a>(&'a self, id: &'a EntityId) -> impl Iterator<Item = EdgeKey> + 'a {
        Self::adj_range(&self.incoming, id).map(|(t, k, s)| EdgeKey {
            kind: *k,
            source: s.clone(),
            target: t.clone(),
        })
    }

    fn push_event(&mut self, ev: TelemetryEvent) {
        if self.limits.max_events == 0 {
            self.events_dropped += 1;
            return;
        }
        if self.events.len() >= self.limits.max_events {
            self.events.pop_front();
            self.events_dropped += 1;
        }
        self.events.push_back(ev);
    }

    // ----- queries -------------------------------------------------------

    pub fn node(&self, id: &EntityId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// All nodes of one kind, in id order.
    pub fn nodes_of(&self, kind: EntityKind) -> impl Iterator<Item = &Node> {
        let prefix = EntityId::kind_prefix(kind);
        let start = prefix.clone();
        self.nodes
            .range::<str, _>((Bound::Included(start.as_str()), Bound::Unbounded))
            .take_while(move |(id, _)| id.as_str().starts_with(&prefix))
            .map(|(_, n)| n)
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }

    pub fn edges_of(&self, kind: EdgeKind) -> impl Iterator<Item = &Edge> {
        let lo = EdgeKey {
            kind,
            source: EntityId::min(),
            target: EntityId::min(),
        };
        self.edges
            .range(lo..)
            .take_while(move |(k, _)| k.kind == kind)
            .map(|(_, e)| e)
    }

    pub fn edge(&self, key: &EdgeKey) -> Option<&Edge> {
        self.edges.get(key)
    }

    /// Edges leaving `id`.
    pub fn outgoing<'a>(&'a self, id: &'a EntityId) -> impl Iterator<Item = &'a Edge> + 'a {
        self.outgoing_keys(id)
            .filter_map(move |k| self.edges.get(&k))
    }

    /// Edges arriving at `id`.
    pub fn incoming<'a>(&'a self, id: &'a EntityId) -> impl Iterator<Item = &'a Edge> + 'a {
        self.incoming_keys(id)
            .filter_map(move |k| self.edges.get(&k))
    }

    /// Whether both endpoints of an edge are present.
    pub fn is_resolved(&self, edge: &Edge) -> bool {
        self.nodes.contains_key(&edge.key.source) && self.nodes.contains_key(&edge.key.target)
    }

    pub fn events(&self) -> impl DoubleEndedIterator<Item = &TelemetryEvent> + ExactSizeIterator {
        self.events.iter()
    }

    pub fn set_collector_status(&mut self, id: CollectorId, status: CollectorStatus) {
        if self.collectors.get(&id) != Some(&status) {
            self.collectors.insert(id, status);
            self.generation += 1;
        }
    }

    pub fn collector_status(&self, id: CollectorId) -> Option<&CollectorStatus> {
        self.collectors.get(&id)
    }

    pub fn collectors(&self) -> impl Iterator<Item = (&CollectorId, &CollectorStatus)> {
        self.collectors.iter()
    }

    /// Whether the data `source` contributed should be shown as stale.
    pub fn is_stale(&self, source: CollectorId) -> bool {
        self.collectors
            .get(&source)
            .is_some_and(|s| s.state.is_stale())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const PROC: CollectorId = CollectorId("processes");
    const SOCK: CollectorId = CollectorId("sockets");

    fn pid(n: u32) -> EntityId {
        EntityId::new(EntityKind::Process, format_args!("{n}:1"))
    }

    fn sock(n: u32) -> EntityId {
        EntityId::new(EntityKind::Socket, format_args!("net1:{n}"))
    }

    fn procs(seq: u64, pids: &[u32]) -> GraphPatch {
        let mut p =
            GraphPatch::new(PROC, seq, Timestamp(seq)).complete(Scope::Nodes(EntityKind::Process));
        for &n in pids {
            p.node(NodeUpdate::new(EntityKind::Process, pid(n)).metric("cpu", Metric::f64(1.0)));
        }
        p
    }

    #[test]
    fn complete_scope_removes_unseen_entities_and_their_edges() {
        let mut g = SystemGraph::default();
        g.apply(procs(1, &[1, 2]));
        let mut s = GraphPatch::new(SOCK, 1, Timestamp(1));
        s.edge(EdgeUpdate::new(
            EdgeKind::OwnsSocket,
            pid(2),
            sock(9),
            Evidence::KernelConfirmed,
        ));
        g.apply(s);
        assert_eq!(g.edge_count(), 1);
        g.apply(procs(2, &[1]));
        assert!(g.node(&pid(2)).is_none());
        assert_eq!(
            g.edge_count(),
            0,
            "obsolete relationship removed with its process"
        );
    }

    #[test]
    fn partial_scan_does_not_delete() {
        let mut g = SystemGraph::default();
        g.apply(procs(1, &[1, 2]));
        let mut p = GraphPatch::new(PROC, 2, Timestamp(2));
        p.node(NodeUpdate::new(EntityKind::Process, pid(1)));
        g.apply(p);
        assert!(g.node(&pid(2)).is_some());
    }

    #[test]
    fn old_and_duplicate_patches_are_rejected() {
        let mut g = SystemGraph::default();
        g.apply(procs(5, &[1]));
        assert_eq!(g.apply(procs(5, &[1, 2])), ApplyOutcome::Stale);
        assert_eq!(g.apply(procs(4, &[])), ApplyOutcome::Stale);
        assert!(g.node(&pid(1)).is_some());
        assert!(g.node(&pid(2)).is_none());
    }

    #[test]
    fn collector_cannot_remove_other_collectors_observations() {
        let mut g = SystemGraph::default();
        g.apply(procs(1, &[1]));
        let mut s =
            GraphPatch::new(SOCK, 1, Timestamp(1)).complete(Scope::Nodes(EntityKind::Process));
        s.node(NodeUpdate::new(EntityKind::Process, pid(1)).metric("sockets", Metric::u64(3)));
        g.apply(s);
        // sockets no longer reports pid 1: only its metric goes.
        g.apply(GraphPatch::new(SOCK, 2, Timestamp(2)).complete(Scope::Nodes(EntityKind::Process)));
        let n = g.node(&pid(1)).expect("still owned by processes");
        assert!(n.metric("sockets").is_none());
        assert!(n.metric("cpu").is_some());
    }

    #[test]
    fn socket_seen_before_process_is_unresolved_then_reconciled() {
        let mut g = SystemGraph::default();
        let mut s = GraphPatch::new(SOCK, 1, Timestamp(1));
        s.node(NodeUpdate::new(EntityKind::Socket, sock(9)));
        s.edge(EdgeUpdate::new(
            EdgeKind::OwnsSocket,
            pid(7),
            sock(9),
            Evidence::KernelConfirmed,
        ));
        g.apply(s);
        let e = g.edges().next().unwrap().clone();
        assert!(!g.is_resolved(&e));
        g.apply(procs(1, &[7]));
        assert!(g.is_resolved(&e));
        assert_eq!(g.outgoing(&pid(7)).count(), 1);
        assert_eq!(g.incoming(&sock(9)).count(), 1);
    }

    #[test]
    fn augment_never_creates_entities() {
        let mut g = SystemGraph::default();
        let mut s = GraphPatch::new(SOCK, 1, Timestamp(1));
        s.node(NodeUpdate::augment(EntityKind::Process, pid(1)).metric("rx", Metric::u64(1)));
        g.apply(s);
        assert!(g.node(&pid(1)).is_none());
        g.apply(procs(1, &[1]));
        let mut s = GraphPatch::new(SOCK, 2, Timestamp(2));
        s.node(NodeUpdate::augment(EntityKind::Process, pid(1)).metric("rx", Metric::u64(1)));
        g.apply(s);
        assert!(g.node(&pid(1)).unwrap().metric("rx").is_some());
    }

    #[test]
    fn shared_socket_has_multiple_owners() {
        let mut g = SystemGraph::default();
        let mut s = GraphPatch::new(SOCK, 1, Timestamp(1));
        s.node(NodeUpdate::new(EntityKind::Socket, sock(9)));
        s.edge(EdgeUpdate::new(
            EdgeKind::OwnsSocket,
            pid(1),
            sock(9),
            Evidence::KernelConfirmed,
        ));
        s.edge(EdgeUpdate::new(
            EdgeKind::OwnsSocket,
            pid(2),
            sock(9),
            Evidence::KernelConfirmed,
        ));
        g.apply(s);
        assert_eq!(g.incoming(&sock(9)).count(), 2);
    }

    #[test]
    fn kind_mismatch_and_limits_are_rejected() {
        let mut g = SystemGraph::new(GraphLimits {
            max_nodes: 2,
            max_edges: 10,
            max_events: 2,
        });
        let mut p = GraphPatch::new(PROC, 1, Timestamp(1));
        p.node(NodeUpdate::new(EntityKind::Socket, pid(1)));
        for n in 0..5 {
            p.node(NodeUpdate::new(EntityKind::Process, pid(n)));
        }
        assert_eq!(
            g.apply(p),
            ApplyOutcome::Applied {
                added: 2,
                removed: 0,
                rejected: 4
            }
        );
    }

    #[test]
    fn event_ring_is_bounded() {
        let mut g = SystemGraph::new(GraphLimits {
            max_events: 3,
            ..Default::default()
        });
        let mut p = GraphPatch::new(PROC, 1, Timestamp(1));
        for i in 0..10 {
            p.events.push(TelemetryEvent::new(
                Timestamp(i),
                Severity::Info,
                EventCategory::Kernel,
                PROC,
                "x",
            ));
        }
        g.apply(p);
        assert_eq!(g.events().len(), 3);
        assert_eq!(g.events_dropped(), 7);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Patch {
            src: bool,
            seq: u64,
            complete: bool,
            nodes: Vec<u8>,
            edges: Vec<(u8, u8)>,
        },
        Retire(bool),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (
                any::<bool>(),
                0u64..20,
                any::<bool>(),
                proptest::collection::vec(0u8..12, 0..8),
                proptest::collection::vec((0u8..12, 0u8..12), 0..8)
            )
                .prop_map(|(src, seq, complete, nodes, edges)| Op::Patch {
                    src,
                    seq,
                    complete,
                    nodes,
                    edges
                }),
            any::<bool>().prop_map(Op::Retire),
        ]
    }

    proptest! {
        #[test]
        fn graph_invariants_hold_after_arbitrary_patches(ops in proptest::collection::vec(op(), 0..40)) {
            let mut g = SystemGraph::default();
            for op in ops {
                match op {
                    Op::Patch { src, seq, complete, nodes, edges } => {
                        let source = if src { PROC } else { SOCK };
                        let mut p = GraphPatch::new(source, seq, Timestamp(seq));
                        if complete {
                            p = p.complete(Scope::Nodes(EntityKind::Process)).complete(Scope::Edges(EdgeKind::ParentOf));
                        }
                        for n in nodes { p.node(NodeUpdate::new(EntityKind::Process, pid(u32::from(n)))); }
                        for (a, b) in edges { p.edge(EdgeUpdate::new(EdgeKind::ParentOf, pid(u32::from(a)), pid(u32::from(b)), Evidence::KernelConfirmed)); }
                        g.apply(p);
                    }
                    Op::Retire(src) => { g.retire_collector(if src { PROC } else { SOCK }); }
                }
                // Adjacency indexes mirror the edge store exactly.
                prop_assert_eq!(g.outgoing.len(), g.edges.len());
                prop_assert_eq!(g.incoming.len(), g.edges.len());
                for e in g.edges() {
                    prop_assert!(!e.contributors.is_empty());
                    prop_assert!(g.outgoing.contains(&(e.key.source.clone(), e.key.kind, e.key.target.clone())));
                }
                for n in g.nodes() {
                    prop_assert!(!n.contributors.is_empty());
                    prop_assert_eq!(n.id.kind(), Some(n.kind));
                }
            }
        }
    }
}
