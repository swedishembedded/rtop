// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Turning metrics into honest table cells.
//!
//! | shown        | meaning                          |
//! |--------------|----------------------------------|
//! | `0`          | measured zero                    |
//! | `N/A (perm)` | unavailable, with the reason     |
//! | `…`          | first sample taken, rate pending |
//! | `~42 MiB`    | estimate                         |
//! | `42 MiB*`    | last known value, collector stale|

use ratatui::style::Style;
use ratatui::text::Span;
use rtop_core::graph::MetricEntry;
use rtop_core::units::{format_bytes, format_duration_secs, format_rate};
use rtop_core::{Metric, Node, SystemGraph};

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    Percent,
    Bytes,
    Rate,
    Count,
    PerSecond,
    Float1,
    Float2,
    Celsius,
    Watts,
    Mhz,
    Duration,
    Ms,
}

pub fn value(v: f64, f: Fmt) -> String {
    match f {
        Fmt::Percent => format!("{v:.1}%"),
        Fmt::Bytes => format_bytes(v),
        Fmt::Rate => format_rate(v),
        Fmt::Count => format!("{v:.0}"),
        Fmt::PerSecond => {
            if v >= 10_000.0 {
                format!("{:.1}k/s", v / 1000.0)
            } else if v >= 100.0 {
                format!("{v:.0}/s")
            } else {
                format!("{v:.1}/s")
            }
        }
        Fmt::Float1 => format!("{v:.1}"),
        Fmt::Float2 => format!("{v:.2}"),
        Fmt::Celsius => format!("{v:.0}°C"),
        Fmt::Watts => format!("{v:.1} W"),
        Fmt::Mhz => {
            if v >= 1000.0 {
                format!("{:.2} GHz", v / 1000.0)
            } else {
                format!("{v:.0} MHz")
            }
        }
        Fmt::Duration => format_duration_secs(v),
        Fmt::Ms => format!("{v:.2} ms"),
    }
}

/// Formats one metric entry with availability markers.
pub fn entry(g: &SystemGraph, e: Option<&MetricEntry>, f: Fmt) -> (String, Style) {
    let Some(e) = e else {
        return ("N/A".into(), theme::faint());
    };
    let stale = g.is_stale(e.source);
    match e.metric {
        Metric::Valid(v) => {
            let s = value(v.as_f64(), f);
            if stale {
                (format!("{s}*"), theme::dim())
            } else {
                (s, Style::default())
            }
        }
        Metric::Estimated(v) => (
            format!("~{}{}", value(v.as_f64(), f), if stale { "*" } else { "" }),
            theme::dim(),
        ),
        Metric::Unavailable(r) => (r.short().to_owned(), theme::faint()),
    }
}

pub fn metric(g: &SystemGraph, n: &Node, key: &str, f: Fmt) -> (String, Style) {
    entry(g, n.metrics.get(key), f)
}

pub fn span(g: &SystemGraph, n: &Node, key: &str, f: Fmt) -> Span<'static> {
    let (t, s) = metric(g, n, key, f);
    Span::styled(t, s)
}

/// Sum of two rate metrics (read + write); unavailable only if both are.
pub fn pair(g: &SystemGraph, n: &Node, a: &str, b: &str, f: Fmt) -> (String, Style) {
    match (n.f64(a), n.f64(b)) {
        (None, None) => metric(g, n, a, f),
        (x, y) => (
            value(x.unwrap_or(0.0) + y.unwrap_or(0.0), f),
            Style::default(),
        ),
    }
}

/// Truncates to `width` display columns, marking the cut with `…`.
pub fn truncate(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut w = 0;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_owned();
    }
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn truncation_is_width_aware() {
        assert_eq!(truncate("chrome", 10), "chrome");
        assert_eq!(truncate("chromium-browser", 8), "chromiu…");
        assert_eq!(truncate("日本語のプロセス", 7).width(), 7);
    }

    proptest! {
        #[test]
        fn truncate_never_exceeds_width(s in "\\PC*", w in 0usize..40) {
            prop_assert!(truncate(&s, w).width() <= w.max(1));
        }
    }
}
