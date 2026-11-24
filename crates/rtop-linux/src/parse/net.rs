// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! `/proc/net/dev`, `/proc/net/{tcp,udp,raw}{,6}`, `/proc/net/unix` and
//! `/proc/net/snmp`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetDev {
    pub name: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

pub fn net_dev(text: &str) -> Vec<NetDev> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let f: Vec<u64> = rest
                .split_whitespace()
                .map_while(|v| v.parse().ok())
                .collect();
            if f.len() < 16 {
                return None;
            }
            Some(NetDev {
                name: name.trim().to_owned(),
                rx_bytes: f[0],
                rx_packets: f[1],
                rx_errors: f[2],
                rx_dropped: f[3],
                tx_bytes: f[8],
                tx_packets: f[9],
                tx_errors: f[10],
                tx_dropped: f[11],
            })
        })
        .collect()
}

/// TCP states as numbered by the kernel (`include/net/tcp_states.h`).
pub fn tcp_state_name(state: u8) -> &'static str {
    match state {
        1 => "established",
        2 => "syn_sent",
        3 => "syn_recv",
        4 => "fin_wait1",
        5 => "fin_wait2",
        6 => "time_wait",
        7 => "close",
        8 => "close_wait",
        9 => "last_ack",
        10 => "listen",
        11 => "closing",
        12 => "new_syn_recv",
        _ => "unknown",
    }
}

/// One row of a `/proc/net/{tcp,udp,raw}{,6}` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InetRow {
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub state: u8,
    pub tx_queue: u64,
    pub rx_queue: u64,
    pub uid: u32,
    pub inode: u64,
}

