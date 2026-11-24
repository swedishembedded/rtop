// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! `/dev/kmsg` records and classification of security-relevant kernel
//! messages.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsgRecord {
    /// Syslog priority (0 = emergency ... 7 = debug).
    pub level: u8,
    pub sequence: u64,
    /// Microseconds since boot (CLOCK_MONOTONIC based).
    pub timestamp_us: u64,
    pub message: String,
}

/// Parses one record: `prio,seq,ts_usec,flags[,...];message\n[ continuation]`.
pub fn record(raw: &str) -> Option<KmsgRecord> {
    let (header, rest) = raw.split_once(';')?;
    let mut f = header.split(',');
    let prio: u32 = f.next()?.parse().ok()?;
    let sequence = f.next()?.parse().ok()?;
    let timestamp_us = f.next()?.parse().ok()?;
    let message = rest.lines().next().unwrap_or("").to_owned();
    Some(KmsgRecord {
        level: (prio & 7) as u8,
        sequence,
        timestamp_us,
        message,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelEventKind {
    AppArmorDenied,
    SelinuxDenied,
    SeccompViolation,
    OomKill,
    Segfault,
    FirewallLog,
    AuditOther,
    HardwareError,
}

impl KernelEventKind {
    pub fn describe(self) -> &'static str {
        match self {
            KernelEventKind::AppArmorDenied => "AppArmor access denied",
            KernelEventKind::SelinuxDenied => "SELinux access denied",
            KernelEventKind::SeccompViolation => "Seccomp filter violation",
            KernelEventKind::OomKill => "Out-of-memory kill",
            KernelEventKind::Segfault => "Segmentation fault",
            KernelEventKind::FirewallLog => "Firewall log match",
            KernelEventKind::AuditOther => "Audit event",
            KernelEventKind::HardwareError => "Hardware error",
        }
    }

    pub fn is_security(self) -> bool {
        matches!(
            self,
            KernelEventKind::AppArmorDenied
                | KernelEventKind::SelinuxDenied
                | KernelEventKind::SeccompViolation
                | KernelEventKind::FirewallLog
                | KernelEventKind::AuditOther
        )
    }
}

/// Classifies a kernel message; `None` for ordinary log noise.
pub fn classify(message: &str) -> Option<KernelEventKind> {
    let m = message;
    if m.contains("apparmor=\"DENIED\"") {
        Some(KernelEventKind::AppArmorDenied)
    } else if m.contains("avc:  denied") || m.contains("avc: denied") {
        Some(KernelEventKind::SelinuxDenied)
    } else if m.contains("type=1326") || (m.contains("audit") && m.contains("seccomp")) {
        Some(KernelEventKind::SeccompViolation)
    } else if m.contains("Out of memory: Killed process") || m.contains("oom-kill:") {
        Some(KernelEventKind::OomKill)
    } else if m.contains(" segfault at ") {
        Some(KernelEventKind::Segfault)
    } else if m.contains(" IN=") && m.contains(" OUT=") && m.contains(" PROTO=") {
        Some(KernelEventKind::FirewallLog)
    } else if m.contains("Machine check")
        || m.contains("EDAC") && m.contains("error")
        || m.contains("AER:")
    {
        Some(KernelEventKind::HardwareError)
    } else if m.starts_with("audit:") {
        Some(KernelEventKind::AuditOther)
    } else {
        None
    }
}

/// Extracts `pid=N` (audit records) or `Killed process N` (OOM).
pub fn subject_pid(message: &str) -> Option<u32> {
    let digits = |s: &str| {
        s.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    if let Some(i) = message.find("Killed process ") {
        return digits(&message[i + 15..]);
    }
    message
        .split(' ')
        .find_map(|w| w.strip_prefix("pid="))
        .and_then(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records_and_classifies() {
        let r = record("4,1234,5678901,-;audit: type=1400 apparmor=\"DENIED\" operation=\"open\" pid=4242 comm=\"cat\"\n SUBSYSTEM=x").unwrap();
        assert_eq!((r.level, r.sequence, r.timestamp_us), (4, 1234, 5_678_901));
        assert_eq!(classify(&r.message), Some(KernelEventKind::AppArmorDenied));
        assert_eq!(subject_pid(&r.message), Some(4242));
        assert_eq!(
            subject_pid("Out of memory: Killed process 77 (java)"),
            Some(77)
        );
        assert_eq!(
            classify("[UFW BLOCK] IN=wlan0 OUT= MAC=x SRC=1.2.3.4 PROTO=TCP"),
            Some(KernelEventKind::FirewallLog)
        );
        assert_eq!(classify("usb 1-1: new device"), None);
        assert!(record("garbage").is_none());
    }
}
