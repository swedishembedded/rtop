// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Views, their panels and the ways rows can be sorted.

use rtop_core::{EdgeKey, EntityId};

/// The ten global views, selected with `0`–`9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum View {
    Overview,
    Processes,
    Cpu,
    Memory,
    Storage,
    Network,
    Containers,
    Gpu,
    Security,
    Sensors,
}

impl View {
    pub const ALL: [View; 10] = [
        View::Overview,
        View::Processes,
        View::Cpu,
        View::Memory,
        View::Storage,
        View::Network,
        View::Containers,
        View::Gpu,
        View::Security,
        View::Sensors,
    ];

    pub fn index(self) -> usize {
        View::ALL.iter().position(|v| *v == self).expect("listed")
    }

    pub fn from_digit(d: u32) -> Option<View> {
        View::ALL.get(d as usize).copied()
    }

    pub fn title(self) -> &'static str {
        match self {
            View::Overview => "Overview",
            View::Processes => "Processes",
            View::Cpu => "CPU",
            View::Memory => "Memory",
            View::Storage => "Storage",
            View::Network => "Network",
            View::Containers => "Containers",
            View::Gpu => "GPU",
            View::Security => "Security",
            View::Sensors => "Sensors",
        }
    }

    pub fn parse(s: &str) -> Option<View> {
        let s = s.to_ascii_lowercase();
        if let Ok(d) = s.parse::<u32>() {
            return View::from_digit(d);
        }
        View::ALL
            .into_iter()
            .find(|v| v.title().to_ascii_lowercase().starts_with(&s))
    }

    /// Table panels of the view, in focus order.
    pub fn panels(self) -> &'static [Panel] {
        match self {
            View::Overview => &[Panel::Processes],
            View::Processes => &[Panel::Processes],
            View::Cpu => &[Panel::Cpus, Panel::Processes],
            View::Memory => &[Panel::Processes],
            View::Storage => &[Panel::BlockDevices, Panel::Mounts],
            View::Network => &[Panel::Communication, Panel::Sockets, Panel::Interfaces],
            View::Containers => &[Panel::Containers, Panel::Services],
            View::Gpu => &[Panel::Gpus, Panel::GpuClients],
            View::Security => &[Panel::Events, Panel::Collectors],
            View::Sensors => &[Panel::Sensors, Panel::Batteries],
        }
    }

    /// The sort a view's process table starts with.
    pub fn default_sort(self) -> SortKey {
        match self {
            View::Memory => SortKey::Mem,
            View::Storage => SortKey::Io,
            View::Network => SortKey::Net,
            View::Gpu => SortKey::Gpu,
            _ => SortKey::Cpu,
        }
    }
}

/// A table on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Panel {
    Processes,
    Cpus,
    BlockDevices,
    Mounts,
    Interfaces,
    Sockets,
    Communication,
    Containers,
    Services,
    Gpus,
    GpuClients,
    Sensors,
    Batteries,
    Events,
    Collectors,
    /// Relationships of the inspected entity.
    Relations,
}

impl Panel {
    pub fn title(self) -> &'static str {
        match self {
            Panel::Processes => "Processes",
            Panel::Cpus => "Logical CPUs",
            Panel::BlockDevices => "Block devices",
            Panel::Mounts => "Filesystems",
            Panel::Interfaces => "Interfaces",
            Panel::Sockets => "Connections",
            Panel::Communication => "Communication",
            Panel::Containers => "Containers",
            Panel::Services => "Services",
            Panel::Gpus => "GPUs & accelerators",
            Panel::GpuClients => "GPU processes",
            Panel::Sensors => "Sensors",
            Panel::Batteries => "Power",
            Panel::Events => "Security & kernel events",
            Panel::Collectors => "Telemetry sources",
            Panel::Relations => "Relationships",
        }
    }

    /// Sort keys meaningful for this panel (cycled with `w`/`b`).
    pub fn sort_keys(self) -> &'static [SortKey] {
        use SortKey::*;
        match self {
            Panel::Processes => &[Cpu, Mem, Pss, Virt, Io, Net, Gpu, Threads, Age, Pid, Name],
            Panel::Cpus => &[Name, Cpu],
            Panel::BlockDevices => &[Io, Name],
            Panel::Mounts => &[Mem, Name],
            Panel::Interfaces => &[Net, Name],
            Panel::Sockets => &[Net, Name],
            Panel::Communication => &[Net, Name],
            Panel::Containers | Panel::Services => &[Cpu, Mem, Io, Net, Name],
            Panel::Gpus => &[Gpu, Mem, Name],
            Panel::GpuClients => &[Gpu, Mem, Name],
            Panel::Sensors => &[Name, Value],
            Panel::Batteries | Panel::Events | Panel::Collectors | Panel::Relations => &[Name],
        }
    }

    pub fn default_sort(self) -> SortKey {
        self.sort_keys()[0]
    }

    /// Whether rows are naturally ordered oldest→newest (events) rather
    /// than descending by value.
    pub fn default_descending(self) -> bool {
        !matches!(self.default_sort(), SortKey::Name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SortKey {
    Cpu,
    Mem,
    Pss,
    Virt,
    Io,
    Net,
    Gpu,
    Threads,
    Age,
    Pid,
    Name,
    Value,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Cpu => "CPU",
            SortKey::Mem => "memory",
            SortKey::Pss => "proportional memory",
            SortKey::Virt => "virtual memory",
            SortKey::Io => "storage I/O",
            SortKey::Net => "network",
            SortKey::Gpu => "GPU",
            SortKey::Threads => "threads",
            SortKey::Age => "age",
            SortKey::Pid => "PID",
            SortKey::Name => "name",
            SortKey::Value => "value",
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        Some(match s.to_ascii_lowercase().as_str() {
            "cpu" | "c" => SortKey::Cpu,
            "mem" | "memory" | "rss" | "m" => SortKey::Mem,
            "pss" | "p" => SortKey::Pss,
            "virt" | "vsz" | "v" => SortKey::Virt,
            "io" | "disk" | "i" => SortKey::Io,
            "net" | "network" | "n" => SortKey::Net,
            "gpu" | "g" => SortKey::Gpu,
            "threads" | "t" => SortKey::Threads,
            "age" | "a" => SortKey::Age,
            "pid" | "d" => SortKey::Pid,
            "name" => SortKey::Name,
            "value" => SortKey::Value,
            _ => return None,
        })
    }
}

/// Stable identity of a table row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowId {
    Node(EntityId),
    Edge(EdgeKey),
    /// Position in an append-only list (events), counted from the oldest
    /// event ever seen, so it stays stable as the ring buffer rotates.
    Seq(u64),
    Collector(&'static str),
}

impl RowId {
    pub fn entity(&self) -> Option<&EntityId> {
        match self {
            RowId::Node(id) => Some(id),
            _ => None,
        }
    }
}
