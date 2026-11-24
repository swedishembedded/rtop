// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Socket discovery: `NETLINK_SOCK_DIAG` with a procfs fallback.
//!
//! Netlink gives what procfs cannot: Unix socket peer inodes
//! (`UNIX_DIAG_PEER`, so local IPC relationships are kernel-confirmed) and
//! per-connection TCP byte counters from `tcp_info` (so per-socket and
//! per-process TCP throughput needs no eBPF). Only metadata is read; socket
//! payloads are never inspected.
//!
//! Both backends observe the network namespace rtop runs in.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use crate::fs::{KernelError, KernelFs, KernelFsExt};
use crate::parse::net;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InetProto {
    Tcp,
    Udp,
}

impl InetProto {
    pub fn as_str(self) -> &'static str {
        match self {
            InetProto::Tcp => "tcp",
            InetProto::Udp => "udp",
        }
    }
}

/// TCP connection statistics from `tcp_info`, where the kernel provides it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TcpCounters {
    pub bytes_acked: Option<u64>,
    pub bytes_received: Option<u64>,
    pub total_retrans: Option<u32>,
    pub rtt_us: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InetSocket {
    pub proto: InetProto,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub state: u8,
    pub rx_queue: u64,
    pub tx_queue: u64,
    pub uid: u32,
    pub inode: u64,
    pub tcp: Option<TcpCounters>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixSocket {
    pub inode: u64,
    pub sock_type: u16,
    pub state: u8,
    pub listening: bool,
    pub path: String,
    /// Inode of the connected peer, when the kernel reports it.
    pub peer: Option<u64>,
    pub rx_queue: Option<u32>,
    pub tx_queue: Option<u32>,
}

/// Source of socket tables.
pub trait SockDiag: Send + Sync + std::fmt::Debug {
    fn inet(&self, proto: InetProto) -> Result<Vec<InetSocket>, KernelError>;
    fn unix(&self) -> Result<Vec<UnixSocket>, KernelError>;
    /// Human-readable backend name (for collector diagnostics).
    fn backend(&self) -> &'static str;
}

/// Reads `/proc/net/*` through a [`KernelFs`]: works on fixtures and
/// everywhere, but has no Unix peers or TCP byte counters.
#[derive(Debug, Clone)]
pub struct ProcNetDiag {
    fs: Arc<dyn KernelFs>,
}

impl ProcNetDiag {
    pub fn new(fs: Arc<dyn KernelFs>) -> Self {
        ProcNetDiag { fs }
    }
}

const TABLE_LIMIT: usize = 64 << 20;

impl SockDiag for ProcNetDiag {
    fn inet(&self, proto: InetProto) -> Result<Vec<InetSocket>, KernelError> {
        let mut out = Vec::new();
        let mut any = false;
        for v in ["", "6"] {
            let path = format!("/proc/net/{}{v}", proto.as_str());
            match self.fs.read_string(&path, TABLE_LIMIT) {
                Ok(t) => {
                    any = true;
                    out.extend(net::inet_table(&t).into_iter().map(|r| InetSocket {
                        proto,
                        local: r.local,
                        remote: r.remote,
                        state: r.state,
                        rx_queue: r.rx_queue,
                        tx_queue: r.tx_queue,
                        uid: r.uid,
                        inode: r.inode,
                        tcp: None,
                    }))
                }
                Err(KernelError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        if any {
            Ok(out)
        } else {
            Err(KernelError::NotFound)
        }
    }

    fn unix(&self) -> Result<Vec<UnixSocket>, KernelError> {
        let t = self.fs.read_string("/proc/net/unix", TABLE_LIMIT)?;
        Ok(net::unix_table(&t)
            .into_iter()
            .map(|r| UnixSocket {
                inode: r.inode,
                sock_type: r.sock_type,
                state: r.state,
                listening: r.listening,
                path: r.path,
                peer: None,
                rx_queue: None,
                tx_queue: None,
            })
            .collect())
    }

    fn backend(&self) -> &'static str {
        "procfs"
    }
}

/// `NETLINK_SOCK_DIAG` backend, falling back to procfs per family when
/// netlink is refused (e.g. restrictive seccomp profiles).
#[derive(Debug)]
pub struct NetlinkDiag {
    fallback: ProcNetDiag,
    timeout: Duration,
}

impl NetlinkDiag {
    pub fn new(fs: Arc<dyn KernelFs>) -> Self {
        NetlinkDiag {
            fallback: ProcNetDiag::new(fs),
            timeout: Duration::from_secs(2),
        }
    }
}

impl SockDiag for NetlinkDiag {
    fn inet(&self, proto: InetProto) -> Result<Vec<InetSocket>, KernelError> {
        let mut out = Vec::new();
        for family in [libc::AF_INET, libc::AF_INET6] {
            match nl::dump_inet(family as u8, proto, self.timeout) {
                Ok(v) => out.extend(v),
                Err(_) => return self.fallback.inet(proto),
            }
        }
        Ok(out)
    }

    fn unix(&self) -> Result<Vec<UnixSocket>, KernelError> {
        nl::dump_unix(self.timeout).or_else(|_| self.fallback.unix())
    }

    fn backend(&self) -> &'static str {
        "netlink"
    }
}

/// Raw netlink encoding/decoding. Message layouts follow
/// `include/uapi/linux/{netlink,sock_diag,inet_diag,unix_diag,tcp}.h`.
mod nl {
    use super::*;

    const SOCK_DIAG_BY_FAMILY: u16 = 20;
    const NLMSG_ERROR: u16 = 2;
    const NLMSG_DONE: u16 = 3;
    const NLM_F_REQUEST: u16 = 1;
    const NLM_F_DUMP: u16 = 0x300;
    const INET_DIAG_INFO: u16 = 2;
    const UNIX_DIAG_NAME: u16 = 0;
    const UNIX_DIAG_PEER: u16 = 2;
    const UNIX_DIAG_RQLEN: u16 = 4;
    const UDIAG_SHOW_NAME: u32 = 1;
    const UDIAG_SHOW_PEER: u32 = 4;
    const UDIAG_SHOW_RQLEN: u32 = 16;
    /// Upper bound on one dump, so a pathological table cannot exhaust memory.
    const MAX_DUMP_BYTES: usize = 256 << 20;

    fn open(timeout: Duration) -> Result<OwnedFd, KernelError> {
        // SAFETY: plain socket creation.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::NETLINK_SOCK_DIAG,
            )
        };
        if fd < 0 {
            return Err(KernelError::from_io(&std::io::Error::last_os_error()));
        }
        // SAFETY: fresh fd owned by us.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        // SAFETY: valid fd and correctly sized option value.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&tv as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as u32,
            );
        }
        Ok(fd)
    }

    fn request(fd: &OwnedFd, payload: &[u8]) -> Result<(), KernelError> {
        let len = 16 + payload.len();
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&(len as u32).to_ne_bytes());
        msg.extend_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        msg.extend_from_slice(&1u32.to_ne_bytes()); // seq
        msg.extend_from_slice(&0u32.to_ne_bytes()); // pid (kernel assigns)
        msg.extend_from_slice(payload);
        // SAFETY: zeroed sockaddr_nl addresses the kernel.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        // SAFETY: valid buffers and address.
        let r = unsafe {
            libc::sendto(
                fd.as_raw_fd(),
                msg.as_ptr().cast(),
                msg.len(),
                0,
                (&addr as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if r < 0 {
            Err(KernelError::from_io(&std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    /// Receives a dump, calling `each(payload)` for every SOCK_DIAG message.
    fn receive(fd: &OwnedFd, mut each: impl FnMut(&[u8])) -> Result<(), KernelError> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            // SAFETY: buf is valid for writes of its length.
            let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                return Err(KernelError::from_io(&std::io::Error::last_os_error()));
            }
            let n = n as usize;
            total += n;
            if total > MAX_DUMP_BYTES {
                return Err(KernelError::TooLarge {
                    limit: MAX_DUMP_BYTES,
                });
            }
            let mut off = 0;
            while off + 16 <= n {
                let len =
                    u32::from_ne_bytes(buf[off..off + 4].try_into().expect("4 bytes")) as usize;
                let ty = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().expect("2 bytes"));
                if len < 16 || off + len > n {
                    return Err(KernelError::Invalid("netlink length"));
                }
                match ty {
                    NLMSG_DONE => return Ok(()),
                    NLMSG_ERROR => {
                        let code = buf
                            .get(off + 16..off + 20)
                            .map(|b| i32::from_ne_bytes(b.try_into().expect("4 bytes")))
                            .unwrap_or(-1);
                        return if code == 0 {
                            Ok(())
                        } else {
                            Err(KernelError::Os(-code))
                        };
                    }
                    SOCK_DIAG_BY_FAMILY => each(&buf[off + 16..off + len]),
                    _ => {}
                }
                off += (len + 3) & !3;
            }
        }
    }

    /// Iterates `(type, payload)` route attributes.
    fn attrs(mut b: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
        std::iter::from_fn(move || {
            if b.len() < 4 {
                return None;
            }
            let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
            let ty = u16::from_ne_bytes([b[2], b[3]]);
            if len < 4 || len > b.len() {
                return None;
            }
            let payload = &b[4..len];
            b = &b[((len + 3) & !3).min(b.len())..];
            Some((ty & 0x3fff, payload))
        })
    }

    fn u32_at(b: &[u8], off: usize) -> Option<u32> {
        b.get(off..off + 4)
            .map(|s| u32::from_ne_bytes(s.try_into().expect("4 bytes")))
    }

    fn u64_at(b: &[u8], off: usize) -> Option<u64> {
        b.get(off..off + 8)
            .map(|s| u64::from_ne_bytes(s.try_into().expect("8 bytes")))
    }

    fn addr(family: u8, port_be: &[u8], ip: &[u8]) -> Option<SocketAddr> {
        let port = u16::from_be_bytes([port_be[0], port_be[1]]);
        let ip = if i32::from(family) == libc::AF_INET {
            IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]))
        } else {
            IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(ip.get(..16)?).ok()?))
        };
        Some(SocketAddr::new(ip, port))
    }

    /// Decodes one `inet_diag_msg` (+ attributes). Public for fuzzing.
    pub fn decode_inet(proto: InetProto, m: &[u8]) -> Option<InetSocket> {
        if m.len() < 72 {
            return None;
        }
        let family = m[0];
        let id = &m[4..52];
        let local = addr(family, &id[0..2], &id[4..20])?;
        let remote = addr(family, &id[2..4], &id[20..36])?;
        let mut tcp = None;
        for (ty, p) in attrs(&m[72..]) {
            if ty == INET_DIAG_INFO && proto == InetProto::Tcp {
                // struct tcp_info offsets; fields absent on older kernels
                // simply stay None.
                tcp = Some(TcpCounters {
                    rtt_us: u32_at(p, 68),
                    total_retrans: u32_at(p, 100),
                    bytes_acked: u64_at(p, 120),
                    bytes_received: u64_at(p, 128),
                });
            }
        }
        Some(InetSocket {
            proto,
            local,
            remote,
            state: m[1],
            rx_queue: u64::from(u32_at(m, 56)?),
            tx_queue: u64::from(u32_at(m, 60)?),
            uid: u32_at(m, 64)?,
            inode: u64::from(u32_at(m, 68)?),
            tcp,
        })
    }

    /// Decodes one `unix_diag_msg` (+ attributes). Public for fuzzing.
    pub fn decode_unix(m: &[u8]) -> Option<UnixSocket> {
        if m.len() < 16 {
            return None;
        }
        let mut s = UnixSocket {
            inode: u64::from(u32_at(m, 4)?),
            sock_type: u16::from(m[1]),
            state: m[2],
            listening: m[2] == 10, // TCP_LISTEN
            path: String::new(),
            peer: None,
            rx_queue: None,
            tx_queue: None,
        };
        for (ty, p) in attrs(&m[16..]) {
            match ty {
                UNIX_DIAG_NAME => {
                    s.path = if p.first() == Some(&0) {
                        format!("@{}", String::from_utf8_lossy(&p[1..]))
                    } else {
                        String::from_utf8_lossy(p.split(|&b| b == 0).next().unwrap_or(p))
                            .into_owned()
                    }
                }
                UNIX_DIAG_PEER => s.peer = u32_at(p, 0).filter(|v| *v != 0).map(u64::from),
                UNIX_DIAG_RQLEN => {
                    s.rx_queue = u32_at(p, 0);
                    s.tx_queue = u32_at(p, 4);
                }
                _ => {}
            }
        }
        Some(s)
    }

    pub fn dump_inet(
        family: u8,
        proto: InetProto,
        timeout: Duration,
    ) -> Result<Vec<InetSocket>, KernelError> {
        let fd = open(timeout)?;
        let mut req = vec![0u8; 56];
        req[0] = family;
        req[1] = match proto {
            InetProto::Tcp => libc::IPPROTO_TCP as u8,
            InetProto::Udp => libc::IPPROTO_UDP as u8,
        };
        req[2] = 1 << (INET_DIAG_INFO - 1); // idiag_ext
        req[4..8].copy_from_slice(&u32::MAX.to_ne_bytes()); // all states
        req[48..56].copy_from_slice(&[0xff; 8]); // INET_DIAG_NOCOOKIE
        request(&fd, &req)?;
        let mut out = Vec::new();
        receive(&fd, |m| out.extend(decode_inet(proto, m)))?;
        Ok(out)
    }

    pub fn dump_unix(timeout: Duration) -> Result<Vec<UnixSocket>, KernelError> {
        let fd = open(timeout)?;
        let mut req = vec![0u8; 24];
        req[0] = libc::AF_UNIX as u8;
        req[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());
        req[12..16]
            .copy_from_slice(&(UDIAG_SHOW_NAME | UDIAG_SHOW_PEER | UDIAG_SHOW_RQLEN).to_ne_bytes());
        req[16..24].copy_from_slice(&[0xff; 8]);
        request(&fd, &req)?;
        let mut out = Vec::new();
        receive(&fd, |m| out.extend(decode_unix(m)))?;
        Ok(out)
    }
}

