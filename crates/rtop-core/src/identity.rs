// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Stable identities for graph entities, edges and collectors.
//!
//! An [`EntityId`] is `"<kind>:<key>"`, where the key is chosen so that a
//! recycled kernel number never aliases an earlier entity: processes are
//! keyed by PID *and* start time, sockets by inode within a network
//! namespace, and so on. Ids are reference-counted strings so they can be
//! held in several indexes cheaply.

use std::fmt;
use std::sync::Arc;

/// Name of the collector that produced an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CollectorId(pub &'static str);

impl CollectorId {
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for CollectorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$variant),)+ _ => None }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(
    /// Kind of a graph node.
    EntityKind {
        Host => "host",
        CpuPackage => "cpu_package",
        CpuCore => "cpu_core",
        LogicalCpu => "cpu",
        NumaNode => "numa_node",
        Process => "process",
        Thread => "thread",
        Cgroup => "cgroup",
        Container => "container",
        Service => "service",
        NetNamespace => "net_namespace",
        Interface => "interface",
        Socket => "socket",
        RemoteEndpoint => "remote_endpoint",
        Mount => "mount",
        Volume => "volume",
        BlockDevice => "block_device",
        Gpu => "gpu",
        GpuClient => "gpu_client",
        Sensor => "sensor",
        Battery => "battery",
        PowerSupply => "power_supply",
        FirewallRule => "firewall_rule",
        Collector => "collector",
    }
);

string_enum!(
    /// Kind of a graph edge (relationship).
    EdgeKind {
        Contains => "contains",
        ParentOf => "parent_of",
        HasThread => "has_thread",
        MemberOf => "member_of",
        RunsOn => "runs_on",
        OwnsSocket => "owns_socket",
        ConnectedTo => "connected_to",
        CommunicatesWith => "communicates_with",
        ListensOn => "listens_on",
        Mounts => "mounts",
        MountedAt => "mounted_at",
        BackedBy => "backed_by",
        UsesGpu => "uses_gpu",
        ObservedOn => "observed_on",
        Measures => "measures",
        AccountedTo => "accounted_to",
    }
);

string_enum!(
    /// How strongly a relationship is established.
    Evidence {
        KernelConfirmed => "kernel_confirmed",
        SocketPeerConfirmed => "socket_peer_confirmed",
        EndpointMatched => "endpoint_matched",
        ConntrackCorrelated => "conntrack_correlated",
        Inferred => "inferred",
        Unresolved => "unresolved",
    }
);

/// Stable, collision-free identity of a graph node: `"<kind>:<key>"`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(Arc<str>);

impl EntityId {
    pub fn new(kind: EntityKind, key: impl fmt::Display) -> Self {
        EntityId(Arc::from(format!("{}:{}", kind.as_str(), key)))
    }

    /// Parses an id previously produced by [`EntityId::new`].
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, key) = s.split_once(':')?;
        EntityKind::parse(kind)?;
        if key.is_empty() {
            return None;
        }
        Some(EntityId(Arc::from(s)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> Option<EntityKind> {
        self.0
            .split_once(':')
            .and_then(|(k, _)| EntityKind::parse(k))
    }

    /// The part after `<kind>:`.
    pub fn key(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, k)| k)
    }

    /// Smallest possible id; used as a range bound.
    pub fn min() -> Self {
        EntityId(Arc::from(""))
    }

    /// Prefix shared by every id of `kind`.
    pub fn kind_prefix(kind: EntityKind) -> String {
        format!("{}:", kind.as_str())
    }

    pub fn host() -> Self {
        EntityId::new(EntityKind::Host, "local")
    }

    pub fn process(identity: &ProcessIdentity) -> Self {
        EntityId::new(
            EntityKind::Process,
            format_args!("{}:{}", identity.pid, identity.start_time_ticks),
        )
    }
}

// Ord/Eq/Hash derive from the inner `str`, so borrowing as `str` is
// consistent and allows prefix range queries without allocating ids.
impl std::borrow::Borrow<str> for EntityId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", &*self.0)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of an edge: at most one edge of a kind between two entities.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKey {
    pub kind: EdgeKind,
    pub source: EntityId,
    pub target: EntityId,
}

/// Identity of a process that survives PID reuse.
///
/// Two processes with the same PID differ in `start_time_ticks` (field 22 of
/// `/proc/PID/stat`, in clock ticks since boot). Combined with the boot id it
/// is unique for the lifetime of the machine's current boot and
/// distinguishable across reboots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time_ticks: u64,
}

impl ProcessIdentity {
    pub fn entity_id(&self) -> EntityId {
        EntityId::process(self)
    }

    /// Recovers the identity from a process [`EntityId`].
    pub fn from_entity_id(id: &EntityId) -> Option<Self> {
        if id.kind()? != EntityKind::Process {
            return None;
        }
        let (pid, start) = id.key().split_once(':')?;
        Some(ProcessIdentity {
            pid: pid.parse().ok()?,
            start_time_ticks: start.parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_id_round_trips_and_distinguishes_pid_reuse() {
        let a = ProcessIdentity {
            pid: 42,
            start_time_ticks: 100,
        };
        let b = ProcessIdentity {
            pid: 42,
            start_time_ticks: 900,
        };
        assert_ne!(a.entity_id(), b.entity_id());
        assert_eq!(ProcessIdentity::from_entity_id(&a.entity_id()), Some(a));
        assert_eq!(a.entity_id().kind(), Some(EntityKind::Process));
    }

    #[test]
    fn parse_rejects_unknown_kinds_and_empty_keys() {
        assert!(EntityId::parse("bogus:1").is_none());
        assert!(EntityId::parse("process:").is_none());
        assert!(EntityId::parse("socket:net1:5").is_some());
    }
}
