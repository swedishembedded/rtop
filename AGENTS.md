# AGENTS.md - rtop

rtop is a real-time Linux telemetry graph with a Vim- and mouse-native
terminal interface and a YAML/JSON command line (`rtop --stats`). Every
subsystem (CPU, memory, processes, storage, network, sockets, cgroups and
containers, GPUs, sensors, security events) contributes entities, relations
and metrics to one graph; the TUI and the exporter are projections of it.

This file routes you to the file that owns a subject. It holds rules and
locations, not status.

## Security (hard rules)

- Never commit or print secrets, keys, tokens, customer data, dumps or logs; a secret found in a diff, output or history is reported, never repeated.
- Kernel data is untrusted: process names, command lines, paths and log lines go through `rtop_core::text::sanitize` before display or export.
- rtop observes; it never changes firewall, audit, logging, sysctl or container configuration, never invokes sudo, never uses real-time scheduling and never touches the Docker socket's permissions.
- Process actions target `ProcessIdentity` (PID + start time) through a verified pidfd (`crates/rtop-actions`); never signal a bare PID.
- Nothing committed may describe a real machine: fixtures, screenshots and examples come from the invented demo machine (`crates/rtop-collectors/src/demo.rs`). `make check/privacy` enforces it.
- Tests signal only child processes they started.
- Dependencies are pinned in `Cargo.lock`; `make check/audit` must be clean; GitHub actions are pinned to commits.

## Layout

| Path | Owns |
| ---- | ---- |
| `crates/rtop-core` | Graph, patches and reconciliation, metric availability, counters, history, sanitising, units |
| `crates/rtop-linux` | `KernelFs` (real, rooted, in-memory with fault injection), procfs/sysfs parsers, netlink (sock_diag, nftables), syscalls |
| `crates/rtop-collectors` | One collector per subsystem, metric registry (`metrics.rs`), process inspection, demo machine |
| `crates/rtop-runtime` | Worker threads, deadline scheduling, blocked-collector containment, emergency mode, `snapshot` |
| `crates/rtop-actions` | pidfd-verified signalling |
| `crates/rtop-app` | Pure reducer: modes, key registry (`keymap.rs`), navigation, projections, filters, insights |
| `crates/rtop-tui` | Rendering, hit map, terminal driver, HTML screenshot renderer |
| `crates/rtop-export` | YAML/JSON document contract |
| `crates/rtop` | Binary: CLI, interactive event loop, PTY end-to-end tests |
| `docs/architecture.md` | Design decisions and invariants |
| `scripts/` | `shots.sh` (WebP screenshots), `check-privacy.sh` |

## Rules

- The reducer (`rtop-app`) performs no I/O and reads no clock; effects carry side effects out.
- The render path never reads the kernel; collectors never touch application state.
- Every metric is a `Metric` with explicit availability; never publish zero for unknown. New metric keys are added to `crates/rtop-collectors/src/metrics.rs` (a test enforces it) and encode their unit in the suffix.
- Every queue, cache and history is bounded.
- A key binding is added only to `BINDINGS` in `keymap.rs`; help is generated from it and mouse input maps to the same `Cmd`s.
- UI changes are checked visually: `make shots`, then look at `target/shots/*.webp`.

## Workflow

`make help` lists every target. Before committing: `make check` and `make test`.
Releases: `make release/patch|minor|major` bumps with bump2version, dates `CHANGELOG.md`, commits
and tags; pushing the tag runs `.github/workflows/release.yml`, which publishes
only when every check passes.
