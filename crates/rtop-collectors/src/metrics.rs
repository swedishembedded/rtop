// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Registry of every metric rtop publishes: unit, semantics and source.
//!
//! The YAML/JSON exports and the TUI's help derive descriptions from this
//! table, and a test asserts that every metric any collector emits on the
//! demo machine is registered here, so the registry cannot drift from the
//! implementation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Percent,
    Bytes,
    BytesPerSecond,
    PerSecond,
    Count,
    Seconds,
    Milliseconds,
    Celsius,
    Watts,
    Mhz,
    Ratio,
    Other(&'static str),
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Unit::Percent => "percent",
            Unit::Bytes => "bytes",
            Unit::BytesPerSecond => "bytes_per_second",
            Unit::PerSecond => "per_second",
            Unit::Count => "count",
            Unit::Seconds => "seconds",
            Unit::Milliseconds => "milliseconds",
            Unit::Celsius => "celsius",
            Unit::Watts => "watts",
            Unit::Mhz => "megahertz",
            Unit::Ratio => "ratio",
            Unit::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MetricSpec {
    pub key: &'static str,
    pub unit: Unit,
    pub description: &'static str,
}

/// Unit inferred from the metric key suffix; every key follows this naming
/// convention so units are unambiguous in exports.
pub fn unit_of(key: &str) -> Unit {
    let k = key;
    if k.ends_with("_bytes_per_second") {
        Unit::BytesPerSecond
    } else if k.ends_with("_per_second")
        || k.ends_with("_iops")
        || k == "read_iops"
        || k == "write_iops"
    {
        Unit::PerSecond
    } else if k.ends_with("_percent") || k.starts_with("psi_") {
        Unit::Percent
    } else if k.ends_with("_bytes") {
        Unit::Bytes
    } else if k.ends_with("_seconds") {
        Unit::Seconds
    } else if k.ends_with("_ms") {
        Unit::Milliseconds
    } else if k.ends_with("_celsius") {
        Unit::Celsius
    } else if k.ends_with("_w") {
        Unit::Watts
    } else if k.ends_with("_mhz") {
        Unit::Mhz
    } else if k.ends_with("_wh") {
        Unit::Other("watt_hours")
    } else if k.ends_with("_ah") {
        Unit::Other("ampere_hours")
    } else if k.ends_with("_v") {
        Unit::Other("volts")
    } else if k.ends_with("_a") {
        Unit::Other("amperes")
    } else if k.ends_with("_mbps") {
        Unit::Other("megabits_per_second")
    } else {
        Unit::Count
    }
}

macro_rules! specs {
    ($($key:literal => $desc:literal),+ $(,)?) => {
        /// Every published metric.
        pub const SPECS: &[(&str, &str)] = &[$(($key, $desc)),+];
    };
}

specs! {
    "load_1" => "1-minute load average",
    "load_5" => "5-minute load average",
    "load_15" => "15-minute load average",
    "tasks_runnable" => "Runnable scheduling entities",
    "tasks_total" => "Scheduling entities in the system",
    "uptime_seconds" => "Time since boot",
    "logical_cpus" => "Online logical CPUs",
    "cpu_usage_percent" => "CPU busy time (per-CPU convention for processes: 100% = one CPU)",
    "cpu_user_percent" => "User and nice time (guest time already included)",
    "cpu_system_percent" => "Kernel, IRQ and softirq time",
    "cpu_iowait_percent" => "Idle time with outstanding I/O (not busy time)",
    "cpu_steal_percent" => "Time stolen by the hypervisor",
    "context_switches_per_second" => "Context switches",
    "interrupts_per_second" => "Hardware interrupts",
    "forks_per_second" => "Processes and threads created",
    "procs_blocked" => "Tasks blocked on I/O",
    "frequency_mhz" => "Current frequency reported by cpufreq (not measured effective frequency)",
    "memory_total_bytes" => "Physical memory",
    "memory_available_bytes" => "Memory available without swapping (MemAvailable)",
    "memory_used_bytes" => "Total minus available",
    "memory_used_percent" => "Used as share of total",
    "memory_free_bytes" => "Completely unused memory",
    "memory_buffers_bytes" => "Block device buffers",
    "memory_cached_bytes" => "Page cache",
    "memory_shared_bytes" => "Shared memory and tmpfs",
    "memory_dirty_bytes" => "Dirty pages awaiting writeback",
    "memory_slab_reclaimable_bytes" => "Reclaimable kernel slab",
    "memory_slab_unreclaimable_bytes" => "Unreclaimable kernel slab",
    "memory_anon_bytes" => "Anonymous memory",
    "swap_total_bytes" => "Swap capacity",
    "swap_free_bytes" => "Unused swap",
    "swap_used_bytes" => "Swap occupancy (not pressure)",
    "zswap_bytes" => "Compressed swap cache size",
    "swap_in_pages_per_second" => "Pages read back from swap (active memory contention)",
    "swap_out_pages_per_second" => "Pages written to swap",
    "major_faults_per_second" => "Page faults requiring I/O",
    "page_faults_per_second" => "All page faults",
    "minor_faults_per_second" => "Page faults served from memory",
    "direct_reclaim_pages_per_second" => "Pages scanned by direct reclaim (allocations stalled on reclaim)",
    "oom_kills_total" => "OOM kills since boot / in the cgroup",
    "oom_kills_per_second" => "OOM kills",
    "psi_cpu_some_avg10" => "Share of time some task waited for CPU (10 s)",
    "psi_cpu_some_avg60" => "Share of time some task waited for CPU (60 s)",
    "psi_cpu_some_avg300" => "Share of time some task waited for CPU (300 s)",
    "psi_cpu_some_stall_percent" => "CPU stall share over the last sample interval",
    "psi_cpu_full_avg10" => "Share of time all non-idle tasks waited for CPU (10 s)",
    "psi_cpu_full_avg60" => "Share of time all non-idle tasks waited for CPU (60 s)",
    "psi_cpu_full_avg300" => "Share of time all non-idle tasks waited for CPU (300 s)",
    "psi_cpu_full_stall_percent" => "Full CPU stall share over the last sample interval",
    "psi_memory_some_avg10" => "Share of time some task stalled on memory (10 s)",
    "psi_memory_some_avg60" => "Share of time some task stalled on memory (60 s)",
    "psi_memory_some_avg300" => "Share of time some task stalled on memory (300 s)",
    "psi_memory_some_stall_percent" => "Memory stall share over the last sample interval",
    "psi_memory_full_avg10" => "Share of time all non-idle tasks stalled on memory (10 s)",
    "psi_memory_full_avg60" => "Share of time all non-idle tasks stalled on memory (60 s)",
    "psi_memory_full_avg300" => "Share of time all non-idle tasks stalled on memory (300 s)",
    "psi_memory_full_stall_percent" => "Full memory stall share over the last sample interval",
    "psi_io_some_avg10" => "Share of time some task stalled on I/O (10 s)",
    "psi_io_some_avg60" => "Share of time some task stalled on I/O (60 s)",
    "psi_io_some_avg300" => "Share of time some task stalled on I/O (300 s)",
    "psi_io_some_stall_percent" => "I/O stall share over the last sample interval",
    "psi_io_full_avg10" => "Share of time all non-idle tasks stalled on I/O (10 s)",
    "psi_io_full_avg60" => "Share of time all non-idle tasks stalled on I/O (60 s)",
    "psi_io_full_avg300" => "Share of time all non-idle tasks stalled on I/O (300 s)",
    "psi_io_full_stall_percent" => "Full I/O stall share over the last sample interval",
    "processes_total" => "Processes",
    "processes_running" => "Processes in state R",
    "processes_blocked" => "Processes in uninterruptible sleep (D)",
    "processes_zombie" => "Zombie processes",
    "threads_total" => "Threads",
    "cpu_time_seconds" => "Accumulated CPU time",
    "rss_bytes" => "Resident set size",
    "virtual_bytes" => "Virtual address space size",
    "swap_bytes" => "Swapped-out memory",
    "threads" => "Threads",
    "nice" => "Nice value",
    "priority" => "Kernel scheduling priority",
    "age_seconds" => "Time since the process started",
    "io_read_bytes_per_second" => "Bytes read from storage",
    "io_write_bytes_per_second" => "Bytes written to storage",
    "sockets_open" => "Sockets held",
    "tcp_rx_bytes_per_second" => "TCP payload received (tcp_info bytes_received)",
    "tcp_tx_bytes_per_second" => "TCP payload acknowledged by peers (tcp_info bytes_acked)",
    "size_bytes" => "Device capacity",
    "read_bytes_per_second" => "Bytes read",
    "write_bytes_per_second" => "Bytes written",
    "read_iops" => "Completed reads",
    "write_iops" => "Completed writes",
    "discard_iops" => "Completed discards",
    "utilization_percent" => "Time with I/O in flight (not saturation on parallel devices)",
    "queue_depth" => "Average queue depth (weighted I/O time)",
    "in_flight" => "Requests in flight",
    "read_latency_ms" => "Average read completion time (block layer)",
    "write_latency_ms" => "Average write completion time (block layer)",
    "disk_read_bytes_per_second" => "Read throughput of whole physical disks",
    "disk_write_bytes_per_second" => "Write throughput of whole physical disks",
    "capacity_bytes" => "Filesystem size",
    "used_bytes" => "Used space",
    "available_bytes" => "Space available to unprivileged users",
    "used_percent" => "Used / (used + available), as df",
    "inodes_total" => "Inodes",
    "inodes_free" => "Free inodes",
    "link_speed_mbps" => "Negotiated link speed",
    "mtu" => "Maximum transmission unit",
    "rx_bytes_per_second" => "Bytes received",
    "tx_bytes_per_second" => "Bytes transmitted",
    "rx_packets_per_second" => "Packets received",
    "tx_packets_per_second" => "Packets transmitted",
    "rx_errors_per_second" => "Receive errors",
    "tx_errors_per_second" => "Transmit errors",
    "rx_dropped_per_second" => "Received packets dropped",
    "tx_dropped_per_second" => "Transmitted packets dropped",
    "rx_bytes_total" => "Bytes received since the interface appeared",
    "tx_bytes_total" => "Bytes transmitted since the interface appeared",
    "network_rx_bytes_per_second" => "Bytes received on all non-loopback interfaces",
    "network_tx_bytes_per_second" => "Bytes sent on all non-loopback interfaces",
    "tcp_retransmits_per_second" => "TCP segments retransmitted",
    "tcp_active_opens_per_second" => "Outgoing TCP connections",
    "tcp_passive_opens_per_second" => "Accepted TCP connections",
    "tcp_in_errors_per_second" => "TCP segments received in error",
    "tcp_attempt_fails_per_second" => "Failed TCP connection attempts",
    "udp_in_errors_per_second" => "UDP receive errors",
    "udp_rcvbuf_errors_per_second" => "UDP datagrams dropped for full receive buffers",
    "tcp_established" => "Established TCP connections",
    "rx_queue_bytes" => "Socket receive queue",
    "tx_queue_bytes" => "Socket send queue",
    "retransmits_total" => "Retransmitted segments on this connection",
    "rtt_ms" => "Smoothed round-trip time",
    "connections" => "Established connections",
    "sockets" => "Sockets carrying the relationship",
    "sockets_tcp_listening" => "Listening TCP sockets",
    "sockets_tcp_established" => "Established TCP sockets",
    "sockets_tcp_time_wait" => "TCP sockets in TIME_WAIT",
    "sockets_udp" => "UDP sockets",
    "sockets_unix" => "Unix domain sockets",
    "cpu_throttled_percent" => "Time the group was throttled by its CPU quota",
    "cpu_throttle_events_per_second" => "CPU quota throttling periods",
    "cpu_quota_cpus" => "CPU quota in CPUs (cpu.max)",
    "memory_current_bytes" => "Memory charged to the group (memory.current)",
    "memory_file_bytes" => "File cache charged to the group",
    "memory_kernel_bytes" => "Kernel memory charged to the group",
    "memory_shmem_bytes" => "Shared memory charged to the group",
    "memory_working_set_bytes" => "Estimated working set: charged minus inactive file cache",
    "memory_peak_bytes" => "Peak charged memory",
    "memory_limit_bytes" => "memory.max",
    "swap_current_bytes" => "Swap charged to the group",
    "pids_current" => "Tasks in the group",
    "processes" => "Processes directly in the group",
    "restarts_total" => "Container restarts",
    "exclusive_bytes" => "Space used exclusively by the volume (only when cheaply measurable)",
    "value" => "Sensor reading in the sensor's unit",
    "max" => "Sensor high threshold",
    "critical" => "Sensor critical threshold",
    "min" => "Sensor low threshold",
    "temperature_max_celsius" => "Hottest temperature sensor",
    "cpu_package_power_w" => "CPU package power (RAPL)",
    "capacity_percent" => "Battery charge",
    "energy_now_wh" => "Remaining energy",
    "energy_full_wh" => "Energy when full",
    "energy_full_design_wh" => "Design energy",
    "charge_now_ah" => "Remaining charge",
    "charge_full_ah" => "Charge when full",
    "voltage_v" => "Voltage",
    "current_a" => "Current",
    "cycle_count" => "Charge cycles",
    "power_w" => "Power",
    "health_percent" => "Full capacity as share of design capacity",
    "time_remaining_seconds" => "Estimated runtime remaining",
    "gpu_usage_percent" => "GPU engine busy time",
    "memory_busy_percent" => "GPU memory controller busy time",
    "vram_total_bytes" => "Device memory",
    "vram_used_bytes" => "Device memory in use",
    "gtt_total_bytes" => "GTT (host memory mapped for the GPU)",
    "gtt_used_bytes" => "GTT in use",
    "frequency_max_mhz" => "Maximum frequency",
    "temperature_celsius" => "Temperature",
    "gpu_memory_bytes" => "GPU memory used by the process",
    "security_events_total" => "Security-related kernel log events since rtop started",
    "security_events_per_minute" => "Security-related kernel log events in the last minute",
    "kernel_oom_events_total" => "OOM kill messages in the kernel log",
    "conntrack_entries" => "Tracked connections",
    "conntrack_max" => "Connection tracking table size",
    "conntrack_drops_per_second" => "Packets dropped by connection tracking",
    "conntrack_early_drops_per_second" => "Entries evicted early",
    "conntrack_insert_failed_per_second" => "Failed conntrack insertions",
    "conntrack_invalid_per_second" => "Invalid packets seen by connection tracking",
    "firewall_rules" => "nftables rules",
    "firewall_counted_rules" => "nftables rules with a counter expression",
    "firewall_packets_per_second" => "Packets matched by counted firewall rules",
    "firewall_drop_packets_per_second" => "Packets matched by counted rules whose verdict is drop",
    "packets_total" => "Packets matched by the rule since its counter started",
    "bytes_total" => "Bytes matched by the rule since its counter started",
    "packets_per_second" => "Packets matched by the rule",
    "bytes_per_second" => "Bytes matched by the rule",
    "rtop_cpu_percent" => "rtop's own CPU usage",
    "rtop_rss_bytes" => "rtop's own resident memory",
    "rtop_threads" => "rtop's threads",
    "rtop_nice" => "Nice value of rtop's main thread",
}

pub fn describe(key: &str) -> Option<MetricSpec> {
    SPECS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(k, d)| MetricSpec {
            key: k,
            unit: unit_of(k),
            description: d,
        })
}