fn hex_addr(s: &str) -> Option<SocketAddr> {
    let (a, p) = s.split_once(':')?;
    let port = u16::from_str_radix(p, 16).ok()?;
    // The kernel prints each 32-bit word in host byte order.
    let ip = match a.len() {
        8 => IpAddr::V4(Ipv4Addr::from(
            u32::from_str_radix(a, 16).ok()?.to_ne_bytes(),
        )),
        32 => {
            let mut bytes = [0u8; 16];
            for (i, chunk) in bytes.chunks_mut(4).enumerate() {
                let w = u32::from_str_radix(a.get(i * 8..i * 8 + 8)?, 16).ok()?;
                chunk.copy_from_slice(&w.to_ne_bytes());
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

pub fn inet_table(text: &str) -> Vec<InetRow> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 {
                return None;
            }
            let (tx, rx) = f[4].split_once(':')?;
            Some(InetRow {
                local: hex_addr(f[1])?,
                remote: hex_addr(f[2])?,
                state: u8::from_str_radix(f[3], 16).ok()?,
                tx_queue: u64::from_str_radix(tx, 16).ok()?,
                rx_queue: u64::from_str_radix(rx, 16).ok()?,
                uid: f[7].parse().ok()?,
                inode: f[9].parse().ok()?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixRow {
    pub inode: u64,
    /// SOCK_STREAM=1, SOCK_DGRAM=2, SOCK_SEQPACKET=5.
    pub sock_type: u16,
    /// SS_UNCONNECTED=1, SS_CONNECTING=2, SS_CONNECTED=3, SS_DISCONNECTING=4.
    pub state: u8,
    pub listening: bool,
    /// Filesystem path, `@name` for abstract sockets, empty when unnamed.
    pub path: String,
}

pub fn unix_table(text: &str) -> Vec<UnixRow> {
    const SO_ACCEPTCON: u32 = 1 << 16;
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.splitn(8, ' ').filter(|s| !s.is_empty()).collect();
            if f.len() < 7 {
                return None;
            }
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            Some(UnixRow {
                sock_type: u16::from_str_radix(f[4], 16).ok()?,
                state: u8::from_str_radix(f[5], 16).ok()?,
                inode: f[6].parse().ok()?,
                listening: flags & SO_ACCEPTCON != 0,
                path: f.get(7).map(|p| p.trim().to_owned()).unwrap_or_default(),
            })
        })
        .collect()
}

pub fn unix_type_name(t: u16) -> &'static str {
    match t {
        1 => "stream",
        2 => "dgram",
        5 => "seqpacket",
        _ => "unknown",
    }
}

/// `/proc/net/snmp` / `netstat`: header/value line pairs per protocol.
/// Returns `(protocol, field, value)`; mismatched pairs are skipped.
pub fn snmp(text: &str) -> Vec<(String, String, i64)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for pair in lines.chunks(2) {
        let [h, v] = pair else { continue };
        let (Some((hp, hf)), Some((vp, vf))) = (h.split_once(':'), v.split_once(':')) else {
            continue;
        };
        if hp != vp {
            continue;
        }
        let names: Vec<&str> = hf.split_whitespace().collect();
        let vals: Vec<&str> = vf.split_whitespace().collect();
        if names.len() != vals.len() {
            continue;
        }
        for (n, v) in names.iter().zip(vals) {
            if let Ok(v) = v.parse() {
                out.push((hp.to_owned(), (*n).to_owned(), v));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn net_dev_parses_names_with_unusual_characters() {
        let t = "Inter-|   Receive\n face |bytes\n    lo: 100 2 0 0 0 0 0 0 100 2 0 0 0 0 0 0\nwlp0s20f3: 5000 10 1 2 0 0 0 0 7000 9 3 4 0 0 0 0\n";
        let d = net_dev(t);
        assert_eq!(d.len(), 2);
        assert_eq!(
            (
                d[1].name.as_str(),
                d[1].rx_bytes,
                d[1].tx_bytes,
                d[1].tx_dropped
            ),
            ("wlp0s20f3", 5000, 7000, 4)
        );
    }

    #[test]
    fn inet_tables_decode_v4_and_v6() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1538 00000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 4242 1 0\n";
        let r = &inet_table(v4)[0];
        assert_eq!(r.local, "127.0.0.1:5432".parse().unwrap());
        assert_eq!((r.state, r.uid, r.inode), (10, 999, 4242));
        let v6 = "hdr\n   0: 00000000000000000000000001000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000001 00:00000000 00000000     0        0 77 1 0\n";
        let r6 = &inet_table(v6)[0];
        assert_eq!(r6.local, "[::1]:22".parse().unwrap());
        assert_eq!(r6.rx_queue, 1);
    }

    #[test]
    fn unix_table_paths_and_listening() {
        let t = "Num       RefCount Protocol Flags    Type St Inode Path\n0000000000000000: 00000002 00000000 00010000 0001 01 1234 /run/docker.sock\n0000000000000000: 00000003 00000000 00000000 0001 03 1235\n0000000000000000: 00000002 00000000 00000000 0002 01 99 @/tmp/.X11 with space\n";
        let u = unix_table(t);
        assert!(u[0].listening && u[0].path == "/run/docker.sock");
        assert_eq!((u[1].state, u[1].path.as_str()), (3, ""));
        assert_eq!(u[2].path, "@/tmp/.X11 with space");
    }

    #[test]
    fn snmp_pairs() {
        let t = "Tcp: RtoMin ActiveOpens RetransSegs\nTcp: 200 10 7\nUdp: InDatagrams\nUdp: 5 6\n";
        let s = snmp(t);
        assert_eq!(
            s,
            vec![
                ("Tcp".into(), "RtoMin".into(), 200),
                ("Tcp".into(), "ActiveOpens".into(), 10),
                ("Tcp".into(), "RetransSegs".into(), 7)
            ]
        );
    }

    proptest! {
        #[test]
        fn never_panic(s in "\\PC*") {
            let _ = net_dev(&s);
            let _ = inet_table(&s);
            let _ = unix_table(&s);
            let _ = snmp(&s);
        }
    }
}
