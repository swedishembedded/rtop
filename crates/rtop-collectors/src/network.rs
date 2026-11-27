// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Network interfaces (`/proc/net/dev`) and protocol counters
//! (`/proc/net/snmp`).

use std::collections::HashMap;
use std::time::Duration;

use rtop_core::counter::{CounterWidth, RateTracker};
use rtop_core::{CollectorId, EntityId, EntityKind, Metric, NodeUpdate, Scope, Unavailable};
use rtop_linux::fs::KernelFsExt;
use rtop_linux::parse::net;

use crate::{Collector, CollectorError, Descriptor, Env, Isolation, SMALL, Sample, SampleContext};

pub const ID: CollectorId = CollectorId("network");

#[derive(Debug)]
pub struct NetworkCollector {
    env: Env,
    rates: RateTracker<(String, &'static str)>,
    physical: HashMap<String, bool>,
    /// Whether an interface is a fabric NIC with hardware telemetry.
    fabric: HashMap<String, bool>,
}

impl NetworkCollector {
    pub fn new(env: Env) -> Self {
        NetworkCollector {
            env,
            rates: RateTracker::new(),
            physical: HashMap::new(),
            fabric: HashMap::new(),
        }
    }
}

/// A CXI telemetry counter: `<value>@<seconds>.<nanoseconds>`.
pub fn cxi_counter(text: &str) -> Option<u64> {
    text.trim().split('@').next()?.parse().ok()
}

pub fn interface_id(name: &str) -> EntityId {
    EntityId::new(EntityKind::Interface, name)
}

impl Collector for NetworkCollector {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: ID,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            isolation: Isolation::Fast,
            essential: false,
            nice: 0,
        }
    }

    fn sample(&mut self, ctx: &SampleContext) -> Result<Sample, CollectorError> {
        let fs = self.env.fs.clone();
        let text = fs
            .read_string("/proc/net/dev", SMALL)
            .map_err(|e| CollectorError::from_kernel("/proc/net/dev", &e))?;
        let now = ctx.now;
        let mut patch = ctx.patch(ID).complete(Scope::Nodes(EntityKind::Interface));
        let (mut rx, mut tx, mut have) = (0.0, 0.0, false);
        // Fabric totals over every fabric NIC; `None` without one.
        let mut fabric_total: [Option<f64>; 2] = [None, None];
        self.rates.begin();
        let devs = net::net_dev(&text);
        for d in &devs {
            let sys = format!("/sys/class/net/{}", d.name);
            let physical = *self
                .physical
                .entry(d.name.clone())
                .or_insert_with(|| fs.read_link(&format!("{sys}/device")).is_ok());
            let mut n = NodeUpdate::new(EntityKind::Interface, interface_id(&d.name))
                .text("name", &d.name)
                .attr("physical", physical);
            if let Ok(s) = fs.read_attr(&format!("{sys}/operstate")) {
                n = n.attr("operstate", s);
            }
            if let Ok(speed) = fs.read_i64(&format!("{sys}/speed")) {
                // -1 = unknown (down links, virtual devices).
                n.push_metric(
                    "link_speed_mbps",
                    if speed > 0 {
                        Metric::i64(speed)
                    } else {
                        Metric::unavailable(Unavailable::NotMeasurable)
                    },
                );
            }
            if let Ok(mtu) = fs.read_u64(&format!("{sys}/mtu")) {
                n.push_metric("mtu", Metric::u64(mtu));
            }
            // HPE Slingshot (CXI) NICs count all traffic, including the
            // kernel-bypass RDMA the netdev counters never see.
            let tele = format!("{sys}/device/telemetry");
            let fabric = physical
                && *self.fabric.entry(d.name.clone()).or_insert_with(|| {
                    fs.read_attr(&format!("{tele}/hni_sts_rx_ok_octets"))
                        .is_ok()
                });
            if fabric {
                n = n.attr("fabric", "cxi");
                for (k, f) in [("fabric_link", "link"), ("fabric_speed", "speed")] {
                    if let Ok(v) = fs.read_attr(&format!("{sys}/device/port/{f}")) {
                        n = n.text(k, &v);
                    }
                }
                for (k, f) in [
                    ("fabric_rx_bytes_per_second", "hni_sts_rx_ok_octets"),
                    ("fabric_tx_bytes_per_second", "hni_sts_tx_ok_octets"),
                ] {
                    if let Some(v) = fs
                        .read_attr(&format!("{tele}/{f}"))
                        .ok()
                        .and_then(|t| cxi_counter(&t))
                    {
                        let m =
                            self.rates
                                .metric((d.name.clone(), k), v, now, CounterWidth::Bits64);
                        let slot = usize::from(k.starts_with("fabric_tx"));
                        if let Some(r) = m.as_f64() {
                            *fabric_total[slot].get_or_insert(0.0) += r;
                        }
                        n.push_metric(k, m);
                    }
                }
            }
            if fs.read_dir(&format!("{sys}/wireless")).is_ok() {
                n = n.attr("wireless", true);
            }
            let key = d.name.clone();
            let mut rate = |k: &'static str, v: u64| {
                self.rates
                    .metric((key.clone(), k), v, now, CounterWidth::Bits64)
            };
            let r = rate("rx_bytes_per_second", d.rx_bytes);
            let t = rate("tx_bytes_per_second", d.tx_bytes);
            if d.name != "lo"
                && let (Some(a), Some(b)) = (r.as_f64(), t.as_f64())
            {
                rx += a;
                tx += b;
                have = true;
            }
            n.push_metric("rx_bytes_per_second", r);
            n.push_metric("tx_bytes_per_second", t);
            n.push_metric(
                "rx_packets_per_second",
                rate("rx_packets_per_second", d.rx_packets),
            );
            n.push_metric(
                "tx_packets_per_second",
                rate("tx_packets_per_second", d.tx_packets),
            );
            n.push_metric(
                "rx_errors_per_second",
                rate("rx_errors_per_second", d.rx_errors),
            );
            n.push_metric(
                "tx_errors_per_second",
                rate("tx_errors_per_second", d.tx_errors),
            );
            n.push_metric(
                "rx_dropped_per_second",
                rate("rx_dropped_per_second", d.rx_dropped),
            );
            n.push_metric(
                "tx_dropped_per_second",
                rate("tx_dropped_per_second", d.tx_dropped),
            );
            n.push_metric("rx_bytes_total", Metric::u64(d.rx_bytes));
            n.push_metric("tx_bytes_total", Metric::u64(d.tx_bytes));
            patch.node(n);
        }
        self.physical
            .retain(|k, _| devs.iter().any(|d| &d.name == k));
        self.fabric.retain(|k, _| devs.iter().any(|d| &d.name == k));

        let mut h = NodeUpdate::new(EntityKind::Host, EntityId::host());
        for (k, v) in [
            ("fabric_rx_bytes_per_second", fabric_total[0]),
            ("fabric_tx_bytes_per_second", fabric_total[1]),
        ] {
            if let Some(v) = v {
                h.push_metric(k, Metric::f64(v));
            }
        }
        let agg = |v: f64| {
            if have {
                Metric::f64(v)
            } else {
                Metric::unavailable(Unavailable::Pending)
            }
        };
        h.push_metric("network_rx_bytes_per_second", agg(rx));
        h.push_metric("network_tx_bytes_per_second", agg(tx));
        if let Ok(snmp) = fs.read_string("/proc/net/snmp", SMALL) {
            for (proto, field, v) in net::snmp(&snmp) {
                let key = match (proto.as_str(), field.as_str()) {
                    ("Tcp", "RetransSegs") => "tcp_retransmits_per_second",
                    ("Tcp", "ActiveOpens") => "tcp_active_opens_per_second",
                    ("Tcp", "PassiveOpens") => "tcp_passive_opens_per_second",
                    ("Tcp", "InErrs") => "tcp_in_errors_per_second",
                    ("Tcp", "AttemptFails") => "tcp_attempt_fails_per_second",
                    ("Udp", "InErrors") => "udp_in_errors_per_second",
                    ("Udp", "RcvbufErrors") => "udp_rcvbuf_errors_per_second",
                    ("Tcp", "CurrEstab") => {
                        h.push_metric("tcp_established", Metric::i64(v));
                        continue;
                    }
                    _ => continue,
                };
                let m = self.rates.metric(
                    (String::new(), key),
                    u64::try_from(v).unwrap_or(0),
                    now,
                    CounterWidth::Bits64,
                );
                h.push_metric(key, m);
            }
        }
        self.rates.sweep();
        patch.node(h);
        Ok(Sample {
            patch,
            degraded: None,
        })
    }
}

#[cfg(test)]
mod fabric_tests {
    #[test]
    fn cxi_counters_parse_value_before_timestamp() {
        assert_eq!(
            super::cxi_counter("1576540091879@1795739423.667124381\n"),
            Some(1_576_540_091_879)
        );
        assert_eq!(super::cxi_counter("garbage"), None);
    }
}
