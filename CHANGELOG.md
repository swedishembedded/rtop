# Changelog

All notable changes to rtop are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and rtop adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The public
interfaces covered by semantic versioning are the command-line options, the
`rtop --stats` document (`schema_version`) and the key bindings.

## [Unreleased]

### Added

- Pane borders can be dragged with the mouse to resize the panes on both
  sides; a double click restores the view's own proportions.

### Fixed

- The overview's card grid and the border between the cards and the process
  table can be dragged too, and pressing a card's border no longer opens
  that card's view.

## [1.1.0] - 2026-11-25

### Added

- Disk health: NVMe SMART / Health log and ATA SMART via read-only ioctls,
  with a per-drive verdict, wear, temperature and error counters in the
  Storage view and as insights.
- `make test/soak`: long-running UI soak that fails on memory growth or
  rising CPU.

### Fixed

- Graph history grew with every inspected entity and slowed the event
  loop over long sessions; it is now bounded by live entities.

### Changed

- Lower idle cost: cached sensor discovery, slow sensor inputs refreshed
  every 10 s, duplicate thermal zones skipped, telemetry redraws at most
  twice per second.

## [1.0.0] - 2026-11-24

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
