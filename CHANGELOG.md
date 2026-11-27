# Changelog

All notable changes to rtop are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and rtop adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The public
interfaces covered by semantic versioning are the command-line options, the
`rtop --stats` document (`schema_version`) and the key bindings.

## [Unreleased]

### Added

- Slurm clusters: inside a Slurm job rtop starts one `rtop --agent` per
  other node of the job (`srun --overlap --gres=gpu:<n>`), adds those
  nodes' GPUs to the GPU view (`<node> #<index> <name>`) and shows the
  job's nodes in a new Nodes view (`c`, present only inside a Slurm job)
  with Slurm state, CPU, load, memory, GPU utilisation and power, and
  fabric traffic. `--no-cluster` and `--cluster-wrapper` control it.
- Fabric traffic of HPE Slingshot (CXI) NICs, including RDMA, per
  interface and for the host.

- Full NVIDIA GPU detail through NVML: identity (UUID, serial, VBIOS, board
  part, architecture, compute capability, cores, bus width), driver, CUDA and
  NVML versions, graphics/SM/memory/video clocks with their maxima, P-state
  and clock event (throttle) reasons, power and its limits, temperature
  thresholds and memory temperature, fan, encoder/decoder, PCIe link and
  replays, NVLinks, ECC and row-remapping counters, persistence/compute/MIG
  modes, and per-process SM utilisation.
- The GPU view details the GPU under the cursor (click or `j`/`k`) in
  titled sections (load, memory, power, thermal, clocks, links, errors,
  configuration, identity, driver and firmware, and everything else the
  collector reports), beside a pane of utilisation, memory and power
  history; a GPU's inspection page shows the same panels. GPUs are
  labelled with their driver index (`#0 …`).
- NVLink send/receive rates (while the GPU view is shown), C2C links,
  energy since driver load, brand, NUMA node, GSP firmware, InfoROM, board
  and PCI ids.
- When this process may open only some GPUs (e.g. a Slurm step granted
  fewer GPUs than the node has), the GPU view names the denied
  `/dev/nvidia*` nodes and the Slurm job and step.
- NVIDIA GPUs stay visible from `/proc/driver/nvidia` when NVML cannot be
  used, and the GPU view says why; NVML is searched for in the usual driver
  locations and retried every 30 samples.
- `make test/leak` runs the heap-growth test in a release build.

### Changed

- NVML queries are tiered by cost, and the per-GPU process list is read only
  while shown; the DRM descriptor scan runs only when a DRM GPU that NVML
  does not cover exists. On a 4-GPU node the GPU collector's sample time in
  `rtop --stats` fell from 38.5 ms to about 15 ms.
- `make test` no longer runs the multi-minute heap-growth test.
- `rtop --stats --timeout` defaults to 10 s, so agents on other Slurm nodes
  can report.

## [1.2.0] - 2026-11-25

### Added

- Pane borders can be dragged with the mouse to resize the panes on both
  sides; a double click restores the view's own proportions.

### Changed

- Load is drawn in three lanes: graphs and bars of a 0–100 % load (CPU,
  CPU cores, memory, swap, GPU, pressure) are green up to 50 %, yellow up to
  80 % and red above, and values use the same thresholds. The overview's
  load cards no longer draw a bar above their graph; the percentage stays.

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