pub use nl::{decode_inet, decode_unix};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memfs::MemoryKernelFs;
    use proptest::prelude::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn netlink_resolves_unix_socketpair_peers() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let ino = |s: &std::os::unix::net::UnixStream| {
            std::fs::metadata(format!("/proc/self/fd/{}", s.as_raw_fd()))
                .unwrap()
                .ino()
        };
        let (ia, ib) = (ino(&a), ino(&b));
        // Where netlink is permitted the kernel must confirm the peer.
        let Ok(socks) = nl::dump_unix(Duration::from_secs(2)) else {
            return;
        };
        let sa = socks
            .iter()
            .find(|s| s.inode == ia)
            .expect("socket a listed");
        assert_eq!(sa.peer, Some(ib));
        assert_eq!(sa.sock_type, 1);
        drop(b);
    }

    #[test]
    fn netlink_reports_tcp_byte_counters_for_loopback() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut s, _) = l.accept().unwrap();
        c.write_all(&[7u8; 10_000]).unwrap();
        let mut buf = vec![0u8; 10_000];
        s.read_exact(&mut buf).unwrap();
        let socks = nl::dump_inet(libc::AF_INET as u8, InetProto::Tcp, Duration::from_secs(2));
        let Ok(socks) = socks else { return }; // netlink unavailable in this sandbox
        let client = socks
            .iter()
            .find(|x| x.local == c.local_addr().unwrap())
            .expect("client socket");
        assert_eq!(client.state, 1);
        let acked = client
            .tcp
            .and_then(|t| t.bytes_acked)
            .expect("tcp_info present");
        assert!(acked >= 10_000, "bytes_acked {acked}");
        let server = socks
            .iter()
            .find(|x| x.local == s.local_addr().unwrap() && x.state == 1)
            .expect("server socket");
        assert!(server.tcp.and_then(|t| t.bytes_received).unwrap() >= 10_000);
    }

    #[test]
    fn procfs_fallback_parses_fixture_tables() {
        let fs = MemoryKernelFs::new();
        fs.file("/proc/net/tcp", "hdr\n   0: 0100007F:1538 00000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 4242 1 0\n");
        fs.file(
            "/proc/net/unix",
            "hdr\n0000000000000000: 00000002 00000000 00010000 0001 01 1234 /run/x.sock\n",
        );
        let d = ProcNetDiag::new(Arc::new(fs));
        assert_eq!(d.inet(InetProto::Tcp).unwrap()[0].inode, 4242);
        assert_eq!(d.inet(InetProto::Udp), Err(KernelError::NotFound));
        assert!(d.unix().unwrap()[0].listening);
    }

    proptest! {
        #[test]
        fn decoders_never_panic(b in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = decode_inet(InetProto::Tcp, &b);
            let _ = decode_unix(&b);
        }
    }
}
