// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Minimal netlink dump client shared by the socket diagnostics and
//! nftables providers. Only read-only dump requests are ever sent.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::fs::KernelError;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_DUMP: u16 = 0x300;
/// Upper bound on one dump, so a pathological table cannot exhaust memory.
const MAX_DUMP_BYTES: usize = 256 << 20;

/// A netlink socket of one protocol with a receive timeout.
#[derive(Debug)]
pub struct Socket(OwnedFd);

impl Socket {
    pub fn open(protocol: i32, timeout: Duration) -> Result<Socket, KernelError> {
        // SAFETY: plain socket creation.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                protocol,
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
        Ok(Socket(fd))
    }

    /// Sends a `NLM_F_DUMP` request of `msg_type` and calls `each` with the
    /// payload of every reply of the same type until the dump ends.
    pub fn dump(
        &self,
        msg_type: u16,
        payload: &[u8],
        mut each: impl FnMut(&[u8]),
    ) -> Result<(), KernelError> {
        let len = 16 + payload.len();
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&(len as u32).to_ne_bytes());
        msg.extend_from_slice(&msg_type.to_ne_bytes());
        msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        msg.extend_from_slice(&1u32.to_ne_bytes()); // seq
        msg.extend_from_slice(&0u32.to_ne_bytes()); // pid: kernel assigns
        msg.extend_from_slice(payload);
        // SAFETY: zeroed sockaddr_nl addresses the kernel.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        // SAFETY: valid buffers and address.
        let r = unsafe {
            libc::sendto(
                self.0.as_raw_fd(),
                msg.as_ptr().cast(),
                msg.len(),
                0,
                (&addr as *const libc::sockaddr_nl).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };
        if r < 0 {
            return Err(KernelError::from_io(&std::io::Error::last_os_error()));
        }
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            // SAFETY: buf is valid for writes of its length.
            let n =
                unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
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
                            Err(KernelError::from_io(&std::io::Error::from_raw_os_error(
                                -code,
                            )))
                        };
                    }
                    t if t == msg_type => each(&buf[off + 16..off + len]),
                    _ => {}
                }
                off += (len + 3) & !3;
            }
        }
    }
}

/// Iterates `(type, payload)` attributes; the nested/byte-order flag bits
/// are masked off the type.
pub fn attrs(mut b: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
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

pub fn u32_ne(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_ne_bytes(s.try_into().expect("4 bytes")))
}

pub fn u64_ne(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_ne_bytes(s.try_into().expect("8 bytes")))
}

pub fn u32_be(b: &[u8]) -> Option<u32> {
    b.get(..4)
        .map(|s| u32::from_be_bytes(s.try_into().expect("4 bytes")))
}

pub fn u64_be(b: &[u8]) -> Option<u64> {
    b.get(..8)
        .map(|s| u64::from_be_bytes(s.try_into().expect("8 bytes")))
}

/// NUL-terminated netlink string attribute.
pub fn string(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(b)).into_owned()
}
