// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for monitoring software that stays
// responsive when the system it watches is overloaded or failing. If your
// team needs expertise in fault-tolerant real-time runtimes then you can
// procure our services by sending an email to info@swedishembedded.com.

//! The collector runtime.
//!
//! Every collector runs on its own worker thread, at its own nice value, and
//! is driven by a deadline scheduler owned by the caller's event loop. The
//! invariants that keep rtop responsive:
//!
//! * **At most one outstanding sample per collector.** A collector slower
//!   than its interval never queues work; it misses deadlines, which are
//!   counted, and its interval stretches adaptively.
//! * **Blocked collectors are contained.** A worker that has not answered
//!   within its timeout is marked [`CollectorState::Blocked`]; its last data
//!   stays visible as stale, and no replacement thread is spawned (a thread
//!   stuck in uninterruptible kernel sleep cannot be killed, so spawning
//!   more would only leak threads).
//! * **Bounded everything.** One request slot per worker and one result slot
//!   per outstanding request: the result channel can never hold more than
//!   one message per collector.
//! * **Failures back off.** Errors double the retry interval up to a cap;
//!   permanently unsupported sources are retried rarely.
//! * **No polling.** [`Runtime::next_deadline`] tells the event loop how long
//!   it may sleep.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use rtop_collectors::{Collector, CollectorError, Descriptor, Sample, SampleContext};
use rtop_core::{
    Clock, CollectorId, CollectorState, CollectorStatus, GraphPatch, SystemGraph, Timestamp,
    Unavailable,
};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(30);
const MAX_STRETCH: u32 = 10;

/// A finished sample travelling back from a worker.
#[derive(Debug)]
pub struct Completion {
    index: usize,
    sequence: u64,
    result: Result<Sample, CollectorError>,
    duration: Duration,
}

/// What the event loop should apply to the graph after a completion.
#[derive(Debug)]
pub struct Applied {
    pub collector: CollectorId,
    pub patch: Option<GraphPatch>,
    pub status: CollectorStatus,
}

#[derive(Debug)]
struct Worker {
    desc: Descriptor,
    requests: Sender<SampleContext>,
    outstanding: Option<(u64, Timestamp)>,
    next_due: Timestamp,
    status: CollectorStatus,
    enabled: bool,
    sequence: u64,
    consecutive_failures: u32,
    effective_interval: Duration,
}

/// Deadline scheduler and supervisor for a set of collectors.
#[derive(Debug)]
pub struct Runtime {
    workers: Vec<Worker>,
    completions: Receiver<Completion>,
    clock: std::sync::Arc<dyn Clock>,
    emergency: bool,
    interval_scale: f64,
    paused: bool,
}

