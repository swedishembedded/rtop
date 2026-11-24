// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Sockets, their owners and who talks to whom.
//!
//! Relationships are only asserted with evidence:
//!
//! * `owns_socket` — `/proc/PID/fd/N` links to `socket:[inode]`
//!   (kernel-confirmed). A socket held by several processes (inherited
//!   across fork, passed with `SCM_RIGHTS`) has several owners.
//! * `connected_to` between Unix sockets — the kernel's `UNIX_DIAG_PEER`
//!   (socket-peer-confirmed). Between TCP sockets — both endpoints observed
//!   in this namespace with mirrored addresses (endpoint-matched).
//! * `communicates_with` between processes — derived from the two above;
//!   never inferred from compatible-looking addresses alone.
//!
//! Throughput is the TCP payload counted by the kernel in `tcp_info`
//! (`bytes_received`, `bytes_acked`). Sockets without such counters (UDP,
//! Unix, procfs fallback) report traffic as unavailable rather than
//! borrowing interface-wide rates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{
    CollectorId, EdgeKind, EdgeUpdate, EntityId, EntityKind, Evidence, Metric, NodeUpdate,
    ProcessIdentity, Scope, Unavailable,
};
use rtop_linux::KernelError;
use rtop_linux::parse::{net, pid};
use rtop_linux::sockdiag::{InetProto, InetSocket, UnixSocket};

use crate::{
    Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext,
    Subscriptions,
};

pub const ID: CollectorId = CollectorId("sockets");
const MAX_SOCKETS: usize = 100_000;
const MAX_FDS_PER_PASS: usize = 2_000_000;
const MAX_REMOTES: usize = 10_000;

#[derive(Debug)]
pub struct SocketCollector {
    env: Env,
    rates: RateTracker<(u64, SocketAddr, SocketAddr, bool)>,
    buf: Vec<u8>,
}

impl SocketCollector {
    pub fn new(env: Env) -> Self {
        SocketCollector {
            env,
            rates: RateTracker::new(),
            buf: Vec::new(),
        }
    }

