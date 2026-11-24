// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The interactive event loop.
//!
//! One thread owns the application state and the graph. Everything slow or
//! blocking happens elsewhere: collectors on runtime workers, deep
//! inspection on an inspector thread, process signals on an action thread,
//! terminal output on a writer thread. The loop drains keyboard input before
//! telemetry and executes the effects of a key (e.g. a SIGKILL request)
//! before it renders anything.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, select};
use rtop_actions::{Outcome, ProcessController};
use rtop_app::state::Target;
use rtop_app::{AppState, Effect, Event, reduce};
use rtop_collectors::Subscriptions;
use rtop_collectors::inspect::{self, ProcessDetail};
use rtop_core::{Clock, MonotonicClock, ProcessIdentity, SystemGraph};
use rtop_linux::KernelFs;
use rtop_runtime::Runtime;
use rtop_tui::HitMap;
use rtop_tui::terminal::{Input, MouseTracker, Term, spawn_input};

pub struct Session {
    pub collectors: Vec<Box<dyn rtop_collectors::Collector>>,
    pub subscriptions: Arc<Subscriptions>,
    pub fs: Arc<dyn KernelFs>,
    pub clock_ticks: u64,
    pub controller: Box<dyn ProcessController>,
    pub actions_enabled: bool,
    pub mouse: bool,
    /// Simulated machine to advance in real time (demo mode).
    pub tick: Option<Box<dyn Fn() + Send>>,
}

type ActionRequest = (Vec<Target>, i32, &'static str);
type ActionResult = (Target, &'static str, Outcome);
type DetailResult = Result<Box<ProcessDetail>, (ProcessIdentity, String)>;

fn spawn_actions(
    controller: Box<dyn ProcessController>,
) -> (Sender<ActionRequest>, Receiver<ActionResult>) {
    let (tx, rx) = bounded::<ActionRequest>(64);
    let (done_tx, done_rx) = bounded::<ActionResult>(256);
    let _ = std::thread::Builder::new()
        .name("rtop-actions".into())
        .spawn(move || {
            rtop_linux::sys::set_thread_nice(-5);
            for (targets, signal, name) in rx {
                for t in targets {
                    let outcome = controller.signal(t.identity, signal);
                    if done_tx.send((t, name, outcome)).is_err() {
                        return;
                    }
                }
            }
        });
    (tx, done_rx)
}

fn spawn_inspector(
    fs: Arc<dyn KernelFs>,
    ticks: u64,
) -> (Sender<ProcessIdentity>, Receiver<DetailResult>) {
    let (tx, rx) = bounded::<ProcessIdentity>(1);
    let (done_tx, done_rx) = bounded::<DetailResult>(4);
    let _ = std::thread::Builder::new()
        .name("rtop-inspect".into())
        .spawn(move || {
            rtop_linux::sys::set_thread_nice(5);
            for id in rx {
                let r = inspect::inspect(fs.as_ref(), id, ticks)
                    .map(Box::new)
                    .map_err(|e| (id, e.to_string()));
                if done_tx.send(r).is_err() {
                    return;
                }
            }
        });
    (tx, done_rx)
}

fn spawn_signals() -> Receiver<i32> {
    let (tx, rx) = bounded::<i32>(4);
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ]) {
        let _ = std::thread::Builder::new()
            .name("rtop-signals".into())
            .spawn(move || {
                for s in signals.forever() {
                    let _ = tx.try_send(s);
                }
            });
    }
    rx
}

