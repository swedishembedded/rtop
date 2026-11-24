// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Human-readable formatting and parsing of quantities.
//!
//! Byte quantities use IEC binary prefixes (KiB = 1024 B) everywhere, both
//! when displayed and when parsed from filter expressions such as
//! `rss>1GiB`, so what the user types means what the screen shows.

const IEC: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// `1536` → `"1.5 KiB"`.
pub fn format_bytes(bytes: f64) -> String {
    if !bytes.is_finite() {
        return "N/A".into();
    }
    let neg = bytes < 0.0;
    let mut v = bytes.abs();
    let mut i = 0;
    while v >= 1024.0 && i + 1 < IEC.len() {
        v /= 1024.0;
        i += 1;
    }
    let sign = if neg { "-" } else { "" };
    if i == 0 {
        format!("{sign}{v:.0} B")
    } else if v >= 100.0 {
        format!("{sign}{v:.0} {}", IEC[i])
    } else if v >= 10.0 {
        format!("{sign}{v:.1} {}", IEC[i])
    } else {
        format!("{sign}{v:.2} {}", IEC[i])
    }
}

/// `2048.0` → `"2.00 KiB/s"`.
pub fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

/// Seconds → compact duration (`"3d04h"`, `"12:05"`).
pub fn format_duration_secs(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "N/A".into();
    }
    let s = secs as u64;
    let (d, h, m, sec) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60, s % 60);
    if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m:02}:{sec:02}")
    }
}

/// Parses `512`, `4k`, `1.5GiB`, `200MiB`, `10MB` into bytes. Decimal SI
/// suffixes (kB, MB, GB) are accepted and mean powers of 1000.
pub fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024f64.powi(2),
        "g" | "gib" => 1024f64.powi(3),
        "t" | "tib" => 1024f64.powi(4),
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };
    Some(n * mult)
}

/// Parses `500ms`, `2s`, `1m`, `1h`, `1d` into seconds.
pub fn parse_duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match suffix {
        "ms" => 1e-3,
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        _ => return None,
    };
    Some(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_trip_through_display_and_parse() {
        assert_eq!(format_bytes(1536.0), "1.50 KiB");
        assert_eq!(format_bytes(13.9 * 1024f64.powi(3)), "13.9 GiB");
        assert_eq!(parse_size("1GiB"), Some(1024f64.powi(3)));
        assert_eq!(parse_size("200MiB"), Some(200.0 * 1024f64.powi(2)));
        assert_eq!(parse_size("10MB"), Some(1e7));
        assert_eq!(parse_size("lots"), None);
    }

    #[test]
    fn durations_parse_and_format() {
        assert_eq!(parse_duration_secs("1h"), Some(3600.0));
        assert_eq!(parse_duration_secs("250ms"), Some(0.25));
        assert_eq!(format_duration_secs(90_061.0), "1d01h");
        assert_eq!(format_duration_secs(65.0), "01:05");
    }
}
