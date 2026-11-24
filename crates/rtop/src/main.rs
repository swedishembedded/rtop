// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for understanding and recovering
// overloaded Linux systems for its clients. If your team needs expertise in
// Linux observability tooling then you can procure our services by sending
// an email to info@swedishembedded.com.

//! `rtop`: interactive TUI by default, `rtop --stats` for a YAML/JSON
//! snapshot of the whole telemetry graph.

use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

mod tui;

use clap::Parser;
use rtop_collectors::demo::DemoMachine;
use rtop_collectors::{Collector, Env, Subscriptions};
use rtop_core::{CollectorState, MonotonicClock, units};

#[derive(Debug, Parser)]
#[command(name = "rtop", version, about = "Real-time Linux telemetry graph and process manager", long_about = None)]
struct Cli {
    /// Print the complete accessible telemetry graph as YAML and exit.
    #[arg(long)]
    stats: bool,
    /// With --stats: emit JSON instead of YAML.
    #[arg(long, requires = "stats")]
    json: bool,
    /// Rate measurement window for --stats (e.g. 500ms, 2s).
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    interval: Duration,
    /// Upper bound on --stats collection time.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    timeout: Duration,
    /// With --stats: exit with status 2 when any collector did not deliver.
    #[arg(long, requires = "stats")]
    strict: bool,
    /// Observe the built-in simulated demo machine instead of this system.
    #[arg(long, conflicts_with = "sysroot")]
    demo: bool,
    /// Observe a captured procfs/sysfs tree rooted at DIR (read-only, no actions).
    #[arg(long, value_name = "DIR")]
    sysroot: Option<std::path::PathBuf>,
    /// Do not capture the mouse (keyboard only; lets the terminal select text).
    #[arg(long)]
    no_mouse: bool,
    /// Monitoring only: disable every process action.
    #[arg(long)]
    read_only: bool,
}

/// Process actions on the simulated machine: terminating signals remove the
/// process, as if it exited.
#[derive(Debug)]
struct DemoController(DemoMachine);

impl rtop_actions::ProcessController for DemoController {
    fn signal(&self, target: rtop_core::ProcessIdentity, signal: i32) -> rtop_actions::Outcome {
        if self.0.identity(target.pid) != Some(target) {
            return rtop_actions::Outcome::Gone;
        }
        let name = rtop_linux::sys::signals()
            .into_iter()
            .find(|s| s.number == signal)
            .map_or("", |s| s.name);
        match name {
            "SIGKILL" | "SIGTERM" | "SIGINT" | "SIGQUIT" => {
                self.0.kill(target.pid);
                rtop_actions::Outcome::Exited
            }
            _ => rtop_actions::Outcome::Delivered,
        }
    }
}

/// Kernel access, action controller, whether actions are enabled, and the
/// simulated clock tick (demo only) for an interactive session.
type Observed = (
    Arc<dyn rtop_linux::KernelFs>,
    Box<dyn rtop_actions::ProcessController>,
    bool,
    Option<Box<dyn Fn() + Send>>,
);

fn interactive(cli: &Cli) -> ExitCode {
    use std::io::IsTerminal;
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        eprintln!(
            "rtop: the interactive interface needs a terminal; use `rtop --stats` for YAML output"
        );
        return ExitCode::FAILURE;
    }
    let subscriptions = Arc::new(Subscriptions::default());
    let t = target(cli, subscriptions.clone());
    let (fs, controller, enabled, tick): Observed = match (&t.demo, &cli.sysroot) {
        (Some(m), _) => {
            let m2 = m.clone();
            (
                m.fs(),
                Box::new(DemoController(m.clone())),
                !cli.read_only,
                Some(Box::new(move || m2.advance(1.0))),
            )
        }
        (None, Some(root)) => (
            Arc::new(rtop_linux::RealKernelFs::rooted(root)),
            Box::new(rtop_actions::DisabledController(
                "observing a captured system tree".into(),
            )),
            false,
            None,
        ),
        (None, None) => {
            let pidfd = rtop_actions::pidfd_supported();
            let controller: Box<dyn rtop_actions::ProcessController> = if cli.read_only {
                Box::new(rtop_actions::DisabledController(
                    "rtop was started with --read-only".into(),
                ))
            } else if pidfd {
                Box::new(rtop_actions::PidfdController::new(
                    rtop_linux::RealKernelFs::live(),
                ))
            } else {
                Box::new(rtop_actions::DisabledController(
                    "this kernel lacks pidfd (Linux 5.3+), so actions could hit a recycled PID"
                        .into(),
                ))
            };
            (
                Arc::new(rtop_linux::RealKernelFs::live()),
                controller,
                pidfd && !cli.read_only,
                None,
            )
        }
    };
    let session = tui::Session {
        collectors: t.collectors,
        subscriptions,
        fs,
        clock_ticks: if t.demo.is_some() || cli.sysroot.is_some() {
            100
        } else {
            rtop_linux::sys::clock_ticks()
        },
        controller,
        actions_enabled: enabled,
        mouse: !cli.no_mouse,
        tick,
    };
    match tui::run(session) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rtop: terminal error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    units::parse_duration_secs(s)
        .filter(|v| *v > 0.0 && *v <= 3600.0)
        .map(Duration::from_secs_f64)
        .ok_or_else(|| format!("invalid duration '{s}' (use e.g. 500ms, 2s, 1m)"))
}

/// The observed system and the collectors watching it.
struct Target {
    collectors: Vec<Box<dyn Collector>>,
    demo: Option<DemoMachine>,
}

fn target(cli: &Cli, subscriptions: Arc<Subscriptions>) -> Target {
    if cli.demo {
        let m = DemoMachine::new();
        let env = m.env(subscriptions);
        return Target {
            collectors: m.collectors(&env),
            demo: Some(m),
        };
    }
    let env = match &cli.sysroot {
        Some(root) => Env::simulated(
            Arc::new(rtop_linux::RealKernelFs::rooted(root)),
            subscriptions,
        ),
        None => Env::live(subscriptions),
    };
    Target {
        collectors: rtop_collectors::all(&env),
        demo: None,
    }
}

fn stats(cli: &Cli) -> ExitCode {
    let t = target(cli, Subscriptions::all());
    // The demo machine has no clock of its own: advance it between rounds.
    let demo = t.demo.clone();
    let graph = rtop_runtime::snapshot_with(
        t.collectors,
        Arc::new(MonotonicClock::new()),
        cli.interval,
        cli.timeout,
        move || {
            if let Some(m) = demo {
                m.advance(1.0);
            }
        },
    );
    let doc = rtop_export::document(
        &graph,
        rtop_export::ExportOptions {
            sample_window_ms: cli.interval.as_millis() as u64,
        },
    );
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let written = if cli.json {
        rtop_export::write_json(&mut out, &doc, 0).and_then(|_| out.write_all(b"\n"))
    } else {
        rtop_export::write_yaml(&mut out, &doc)
    }
    .and_then(|_| out.flush());
    match written {
        Ok(()) => {}
        // `rtop --stats | head` closing the pipe early is not an error.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rtop: writing output: {e}");
            return ExitCode::FAILURE;
        }
    }
    let incomplete: Vec<_> = graph
        .collectors()
        .filter(|(_, s)| {
            !matches!(
                s.state,
                CollectorState::Available | CollectorState::Degraded
            )
        })
        .map(|(id, _)| id.as_str())
        .collect();
    if cli.strict && !incomplete.is_empty() {
        eprintln!("rtop: incomplete collection: {}", incomplete.join(", "));
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.stats {
        return stats(&cli);
    }
    interactive(&cli)
}
