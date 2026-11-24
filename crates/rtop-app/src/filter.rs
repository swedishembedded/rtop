// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Search and filter expressions.
//!
//! ```text
//! chrome                      name or command line contains "chrome"
//! user:martin  state:D        attribute contains / equals
//! cpu>10  rss>1GiB  age>1h    numeric comparisons with units
//! name:claude AND rss>200MiB  explicit AND (juxtaposition is AND too)
//! nginx OR postgres           OR binds weaker than AND
//! !kworker  -kworker          negation
//! ```
//!
//! An expression is parsed once per edit into a [`Filter`] and then
//! evaluated per row without further parsing.

use rtop_core::units::{parse_duration_secs, parse_size};
use rtop_core::{EntityKind, Node};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

#[derive(Debug, Clone, PartialEq)]
enum Term {
    Text(String),
    Attr {
        key: String,
        value: String,
    },
    Num {
        metric: &'static [&'static str],
        op: Op,
        value: f64,
    },
    Not(Box<Term>),
}

/// A compiled expression: OR of AND-groups.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Filter {
    groups: Vec<Vec<Term>>,
    source: String,
}

/// Metric family a filter key compares, and how its value is parsed.
type NumericKey = (&'static [&'static str], fn(&str) -> Option<f64>);

fn numeric_metric(key: &str) -> Option<NumericKey> {
    let plain = |s: &str| s.parse::<f64>().ok();
    Some(match key {
        "cpu" => (&["cpu_usage_percent"], plain),
        "rss" | "mem" | "memory" => (
            &[
                "rss_bytes",
                "memory_current_bytes",
                "vram_used_bytes",
                "used_bytes",
            ],
            parse_size,
        ),
        "virt" | "vsz" => (&["virtual_bytes"], parse_size),
        "swap" => (&["swap_bytes", "swap_current_bytes"], parse_size),
        "threads" => (&["threads"], plain),
        "io" => (
            &[
                "io_read_bytes_per_second",
                "io_write_bytes_per_second",
                "read_bytes_per_second",
                "write_bytes_per_second",
            ],
            parse_size,
        ),
        "net" => (
            &[
                "tcp_rx_bytes_per_second",
                "tcp_tx_bytes_per_second",
                "rx_bytes_per_second",
                "tx_bytes_per_second",
                "network_rx_bytes_per_second",
            ],
            parse_size,
        ),
        "gpu" => (&["gpu_usage_percent"], plain),
        "age" => (&["age_seconds"], parse_duration_secs),
        "nice" => (&["nice"], plain),
        "temp" => (&["value", "temperature_celsius"], plain),
        _ => return None,
    })
}

fn parse_term(tok: &str) -> Result<Term, String> {
    if let Some(rest) = tok
        .strip_prefix('!')
        .or_else(|| tok.strip_prefix('-').filter(|r| !r.is_empty()))
    {
        return Ok(Term::Not(Box::new(parse_term(rest)?)));
    }
    if tok == "pid" {
        return Ok(Term::Text(tok.into()));
    }
    for (sym, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
    ] {
        if let Some((k, v)) = tok.split_once(sym) {
            let key = k.to_ascii_lowercase();
            if key == "pid" {
                let value = v.parse().map_err(|_| format!("'{v}' is not a PID"))?;
                return Ok(Term::Num {
                    metric: &["pid"],
                    op,
                    value,
                });
            }
            let (metric, parse) = numeric_metric(&key).ok_or_else(|| format!("unknown metric '{k}' (try cpu, rss, virt, swap, threads, io, net, gpu, age, nice, pid)"))?;
            let value = parse(v).ok_or_else(|| format!("cannot read '{v}' as a value for {k}"))?;
            return Ok(Term::Num { metric, op, value });
        }
    }
    if let Some((k, v)) = tok.split_once(':') {
        if !k.is_empty() && !v.is_empty() {
            return Ok(Term::Attr {
                key: k.to_ascii_lowercase(),
                value: v.to_ascii_lowercase(),
            });
        }
    }
    Ok(Term::Text(tok.to_ascii_lowercase()))
}