    fn netns(&self) -> String {
        self.env
            .fs
            .read_link("/proc/self/ns/net")
            .ok()
            .and_then(|l| {
                l.strip_prefix("net:[")
                    .and_then(|r| r.strip_suffix(']'))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "0".into())
    }
}

pub fn socket_id(netns: &str, inode: u64) -> EntityId {
    EntityId::new(EntityKind::Socket, format_args!("{netns}:{inode}"))
}

fn proto_name(s: &InetSocket) -> &'static str {
    match (s.proto, s.local.is_ipv4()) {
        (InetProto::Tcp, true) => "tcp",
        (InetProto::Tcp, false) => "tcp6",
        (InetProto::Udp, true) => "udp",
        (InetProto::Udp, false) => "udp6",
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Traffic {
    rx: Option<f64>,
    tx: Option<f64>,
}

impl Collector for SocketCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
            isolation: Isolation::Expensive,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let ns = self.netns();
        let now = ctx.now;
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::Socket))
            .complete(Scope::Nodes(EntityKind::RemoteEndpoint))
            .complete(Scope::Nodes(EntityKind::Process))
            .complete(Scope::Edges(EdgeKind::OwnsSocket))
            .complete(Scope::Edges(EdgeKind::ConnectedTo))
            .complete(Scope::Edges(EdgeKind::CommunicatesWith));
        let mut degraded = Vec::new();

        let mut inet = Vec::new();
        for proto in [InetProto::Tcp, InetProto::Udp] {
            match self.env.diag.inet(proto) {
                Ok(v) => inet.extend(v),
                Err(KernelError::NotFound) => {}
                Err(e) => degraded.push(format!("{} sockets: {e}", proto.as_str())),
            }
        }
        let unix = match self.env.diag.unix() {
            Ok(v) => v,
            Err(e) => {
                degraded.push(format!("unix sockets: {e}"));
                Vec::new()
            }
        };
        if inet.is_empty() && unix.is_empty() && !degraded.is_empty() {
            return Err(CollectorError::new(
                Unavailable::Unsupported,
                degraded.join("; "),
            ));
        }

        let mut traffic: HashMap<u64, Traffic> = HashMap::new();
        let mut ids: HashMap<u64, EntityId> = HashMap::new();
        let (mut listen, mut established, mut time_wait) = (0u64, 0u64, 0u64);
        self.rates.begin();
        let local_ips: HashSet<IpAddr> = inet
            .iter()
            .map(|s| s.local.ip())
            .filter(|ip| !ip.is_unspecified())
            .collect();
        let mut by_tuple: HashMap<(SocketAddr, SocketAddr), u64> = HashMap::new();
        let mut remotes: BTreeMap<IpAddr, (Traffic, u64)> = BTreeMap::new();
        let mut remote_edges = Vec::new();

        for s in inet.iter().take(MAX_SOCKETS) {
            match (s.proto, s.state) {
                (InetProto::Tcp, 10) => listen += 1,
                (InetProto::Tcp, 1) => established += 1,
                (InetProto::Tcp, 6) => time_wait += 1,
                _ => {}
            }
            if s.inode == 0 {
                continue; // TIME_WAIT and orphaned sockets have no owner.
            }
            let id = socket_id(&ns, s.inode);
            let state = match s.proto {
                InetProto::Tcp => net::tcp_state_name(s.state),
                InetProto::Udp if s.remote.port() != 0 => "connected",
                InetProto::Udp => "unconnected",
            };
            let mut n = NodeUpdate::new(EntityKind::Socket, id.clone())
                .attr("protocol", proto_name(s))
                .attr("inode", s.inode)
                .attr("local_address", s.local.to_string())
                .attr("remote_address", s.remote.to_string())
                .attr("state", state)
                .attr("uid", s.uid)
                .metric("rx_queue_bytes", Metric::u64(s.rx_queue))
                .metric("tx_queue_bytes", Metric::u64(s.tx_queue));
            let mut t = Traffic::default();
            match s.tcp {
                Some(tcp) => {
                    let mut rate = |v: Option<u64>, dir: bool| match v {
                        Some(v) => self.rates.metric(
                            (s.inode, s.local, s.remote, dir),
                            v,
                            now,
                            CounterWidth::Bits64,
                        ),
                        None => Metric::unavailable(Unavailable::Unsupported),
                    };
                    let rx = rate(tcp.bytes_received, false);
                    let tx = rate(tcp.bytes_acked, true);
                    t = Traffic {
                        rx: rx.as_f64(),
                        tx: tx.as_f64(),
                    };
                    n.push_metric("rx_bytes_per_second", rx);
                    n.push_metric("tx_bytes_per_second", tx);
                    if let Some(r) = tcp.total_retrans {
                        n.push_metric("retransmits_total", Metric::u64(u64::from(r)));
                    }
                    if let Some(rtt) = tcp.rtt_us {
                        n.push_metric("rtt_ms", Metric::f64(f64::from(rtt) / 1000.0));
                    }
                }
                None => {
                    let reason = if s.proto == InetProto::Tcp {
                        Unavailable::Unsupported
                    } else {
                        Unavailable::NotMeasurable
                    };
                    n.push_metric("rx_bytes_per_second", Metric::unavailable(reason));
                    n.push_metric("tx_bytes_per_second", Metric::unavailable(reason));
                }
            }
            patch.node(n);
            traffic.insert(s.inode, t);
            ids.insert(s.inode, id.clone());
            if s.proto == InetProto::Tcp && s.state == 1 {
                by_tuple.insert((s.local, s.remote), s.inode);
                let rip = s.remote.ip();
                if !rip.is_loopback()
                    && !local_ips.contains(&rip)
                    && (remotes.len() < MAX_REMOTES || remotes.contains_key(&rip))
                {
                    let e = remotes.entry(rip).or_default();
                    e.0.rx = Some(e.0.rx.unwrap_or(0.0) + t.rx.unwrap_or(0.0));
                    e.0.tx = Some(e.0.tx.unwrap_or(0.0) + t.tx.unwrap_or(0.0));
                    e.1 += 1;
                    remote_edges.push((id, EntityId::new(EntityKind::RemoteEndpoint, rip)));
                }
            }
        }
        self.rates.sweep();

        // TCP peers inside this namespace: mirrored 4-tuples.
        let mut peers: Vec<(u64, u64, Evidence, &'static str)> = Vec::new();
        for (&(l, r), &ino) in &by_tuple {
            if let Some(&peer) = by_tuple.get(&(r, l)) {
                peers.push((ino, peer, Evidence::EndpointMatched, "tcp"));
            }
        }
        for u in unix.iter().take(MAX_SOCKETS) {
            let id = socket_id(&ns, u.inode);
            patch.node(unix_node(&id, u));
            ids.insert(u.inode, id);
            if let Some(peer) = u.peer {
                peers.push((u.inode, peer, Evidence::SocketPeerConfirmed, "unix"));
            }
        }
        for (a, b, ev, proto) in &peers {
            if let (Some(ia), Some(ib)) = (ids.get(a), ids.get(b)) {
                patch.edge(
                    EdgeUpdate::new(EdgeKind::ConnectedTo, ia.clone(), ib.clone(), *ev)
                        .attr("protocol", *proto),
                );
            }
        }
        for (ip, (t, count)) in &remotes {
            let id = EntityId::new(EntityKind::RemoteEndpoint, ip);
            let opt = |v: Option<f64>| {
                v.map_or(Metric::unavailable(Unavailable::Unsupported), Metric::f64)
            };
            patch.node(
                NodeUpdate::new(EntityKind::RemoteEndpoint, id)
                    .attr("name", ip.to_string())
                    .attr("address", ip.to_string())
                    .metric("connections", Metric::u64(*count))
                    .metric("rx_bytes_per_second", opt(t.rx))
                    .metric("tx_bytes_per_second", opt(t.tx)),
            );
        }
        for (s, r) in remote_edges {
            patch.edge(
                EdgeUpdate::new(EdgeKind::ConnectedTo, s, r, Evidence::KernelConfirmed)
                    .attr("protocol", "tcp"),
            );
        }

        if Subscriptions::get(&self.env.subscriptions.socket_ownership) {
            let owners = self.scan_owners(&ids, &mut degraded);
            let mut per_proc: HashMap<ProcessIdentity, (Traffic, u64)> = HashMap::new();
            for (ino, procs) in &owners {
                let t = traffic.get(ino).copied().unwrap_or_default();
                for p in procs {
                    patch.edge(EdgeUpdate::new(
                        EdgeKind::OwnsSocket,
                        p.entity_id(),
                        ids[ino].clone(),
                        Evidence::KernelConfirmed,
                    ));
                    let e = per_proc.entry(*p).or_default();
                    e.0.rx = sum(e.0.rx, t.rx);
                    e.0.tx = sum(e.0.tx, t.tx);
                    e.1 += 1;
                }
            }
            for (p, (t, count)) in per_proc {
                let m = |v: Option<f64>| {
                    v.map_or(Metric::unavailable(Unavailable::NotMeasurable), Metric::f64)
                };
                patch.node(
                    NodeUpdate::augment(EntityKind::Process, p.entity_id())
                        .metric("sockets_open", Metric::u64(count))
                        .metric("tcp_rx_bytes_per_second", m(t.rx))
                        .metric("tcp_tx_bytes_per_second", m(t.tx)),
                );
            }
            // Process-level communication from socket peers.
            let mut comm: BTreeMap<
                (ProcessIdentity, ProcessIdentity, &'static str),
                (Evidence, f64, u64),
            > = BTreeMap::new();
            for (a, b, ev, proto) in &peers {
                let (Some(oa), Some(ob)) = (owners.get(a), owners.get(b)) else {
                    continue;
                };
                let tx = traffic.get(a).and_then(|t| t.tx).unwrap_or(0.0);
                for pa in oa {
                    for pb in ob.iter().filter(|pb| *pb != pa) {
                        let e = comm.entry((*pa, *pb, proto)).or_insert((*ev, 0.0, 0));
                        e.1 += tx;
                        e.2 += 1;
                    }
                }
            }
            for ((a, b, proto), (ev, tx, n)) in comm {
                let tx_metric = if proto == "tcp" {
                    Metric::f64(tx)
                } else {
                    Metric::unavailable(Unavailable::NotMeasurable)
                };
                patch.edge(
                    EdgeUpdate::new(EdgeKind::CommunicatesWith, a.entity_id(), b.entity_id(), ev)
                        .attr("protocol", proto)
                        .metric("sockets", Metric::u64(n))
                        .metric("tx_bytes_per_second", tx_metric),
                );
            }
        }

        patch.node(
            NodeUpdate::new(EntityKind::Host, EntityId::host())
                .metric("sockets_tcp_listening", Metric::u64(listen))
                .metric("sockets_tcp_established", Metric::u64(established))
                .metric("sockets_tcp_time_wait", Metric::u64(time_wait))
                .metric(
                    "sockets_udp",
                    Metric::u64(inet.iter().filter(|s| s.proto == InetProto::Udp).count() as u64),
                )
                .metric("sockets_unix", Metric::u64(unix.len() as u64)),
        );
        let degraded = (!degraded.is_empty()).then(|| degraded.join("; "));
        Ok(Sample { patch, degraded })
    }
}