pub fn run(mut session: Session) -> std::io::Result<()> {
    // Interaction first: the event loop asks for the highest priority it is
    // allowed; refusal (no CAP_SYS_NICE / RLIMIT_NICE) is fine.
    rtop_linux::sys::set_thread_nice(-10);
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let mut rt = Runtime::start(std::mem::take(&mut session.collectors), clock.clone());
    let (action_tx, action_rx) = spawn_actions(session.controller);
    let (inspect_tx, inspect_rx) = spawn_inspector(session.fs.clone(), session.clock_ticks);
    let signal_rx = spawn_signals();
    let stop = Arc::new(AtomicBool::new(false));
    let mut term = Term::start(session.mouse)?;
    let input_rx = spawn_input(stop.clone());
    let (tick_tx, tick_rx) = bounded::<()>(1);
    if let Some(tick) = session.tick.take() {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                tick();
                let _ = tick_tx.try_send(());
            }
        });
    }

    let mut state = AppState::new(session.actions_enabled);
    let (w, h) = term.size();
    let mut graph = SystemGraph::default();
    let mut hits = HitMap::default();
    let mut mouse = MouseTracker::default();
    let mut effects = Vec::new();
    reduce(
        &mut state,
        &graph,
        Event::Resize {
            width: w,
            height: h,
        },
        clock.now(),
        &mut effects,
    );

    // Telemetry changes are coalesced into at most one table refresh and
    // frame per FRAME; input renders immediately.
    const FRAME: Duration = Duration::from_millis(250);
    let mut last_frame = Instant::now() - FRAME;
    let mut graph_dirty = false;
    let mut urgent = true;

    let result = loop {
        for (id, st) in rt.dispatch() {
            graph.set_collector_status(id, st);
        }
        // Keys before telemetry: drain pending input first (bounded).
        let mut events: Vec<Event> = Vec::new();
        for _ in 0..64 {
            match input_rx.try_recv() {
                Ok(i) => {
                    urgent = true;
                    events.extend(input_event(i, &mut term, &hits, &mut mouse));
                }
                Err(_) => break,
            }
        }
        if events.is_empty() {
            let frame_due = last_frame.elapsed() >= FRAME;
            if graph_dirty && frame_due {
                graph_dirty = false;
                reduce(
                    &mut state,
                    &graph,
                    Event::GraphChanged,
                    clock.now(),
                    &mut effects,
                );
            }
            if state.dirty && (urgent || frame_due) && term.ready() {
                if let Err(e) = term.draw(|f| rtop_tui::render(f, &state, &graph, &mut hits)) {
                    break Err(e);
                }
                state.dirty = false;
                urgent = false;
                last_frame = Instant::now();
            }
            let mut timeout = rt
                .next_deadline()
                .clamp(Duration::from_millis(5), Duration::from_secs(1));
            if graph_dirty || state.dirty {
                let until_frame = FRAME.saturating_sub(last_frame.elapsed());
                timeout = timeout.min(until_frame.max(Duration::from_millis(5)));
            }
            select! {
                recv(input_rx) -> i => match i {
                    Ok(i) => {
                        urgent = true;
                        events.extend(input_event(i, &mut term, &hits, &mut mouse));
                    }
                    Err(_) => break Ok(()),
                },
                recv(rt.completions()) -> c => {
                    if let Ok(c) = c {
                        let a = rt.complete(c);
                        Runtime::apply(&mut graph, a);
                        // Coalesce whatever else finished meanwhile.
                        while let Ok(c) = rt.completions().try_recv() {
                            let a = rt.complete(c);
                            Runtime::apply(&mut graph, a);
                        }
                        graph_dirty = true;
                    }
                },
                recv(action_rx) -> r => if let Ok((target, signal_name, outcome)) = r {
                    urgent = true;
                    events.push(Event::ActionDone { target, signal_name, outcome });
                },
                recv(inspect_rx) -> d => if let Ok(d) = d {
                    events.push(Event::Detail(d));
                },
                recv(signal_rx) -> _ => break Ok(()),
                recv(tick_rx) -> _ => {},
                default(timeout) => events.push(Event::Tick),
            }
        }
        let mut quit = false;
        let mut fatal = None;
        for ev in events {
            reduce(&mut state, &graph, ev, clock.now(), &mut effects);
            for fx in effects.drain(..) {
                match fx {
                    Effect::Signal {
                        targets,
                        signal,
                        signal_name,
                    } => {
                        if action_tx.try_send((targets, signal, signal_name)).is_err() {
                            state.notice(
                                "Too many pending actions; wait for earlier ones to finish",
                                true,
                            );
                        }
                    }
                    Effect::Inspect(Some(id)) => {
                        let _ = inspect_tx.try_send(id);
                    }
                    Effect::Inspect(None) => {}
                    Effect::Subscribe {
                        process_io,
                        socket_owners,
                        gpu_clients,
                    } => {
                        session
                            .subscriptions
                            .process_io
                            .store(process_io, Ordering::Relaxed);
                        session
                            .subscriptions
                            .socket_ownership
                            .store(socket_owners, Ordering::Relaxed);
                        session
                            .subscriptions
                            .gpu_clients
                            .store(gpu_clients, Ordering::Relaxed);
                    }
                    Effect::Emergency(on) => {
                        for (id, st) in rt.set_emergency(on) {
                            graph.set_collector_status(id, st);
                        }
                    }
                    Effect::Pause(p) => rt.set_paused(p),
                    Effect::IntervalScale(s) => rt.set_interval_scale(s),
                    Effect::Copy(text) => term.copy(&text),
                    Effect::SuspendSelf => match term.suspend() {
                        Ok(()) => state.dirty = true,
                        Err(e) => fatal = Some(e),
                    },
                    Effect::Quit => quit = true,
                }
            }
        }
        if let Some(e) = fatal {
            break Err(e);
        }
        if quit {
            break Ok(());
        }
    };
    stop.store(true, Ordering::Relaxed);
    drop(term);
    result
}

fn input_event(
    i: Input,
    term: &mut Term,
    hits: &HitMap,
    mouse: &mut MouseTracker,
) -> Option<Event> {
    match i {
        Input::Key(k) => Some(Event::Key(k)),
        Input::Mouse(m) => mouse.translate(m, hits, Instant::now()).map(Event::Mouse),
        Input::Resize(w, h) => {
            let _ = term.resized();
            Some(Event::Resize {
                width: w,
                height: h,
            })
        }
    }
}