impl Filter {
    pub fn parse(expr: &str) -> Result<Filter, String> {
        let mut groups = vec![Vec::new()];
        for tok in expr.split_whitespace() {
            match tok {
                "AND" | "and" | "&&" => {}
                "OR" | "or" | "||" => groups.push(Vec::new()),
                t => groups.last_mut().expect("non-empty").push(parse_term(t)?),
            }
        }
        groups.retain(|g| !g.is_empty());
        Ok(Filter {
            groups,
            source: expr.trim().to_owned(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn matches(&self, n: &Node) -> bool {
        self.groups.is_empty()
            || self
                .groups
                .iter()
                .any(|g| g.iter().all(|t| term_matches(t, n)))
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase().contains(needle)
}

fn term_matches(t: &Term, n: &Node) -> bool {
    match t {
        Term::Not(inner) => !term_matches(inner, n),
        Term::Text(s) => {
            contains_ci(n.label(), s)
                || n.text("cmdline").is_some_and(|c| contains_ci(c, s))
                || n.attr("pid")
                    .and_then(|p| p.as_u64())
                    .is_some_and(|p| p.to_string() == *s)
                || (n.kind != EntityKind::Process && contains_ci(n.id.as_str(), s))
        }
        Term::Attr { key, value } => {
            let attr = match key.as_str() {
                "name" => Some(n.label().to_owned()),
                "cmd" | "cmdline" => n.text("cmdline").map(str::to_owned),
                "state" => {
                    // state:D matches the code exactly, state:sleep the name.
                    let code = n.text("state_code").unwrap_or("");
                    if value.len() == 1 {
                        return code.eq_ignore_ascii_case(value);
                    }
                    n.text("state").map(str::to_owned)
                }
                "kind" => Some(n.kind.as_str().to_owned()),
                k => n.attr(k).map(|a| match a {
                    rtop_core::Attr::Text(s) => s.clone(),
                    rtop_core::Attr::Int(v) => v.to_string(),
                    rtop_core::Attr::UInt(v) => v.to_string(),
                    rtop_core::Attr::Bool(v) => v.to_string(),
                }),
            };
            attr.is_some_and(|a| contains_ci(&a, value))
        }
        Term::Num { metric, op, value } => {
            let v = if metric == &["pid"] {
                n.attr("pid").and_then(|p| p.as_u64()).map(|p| p as f64)
            } else {
                // Sum the metric family (e.g. read + write) when several exist.
                let vals: Vec<f64> = metric.iter().filter_map(|m| n.f64(m)).collect();
                let pair = metric.len() > 1
                    && metric
                        .iter()
                        .take(2)
                        .all(|m| m.contains("_bytes_per_second"));
                if pair {
                    (!vals.is_empty()).then(|| vals.iter().take(2).sum())
                } else {
                    vals.first().copied()
                }
            };
            // Unavailable values never match a comparison.
            v.is_some_and(|v| match op {
                Op::Gt => v > *value,
                Op::Ge => v >= *value,
                Op::Lt => v < *value,
                Op::Le => v <= *value,
                Op::Eq => (v - value).abs() < 1e-9,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rtop_core::{
        CollectorId, EntityId, GraphPatch, Metric, NodeUpdate, SystemGraph, Timestamp, Unavailable,
    };

    fn node(name: &str, cmd: &str, user: &str, cpu: Metric, rss: u64, state: &str) -> SystemGraph {
        let mut g = SystemGraph::default();
        let mut p = GraphPatch::new(CollectorId("t"), 1, Timestamp(1));
        p.node(
            NodeUpdate::new(
                EntityKind::Process,
                EntityId::new(EntityKind::Process, "42:1"),
            )
            .attr("pid", 42u64)
            .text("name", name)
            .text("cmdline", cmd)
            .text("user", user)
            .attr("state_code", state)
            .metric("cpu_usage_percent", cpu)
            .metric("rss_bytes", Metric::u64(rss))
            .metric("age_seconds", Metric::f64(7200.0)),
        );
        g.apply(p);
        g
    }

    fn m(expr: &str, g: &SystemGraph) -> bool {
        Filter::parse(expr)
            .unwrap()
            .matches(g.nodes().next().unwrap())
    }

    #[test]
    fn spec_examples() {
        let g = node(
            "chrome",
            "/opt/chrome --type=renderer",
            "martin",
            Metric::f64(12.0),
            2 << 30,
            "D",
        );
        for yes in [
            "chrome",
            "renderer",
            "user:martin",
            "cpu>10",
            "rss>1GiB",
            "state:D",
            "age>1h",
            "name:chr AND rss>200MiB",
            "nginx OR chrome",
            "!kworker",
            "pid=42",
            "42",
        ] {
            assert!(m(yes, &g), "{yes} should match");
        }
        for no in [
            "firefox",
            "cpu>50",
            "state:S",
            "threads>50",
            "-chrome",
            "chrome AND user:root",
        ] {
            assert!(!m(no, &g), "{no} should not match");
        }
        assert!(Filter::parse("bogus>1").is_err());
        assert!(Filter::parse("rss>lots").is_err());
    }

    #[test]
    fn unavailable_metrics_never_match_comparisons() {
        let g = node(
            "x",
            "",
            "u",
            Metric::Unavailable(Unavailable::Pending),
            0,
            "S",
        );
        assert!(!m("cpu>=0", &g));
        assert!(!m("cpu<1", &g));
    }

    proptest! {
        #[test]
        fn parse_never_panics(s in "\\PC*") {
            let _ = Filter::parse(&s);
        }
    }
}