impl Runtime {
    /// Starts one worker thread per collector.
    pub fn start(collectors: Vec<Box<dyn Collector>>, clock: std::sync::Arc<dyn Clock>) -> Self {
        let (done_tx, done_rx) = bounded::<Completion>(collectors.len().max(1));
        let now = clock.now();
        let mut workers = Vec::with_capacity(collectors.len());
        for (index, mut collector) in collectors.into_iter().enumerate() {
            let desc = collector.descriptor();
            let (req_tx, req_rx) = bounded::<SampleContext>(1);
            let done = done_tx.clone();
            let worker_clock = clock.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("rtop-{}", desc.id))
                .spawn(move || {
                    if desc.nice != 0 {
                        rtop_linux::sys::set_thread_nice(desc.nice);
                    }
                    for ctx in req_rx {
                        let started = worker_clock.now();
                        let result = collector.sample(&ctx);
                        let duration = worker_clock.now().saturating_since(started);
                        if done
                            .send(Completion {
                                index,
                                sequence: ctx.sequence,
                                result,
                                duration,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            let mut status = CollectorStatus::pending(desc.interval);
            if let Err(e) = spawned {
                status.state = CollectorState::Unavailable;
                status.detail = Some(format!("cannot start worker thread: {e}"));
            }
            workers.push(Worker {
                desc,
                requests: req_tx,
                outstanding: None,
                // Slow, infrequent collectors (disk health) usually augment
                // entities other collectors create; let those exist first.
                next_due: if desc.interval >= Duration::from_secs(30) {
                    now.plus(Duration::from_secs(2))
                } else {
                    now
                },
                status,
                enabled: true,
                sequence: 0,
                consecutive_failures: 0,
                effective_interval: desc.interval,
            });
        }
        Runtime {
            workers,
            completions: done_rx,
            clock,
            emergency: false,
            interval_scale: 1.0,
            paused: false,
        }
    }

    /// Channel to select on in the event loop.
    pub fn completions(&self) -> &Receiver<Completion> {
        &self.completions
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &Descriptor> {
        self.workers.iter().map(|w| &w.desc)
    }

    pub fn statuses(&self) -> impl Iterator<Item = (CollectorId, &CollectorStatus)> {
        self.workers.iter().map(|w| (w.desc.id, &w.status))
    }

    pub fn is_emergency(&self) -> bool {
        self.emergency
    }

    /// Emergency mode keeps only essential collectors (CPU, memory, PSI,
    /// processes). Switching is immediate and never waits for workers.
    pub fn set_emergency(&mut self, on: bool) -> Vec<(CollectorId, CollectorStatus)> {
        self.emergency = on;
        let now = self.clock.now();
        let mut changed = Vec::new();
        for w in &mut self.workers {
            let enabled = !on || w.desc.essential;
            if enabled != w.enabled {
                w.enabled = enabled;
                if enabled {
                    w.next_due = now;
                    w.status.state = CollectorState::Pending;
                    w.status.reason = None;
                } else {
                    w.status.state = CollectorState::Disabled;
                    w.status.reason = Some(Unavailable::Disabled);
                }
                changed.push((w.desc.id, w.status.clone()));
            }
        }
        changed
    }

    /// Pauses all sampling (UI "freeze"); data stays as last seen.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if !paused {
            let now = self.clock.now();
            for w in &mut self.workers {
                w.next_due = w.next_due.min(now);
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Multiplies every collector interval (user-selected refresh rate).
    pub fn set_interval_scale(&mut self, scale: f64) {
        self.interval_scale = scale.clamp(0.25, 20.0);
    }

    pub fn interval_scale(&self) -> f64 {
        self.interval_scale
    }

    fn scaled(&self, d: Duration) -> Duration {
        d.mul_f64(self.interval_scale)
    }

    /// Requests a sample from every enabled collector immediately (used by
    /// one-shot snapshots and after resume).
    pub fn request_all_now(&mut self) {
        let now = self.clock.now();
        for w in &mut self.workers {
            w.next_due = now;
        }
        self.dispatch();
    }

    /// Sends due requests and detects blocked workers. Returns status
    /// changes to apply to the graph. Never blocks.
    pub fn dispatch(&mut self) -> Vec<(CollectorId, CollectorStatus)> {
        let now = self.clock.now();
        let mut changed = Vec::new();
        for w in &mut self.workers {
            if let Some((_, sent)) = w.outstanding {
                let waited = now.saturating_since(sent);
                if waited > w.desc.timeout && w.status.state != CollectorState::Blocked {
                    w.status.state = CollectorState::Blocked;
                    w.status.reason = Some(Unavailable::TimedOut);
                    w.status.detail = Some(format!("no answer for {:.1} s", waited.as_secs_f64()));
                    changed.push((w.desc.id, w.status.clone()));
                }
                if now >= w.next_due {
                    // Deadline passed while still busy: count, do not queue.
                    w.status.missed_deadlines += 1;
                    w.next_due = now.plus(w.effective_interval.mul_f64(self.interval_scale));
                }
                continue;
            }
            if !w.enabled || self.paused || now < w.next_due {
                continue;
            }
            w.sequence += 1;
            let ctx = SampleContext {
                now,
                sequence: w.sequence,
            };
            if w.requests.try_send(ctx).is_ok() {
                w.outstanding = Some((w.sequence, now));
            }
            w.next_due = now.plus(w.effective_interval.mul_f64(self.interval_scale));
        }
        changed
    }

    /// Processes one completion from a worker.
    pub fn complete(&mut self, c: Completion) -> Applied {
        let now = self.clock.now();
        let scale = self.interval_scale;
        let w = &mut self.workers[c.index];
        if w.outstanding.map(|(s, _)| s) == Some(c.sequence) {
            w.outstanding = None;
        }
        w.status.last_duration = Some(c.duration);
        // Adapt: a collector that takes longer than half its interval gets a
        // longer one (bounded), so it cannot monopolise its CPU share.
        let stretched = c
            .duration
            .saturating_mul(2)
            .max(w.desc.interval)
            .min(w.desc.interval * MAX_STRETCH);
        w.effective_interval = stretched;
        let patch = match c.result {
            Ok(sample) => {
                w.consecutive_failures = 0;
                w.status.samples += 1;
                w.status.last_success = Some(now);
                w.status.reason = None;
                w.status.state = if !w.enabled {
                    CollectorState::Disabled
                } else if sample.degraded.is_some() {
                    CollectorState::Degraded
                } else {
                    CollectorState::Available
                };
                w.status.detail = sample.degraded;
                Some(sample.patch)
            }
            Err(e) => {
                w.consecutive_failures += 1;
                w.status.failures += 1;
                let permanent = matches!(
                    e.reason,
                    Unavailable::Unsupported
                        | Unavailable::PermissionDenied
                        | Unavailable::NotMeasurable
                );
                w.status.state = CollectorState::Unavailable;
                w.status.reason = Some(e.reason);
                w.status.detail = Some(e.detail);
                let backoff = if permanent {
                    UNSUPPORTED_RETRY
                } else {
                    (w.desc.interval * 2u32.saturating_pow(w.consecutive_failures.min(8)))
                        .min(MAX_BACKOFF)
                };
                w.next_due = now.plus(backoff.mul_f64(scale.max(1.0)));
                None
            }
        };
        w.status.interval = w.effective_interval.mul_f64(scale);
        Applied {
            collector: w.desc.id,
            patch,
            status: w.status.clone(),
        }
    }

    /// Time until the next scheduled action (dispatch or timeout check).
    pub fn next_deadline(&self) -> Duration {
        let now = self.clock.now();
        self.workers
            .iter()
            .filter(|w| w.enabled && !self.paused)
            .map(|w| match w.outstanding {
                Some((_, sent)) if w.status.state != CollectorState::Blocked => {
                    sent.plus(w.desc.timeout).saturating_since(now) + Duration::from_millis(1)
                }
                Some(_) => self.scaled(w.desc.interval),
                None => w.next_due.saturating_since(now),
            })
            .min()
            .unwrap_or(Duration::from_secs(1))
            .min(Duration::from_secs(1))
    }

    /// Whether every enabled collector has answered its latest request.
    pub fn all_idle(&self) -> bool {
        self.workers
            .iter()
            .all(|w| w.outstanding.is_none() || !w.enabled)
    }

    /// Whether a request sent at or after `t` is still unanswered.
    pub fn busy_since(&self, t: Timestamp) -> bool {
        self.workers
            .iter()
            .any(|w| w.outstanding.is_some_and(|(_, sent)| sent >= t))
    }

    /// Applies a completion's status and patch to `graph`.
    pub fn apply(graph: &mut SystemGraph, a: Applied) {
        if let Some(p) = a.patch {
            graph.apply(p);
        }
        graph.set_collector_status(a.collector, a.status);
    }
}

/// Builds a complete graph in one shot: two samples of every collector
/// separated by `window` so rates are real, bounded by `timeout` overall.
/// Collectors that do not answer in time are reported as timed out; their
/// threads are abandoned (they cannot be interrupted) and die with the
/// process.
pub fn snapshot(
    collectors: Vec<Box<dyn Collector>>,
    clock: std::sync::Arc<dyn Clock>,
    window: Duration,
    timeout: Duration,
) -> SystemGraph {
    snapshot_with(collectors, clock, window, timeout, || {})
}

/// [`snapshot`] with a hook run between the baseline and the second round
/// (simulated machines advance their counters there).
pub fn snapshot_with(
    collectors: Vec<Box<dyn Collector>>,
    clock: std::sync::Arc<dyn Clock>,
    window: Duration,
    timeout: Duration,
    between_rounds: impl FnOnce(),
) -> SystemGraph {
    let mut between_rounds = Some(between_rounds);
    let mut rt = Runtime::start(collectors, clock.clone());
    let mut graph = SystemGraph::default();
    for (id, s) in rt.statuses() {
        graph.set_collector_status(id, s.clone());
    }
    let start = std::time::Instant::now();
    let deadline = start + timeout.max(window);
    // The baseline round may use half the budget, so one hung collector
    // cannot starve the second round of every other collector.
    let first_deadline = start + window.max(timeout / 2);
    for round in 0..2 {
        if round == 1 {
            if let Some(hook) = between_rounds.take() {
                hook();
            }
            let wake = start + window;
            if let Some(d) = wake.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(d);
            }
        }
        let round_start = clock.now();
        rt.request_all_now();
        let limit = if round == 0 { first_deadline } else { deadline };
        while rt.busy_since(round_start) {
            let left = limit.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match rt.completions().recv_timeout(left) {
                Ok(c) => {
                    let a = rt.complete(c);
                    Runtime::apply(&mut graph, a);
                }
                Err(_) => break,
            }
        }
    }
    for w in &mut rt.workers {
        if w.outstanding.is_some() {
            w.status.state = CollectorState::Blocked;
            w.status.reason = Some(Unavailable::TimedOut);
            w.status.detail = Some(format!(
                "did not finish within {:.1} s",
                timeout.as_secs_f64()
            ));
            graph.set_collector_status(w.desc.id, w.status.clone());
        }
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtop_collectors::{Isolation, Subscriptions};
    use rtop_core::{EntityId, EntityKind, MonotonicClock, NodeUpdate};
    use rtop_linux::memfs::Gate;
    use std::sync::Arc;

    #[derive(Debug)]
    struct Fake {
        id: CollectorId,
        gate: Option<Arc<Gate>>,
        fail: bool,
    }

    impl Collector for Fake {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: self.id,
                interval: Duration::from_millis(20),
                timeout: Duration::from_millis(50),
                isolation: Isolation::MayBlock,
                essential: self.id.0 == "essential",
                nice: 0,
            }
        }
        fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
            if let Some(g) = &self.gate {
                g.wait();
            }
            if self.fail {
                return Err(CollectorError::new(Unavailable::InvalidData, "boom"));
            }
            let mut p = ctx.patch(self.id);
            p.node(
                NodeUpdate::new(EntityKind::Host, EntityId::host())
                    .metric(self.id.0, rtop_core::Metric::u64(ctx.sequence)),
            );
            Ok(Sample {
                patch: p,
                degraded: None,
            })
        }
    }

    fn drive(rt: &mut Runtime, g: &mut SystemGraph, for_ms: u64) {
        let end = std::time::Instant::now() + Duration::from_millis(for_ms);
        while std::time::Instant::now() < end {
            for (id, s) in rt.dispatch() {
                g.set_collector_status(id, s);
            }
            if let Ok(c) = rt
                .completions()
                .recv_timeout(rt.next_deadline().min(Duration::from_millis(5)))
            {
                let a = rt.complete(c);
                Runtime::apply(g, a);
            }
        }
    }

    #[test]
    fn blocked_collector_cannot_stall_healthy_ones() {
        let gate = Gate::new();
        let collectors: Vec<Box<dyn Collector>> = vec![
            Box::new(Fake {
                id: CollectorId("blocked"),
                gate: Some(gate.clone()),
                fail: false,
            }),
            Box::new(Fake {
                id: CollectorId("healthy"),
                gate: None,
                fail: false,
            }),
        ];
        let mut rt = Runtime::start(collectors, Arc::new(MonotonicClock::new()));
        let mut g = SystemGraph::default();
        drive(&mut rt, &mut g, 300);
        let healthy = g.collector_status(CollectorId("healthy")).unwrap();
        assert!(
            healthy.samples >= 5,
            "healthy sampled {} times",
            healthy.samples
        );
        let blocked = g.collector_status(CollectorId("blocked")).unwrap();
        assert_eq!(blocked.state, CollectorState::Blocked);
        assert!(g.is_stale(CollectorId("blocked")));
        // Recovery: once the kernel call returns, the collector resumes.
        gate.open();
        drive(&mut rt, &mut g, 200);
        assert_eq!(
            g.collector_status(CollectorId("blocked")).unwrap().state,
            CollectorState::Available
        );
    }

    #[test]
    fn failures_back_off_and_emergency_disables_non_essential() {
        let collectors: Vec<Box<dyn Collector>> = vec![
            Box::new(Fake {
                id: CollectorId("failing"),
                gate: None,
                fail: true,
            }),
            Box::new(Fake {
                id: CollectorId("essential"),
                gate: None,
                fail: false,
            }),
        ];
        let mut rt = Runtime::start(collectors, Arc::new(MonotonicClock::new()));
        let mut g = SystemGraph::default();
        drive(&mut rt, &mut g, 300);
        let f = g.collector_status(CollectorId("failing")).unwrap();
        assert_eq!(f.state, CollectorState::Unavailable);
        // 20 ms interval doubling: far fewer than 15 attempts in 300 ms.
        assert!(f.failures <= 5, "{} failures", f.failures);
        for (id, s) in rt.set_emergency(true) {
            g.set_collector_status(id, s);
        }
        assert_eq!(
            g.collector_status(CollectorId("failing")).unwrap().state,
            CollectorState::Disabled
        );
        let before = g
            .collector_status(CollectorId("essential"))
            .unwrap()
            .samples;
        drive(&mut rt, &mut g, 100);
        assert!(
            g.collector_status(CollectorId("essential"))
                .unwrap()
                .samples
                > before
        );
    }

    #[test]
    fn snapshot_is_bounded_by_timeout_and_reports_blocked() {
        let gate = Gate::new();
        let collectors: Vec<Box<dyn Collector>> = vec![
            Box::new(Fake {
                id: CollectorId("blocked"),
                gate: Some(gate.clone()),
                fail: false,
            }),
            Box::new(Fake {
                id: CollectorId("healthy"),
                gate: None,
                fail: false,
            }),
        ];
        let t = std::time::Instant::now();
        let g = snapshot(
            collectors,
            Arc::new(MonotonicClock::new()),
            Duration::from_millis(50),
            Duration::from_millis(300),
        );
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(
            g.collector_status(CollectorId("blocked")).unwrap().state,
            CollectorState::Blocked
        );
        assert_eq!(
            g.collector_status(CollectorId("healthy")).unwrap().samples,
            2
        );
        gate.open();
    }

    #[test]
    fn snapshot_of_demo_machine_has_rates() {
        let m = rtop_collectors::demo::DemoMachine::new();
        let env = m.env(Subscriptions::all());
        let g = snapshot_with(
            m.collectors(&env),
            Arc::new(MonotonicClock::new()),
            Duration::from_millis(50),
            Duration::from_secs(5),
            || m.advance(1.0),
        );
        assert!(
            g.node(&EntityId::host())
                .unwrap()
                .f64("cpu_usage_percent")
                .is_some()
        );
    }
}
