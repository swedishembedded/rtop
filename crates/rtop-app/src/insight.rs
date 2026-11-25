// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Evidence-based explanations of what the numbers mean right now.
//!
//! Each insight is an explainable rule over measured metrics, phrased as a
//! hypothesis with its evidence, never as a verdict. Missing data yields no
//! insight rather than a guess.

use rtop_core::{EntityId, EntityKind, Severity, SystemGraph};

#[derive(Debug, Clone, PartialEq)]
pub struct Insight {
    pub severity: Severity,
    pub text: String,
    pub subject: Option<EntityId>,
}

fn push(out: &mut Vec<Insight>, severity: Severity, text: String, subject: Option<EntityId>) {
    out.push(Insight {
        severity,
        text,
        subject,
    });
}

pub fn insights(g: &SystemGraph) -> Vec<Insight> {
    let mut out = Vec::new();
    let Some(h) = g.node(&EntityId::host()) else {
        return out;
    };
    let f = |k: &str| h.f64(k);

    if let (Some(used), Some(total)) = (f("swap_used_bytes"), f("swap_total_bytes"))
        && total > 0.0
        && used / total > 0.8
    {
        let pct = 100.0 * used / total;
        match (f("swap_in_pages_per_second"), f("psi_memory_some_avg10")) {
            (Some(sin), Some(psi)) if sin > 50.0 || psi > 5.0 => push(
                &mut out,
                Severity::Critical,
                format!(
                    "Swap {pct:.0}% occupied and actively paging: {sin:.0} pages/s in, memory PSI {psi:.1}% — active memory contention"
                ),
                None,
            ),
            (Some(sin), psi) => push(
                &mut out,
                Severity::Info,
                format!(
                    "Swap {pct:.0}% occupied but only {sin:.0} pages/s swapped in{} — inactive pages parked in swap, not current pressure",
                    psi.map_or(String::new(), |p| format!(", memory PSI {p:.1}%"))
                ),
                None,
            ),
            (None, _) => push(
                &mut out,
                Severity::Warning,
                format!(
                    "Swap {pct:.0}% occupied; paging rate unknown, so pressure cannot be judged"
                ),
                None,
            ),
        }
    }
    if let Some(psi) = f("psi_memory_full_avg10").filter(|p| *p > 1.0) {
        push(
            &mut out,
            Severity::Critical,
            format!("All runnable tasks stalled on memory {psi:.1}% of the time (PSI full)"),
            None,
        );
    }
    if let (Some(psi), Some(cpu)) = (f("psi_cpu_some_avg10"), f("cpu_usage_percent"))
        && psi > 20.0
    {
        push(
            &mut out,
            Severity::Warning,
            format!(
                "CPU contention: tasks waited for a CPU {psi:.0}% of the time (CPU busy {cpu:.0}%)"
            ),
            None,
        );
    }
    if let (Some(io), Some(cpu)) = (f("psi_io_some_avg10"), f("cpu_usage_percent"))
        && io > 10.0
        && cpu < 50.0
    {
        push(
            &mut out,
            Severity::Warning,
            format!("Storage bottleneck likely: I/O PSI {io:.0}% while CPU is only {cpu:.0}% busy"),
            None,
        );
    }
    let blocked: Vec<_> = g
        .nodes_of(EntityKind::Process)
        .filter(|p| p.text("state_code") == Some("D"))
        .collect();
    if let Some(first) = blocked.first() {
        let names: Vec<&str> = blocked.iter().take(3).map(|p| p.label()).collect();
        push(
            &mut out,
            Severity::Warning,
            format!(
                "{} process(es) in uninterruptible kernel wait (D): {}",
                blocked.len(),
                names.join(", ")
            ),
            Some(first.id.clone()),
        );
    }
    for c in g
        .nodes_of(EntityKind::Container)
        .chain(g.nodes_of(EntityKind::Service))
    {
        if let Some(t) = c.f64("cpu_throttled_percent").filter(|t| *t > 5.0) {
            push(
                &mut out,
                Severity::Warning,
                format!(
                    "{} is CPU-throttled {t:.0}% of the time by its quota",
                    c.label()
                ),
                Some(c.id.clone()),
            );
        }
    }
    for s in g.nodes_of(EntityKind::Sensor) {
        if let (Some(v), Some(crit)) = (s.f64("value"), s.f64("critical").or_else(|| s.f64("max")))
            && s.text("unit") == Some("celsius")
            && crit > 0.0
            && v >= crit - 5.0
        {
            push(
                &mut out,
                Severity::Critical,
                format!(
                    "{} at {v:.0}°C, within 5°C of its {crit:.0}°C limit",
                    s.label()
                ),
                Some(s.id.clone()),
            );
        }
    }
    for d in g.nodes_of(EntityKind::BlockDevice) {
        match d.text("health") {
            Some("failing") => push(
                &mut out,
                Severity::Critical,
                format!(
                    "Drive {} reports failure: {}",
                    d.label(),
                    d.text("health_detail").unwrap_or("")
                ),
                Some(d.id.clone()),
            ),
            Some("warning") => push(
                &mut out,
                Severity::Warning,
                format!(
                    "Drive {}: {}",
                    d.label(),
                    d.text("health_detail").unwrap_or("")
                ),
                Some(d.id.clone()),
            ),
            _ => {}
        }
    }
    if let Some(r) = f("tcp_retransmits_per_second").filter(|r| *r > 50.0) {
        push(
            &mut out,
            Severity::Warning,
            format!("{r:.0} TCP retransmissions/s: delivery problems or congestion"),
            None,
        );
    }
    for m in g.nodes_of(EntityKind::Mount) {
        if let Some(u) = m.f64("used_percent").filter(|u| *u >= 90.0) {
            push(
                &mut out,
                Severity::Warning,
                format!("{} is {u:.0}% full", m.label()),
                Some(m.id.clone()),
            );
        }
    }
    if let Some(e) = f("security_events_per_minute").filter(|e| *e > 0.0) {
        push(
            &mut out,
            Severity::Info,
            format!("{e:.0} security-related kernel events in the last minute (view 8)"),
            None,
        );
    }
    for (id, st) in g.collectors() {
        if st.state == rtop_core::CollectorState::Blocked {
            push(
                &mut out,
                Severity::Warning,
                format!("Telemetry source '{id}' is not answering; its data is stale"),
                None,
            );
        }
    }
    out.sort_by_key(|i| std::cmp::Reverse(i.severity));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtop_core::{CollectorId, GraphPatch, Metric, NodeUpdate, Timestamp};

    fn host(metrics: &[(&'static str, f64)]) -> SystemGraph {
        let mut g = SystemGraph::default();
        let mut n = NodeUpdate::new(EntityKind::Host, EntityId::host());
        for (k, v) in metrics {
            n.push_metric(k, Metric::f64(*v));
        }
        let mut p = GraphPatch::new(CollectorId("t"), 1, Timestamp(1));
        p.node(n);
        g.apply(p);
        g
    }

    #[test]
    fn full_swap_without_paging_is_not_pressure() {
        let g = host(&[
            ("swap_used_bytes", 99.0),
            ("swap_total_bytes", 100.0),
            ("swap_in_pages_per_second", 0.4),
            ("psi_memory_some_avg10", 0.3),
        ]);
        let i = insights(&g);
        assert_eq!(i[0].severity, Severity::Info);
        assert!(i[0].text.contains("not current pressure"));
    }

    #[test]
    fn full_swap_with_paging_is_contention_and_unknown_is_said() {
        let g = host(&[
            ("swap_used_bytes", 99.0),
            ("swap_total_bytes", 100.0),
            ("swap_in_pages_per_second", 900.0),
            ("psi_memory_some_avg10", 12.0),
        ]);
        assert_eq!(insights(&g)[0].severity, Severity::Critical);
        let g = host(&[("swap_used_bytes", 99.0), ("swap_total_bytes", 100.0)]);
        assert!(insights(&g)[0].text.contains("cannot be judged"));
    }
}