fn sum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

fn unix_node(id: &EntityId, u: &UnixSocket) -> NodeUpdate {
    let state = if u.listening {
        "listen"
    } else {
        match u.state {
            3 | 1 if u.peer.is_some() => "connected",
            3 => "connected",
            1 if u.sock_type == 2 => "unconnected",
            _ => "unconnected",
        }
    };
    let mut n = NodeUpdate::new(EntityKind::Socket, id.clone())
        .attr("protocol", "unix")
        .attr("inode", u.inode)
        .attr("socket_type", net::unix_type_name(u.sock_type))
        .attr("state", state)
        .text("path", &u.path);
    let q = |v: Option<u32>| {
        v.map_or(Metric::unavailable(Unavailable::Unsupported), |v| {
            Metric::u64(u64::from(v))
        })
    };
    n.push_metric("rx_queue_bytes", q(u.rx_queue));
    n.push_metric("tx_queue_bytes", q(u.tx_queue));
    n.push_metric(
        "rx_bytes_per_second",
        Metric::unavailable(Unavailable::NotMeasurable),
    );
    n.push_metric(
        "tx_bytes_per_second",
        Metric::unavailable(Unavailable::NotMeasurable),
    );
    n
}

impl SocketCollector {
    /// Maps socket inode → processes holding a descriptor to it.
    fn scan_owners(
        &mut self,
        known: &HashMap<u64, EntityId>,
        degraded: &mut Vec<String>,
    ) -> HashMap<u64, Vec<ProcessIdentity>> {
        let fs = self.env.fs.clone();
        let mut owners: HashMap<u64, Vec<ProcessIdentity>> = HashMap::new();
        let Ok(entries) = fs.read_dir("/proc") else {
            return owners;
        };
        let (mut fds, mut denied) = (0usize, 0usize);
        for name in entries
            .iter()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            let Ok(pid_num) = name.parse::<u32>() else {
                continue;
            };
            let links = match fs.read_dir(&format!("/proc/{name}/fd")) {
                Ok(l) => l,
                Err(KernelError::PermissionDenied) => {
                    denied += 1;
                    continue;
                }
                Err(_) => continue,
            };
            if links.is_empty()
                || fs
                    .read(&format!("/proc/{name}/stat"), &mut self.buf, SMALL)
                    .is_err()
            {
                continue;
            }
            let Some(st) = pid::stat(&self.buf) else {
                continue;
            };
            let ident = ProcessIdentity {
                pid: pid_num,
                start_time_ticks: st.starttime,
            };
            let mut mine = HashSet::new();
            for fd in links {
                fds += 1;
                if fds > MAX_FDS_PER_PASS {
                    degraded.push("descriptor scan truncated".into());
                    return owners;
                }
                if let Ok(link) = fs.read_link(&format!("/proc/{name}/fd/{fd}")) {
                    if let pid::FdTarget::Socket(ino) = pid::fd_target(&link) {
                        if known.contains_key(&ino) && mine.insert(ino) {
                            owners.entry(ino).or_default().push(ident);
                        }
                    }
                }
            }
        }
        if denied > 0 {
            degraded.push(format!(
                "socket owners unknown for {denied} processes of other users"
            ));
        }
        owners
    }
}
