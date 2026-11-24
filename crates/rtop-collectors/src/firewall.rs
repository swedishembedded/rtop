// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Firewall rule activity from existing nftables counters.
//!
//! A rule becomes a `firewall_rule` entity with packet and byte totals and
//! rates. Rules without a counter are listed as uninstrumented with
//! unavailable metrics; rtop never adds counters to measure them. A rule
//! that is replaced gets a new handle, hence a new entity and baseline, so a
//! reload never produces a false spike.

use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Scope, Unavailable};
use rtop_linux::KernelError;

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("firewall");
const MAX_RULES: usize = 20_000;

#[derive(Debug)]
pub struct FirewallCollector {
    env: Env,
    rates: RateTracker<(String, bool)>,
}

impl FirewallCollector {
    pub fn new(env: Env) -> Self {
        FirewallCollector {
            env,
            rates: RateTracker::new(),
        }
    }
}

impl Collector for FirewallCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(4),
            isolation: Isolation::Expensive,
            essential: false,
            nice: 5,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let rules = self.env.firewall.rules().map_err(|e| match e {
            KernelError::PermissionDenied => CollectorError::new(
                Unavailable::PermissionDenied,
                "reading nftables rules needs CAP_NET_ADMIN",
            ),
            other => CollectorError::from_kernel("nftables", &other),
        })?;
        let mut patch = ctx
            .patch(ID)
            .complete(Scope::Nodes(EntityKind::FirewallRule));
        let (mut pps, mut drop_pps, mut counted) = (0.0, 0.0, 0u64);
        self.rates.begin();
        for r in rules.iter().take(MAX_RULES) {
            let key = format!("{}/{}/{}/{}", r.family, r.table, r.chain, r.handle);
            let verdict = r.verdict.clone().unwrap_or_else(|| "continue".into());
            let label = r
                .comment
                .clone()
                .unwrap_or_else(|| format!("{} {} #{}", r.table, r.chain, r.handle));
            let mut n = NodeUpdate::new(
                EntityKind::FirewallRule,
                EntityId::new(EntityKind::FirewallRule, &key),
            )
            .text("name", &label)
            .attr("family", r.family)
            .text("table", &r.table)
            .text("chain", &r.chain)
            .attr("handle", r.handle)
            .text("verdict", &verdict)
            .attr("instrumented", r.counter.is_some());
            match r.counter {
                Some((packets, bytes)) => {
                    counted += 1;
                    let p = self.rates.metric(
                        (key.clone(), false),
                        packets,
                        ctx.now,
                        CounterWidth::Bits64,
                    );
                    let b = self
                        .rates
                        .metric((key, true), bytes, ctx.now, CounterWidth::Bits64);
                    if let Some(v) = p.as_f64() {
                        pps += v;
                        if verdict == "drop" {
                            drop_pps += v;
                        }
                    }
                    n.push_metric("packets_total", Metric::u64(packets));
                    n.push_metric("bytes_total", Metric::u64(bytes));
                    n.push_metric("packets_per_second", p);
                    n.push_metric("bytes_per_second", b);
                }
                None => {
                    // No counter: hits are unknown, not zero.
                    for k in [
                        "packets_total",
                        "bytes_total",
                        "packets_per_second",
                        "bytes_per_second",
                    ] {
                        n.push_metric(k, Metric::unavailable(Unavailable::NotMeasurable));
                    }
                }
            }
            patch.node(n);
        }
        self.rates.sweep();
        let counted_metric = |v: f64| {
            if counted > 0 {
                Metric::f64(v)
            } else {
                Metric::unavailable(Unavailable::NotMeasurable)
            }
        };
        patch.node(
            NodeUpdate::new(EntityKind::Host, EntityId::host())
                .metric("firewall_rules", Metric::u64(rules.len() as u64))
                .metric("firewall_counted_rules", Metric::u64(counted))
                .metric("firewall_packets_per_second", counted_metric(pps))
                .metric("firewall_drop_packets_per_second", counted_metric(drop_pps)),
        );
        let degraded = (rules.len() > MAX_RULES)
            .then(|| format!("only the first {MAX_RULES} rules are tracked"));
        Ok(Sample { patch, degraded })
    }
}
