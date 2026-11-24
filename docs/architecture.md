# rtop architecture

rtop models the observable Linux system as one typed, directed, attributed
multigraph and lets the user browse it. This document records the decisions
behind that design and the invariants the code relies on.

## Data flow

```
kernel (procfs, sysfs, netlink, syscalls, Docker socket, NVML)
   │  KernelFs / SockDiag / DockerApi      (rtop-linux, injectable)
   ▼
collectors ── GraphPatch ──▶ runtime ──▶ graph owner (event loop)
 (rtop-collectors)          (rtop-runtime)    │ SystemGraph (rtop-core)
                                              ├─▶ reducer + projections (rtop-app) ─▶ renderer (rtop-tui)
                                              └─▶ exporter (rtop-export) ─▶ YAML / JSON
```

## Decisions

| Decision | Why |
| -------- | --- |
| One graph, many projections | Following a bottleneck to a workload, its processes, sockets and peers is a traversal, not a context switch between tools. |
| Collectors emit full-state patches per scope | A newer patch supersedes an older one entirely, so the runtime can keep only the latest result per collector under backpressure. |
| Per-collector provenance on every node, edge, attribute and metric | A collector can only retract what it asserted; partial scans delete nothing; augmenting another collector's entity never creates it. |
| Ordered maps keyed by collision-free ids instead of a slot arena | Ids embed kernel identities (PID + start time, netns + inode), so there are no recycled slots; iteration order is deterministic for exports; all nodes of one kind are one key range. |
| One worker thread per collector, at most one outstanding sample | A slow or hung collector misses deadlines instead of queueing work, and cannot delay others. A thread stuck in uninterruptible kernel sleep cannot be killed, so none is ever replaced. |
| Remote `statvfs` on helper threads with a deadline, one per mount | A dead NFS server makes one mount time out, not the filesystem view. |
| Synchronous workers, no async runtime | procfs and sysfs reads are synchronous kernel operations; async would not make them non-blocking. |
| Pure reducer, effects out | Sessions replay deterministically in tests; keyboard and mouse share one implementation of every command. |
| Input drained before telemetry; action and output threads | A force kill is dispatched before any render; a stalled terminal drops frames instead of blocking input. |
| pidfd for every destructive action | The pidfd pins one process; its start time is verified through it, so PID reuse cannot redirect a signal. Without pidfd support, destructive actions are disabled rather than racy. |
| Explicit availability | `0` is a measurement. Unknown is `N/A` with a reason, first samples are pending, values from stalled collectors are marked stale, heuristics estimated. |
| Hand-written YAML/JSON emitters from one document tree | Identical content in both formats, deterministic order, no unmaintained YAML dependency. |
| Demo machine as a `MemoryKernelFs` | Tests, screenshots and `--demo` run the real collectors against invented data; nothing committed describes a real system. |

## Invariants

1. The render path never reads the kernel.
2. Input handling never waits for telemetry or actions.
3. Collectors never modify application state.
4. No lock is held across a kernel read.
5. Every queue, buffer, history and worker pool is bounded.
6. Every asynchronous effect produces a result event.
7. Every metric carries availability and provenance.
8. No destructive action can silently change its target.

## Known limitations

- cgroup v1 hierarchies are not walked; containers there are not grouped.
- eBPF-based per-process UDP/Unix traffic is not implemented; those
  metrics report `not_measurable`. Firewall counters are read from existing
  nftables `counter` expressions only; legacy iptables is not read.
- Socket relationships cover the network namespace rtop runs in.
- NVML support is exercised only where an NVIDIA driver is present.
