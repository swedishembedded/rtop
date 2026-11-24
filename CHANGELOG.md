# Changelog

All notable changes to rtop are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and rtop adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The public
interfaces covered by semantic versioning are the command-line options, the
`rtop --stats` document (`schema_version`) and the key bindings.

## [Unreleased]

### Added

- Unified telemetry graph: entities, relationships and metrics from every
  collector with provenance, reconciliation and bounded size.
- Collectors for host, CPU (hybrid topology, frequency), memory and paging,
  pressure stall information, processes, block devices, filesystems,
  network interfaces and protocols, sockets with owners, Unix peers, TCP
  throughput and process-to-process communication, cgroup v2 services and
  containers, Docker metadata and volumes, hwmon/thermal/RAPL sensors and
  batteries, GPUs (DRM, per-process DRM fdinfo, NVML), kernel-log security
  events, connection tracking and rtop's own overhead.
- Interactive TUI with ten views, entity inspection with navigable
  relationships and deep process detail, Vim modes, search and filter
  expressions, sorting, process tree, mouse support, context menus,
  generated help, insights and emergency mode.
- Read-only nftables rule counters (per-rule packet/byte totals and rates,
  verdicts, comments) over `NETLINK_NETFILTER`; uninstrumented rules are
  reported as not measurable.
- PID-reuse-safe process actions through pidfd: terminate with
  confirmation, immediate force kill, suspend, resume, any signal.
- `rtop --stats` YAML/JSON export with `--interval`, `--timeout`, `--strict`.
- `--demo` simulated machine, `--sysroot` captured trees, `--read-only`,
  `--no-mouse`.
- CPU view shows every logical CPU without scrolling: full cells, compact
  cells, then a heat strip, depending on terminal size and CPU count.
- Tables start without a selection and `Esc` clears it, so the view stays
  put while rows re-sort.
- Makefile, CI, semantic release pipeline, privacy and advisory gates.
- Test suites: unit, property, render (all views, all sizes), PTY
  journeys, fault injection, differential (free, nproc, df, ss, ps),
  performance and latency budgets, and libFuzzer targets.
